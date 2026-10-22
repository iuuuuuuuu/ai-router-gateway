#![cfg_attr(all(not(debug_assertions), target_os = "windows"), windows_subsystem = "windows")]

use std::net::TcpStream;
use std::path::PathBuf;
use std::process::{Child, Command};
use std::sync::{Mutex, OnceLock};
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::Manager;
use tauri_plugin_window_state::{AppHandleExt, StateFlags};

struct GatewayChild(Mutex<Option<Child>>);

/// Marker arg the autostart registration passes so a login launch stays in the
/// tray instead of popping the window in the user's face.
const AUTOSTART_ARG: &str = "--from-autostart";

/// The directory the gateway keeps `gateway.db` / `gateway.log` in.
///
/// Set once during `setup()` from the Tauri path resolver, because the right
/// answer differs per platform and the helper functions below have no
/// `AppHandle`:
///
/// * Windows keeps it next to the exe — the layout every existing install
///   already has, so upgrading must not move a user's database.
/// * macOS and Linux use the platform data dir (`~/Library/Application
///   Support/...`, `~/.local/share/...`). Next-to-the-exe is NOT usable there:
///   on macOS that is `Contents/MacOS/` inside the .app bundle, which is
///   code-signed and replaced wholesale on update, so the database would be
///   destroyed by the next upgrade.
static DATA_DIR: OnceLock<PathBuf> = OnceLock::new();

fn gw_data_dir() -> PathBuf {
    match DATA_DIR.get() {
        Some(d) => d.clone(),
        // Only reachable if something asks before setup() ran. Falling back to
        // the exe dir keeps the old behaviour instead of panicking.
        None => exe_dir(),
    }
}

fn exe_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."))
}

/// The sidecar's file name as bundled: Tauri strips the `.exe` suffix on Unix.
fn gateway_exe_name() -> &'static str {
    if cfg!(windows) {
        "gw-server.exe"
    } else {
        "gw-server"
    }
}

/// What the window-state plugin is allowed to persist between runs.
///
/// * SIZE + POSITION + MAXIMIZED + FULLSCREEN: what the user actually asked for
///   ("remember the window size instead of always using the default one").
/// * VISIBLE is excluded on purpose: the window is hidden on autostart and on
///   close-to-tray, so a persisted "hidden" would make the next manual launch
///   invisible. Excluding it also keeps the plugin from calling show()+set_focus()
///   while restoring, so an autostart launch stays quietly in the tray.
/// * DECORATIONS is excluded because this app never toggles decorations; keeping
///   it would only add a way for a stale state file to strip the title bar.
fn window_state_flags() -> StateFlags {
    StateFlags::all() & !StateFlags::VISIBLE & !StateFlags::DECORATIONS
}

fn gateway_running(port: u16) -> bool {
    TcpStream::connect(("127.0.0.1", port)).is_ok()
}

