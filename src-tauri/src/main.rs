//! TrailFox desktop shell: starts the collector + 127.0.0.1-only backend, opens the UI in a
//! native window and keeps a tray icon (Open / Pause Collection / Quit).
#![cfg_attr(all(not(debug_assertions), target_os = "windows"), windows_subsystem = "windows")]

use agenttracker_core::collector::{Collector, CollectorDiscovery, FALLBACK_FILE};
use agenttracker_core::db::Db;
use agenttracker_core::engine::Engine;
use agenttracker_core::server::{serve_with, AppState};
use agenttracker_core::{agenttracker_home, APP_NAME, VERSION};
use std::path::PathBuf;
use std::sync::Arc;
use tauri::menu::{Menu, MenuItem};
use tauri::tray::{MouseButton, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Manager, WebviewUrl, WebviewWindowBuilder, WindowEvent};
use tracker_common::security;

const TRAY_ID: &str = "agenttracker-tray";

/// Matches Tauri's `$APPLOCALDATA` for identifier `com.agenttracker.app`, so the fs scope in
/// `capabilities/default.json` covers exactly this folder. `AGENTTRACKER_DATA_DIR` overrides it.
fn app_data_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("AGENTTRACKER_DATA_DIR").filter(|v| !v.is_empty()) {
        return d.into();
    }
    let base = std::env::var_os("LOCALAPPDATA")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("XDG_DATA_HOME").filter(|v| !v.is_empty()).map(PathBuf::from))
        .unwrap_or_else(|| agenttracker_core::user_home().join(".local").join("share"));
    base.join("com.agenttracker.app")
}

/// The hook ships next to the main executable (Tauri `externalBin`).
fn hook_binary() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let name = if cfg!(windows) { "agenttracker-hook.exe" } else { "agenttracker-hook" };
    Some(exe.parent()?.join(name))
}

fn bind_loopback() -> std::io::Result<std::net::TcpListener> {
    // Port 0 = random free port; loopback only.
    let l = std::net::TcpListener::bind(("127.0.0.1", 0))?;
    l.set_nonblocking(true)?;
    Ok(l)
}

struct Runtime {
    state: Arc<AppState>,
    port: u16,
    home: PathBuf,
}

/// Opens the DB, starts the collector worker thread and writes the hook discovery file.
fn build_runtime(port: u16, launch_token: String) -> Runtime {
    let data_dir = app_data_dir();
    let home = agenttracker_home();
    std::fs::create_dir_all(&data_dir).expect("create data dir");
    std::fs::create_dir_all(&home).expect("create ~/.agenttracker");
    let db = Db::open(&data_dir.join("agenttracker.db")).expect("open database");

    let collector = Collector::new(&db);
    let paused = db.get_setting("paused").ok().flatten().as_deref() == Some("1");
    collector.set_paused(paused);

    // Never snapshot our own database or connector state. (Not the whole data dir: the demo
    // task's sample folder lives inside it and must be snapshotted.)
    let excludes = ["agenttracker.db", "agenttracker.db-wal", "agenttracker.db-shm", "agenttracker.db-journal"]
        .iter()
        .map(|f| data_dir.join(f))
        .chain(std::iter::once(home.clone()))
        .collect();
    let engine = Engine::new(db, excludes);
    // Lets agent prompts start automatic tasks; the worker starts after this so queued
    // offline events are handled the same way.
    collector.set_engine(engine.clone());
    collector.spawn_worker(home.join(FALLBACK_FILE));

    let secret = security::token_urlsafe(32);
    let discovery = CollectorDiscovery { port, secret: secret.clone(), pid: std::process::id(), started_at: tracker_common::now_ms() };
    if let Err(e) = discovery.write(&home) {
        eprintln!("could not write collector discovery file: {e}");
    }
    // Tasks that were running when the app last closed: restart observers and record the gap.
    match engine.resume_running() {
        Ok(ids) if !ids.is_empty() => eprintln!("resumed observation for {} task(s)", ids.len()),
        Ok(_) => {}
        Err(e) => eprintln!("could not resume running tasks: {e}"),
    }
    let state = AppState::new(engine, collector, port, Some(launch_token), secret, hook_binary(), data_dir, home.clone());
    // After updating from AgentTracker: re-point hooks and "Start with Windows" at this install.
    let fix_state = state.clone();
    std::thread::spawn(move || {
        for note in fix_state.after_update_fixups() {
            eprintln!("update tidy-up: {note}");
        }
    });
    Runtime { state, port, home }
}

fn tooltip(paused: bool) -> String {
    format!("{APP_NAME} — collection {}", if paused { "paused" } else { "on" })
}

/// Shared with tray, window and second-launch handlers once the runtime has started.
struct Shared {
    state: Arc<AppState>,
    port: u16,
}

fn show_window(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.unminimize();
        let _ = w.show();
        let _ = w.set_focus();
        return;
    }
    let Some(shared) = app.try_state::<Shared>() else { return };
    let url = format!("http://127.0.0.1:{}/launch?token={}", shared.port, shared.state.new_launch_token());
    if let Err(e) = open_window(app, &url) {
        eprintln!("could not open window: {e}");
    }
}

fn open_window(app: &AppHandle, url: &str) -> tauri::Result<()> {
    let url = url.parse().map_err(invalid_url)?;
    WebviewWindowBuilder::new(app, "main", WebviewUrl::External(url))
        .title(APP_NAME)
        .inner_size(1180.0, 820.0)
        .min_inner_size(900.0, 600.0)
        .center()
        .focused(true)
        .build()?
        .set_focus()?;
    Ok(())
}

