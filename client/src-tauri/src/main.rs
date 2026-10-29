#![cfg_attr(all(not(debug_assertions), target_os = "windows"), windows_subsystem = "windows")]

use std::net::TcpStream;
use std::path::PathBuf;
use std::process::{Child, Command};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use tauri::menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{Manager, WebviewUrl, WebviewWindow, WebviewWindowBuilder};
use tauri_plugin_window_state::{AppHandleExt, StateFlags};

struct GatewayChild(Mutex<Option<Child>>);

/// Marker arg the autostart registration passes so a login launch stays in the
/// tray instead of popping the window in the user's face.
const AUTOSTART_ARG: &str = "--from-autostart";

/// 轻量模式：关闭窗口时真正销毁窗口（连带释放 WebView2 进程），只留托盘与网关。
///
/// 开关与「有没有未保存的工作」都必须在 Rust 侧可读：窗口一旦销毁，webview 里
/// 的 localStorage 和内存状态一起没了，判断依据不能放在前端。
static LIGHTWEIGHT: AtomicBool = AtomicBool::new(true);

/// 前端报告「有未保存的工作」。**默认 true**：webview 还没报到之前一律当作有脏数据，
/// 那种情况下最坏也只是退回 hide（老行为），绝不会在不确定时销毁界面把用户输入弄丢。
static UI_UNSAVED: AtomicBool = AtomicBool::new(true);

/// 正在走退出流程。`app.exit(0)` 同样会触发 ExitRequested，不区分就会把退出也拦掉。
static QUITTING: AtomicBool = AtomicBool::new(false);

/// 托盘里的「轻量模式」勾选项，供设置页开关反向同步勾选状态。
static LIGHT_TOGGLE: OnceLock<CheckMenuItem<tauri::Wry>> = OnceLock::new();

/// 客户端自己的偏好（与 `gateway.db` 里的服务端配置无关），落在数据目录。
fn client_settings_path() -> PathBuf {
    gw_data_dir().join("client-settings.json")
}

fn load_client_settings() {
    let Ok(text) = std::fs::read_to_string(client_settings_path()) else {
        return;
    };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else {
        tray_debug("client-settings.json is not valid JSON; keeping defaults");
        return;
    };
    if let Some(b) = v.get("lightweight_mode").and_then(|x| x.as_bool()) {
        LIGHTWEIGHT.store(b, Ordering::SeqCst);
    }
}

fn save_client_settings() {
    let body = serde_json::json!({
        "lightweight_mode": LIGHTWEIGHT.load(Ordering::SeqCst),
    });
    let _ = std::fs::write(client_settings_path(), format!("{body}\n"));
}

/// 设置页读取客户端偏好（目前只有轻量模式）。
#[tauri::command]
fn client_settings() -> serde_json::Value {
    serde_json::json!({ "lightweight_mode": LIGHTWEIGHT.load(Ordering::SeqCst) })
}

/// 前端每次脏状态变化时上报，作为「关窗能不能安全销毁界面」的唯一依据。
#[tauri::command]
fn ui_unsaved(dirty: bool) {
    UI_UNSAVED.store(dirty, Ordering::SeqCst);
}