fn spawn_gateway(app: &tauri::AppHandle) -> Result<(), String> {
    // 9090 = admin plane; if it answers, a gateway instance is already up.
    if gateway_running(9090) {
        return Ok(());
    }
    let dir = gw_data_dir();
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let name = gateway_exe_name();
    let exe = dir.join(name);
    // Dev fallback: reuse the server workspace build output. Only reached when
    // the sidecar has not been copied next to the client yet.
    let exe = if exe.exists() {
        exe
    } else {
        let alt = std::env::current_dir()
            .unwrap()
            .join("../../server/target/release")
            .join(name);
        if alt.exists() {
            // copy next to the client exe so upgrades are self-contained
            std::fs::copy(&alt, &exe).map_err(|e| e.to_string())?;
            exe
        } else {
            return Err(format!("{name} not found: {}", alt.display()));
        }
    };
    let mut cmd = Command::new(&exe);
    cmd.env("GW_DATA_DIR", dir.join("gateway.db")).current_dir(&dir);
    // redirect gateway logs to a file (no console window in release)
    if let Ok(f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("gateway.log"))
    {
        if let Ok(f2) = f.try_clone() {
            cmd.stdout(f).stderr(f2);
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    let child = cmd
        .spawn()
        .map_err(|e| format!("spawn gw-server: {e}"))?;
    // Record the pid so a later quit can still stop this gateway even when the
    // process that spawned it is already gone (no Child handle left to kill).
    let _ = std::fs::write(gateway_pid_path(), child.id().to_string());
    app.state::<GatewayChild>().0.lock().unwrap().replace(child);
    // wait for admin plane readiness (max 10s)
    for _ in 0..50 {
        if gateway_running(9090) {
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    Err("gateway did not become ready within 10s".into())
}

/// Where spawn_gateway records the gateway pid.
fn gateway_pid_path() -> PathBuf {
    gw_data_dir().join("gateway.pid")
}

#[cfg(windows)]
fn run_hidden(exe: &str, args: &[&str]) -> Option<String> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let out = Command::new(exe)
        .args(args)
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .ok()?;
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// tasklist lookup so we never kill a pid that Windows has recycled to another app.
#[cfg(windows)]
fn process_image_name(pid: u32) -> Option<String> {
    let filter = format!("PID eq {pid}");
    let out = run_hidden("tasklist", &["/FI", &filter, "/FO", "CSV", "/NH"])?;
    let line = out.lines().next()?.trim();
    if !line.starts_with('"') {
        return None; // "INFO: No tasks are running..." or empty
    }
    Some(line.split("\",\"").next()?.trim_start_matches('"').to_string())
}

/// Same guard as the Windows `tasklist` path: never signal a pid that the OS
/// has recycled into an unrelated process. `ps -o comm=` prints the executable
/// basename on both Linux and macOS, which is what we compare against.
#[cfg(not(windows))]
fn process_image_name(pid: u32) -> Option<String> {
    let out = Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "comm="])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let name = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if name.is_empty() {
        return None;
    }
    // macOS reports the full path for some binaries; keep the basename.
    Some(name.rsplit('/').next().unwrap_or(&name).to_string())
}

#[cfg(windows)]
fn terminate_pid(pid: u32) {
    let pid_arg = pid.to_string();
    let _ = run_hidden("taskkill", &["/PID", &pid_arg, "/F"]);
}

/// Ask politely, then insist. Windows only gets the `/F` form above, so this
/// path is the gentler of the two: SIGTERM lets the gateway release its SQLite
/// handle before we fall back to SIGKILL.
#[cfg(not(windows))]
fn terminate_pid(pid: u32) {
    let pid_arg = pid.to_string();
    let _ = Command::new("kill").args(["-TERM", &pid_arg]).status();
    for _ in 0..10 {
        std::thread::sleep(std::time::Duration::from_millis(100));
        if !pid_alive(pid) {
            return;
        }
    }
    let _ = Command::new("kill").args(["-KILL", &pid_arg]).status();
}

#[cfg(not(windows))]
fn pid_alive(pid: u32) -> bool {
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Stop the gateway. "退出（停止网关）" must always leave the machine clean: if we
/// spawned the gateway we kill our own child, and if an earlier client instance
/// did (spawn_gateway returns early once 9090 answers) we fall back to the pid
/// recorded at spawn time. Without that fallback the gateway outlived every
/// client and kept holding 8080 / 8443 / 9090.
fn kill_gateway(app: &tauri::AppHandle) {
    if let Some(mut c) = app.state::<GatewayChild>().0.lock().unwrap().take() {
        let _ = c.kill();
        let _ = std::fs::remove_file(gateway_pid_path());
        return;
    }
    let Some(pid) = std::fs::read_to_string(gateway_pid_path())
        .ok()
        .and_then(|s| s.trim().parse::<u32>().ok())
        .filter(|p| *p > 0)
    else {
        return;
    };
    if process_image_name(pid).as_deref() == Some(gateway_exe_name()) {
        terminate_pid(pid);
        tray_debug(&format!("kill_gateway: killed orphan gw-server pid {pid}"));
    }
    let _ = std::fs::remove_file(gateway_pid_path());
}

/// Where run/quit-flag.ps1 drops the graceful-quit request.
fn quit_flag_path() -> PathBuf {
    exe_dir().join("quit.flag")
}

/// Quit from a worker thread so RunEvent::Exit still runs (it stops the gateway
/// child). AppHandle::exit only asks the event loop to exit: if that loop is parked
/// inside a win32 menu modal, the request is accepted but the loop never gets a
/// chance to break, so the process would stay alive. The hard fallback keeps the
/// quit menu item honest - gateway killed, process gone - wherever the loop is stuck.
fn request_quit(app: &tauri::AppHandle) {
    let handle = app.clone();
    std::thread::spawn(move || {
        tray_debug("request_quit: app.exit(0)");
        handle.exit(0);
        std::thread::sleep(std::time::Duration::from_secs(3));
        tray_debug("request_quit: exit(0) did not terminate in 3s -> hard exit");
        kill_gateway(&handle);
        std::process::exit(0);
    });
}

/// Watch for the quit-flag file so the harness (and users) have a graceful,
/// win32-free way to stop the whole app plus its gateway child.
fn watch_quit_flag(app: tauri::AppHandle) {
    let path = quit_flag_path();
    std::thread::spawn(move || loop {
        std::thread::sleep(std::time::Duration::from_millis(500));
        if path.exists() {
            let _ = std::fs::remove_file(&path);
            tray_debug("quit flag detected");
            request_quit(&app);
            return;
        }
    });
}

fn tray_debug(msg: &str) {
    let line = format!("{msg}\n");
    // Prefer the data dir (writable on every platform); fall back to the exe
    // dir only if the data dir does not exist yet.
    let log_dir = {
        let d = gw_data_dir();
        if d.exists() {
            d
        } else {
            exe_dir()
        }
    };
    let _ = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_dir.join("tray-debug.log"))
        .and_then(|mut f| {
            use std::io::Write;
            f.write_all(line.as_bytes())
        });
}

fn show_main(app: &tauri::AppHandle) {
    match app.get_webview_window("main") {
        Some(w) => {
            tray_debug(&format!("show_main: show={:?}", w.show()));
            tray_debug(&format!("show_main: unminimize={:?}", w.unminimize()));
            tray_debug(&format!("show_main: set_focus={:?}", w.set_focus()));
        }
        None => tray_debug("show_main: window 'main' NOT FOUND"),
    }
}

/// Tray icon: left click restores the window, the menu offers show / quit.
fn setup_tray(app: &tauri::AppHandle) -> tauri::Result<()> {
    let show_item = MenuItem::with_id(app, "show", "显示主界面", true, None::<&str>)?;
    let quit_item = MenuItem::with_id(app, "quit", "退出（停止网关）", true, None::<&str>)?;
    let sep = PredefinedMenuItem::separator(app)?;
    let menu = Menu::with_items(app, &[&show_item, &sep, &quit_item])?;

    let mut builder = TrayIconBuilder::with_id("main")
        .tooltip("AI 网关控制台")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| {
            tray_debug(&format!("menu event: {:?}", event.id()));
            match event.id().as_ref() {
            "show" => show_main(app),
            "quit" => request_quit(app),
            _ => {}
            }
        })
        .on_tray_icon_event(|tray, event| {
            tray_debug(&format!("tray event: {event:?}"));
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                show_main(tray.app_handle());
            }
        });
    if let Some(icon) = app.default_window_icon().cloned() {
        builder = builder.icon(icon);
    }
    builder.build(app)?;
    Ok(())
}

