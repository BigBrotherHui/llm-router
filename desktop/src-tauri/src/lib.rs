#![recursion_limit = "512"]

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tauri::{Manager, RunEvent};

#[cfg(desktop)]
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};

mod native;

const DEFAULT_PORT: u16 = 8010;

fn port() -> u16 {
    std::env::var("LLM_ROUTER_PORT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_PORT)
}

#[derive(serde::Serialize)]
struct BootstrapInfo {
    desktop: bool,
    server_url: String,
    version: String,
}

#[tauri::command]
fn bootstrap_info(state: tauri::State<'_, Arc<native::state::AppState>>) -> BootstrapInfo {
    let bound = state.port.load(std::sync::atomic::Ordering::Relaxed);
    BootstrapInfo {
        desktop: cfg!(desktop),
        server_url: format!("http://127.0.0.1:{}", if bound != 0 { bound } else { port() }),
        version: env!("CARGO_PKG_VERSION").to_string(),
    }
}

#[tauri::command]
fn hud_stats(state: tauri::State<'_, Arc<native::state::AppState>>) -> serde_json::Value {
    native::insights::today_summary(&state)
}

#[tauri::command]
fn hud_hide(app: tauri::AppHandle) {
    if let Some(window) = app.get_webview_window("hud") {
        let _ = window.hide();
    }
}

#[tauri::command]
fn hud_open_main(app: tauri::AppHandle) {
    #[cfg(desktop)]
    {
        if let Some(window) = app.get_webview_window("hud") {
            let _ = window.hide();
        }
        show_main_window(&app);
    }
    let _ = app;
}

fn port_open(port: u16) -> bool {
    use std::net::{SocketAddr, TcpStream};
    let addr: SocketAddr = ([127, 0, 0, 1], port).into();
    TcpStream::connect_timeout(&addr, Duration::from_millis(250)).is_ok()
}

#[cfg(windows)]
fn force_borderless(window: &tauri::WebviewWindow) {
    use windows::Win32::Graphics::Dwm::{
        DwmSetWindowAttribute, DWMWA_BORDER_COLOR, DWMWA_USE_IMMERSIVE_DARK_MODE,
        DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        GetWindowLongW, SetWindowLongW, SetWindowPos, GWL_STYLE, SWP_FRAMECHANGED, SWP_NOMOVE,
        SWP_NOSIZE, SWP_NOZORDER, WS_CAPTION, WS_THICKFRAME,
    };
    if let Ok(hwnd) = window.hwnd() {
        unsafe {
            let style = GetWindowLongW(hwnd, GWL_STYLE);
            // 只去掉标题栏/粗边框；保留 MINIMIZEBOX/SYSMENU，
            // 否则 Windows 任务栏按钮不会把「再次点击」识别成最小化。
            let mask = (WS_CAPTION.0 | WS_THICKFRAME.0) as i32;
            let _ = SetWindowLongW(hwnd, GWL_STYLE, style & !mask);
            let _ = SetWindowPos(
                hwnd,
                None,
                0,
                0,
                0,
                0,
                SWP_FRAMECHANGED | SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER,
            );
            // Windows 11：无边框窗口也保留系统级圆角，并把系统窗口描边压成深色。
            let corner = DWMWCP_ROUND;
            let _ = DwmSetWindowAttribute(
                hwnd,
                DWMWA_WINDOW_CORNER_PREFERENCE,
                &corner as *const _ as *const std::ffi::c_void,
                std::mem::size_of_val(&corner) as u32,
            );
            let border: u32 = 0x00140D09; // #090d14 的 COLORREF（BGR）
            let _ = DwmSetWindowAttribute(
                hwnd,
                DWMWA_BORDER_COLOR,
                &border as *const _ as *const std::ffi::c_void,
                std::mem::size_of_val(&border) as u32,
            );
            let dark: i32 = 1;
            let _ = DwmSetWindowAttribute(
                hwnd,
                DWMWA_USE_IMMERSIVE_DARK_MODE,
                &dark as *const _ as *const std::ffi::c_void,
                std::mem::size_of_val(&dark) as u32,
            );
        }
    }
}

#[tauri::command]
async fn start_backend(
    app: tauri::AppHandle,
    state: tauri::State<'_, Arc<native::state::AppState>>,
) -> Result<String, String> {
    let _ = app;
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let p = state.port.load(std::sync::atomic::Ordering::Relaxed);
        if p != 0 && port_open(p) {
            return Ok(format!("http://127.0.0.1:{p}"));
        }
        if Instant::now() > deadline {
            return Err("原生服务 30 秒内没有启动".to_string());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[cfg(desktop)]
fn show_main_window(app: &tauri::AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
    }
}

/// 托盘旁的迷你 HUD：无边框、置顶、无任务栏按钮，数据由 `hud_stats` 供给。
#[cfg(desktop)]
fn show_hud(app: &tauri::AppHandle) {
    if let Some(window) = app.get_webview_window("hud") {
        let _ = window.show();
        let _ = window.set_focus();
        return;
    }
    let mut builder = tauri::WebviewWindowBuilder::new(
        app,
        "hud",
        tauri::WebviewUrl::App("hud.html".into()),
    )
    .title("llm-router HUD")
    .inner_size(326.0, 208.0)
    .min_inner_size(300.0, 190.0)
    .decorations(false)
    .transparent(true)
    .shadow(false)
    .always_on_top(true)
    .skip_taskbar(true)
    .resizable(false)
    .focused(true);
    // 贴主屏右下角（托盘附近），留出任务栏空间。
    if let Ok(Some(monitor)) = app.primary_monitor() {
        let scale = monitor.scale_factor();
        let pos = monitor.position().to_logical::<f64>(scale);
        let size = monitor.size().to_logical::<f64>(scale);
        builder = builder.position(pos.x + size.width - 350.0, pos.y + size.height - 300.0);
    }
    if let Err(e) = builder.build() {
        eprintln!("[hud] 创建迷你 HUD 失败: {e}");
    }
}

#[cfg(desktop)]
fn toggle_hud(app: &tauri::AppHandle) {
    if let Some(window) = app.get_webview_window("hud") {
        if window.is_visible().unwrap_or(false) {
            let _ = window.hide();
        } else {
            let _ = window.show();
            let _ = window.set_focus();
        }
    } else {
        show_hud(app);
    }
}

#[cfg(desktop)]
fn setup_tray(app: &tauri::App) -> tauri::Result<()> {
    let hud = tauri::menu::MenuItem::with_id(app, "hud", "迷你 HUD", true, None::<&str>)?;
    let show = tauri::menu::MenuItem::with_id(app, "show", "显示主界面", true, None::<&str>)?;
    let quit = tauri::menu::MenuItem::with_id(app, "quit", "退出", true, None::<&str>)?;
    let menu = tauri::menu::Menu::with_items(app, &[&hud, &show, &quit])?;
    let icon = app
        .default_window_icon()
        .cloned()
        .expect("窗口图标缺失，无法创建托盘图标");
    let tray = TrayIconBuilder::with_id("main")
        .icon(icon)
        .menu(&menu)
        .tooltip("llm-router")
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id().as_ref() {
            "hud" => toggle_hud(app),
            "show" => show_main_window(app),
            "quit" => app.exit(0),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                toggle_hud(tray.app_handle());
            }
        })
        .build(app)?;
    // TrayIcon 是引用计数资源，必须由应用持有，否则图标会被回收。
    app.manage(tray);
    Ok(())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let app = tauri::Builder::default()
        .setup(|app| {
            #[cfg(desktop)]
            {
                // 手动创建窗口，确保 Windows 上一定走无边框分支（配置文件在
                // 某些打包组合下会被系统标题栏样式覆盖）。
                let _window = tauri::WebviewWindowBuilder::new(
                    app,
                    "main",
                    tauri::WebviewUrl::App("index.html".into()),
                )
                .title("llm-router")
                .inner_size(1280.0, 860.0)
                .min_inner_size(860.0, 560.0)
                .center()
                .resizable(true)
                .decorations(false)
                .transparent(false)
                .shadow(false)
                .background_color(tauri::window::Color(9, 13, 20, 255))
                .build()?;
                setup_tray(app)?;
                let port = port();
                let data_dir = app
                    .path()
                    .app_data_dir()
                    .map_err(|e| format!("找不到应用数据目录: {e}"))?;
                let state_path = std::env::var("LLM_ROUTER_STATE")
                    .map(PathBuf::from)
                    .unwrap_or_else(|_| {
                        // 免安装版放在仓库/项目目录附近时，优先复用已有的 .llmrouter 状态
                        if let Ok(exe) = std::env::current_exe() {
                            let mut dir = exe.parent();
                            while let Some(d) = dir {
                                let candidate = d.join(".llmrouter").join("config.state.json");
                                if candidate.exists() {
                                    return candidate;
                                }
                                dir = d.parent();
                            }
                        }
                        data_dir
                            .join(".llmrouter")
                            .join("config.state.json")
                    });
                let state = Arc::new(native::state::AppState::new(state_path, Some(app.handle().clone())));
                app.manage(state.clone());
                // 托盘 tooltip 常驻显示今日请求量 / 成功率 / P50，相当于「迷你 HUD」。
                {
                    let handle = app.handle().clone();
                    std::thread::spawn(move || loop {
                        std::thread::sleep(Duration::from_secs(15));
                        let payload = {
                            let st = handle.state::<Arc<native::state::AppState>>();
                            native::insights::today_summary(st.inner().as_ref())
                        };
                        let calls = payload["calls"].as_u64().unwrap_or(0);
                        let rate = payload["success_rate"].as_f64().unwrap_or(0.0);
                        let p50 = payload["p50_ms"].as_f64().unwrap_or(0.0);
                        let p50_text = if p50 >= 1000.0 {
                            format!("{:.1}s", p50 / 1000.0)
                        } else {
                            format!("{:.0}ms", p50)
                        };
                        let text = format!(
                            "llm-router · 今日 {} 次 · 成功率 {:.1}% · P50 {}",
                            calls, rate, p50_text
                        );
                        if let Some(tray) = handle.tray_by_id("main") {
                            let _ = tray.set_tooltip(Some(text));
                        }
                    });
                }
                native::scheduler::spawn(state.clone());
                // 无边框：配置在部分 Windows/打包组合下会被系统标题栏覆盖，
                // 等窗口真正创建后再强制设一次 decorations=false。
                {
                    let handle = app.handle().clone();
                    tauri::async_runtime::spawn(async move {
                        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
                        if let Some(window) = handle.get_webview_window("main") {
                            force_borderless(&window);
                        }
                    });
                }
                tauri::async_runtime::spawn(async move {
                    if let Err(e) = native::serve(state, port).await {
                        eprintln!("[native] {e}");
                    }
                });
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            bootstrap_info,
            start_backend,
            hud_stats,
            hud_hide,
            hud_open_main
        ])
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                #[cfg(desktop)]
                {
                    // HUD 的关闭按钮/Alt+F4 只隐藏它，不影响主窗口的关闭策略。
                    if window.label() == "hud" {
                        api.prevent_close();
                        let _ = window.hide();
                        return;
                    }
                    let state = window.app_handle().state::<Arc<native::state::AppState>>();
                    let behavior = state
                        .file
                        .read()
                        .unwrap()
                        .window_behavior
                        .clone()
                        .unwrap_or_else(|| "tray".to_string());
                    if behavior == "close" {
                        window.app_handle().exit(0);
                    } else if behavior == "tray" {
                        api.prevent_close();
                        let _ = window.hide();
                    } else {
                        api.prevent_close();
                        let _ = window.minimize();
                    }
                }
                #[cfg(mobile)]
                {
                    let _ = (window, api);
                }
            }
        })
        .build(tauri::generate_context!())
        .expect("failed to build tauri application");

    app.run(|app_handle, event| match event {
        RunEvent::Ready => {
            #[cfg(desktop)]
            if let Some(window) = app_handle.get_webview_window("main") {
                force_borderless(&window);
            }
        }
        _ => {}
    });
}
