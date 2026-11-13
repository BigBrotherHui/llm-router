pub mod admin;
pub mod checkin;
pub mod journal;
pub mod proxy;
pub mod scheduler;
pub mod state;
pub mod stats;

use std::sync::Arc;

use axum::extract::DefaultBodyLimit;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;

use state::AppState;

const WEBUI: &str = include_str!("../../../../webui.html");

async fn index() -> Response {
    (
        StatusCode::OK,
        [("content-type", "text/html; charset=utf-8")],
        WEBUI,
    )
        .into_response()
}

async fn health() -> Response {
    axum::Json(serde_json::json!({"status": "ok", "version": state::VERSION})).into_response()
}

async fn favicon() -> Response {
    StatusCode::NO_CONTENT.into_response()
}

pub async fn serve(state: Arc<AppState>, port: u16) -> Result<(), String> {
    let app = Router::new()
        .route("/", get(index))
        .route("/ui", get(index))
        .route("/webui", get(index))
        .route("/webui.html", get(index))
        .route("/favicon.ico", get(favicon))
        .route("/health", get(health))
        .route("/healthz", get(health))
        .route("/stats", get(admin::stats_handler))
        .route("/admin/stats", get(admin::stats_handler))
        .route("/admin/upstream/add", post(admin::upstream_add))
        .route("/admin/upstream/update", post(admin::upstream_update))
        .route("/admin/upstream/delete", post(admin::upstream_delete))
        .route("/admin/fetch-models", post(admin::fetch_models))
        .route("/admin/select-models", post(admin::select_models))
        .route("/admin/set-group", post(admin::set_group))
        .route("/admin/model/save", post(admin::model_save))
        .route("/admin/model/delete", post(admin::model_delete))
        .route("/admin/model/quota", post(admin::model_quota))
        .route("/admin/model/orderings", post(admin::model_orderings))
        .route("/admin/model/order/save", post(admin::order_save))
        .route("/admin/model/order/apply", post(admin::order_apply))
        .route("/admin/model/order/delete", post(admin::order_delete))
        .route("/admin/defaults", post(admin::defaults))
        .route("/admin/window", get(admin::window_behavior).post(admin::set_window_behavior))
        .route("/admin/window/minimize", post(admin::window_minimize))
        .route("/admin/window/toggle-maximize", post(admin::window_toggle_maximize))
        .route("/admin/window/drag", post(admin::window_drag))
        .route("/admin/window/close", post(admin::window_close))
        .route("/admin/speedtest", post(admin::speedtest))
        .route("/admin/proxy/save", post(admin::proxy_save))
        .route("/admin/proxy/detect", post(admin::proxy_detect))
        .route("/admin/net/bench", post(admin::net_bench))
        .route("/admin/net/auto", post(admin::net_auto))
        .route("/admin/balance", post(admin::balance))
        .route("/admin/checkin", get(admin::checkin_view))
        .route("/admin/checkin/run", post(admin::checkin_run))
        .route("/admin/checkin/config", post(admin::checkin_config))
        .route("/admin/checkin/settings", post(admin::checkin_settings))
        .route("/admin/state/versions", get(admin::versions))
        .route("/admin/admin-log", get(admin::admin_log))
        .route("/admin/journal/files", get(admin::journal_files))
        .route("/admin/auto-configure", post(admin::auto_configure))
        .route("/admin/rename-group", post(admin::rename_group))
        .route("/admin/journal/index", get(admin::journal_index))
        .route("/admin/journal", get(admin::journal_query))
        .route("/admin/history", get(admin::journal_query))
        .route("/v1/models", get(proxy::list_models))
        .route("/v1/chat/completions", post(proxy::chat_completions))
        .layer(DefaultBodyLimit::max(64 * 1024 * 1024))
        .with_state(state.clone());

    // 请求的端口被占（比如别的程序或旧实例）时，从 +1 开始往后找空闲端口，
    // 避免桌面版静默连到别人身上、出现「两个版本状态不一样」的错觉。
    let mut listener = None;
    let mut actual_port = port;
    for offset in 0..20u16 {
        let p = port + offset;
        match tokio::net::TcpListener::bind(("127.0.0.1", p)).await {
            Ok(l) => {
                listener = Some(l);
                actual_port = p;
                break;
            }
            Err(_) => continue,
        }
    }
    let listener = listener.ok_or_else(|| {
        format!("127.0.0.1:{port} 起连续 20 个端口都被占用，请设置 LLM_ROUTER_PORT 换个端口")
    })?;
    if actual_port != port {
        eprintln!("[native] 端口 {port} 被占，已改用 {actual_port}");
        state.port.store(actual_port, std::sync::atomic::Ordering::Relaxed);
    }
    axum::serve(listener, app)
        .await
        .map_err(|e| format!("HTTP 服务退出: {e}"))
}