/// Windows keeps the database next to the exe (the layout every existing
/// install already has). Elsewhere the exe lives inside a read-only or
/// update-replaced bundle, so the platform data dir is the only place a
/// database can survive an upgrade.
fn resolve_data_dir(app: &tauri::AppHandle) -> PathBuf {
    if cfg!(windows) {
        return exe_dir();
    }
    // Debug builds keep the historical `./data` so dev runs stay self-contained
    // and disposable, matching what the Windows dev path does.
    if cfg!(debug_assertions) {
        return std::env::current_dir().unwrap().join("data");
    }
    app.path()
        .app_data_dir()
        .unwrap_or_else(|_| exe_dir())
}

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .plugin(
            tauri_plugin_window_state::Builder::default()
                .with_state_flags(window_state_flags())
                .build(),
        )
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            Some(vec![AUTOSTART_ARG]),
        ))
        .manage(GatewayChild(Mutex::new(None)))
        .setup(|app| {
            let handle = app.handle().clone();
            // Resolve before any worker thread can ask for it.
            let data_dir = resolve_data_dir(&handle);
            let _ = std::fs::create_dir_all(&data_dir);
            let _ = DATA_DIR.set(data_dir);
            let from_autostart = std::env::args().any(|a| a == AUTOSTART_ARG);
            std::thread::spawn(move || {
                if let Err(e) = spawn_gateway(&handle) {
                    eprintln!("[gw-client] {e}");
                }
            });
            setup_tray(&app.handle())?;
            watch_quit_flag(app.handle().clone());
            if from_autostart {
                // launched at login: sit in the tray, do not steal focus
                if let Some(w) = app.get_webview_window("main") {
                    let _ = w.hide();
                }
            }
            Ok(())
        })
        .on_window_event(|window, event| {
            // closing the window keeps the tray alive; exit-to-quit stops gateway
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                // Persist the geometry right here as well: the tray "quit" path
                // hard-exits the process 3s after app.exit(0), which can skip the
                // plugin's RunEvent::Exit save.
                let _ = window
                    .app_handle()
                    .save_window_state(window_state_flags());
                let _ = window.hide();
                api.prevent_close();
            }
        })
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|app, event| {
            if let tauri::RunEvent::Exit = event {
                kill_gateway(app);
            }
        });
}
