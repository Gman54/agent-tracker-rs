; Houndbit setup hooks.
; Tauri's NSIS template includes this file near the top (bundle.windows.nsis.installerHooks),
; before any pages are defined, so the finish page wording below replaces the defaults.

; ---- Finish page ----------------------------------------------------------------------------
!define MUI_FINISHPAGE_TITLE "Houndbit is installed"
!define MUI_FINISHPAGE_TEXT "Houndbit has been installed on this PC and is ready to use.$\r$\n$\r$\nYou can open it any time from the Start menu. While it is running, it keeps a small icon near the clock.$\r$\n$\r$\nClick Finish to close Setup."
!define MUI_FINISHPAGE_RUN_TEXT "Start Houndbit now"

; ---- Close a running Houndbit in friendly words ----------------------------------------
; Houndbit lives in the tray, so people often still have it open when they reinstall or
; uninstall. Its files are locked while it runs. Ask first, then close it (and its little hook
; helper) so Setup never stops halfway with a "can't write file" error.
!macro AGENTTRACKER_CLOSE_RUNNING what
  nsExec::ExecToStack '"$SYSDIR\cmd.exe" /C ""$SYSDIR\tasklist.exe" /NH /FI "IMAGENAME eq agenttracker.exe" | "$SYSDIR\find.exe" /I "agenttracker.exe""'
  Pop $0
  Pop $1
  ${If} $0 == 0
    ${IfNot} ${Silent}
    ${AndIf} $PassiveMode != 1
      MessageBox MB_OKCANCEL|MB_ICONINFORMATION "Houndbit is open right now (look for its small icon near the clock).$\r$\n$\r$\nSetup needs to close it so it can ${what}. Your saved checks and settings are kept.$\r$\n$\r$\nClick OK to close Houndbit and carry on, or Cancel to stop Setup." /SD IDOK IDOK agenttracker_close_now
      Abort "Setup was stopped, so Houndbit was left running. Nothing was changed."
    ${EndIf}
    agenttracker_close_now:
    DetailPrint "Closing Houndbit..."
    nsExec::Exec '"$SYSDIR\taskkill.exe" /F /T /IM agenttracker.exe'
    Pop $0
    ; Give Windows a moment to let go of the files.
    Sleep 1500
  ${EndIf}
  ; The hook helper only runs for a moment at a time, but make sure none is holding its file.
  nsExec::Exec '"$SYSDIR\taskkill.exe" /F /IM agenttracker-hook.exe'
  Pop $0
!macroend

!macro NSIS_HOOK_PREINSTALL
  !insertmacro AGENTTRACKER_CLOSE_RUNNING "finish installing"
!macroend

; ---- Tidy up an older install made under a previous name ----------------------------------
; The app was called AgentTracker, then TrailFox, and is now Houndbit. Each rename makes Windows
; see a new program in a new folder, so quietly run the old uninstaller to leave one copy, one
; Start menu entry and one Apps-list entry. It only removes the old program files and shortcuts:
; in silent mode the "delete app data" box is never ticked, so saved checks, recordings and
; settings (kept in %LOCALAPPDATA%\com.agenttracker.app, which Houndbit still uses) are left alone.
!macro HOUNDBIT_REMOVE_OLD_INSTALL oldname
  ReadRegStr $R8 HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\${oldname}" "InstallLocation"
  StrCpy $R7 $R8 1
  ${If} $R7 == '"'
    StrCpy $R8 $R8 -1 1 ; Tauri stores this path wrapped in quotes
  ${EndIf}
  ${If} $R8 == ""
    StrCpy $R8 "$LOCALAPPDATA\${oldname}"
  ${EndIf}
  ${If} $R8 != "$INSTDIR"
  ${AndIf} ${FileExists} "$R8\uninstall.exe"
    DetailPrint "Removing the older ${oldname} install (your data is kept)..."
    ExecWait '"$R8\uninstall.exe" /S _?=$R8' $R9
    Delete "$R8\uninstall.exe"
    RMDir "$R8"
  ${EndIf}
!macroend

!macro NSIS_HOOK_POSTINSTALL
  !insertmacro HOUNDBIT_REMOVE_OLD_INSTALL "AgentTracker"
  !insertmacro HOUNDBIT_REMOVE_OLD_INSTALL "TrailFox"

  ; Go straight to the finish page instead of stopping on the progress list with a bare
  ; "Completed" line (Tauri's template keeps that page open by default).
  SetAutoClose true
!macroend

!macro NSIS_HOOK_PREUNINSTALL
  !insertmacro AGENTTRACKER_CLOSE_RUNNING "remove it"
!macroend
