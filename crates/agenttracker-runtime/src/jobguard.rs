//! Locked-job launcher.
//!
//! This lets Houndbit start an AI tool (for example a coding CLI such as
//! `claude`, `cursor`, or even `powershell`) inside a locked Windows "job".
//! A job is a Windows container for one or more processes. We use it to:
//!
//!   * kill the whole tree the moment Houndbit (or the job handle) goes away
//!     (`JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`), so nothing is left running
//!     behind your back;
//!   * optionally cap how much memory the tool may use;
//!   * optionally stop the tool from starting *any* other program at all
//!     (the "no child processes" lever), which is the real "it cannot run
//!     other programs" switch.
//!
//! Honest limits (read these):
//!   * This only covers tools that Houndbit launches *itself*. A tool you
//!     started from the Start menu, or one that is already running, is not
//!     inside this job and is not affected.
//!   * The Windows code can be compiled and checked on Linux (we
//!     cross-compile it), but it can only actually *run* on Windows. It has
//!     not been run on a real Windows box from here.
//!   * "No child processes" is strict. Some AI CLIs legitimately shell out to
//!     `git`, `python`, etc. Turning this on will break those. It is offered
//!     as an optional strict mode, off by default.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

/// What the caller wants to launch.
#[derive(Debug, Clone, Deserialize)]
pub struct LaunchSpec {
    /// Program to run. May be a bare name on PATH ("powershell") or a full
    /// path ("C:/Users/Greg/claude.exe").
    pub program: String,
    /// Extra command-line arguments, already split into pieces.
    #[serde(default)]
    pub args: Vec<String>,
    /// Folder to start in. `None` means "wherever Houndbit is".
    #[serde(default)]
    pub working_dir: Option<String>,
    /// Memory cap in megabytes. `None` means no cap.
    #[serde(default)]
    pub memory_mb: Option<u64>,
    /// If true, the launched tool may not start any other program.
    #[serde(default)]
    pub block_child_processes: bool,
}

/// Metadata about one running locked job. Safe to show in the UI.
#[derive(Debug, Clone, Serialize)]
pub struct RunningJob {
    pub pid: u32,
    pub program: String,
    pub args: Vec<String>,
    pub memory_mb: Option<u64>,
    pub no_child_processes: bool,
    pub started_unix: u64,
}

/// Returned once a launch succeeds.
#[derive(Debug, Clone, Serialize)]
pub struct LaunchOutcome {
    pub pid: u32,
    pub no_child_processes: bool,
    pub memory_mb: Option<u64>,
    pub note: String,
}

/// One entry in the live registry. On Windows it also carries the raw handles
/// so we can stop the job later; on other platforms it is just the metadata.
struct JobEntry {
    meta: RunningJob,
    #[cfg(windows)]
    handles: win::WinHandles,
}

fn registry() -> &'static Mutex<HashMap<u32, JobEntry>> {
    static REG: OnceLock<Mutex<HashMap<u32, JobEntry>>> = OnceLock::new();
    REG.get_or_init(|| Mutex::new(HashMap::new()))
}

#[allow(dead_code)] // used on Windows and in tests
fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Memory cap bounds, in megabytes. Keeps the UI from sending silly values.
const MIN_MEMORY_MB: u64 = 64;
const MAX_MEMORY_MB: u64 = 65_536;

/// Validate a spec before we try to launch. This is deliberately
/// platform-independent so it can be unit-tested on Linux.
pub fn validate_spec(spec: &LaunchSpec) -> Result<(), String> {
    if spec.program.trim().is_empty() {
        return Err("No program chosen to launch.".to_string());
    }
    if let Some(dir) = &spec.working_dir {
        if dir.trim().is_empty() {
            return Err("The working folder is empty. Leave it blank or give a real folder.".to_string());
        }
    }
    if let Some(mb) = spec.memory_mb {
        if mb < MIN_MEMORY_MB {
            return Err(format!("Memory limit is too low; use at least {MIN_MEMORY_MB} MB."));
        }
        if mb > MAX_MEMORY_MB {
            return Err(format!("Memory limit is too high; keep it at or under {MAX_MEMORY_MB} MB."));
        }
    }
    Ok(())
}

