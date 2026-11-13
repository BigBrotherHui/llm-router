#![recursion_limit = "512"]

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tauri::{Manager, RunEvent};

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
fn bootstrap_info() -> BootstrapInfo {
    BootstrapInfo {
        desktop: cfg!(desktop),
        server_url: format!("http://127.0.0.1:{}", port()),
        version: env!("CARGO_PKG_VERSION").to_string(),
    }
}

fn port_open(port: u16) -> bool {
    use std::net::{SocketAddr, TcpStream};
    let addr: SocketAddr = ([127, 0, 0, 1], port).into();
    TcpStream::connect_timeout(&addr, Duration::from_millis(250)).is_ok()
}

#[cfg(windows)]
fn force_borderless(window: &tauri::WebviewWindow) {
    use windows::Win32::Graphics::Dwm::{
        DwmSetWindowAttribute, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        GetWindowLongW, SetWindowLongW, SetWindowPos, GWL_STYLE, SWP_FRAMECHANGED, SWP_NOMOVE,
        SWP_NOSIZE, SWP_NOZORDER, WS_CAPTION, WS_MAXIMIZEBOX, WS_MINIMIZEBOX, WS_SYSMENU,
        WS_THICKFRAME,
    };
    if let Ok(hwnd) = window.hwnd() {
        unsafe {
            let style = GetWindowLongW(hwnd, GWL_STYLE);
            let mask = (WS_CAPTION.0
                | WS_THICKFRAME.0
                | WS_SYSMENU.0
                | WS_MINIMIZEBOX.0
                | WS_MAXIMIZEBOX.0) as i32;
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
            // Windows 11：无边框窗口也保留系统级圆角
            let corner = DWMWCP_ROUND;
            let _ = DwmSetWindowAttribute(
                hwnd,
                DWMWA_WINDOW_CORNER_PREFERENCE,
                &corner as *const _ as *const std::ffi::c_void,
                std::mem::size_of_val(&corner) as u32,
            );
        }
    }
}

#[tauri::command]
async fn start_backend() -> Result<(), String> {
    let port = port();
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        if port_open(port) {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Err(format!("原生服务 30 秒内没有启动（127.0.0.1:{port}）"))
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
                .transparent(true)
                .background_color(tauri::window::Color(9, 13, 20, 0))
                .build()?;
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
        .invoke_handler(tauri::generate_handler![bootstrap_info, start_backend])
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                #[cfg(desktop)]
                {
                    let state = window.app_handle().state::<Arc<native::state::AppState>>();
                    let behavior = state
                        .file
                        .read()
                        .unwrap()
                        .window_behavior
                        .clone()
                        .unwrap_or_else(|| "minimize".to_string());
                    if behavior == "close" {
                        window.app_handle().exit(0);
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