/// 设置页 / 托盘切换轻量模式：三处状态（内存、磁盘、勾选项）一起改。
#[tauri::command]
fn set_lightweight_mode(enabled: bool) {
    LIGHTWEIGHT.store(enabled, Ordering::SeqCst);
    save_client_settings();
    if let Some(item) = LIGHT_TOGGLE.get() {
        let _ = item.set_checked(enabled);
    }
    tray_debug(&format!("lightweight_mode -> {enabled}"));
}

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
        // 先立旗再 exit：ExitRequested 会被「托盘模式不退出」的拦截器看到，
        // 不先声明意图，退出请求会被自己挡回去。
        QUITTING.store(true, Ordering::SeqCst);
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
///
/// The sibling `show.flag` does the opposite: bring the window back. It exists
/// because in lightweight mode the window is destroyed, so "tray -> 显示主界面"
/// is the only way back in — and a probe cannot click a tray icon without
/// stealing the user's mouse and focus.
fn watch_quit_flag(app: tauri::AppHandle) {
    let path = quit_flag_path();
    let show_path = exe_dir().join("show.flag");
    std::thread::spawn(move || loop {
        std::thread::sleep(std::time::Duration::from_millis(500));
        if show_path.exists() {
            let _ = std::fs::remove_file(&show_path);
            tray_debug("show flag detected");
            show_main(&app);
            continue;
        }
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

/// 把主窗口建回来。轻量模式下关窗是 destroy 而不是 hide，窗口对象已经不存在，
/// 托盘再点「显示主界面」必须重建一个 —— 几何由 window-state 插件的 on_window_ready 还原。
fn build_main_window(app: &tauri::AppHandle) -> tauri::Result<WebviewWindow> {
    WebviewWindowBuilder::new(app, "main", WebviewUrl::App("index.html".into()))
        .title("AI 网关控制台")
        .inner_size(1100.0, 720.0)
        .resizable(true)
        // 先不显示：交给 show_main 决定何时亮出来，重建时不会闪一个白框。
        .visible(false)
        .build()
}

/// 拿主窗口，不在就重建。任何「显示主界面」的入口都走这里。
///
/// 重建必须容错：Tauri 对已关闭窗口的登记清理是异步的，窗口刚消失时
/// `get_webview_window("main")` 可能仍返回一个**句柄已死**的对象，直接 show()
/// 会得到 `FailedToSendMessage`；也可能反过来，清理晚到、把我们刚建好的同名窗口
/// 从登记表里按 label 删掉。所以这里探活后再决定，建不出来就等一下重试。
fn ensure_main_window(app: &tauri::AppHandle) -> Result<WebviewWindow, String> {
    let mut last = String::from("no attempt");
    for _ in 0..25 {
        if let Some(w) = app.get_webview_window("main") {
            // 句柄还活着才复用；隐藏的窗口 inner_size() 依然可读，死句柄会报错。
            if w.inner_size().is_ok() {
                return Ok(w);
            }
            tray_debug("ensure_main_window: 'main' handle is stale, waiting for cleanup");
        } else {
            match build_main_window(app) {
                Ok(w) => {
                    tray_debug("ensure_main_window: 'main' rebuilt");
                    return Ok(w);
                }
                Err(e) => last = format!("rebuild main window: {e}"),
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(120));
    }
    Err(last)
}

fn show_main(app: &tauri::AppHandle) {
    match ensure_main_window(app) {
        Ok(w) => {
            tray_debug(&format!("show_main: show={:?}", w.show()));
            tray_debug(&format!("show_main: unminimize={:?}", w.unminimize()));
            tray_debug(&format!("show_main: set_focus={:?}", w.set_focus()));
        }
        Err(e) => tray_debug(&format!("show_main: {e}")),
    }
}

/// Tray icon: left click restores the window, the menu offers show / quit.
fn setup_tray(app: &tauri::AppHandle) -> tauri::Result<()> {
    let show_item = MenuItem::with_id(app, "show", "显示主界面", true, None::<&str>)?;
    // 勾选项本身也是「即使界面被销毁也能改回来」的唯一入口，所以放进托盘一份。
    let light_item = CheckMenuItem::with_id(
        app,
        "lightweight",
        "轻量模式（关窗释放界面内存）",
        true,
        LIGHTWEIGHT.load(Ordering::SeqCst),
        None::<&str>,
    )?;
    let _ = LIGHT_TOGGLE.set(light_item.clone());
    let quit_item = MenuItem::with_id(app, "quit", "退出（停止网关）", true, None::<&str>)?;
    let sep = PredefinedMenuItem::separator(app)?;
    let sep2 = PredefinedMenuItem::separator(app)?;
    let menu = Menu::with_items(app, &[&show_item, &sep, &light_item, &sep2, &quit_item])?;

    let mut builder = TrayIconBuilder::with_id("main")
        .tooltip("AI 网关控制台")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| {
            tray_debug(&format!("menu event: {:?}", event.id()));
            match event.id().as_ref() {
            "show" => show_main(app),
            "lightweight" => {
                let next = !LIGHTWEIGHT.load(Ordering::SeqCst);
                LIGHTWEIGHT.store(next, Ordering::SeqCst);
                save_client_settings();
                if let Some(item) = LIGHT_TOGGLE.get() {
                    let _ = item.set_checked(next);
                }
                tray_debug(&format!("tray toggled lightweight -> {next}"));
            }
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
        .invoke_handler(tauri::generate_handler![
            client_settings,
            ui_unsaved,
            set_lightweight_mode
        ])
        .setup(|app| {
            let handle = app.handle().clone();
            // Resolve before any worker thread can ask for it.
            let data_dir = resolve_data_dir(&handle);
            let _ = std::fs::create_dir_all(&data_dir);
            let _ = DATA_DIR.set(data_dir);
            // 托盘勾选项要显示真实的当前值，所以偏好必须在建托盘之前读。
            load_client_settings();
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
                // plugin's RunEvent::Exit save. Disposing the window (lightweight
                // mode) needs this even more — after destroy there is nothing left
                // to read the geometry from.
                let app = window.app_handle().clone();
                let _ = app.save_window_state(window_state_flags());

                let lightweight = LIGHTWEIGHT.load(Ordering::SeqCst);
                let unsaved = UI_UNSAVED.load(Ordering::SeqCst);
                if lightweight && !unsaved {
                    // 轻量模式：**放它正常关闭**。走 Tauri 自己的关闭路径，WebView2 的
                    // 一串子进程随之退出，登记表也在同一次事件处理里清干净 —— 这比在
                    // 后台线程里 destroy() 可靠：那次实测出现过「清理晚到、把刚重建的同
                    // 名窗口按 label 删掉」，之后所有 show() 都返回 FailedToSendMessage。
                    // 进程不会跟着退：RunEvent::ExitRequested 里有 prevent_exit。
                    tray_debug("close: lightweight mode lets the window close");
                    return;
                }

                if lightweight {
                    // 有未保存的工作：宁可这轮不省内存，也不能把用户输入丢掉。
                    tray_debug("close: unsaved work present -> hiding instead of closing");
                }
                let _ = window.hide();
                api.prevent_close();
            }
        })
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|app, event| match event {
            tauri::RunEvent::Exit => kill_gateway(app),
            tauri::RunEvent::ExitRequested { api, .. } => {
                // 关掉最后一个窗口不等于要退出：托盘图标和网关都得活着，
                // 这正是「只保留服务」的全部意义。只有托盘「退出」那条路径例外。
                if !QUITTING.load(Ordering::SeqCst) {
                    api.prevent_exit();
                }
            }
            _ => {}
        });
}