/// List the locked jobs we currently have running, newest first.
pub fn list_running() -> Vec<RunningJob> {
    let reg = registry().lock().unwrap();
    let mut out: Vec<RunningJob> = reg.values().map(|e| e.meta.clone()).collect();
    out.sort_by(|a, b| b.started_unix.cmp(&a.started_unix).then(a.pid.cmp(&b.pid)));
    out
}

/// Launch a program inside a fresh locked job.
///
/// On non-Windows builds this returns an error explaining it is Windows-only,
/// after validating the spec (so the surrounding logic is still exercised).
pub fn launch_locked(spec: LaunchSpec) -> Result<LaunchOutcome, String> {
    validate_spec(&spec)?;
    #[cfg(windows)]
    {
        win::launch(spec)
    }
    #[cfg(not(windows))]
    {
        let _ = spec;
        Err("Locked-job launching is a Windows-only feature. This build is not Windows, so there is nothing to launch here.".to_string())
    }
}

/// Stop a running locked job by process id. Terminates the whole job tree.
pub fn stop(pid: u32) -> Result<(), String> {
    #[cfg(windows)]
    {
        win::stop(pid)
    }
    #[cfg(not(windows))]
    {
        let mut reg = registry().lock().unwrap();
        if reg.remove(&pid).is_some() {
            Ok(())
        } else {
            Err(format!("No locked job with id {pid} is being tracked."))
        }
    }
}

// Test-only helper so the registry bookkeeping (list/stop) can be checked on
// Linux, where we never actually launch anything.
#[cfg(all(test, not(windows)))]
fn register_for_test(meta: RunningJob) {
    let pid = meta.pid;
    registry().lock().unwrap().insert(pid, JobEntry { meta });
}