fn invalid_url(e: impl std::fmt::Display) -> tauri::Error {
    tauri::Error::Io(std::io::Error::new(std::io::ErrorKind::InvalidInput, e.to_string()))
}

fn main() {
    if std::env::args().any(|a| a == "--headless") {
        let listener = bind_loopback().expect("bind 127.0.0.1");
        let port = listener.local_addr().expect("local addr").port();
        let token = security::token_urlsafe(24);
        let rt = build_runtime(port, token.clone());
        let launch = format!("http://127.0.0.1:{port}/launch?token={token}");
        println!("{APP_NAME} {VERSION} listening on http://127.0.0.1:{port}");
        println!("Open the UI (one-time link): {launch}");
        let tokio_rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().expect("tokio runtime");
        let home = rt.home.clone();
        tokio_rt.block_on(async move {
            let l = tokio::net::TcpListener::from_std(listener).expect("listener");
            agenttracker_runtime::relay::autostart(rt.state.engine.db.clone());
            if let Err(e) = serve_with(l, rt.state, agenttracker_runtime::http::routes()).await {
                eprintln!("server stopped: {e}");
            }
        });
        CollectorDiscovery::remove(&home);
        return;
    }

    tauri::Builder::default()
        // Must be first: if TrailFox is already running (often just as a tray icon), a second
        // launch - Start menu, desktop shortcut, Setup's "Start TrailFox now" - brings the
        // existing window to the front and exits before it touches the database or hook file.
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| show_window(app)))
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_fs::init())
        .setup(move |app| {
            let listener = bind_loopback()?;
            let port = listener.local_addr()?.port();
            let token = security::token_urlsafe(24);
            let Runtime { state, port, .. } = build_runtime(port, token.clone());
            let launch = format!("http://127.0.0.1:{port}/launch?token={token}");
            app.manage(Shared { state: state.clone(), port });
            let server_state = state.clone();
            tauri::async_runtime::spawn(async move {
                let l = tokio::net::TcpListener::from_std(listener).expect("listener");
                // Chat recorder: starts only if the user turned it on in Connections.
                agenttracker_runtime::relay::autostart(server_state.engine.db.clone());
                if let Err(e) = serve_with(l, server_state, agenttracker_runtime::http::routes()).await {
                    eprintln!("server stopped: {e}");
                }
            });

            // ---- tray -------------------------------------------------------------------------
            let paused = state.collector.is_paused();
            let open_i = MenuItem::with_id(app, "open", "Open TrailFox", true, None::<&str>)?;
            let pause_i = MenuItem::with_id(app, "pause", if paused { "Resume Collection" } else { "Pause Collection" }, true, None::<&str>)?;
            let quit_i = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&open_i, &pause_i, &quit_i])?;

            let st_menu = state.clone();
            let pause_for_menu = pause_i.clone();
            let mut tray = TrayIconBuilder::with_id(TRAY_ID)
                .tooltip(tooltip(paused))
                .menu(&menu)
                .show_menu_on_left_click(false)
                .on_menu_event(move |app, ev| match ev.id().as_ref() {
                    "open" => show_window(app),
                    "pause" => {
                        let p = !st_menu.collector.is_paused();
                        st_menu.collector.set_paused(p);
                        let _ = st_menu.engine.db.set_setting("paused", if p { "1" } else { "0" });
                        let _ = pause_for_menu.set_text(if p { "Resume Collection" } else { "Pause Collection" });
                        if let Some(t) = app.tray_by_id(TRAY_ID) {
                            let _ = t.set_tooltip(Some(tooltip(p)));
                        }
                    }
                    "quit" => app.exit(0),
                    _ => {}
                })
                .on_tray_icon_event(|tray, ev| {
                    if let TrayIconEvent::DoubleClick { button: MouseButton::Left, .. } = ev {
                        show_window(tray.app_handle());
                    }
                });
            if let Some(icon) = app.default_window_icon().cloned() {
                tray = tray.icon(icon);
            }
            tray.build(app)?;

            // Keep the tray tooltip + menu text in sync when the UI toggles collection.
            let (handle, st_sync) = (app.handle().clone(), state.clone());
            std::thread::spawn(move || {
                let mut last = st_sync.collector.is_paused();
                loop {
                    std::thread::sleep(std::time::Duration::from_secs(2));
                    let now = st_sync.collector.is_paused();
                    if now != last {
                        last = now;
                        if let Some(t) = handle.tray_by_id(TRAY_ID) {
                            let _ = t.set_tooltip(Some(tooltip(now)));
                        }
                        let _ = pause_i.set_text(if now { "Resume Collection" } else { "Pause Collection" });
                    }
                }
            });

            open_window(app.handle(), &launch)?;
            Ok(())
        })
        // Closing the window hides it; the tray keeps collecting until "Quit".
        // Unless "keep running in the tray" is switched off in Settings.
        .on_window_event(|window, event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                let app = window.app_handle();
                let keep = app.try_state::<Shared>().map_or(true, |s| s.state.setting("keep_in_tray") != "0");
                if !keep {
                    window.app_handle().exit(0);
                    return;
                }
                api.prevent_close();
                let _ = window.hide();
            }
        })
        .build(tauri::generate_context!())
        .expect("error while building TrailFox")
        .run(|app, event| {
            // Only the copy that started the runtime owns the hook discovery file.
            if let (tauri::RunEvent::Exit, Some(shared)) = (event, app.try_state::<Shared>()) {
                CollectorDiscovery::remove(&shared.state.agenttracker_home);
            }
        });
}