#[cfg(windows)]
mod win {
    use super::{now_unix, registry, JobEntry, LaunchOutcome, LaunchSpec, RunningJob};
    use std::mem::{size_of, zeroed};
    use std::ptr::{null, null_mut};
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, SetInformationJobObject, TerminateJobObject,
        JobObjectExtendedLimitInformation, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOB_OBJECT_LIMIT_PROCESS_MEMORY,
    };
    use windows_sys::Win32::System::Threading::{
        CreateProcessW, DeleteProcThreadAttributeList, InitializeProcThreadAttributeList,
        ResumeThread, UpdateProcThreadAttribute, CREATE_SUSPENDED, EXTENDED_STARTUPINFO_PRESENT,
        PROCESS_INFORMATION, PROC_THREAD_ATTRIBUTE_CHILD_PROCESS_POLICY, STARTUPINFOEXW,
        STARTUPINFOW,
    };
    use windows_sys::Win32::System::WindowsProgramming::PROCESS_CREATION_CHILD_PROCESS_RESTRICTED;

    /// Raw handles for a launched job. We own them and close them on stop.
    pub(super) struct WinHandles {
        job: HANDLE,
        process: HANDLE,
    }
    // The handles are only ever touched while holding the registry lock.
    unsafe impl Send for WinHandles {}

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// Standard Windows command-line quoting (same rules as CommandLineToArgvW).
    fn append_quoted(arg: &str, cmd: &mut String) {
        if !arg.is_empty() && !arg.contains([' ', '\t', '"']) {
            cmd.push_str(arg);
            return;
        }
        cmd.push('"');
        let mut chars = arg.chars().peekable();
        loop {
            let mut backslashes = 0usize;
            while let Some('\\') = chars.peek() {
                chars.next();
                backslashes += 1;
            }
            match chars.next() {
                Some('"') => {
                    cmd.extend(std::iter::repeat('\\').take(backslashes * 2 + 1));
                    cmd.push('"');
                }
                Some(c) => {
                    cmd.extend(std::iter::repeat('\\').take(backslashes));
                    cmd.push(c);
                }
                None => {
                    cmd.extend(std::iter::repeat('\\').take(backslashes * 2));
                    break;
                }
            }
        }
        cmd.push('"');
    }

    fn build_command_line(spec: &LaunchSpec) -> Vec<u16> {
        let mut cmd = String::new();
        append_quoted(spec.program.trim(), &mut cmd);
        for a in &spec.args {
            cmd.push(' ');
            append_quoted(a, &mut cmd);
        }
        wide(&cmd)
    }

    pub(super) fn launch(spec: LaunchSpec) -> Result<LaunchOutcome, String> {
        unsafe {
            // 1. Create the job.
            let job: HANDLE = CreateJobObjectW(null(), null());
            if job == 0 {
                return Err("Could not create the Windows job object.".to_string());
            }

            // 2. Set the limits: always kill-on-close, optionally a memory cap.
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = zeroed();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            if let Some(mb) = spec.memory_mb {
                info.BasicLimitInformation.LimitFlags |= JOB_OBJECT_LIMIT_PROCESS_MEMORY;
                info.ProcessMemoryLimit = (mb as usize).saturating_mul(1024 * 1024);
            }
            let ok = SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                &info as *const _ as *const core::ffi::c_void,
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            );
            if ok == 0 {
                CloseHandle(job);
                return Err("Could not apply limits to the job object.".to_string());
            }

            // 3. Build the command line and (optionally) the child-process
            //    restriction attribute list.
            let mut cmdline = build_command_line(&spec);
            let cwd_wide = spec.working_dir.as_ref().map(|d| wide(d.trim()));
            let cwd_ptr = cwd_wide.as_ref().map(|v| v.as_ptr()).unwrap_or(null());

            let mut pi: PROCESS_INFORMATION = zeroed();
            let mut flags = CREATE_SUSPENDED;

            // Attribute-list buffer must outlive CreateProcessW.
            let mut attr_buf: Vec<u8> = Vec::new();

            let create_ok = if spec.block_child_processes {
                // Ask how big the attribute list must be, then allocate it.
                let mut size: usize = 0;
                InitializeProcThreadAttributeList(null_mut(), 1, 0, &mut size);
                if size == 0 {
                    CloseHandle(job);
                    return Err("Could not size the process attribute list.".to_string());
                }
                attr_buf.resize(size, 0);
                let attr_list = attr_buf.as_mut_ptr() as *mut core::ffi::c_void;
                if InitializeProcThreadAttributeList(attr_list, 1, 0, &mut size) == 0 {
                    CloseHandle(job);
                    return Err("Could not build the process attribute list.".to_string());
                }
                let policy: u32 = PROCESS_CREATION_CHILD_PROCESS_RESTRICTED;
                let upd = UpdateProcThreadAttribute(
                    attr_list,
                    0,
                    PROC_THREAD_ATTRIBUTE_CHILD_PROCESS_POLICY as usize,
                    &policy as *const u32 as *const core::ffi::c_void,
                    size_of::<u32>(),
                    null_mut(),
                    null(),
                );
                if upd == 0 {
                    DeleteProcThreadAttributeList(attr_list);
                    CloseHandle(job);
                    return Err("Could not set the no-child-processes policy.".to_string());
                }

                let mut si: STARTUPINFOEXW = zeroed();
                si.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
                si.lpAttributeList = attr_list;
                flags |= EXTENDED_STARTUPINFO_PRESENT;

                let r = CreateProcessW(
                    null(),
                    cmdline.as_mut_ptr(),
                    null(),
                    null(),
                    0,
                    flags,
                    null(),
                    cwd_ptr,
                    &si.StartupInfo as *const STARTUPINFOW,
                    &mut pi,
                );
                DeleteProcThreadAttributeList(attr_list);
                r
            } else {
                let mut si: STARTUPINFOW = zeroed();
                si.cb = size_of::<STARTUPINFOW>() as u32;
                CreateProcessW(
                    null(),
                    cmdline.as_mut_ptr(),
                    null(),
                    null(),
                    0,
                    flags,
                    null(),
                    cwd_ptr,
                    &si as *const STARTUPINFOW,
                    &mut pi,
                )
            };

            if create_ok == 0 {
                CloseHandle(job);
                return Err(format!(
                    "Could not start \"{}\". Check the program name or path.",
                    spec.program.trim()
                ));
            }

            // 4. Put the new process into the job, then let it run.
            if AssignProcessToJobObject(job, pi.hProcess) == 0 {
                // Could not lock it down; do not leave it running outside a job.
                windows_sys::Win32::System::Threading::TerminateProcess(pi.hProcess, 1);
                CloseHandle(pi.hThread);
                CloseHandle(pi.hProcess);
                CloseHandle(job);
                return Err("Could not put the program into the locked job; it was stopped.".to_string());
            }
            ResumeThread(pi.hThread);
            CloseHandle(pi.hThread);

            let pid = pi.dwProcessId;
            let note = if spec.block_child_processes {
                "Started inside a locked job. It will be killed if Houndbit closes, and it cannot start other programs."
            } else {
                "Started inside a locked job. It will be killed if Houndbit closes."
            }
            .to_string();

            let meta = RunningJob {
                pid,
                program: spec.program.trim().to_string(),
                args: spec.args.clone(),
                memory_mb: spec.memory_mb,
                no_child_processes: spec.block_child_processes,
                started_unix: now_unix(),
            };
            let outcome = LaunchOutcome {
                pid,
                no_child_processes: spec.block_child_processes,
                memory_mb: spec.memory_mb,
                note,
            };
            registry().lock().unwrap().insert(
                pid,
                JobEntry {
                    meta,
                    handles: WinHandles {
                        job,
                        process: pi.hProcess,
                    },
                },
            );
            Ok(outcome)
        }
    }

    pub(super) fn stop(pid: u32) -> Result<(), String> {
        let entry = registry().lock().unwrap().remove(&pid);
        match entry {
            Some(e) => unsafe {
                // Terminating the job kills every process inside it.
                TerminateJobObject(e.handles.job, 1);
                CloseHandle(e.handles.process);
                CloseHandle(e.handles.job);
                Ok(())
            },
            None => Err(format!("No locked job with id {pid} is being tracked.")),
        }
    }
}

#[cfg(all(test, not(windows)))]
mod tests {
    use super::*;

    fn spec(program: &str) -> LaunchSpec {
        LaunchSpec {
            program: program.to_string(),
            args: vec![],
            working_dir: None,
            memory_mb: None,
            block_child_processes: false,
        }
    }

    #[test]
    fn empty_program_rejected() {
        assert!(validate_spec(&spec("   ")).is_err());
    }

    #[test]
    fn plain_program_ok() {
        assert!(validate_spec(&spec("powershell")).is_ok());
    }

    #[test]
    fn empty_working_dir_rejected() {
        let mut s = spec("claude");
        s.working_dir = Some("   ".to_string());
        assert!(validate_spec(&s).is_err());
    }

    #[test]
    fn blank_working_dir_as_none_is_fine() {
        let mut s = spec("claude");
        s.working_dir = Some("F:/Test".to_string());
        assert!(validate_spec(&s).is_ok());
    }

    #[test]
    fn memory_bounds_enforced() {
        let mut s = spec("claude");
        s.memory_mb = Some(1);
        assert!(validate_spec(&s).is_err());
        s.memory_mb = Some(MAX_MEMORY_MB + 1);
        assert!(validate_spec(&s).is_err());
        s.memory_mb = Some(2048);
        assert!(validate_spec(&s).is_ok());
    }

    #[test]
    fn launch_is_windows_only_off_windows() {
        let err = launch_locked(spec("powershell")).unwrap_err();
        assert!(err.to_lowercase().contains("windows-only"));
    }

    #[test]
    fn registry_list_and_stop_bookkeeping() {
        let pid = 424242;
        register_for_test(RunningJob {
            pid,
            program: "claude".to_string(),
            args: vec!["--help".to_string()],
            memory_mb: Some(2048),
            no_child_processes: true,
            started_unix: now_unix(),
        });
        assert!(list_running().iter().any(|j| j.pid == pid));
        assert!(stop(pid).is_ok());
        assert!(!list_running().iter().any(|j| j.pid == pid));
        // Stopping again is a clean error, not a panic.
        assert!(stop(pid).is_err());
    }
}
