pub mod admin;
pub mod affinity;
pub mod agents;
pub mod anthropic;
pub mod checkin;
pub mod collector;
pub mod health;
pub mod insights;
pub mod journal;
pub mod journal_index;
pub mod proxy;
pub mod relay;
pub mod scheduler;
pub mod sessions;
pub mod state;
pub mod stats;
pub mod transfer;
pub mod update;

use std::hash::{Hash, Hasher};
use std::sync::{Arc, OnceLock};

use axum::extract::{DefaultBodyLimit, State};
use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE, ETAG, IF_NONE_MATCH};
use axum::http::{HeaderMap, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use tower_http::compression::predicate::{DefaultPredicate, NotForContentType, Predicate};
use tower_http::compression::CompressionLayer;

use state::AppState;

const WEBUI: &str = include_str!("../../../../webui.html");
const WEBUI_CSS: &str = include_str!("../../../../webui.css");
const WEBUI_JS: &str = include_str!("../../../../webui.js");

fn webui_etag() -> &'static str {
    static ETAG_VALUE: OnceLock<String> = OnceLock::new();
    ETAG_VALUE.get_or_init(|| {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        WEBUI.hash(&mut hasher);
        format!("\"{:x}\"", hasher.finish())
    })
}

async fn index(headers: HeaderMap) -> Response {
    let etag = webui_etag();
    let want = headers
        .get(IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let matched = want.split(',').any(|candidate| {
        candidate
            .trim()
            .trim_start_matches("W/")
            .trim_matches('"')
            == etag.trim_matches('"')
    });
    if matched {
        return StatusCode::NOT_MODIFIED.into_response();
    }
    (
        StatusCode::OK,
        [
            (CONTENT_TYPE, "text/html; charset=utf-8"),
            (ETAG, etag),
            (CACHE_CONTROL, "no-cache"),
        ],
        WEBUI,
    )
        .into_response()
}

async fn webui_css() -> Response {
    (
        StatusCode::OK,
        [(CONTENT_TYPE, "text/css; charset=utf-8"), (CACHE_CONTROL, "no-cache")],
        WEBUI_CSS,
    )
        .into_response()
}

async fn webui_js() -> Response {
    (
        StatusCode::OK,
        [(CONTENT_TYPE, "text/javascript; charset=utf-8"), (CACHE_CONTROL, "no-cache")],
        WEBUI_JS,
    )
        .into_response()
}

async fn health() -> Response {
    axum::Json(serde_json::json!({"status": "ok", "version": state::VERSION})).into_response()
}

async fn favicon() -> Response {
    StatusCode::NO_CONTENT.into_response()
}

async fn journal_db_status(State(state): State<Arc<AppState>>) -> Response {
    axum::Json(journal_index::status(&state)).into_response()
}

async fn journal_db_sync(State(state): State<Arc<AppState>>) -> Response {
    match journal_index::sync(&state) {
        Ok(n) => axum::Json(serde_json::json!({"ok": true, "added": n})).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            axum::Json(serde_json::json!({"ok": false, "error": e})),
        )
            .into_response(),
    }
}

/// SSE：请求完成 / 状态保存时推一个极小事件，前端收到立刻刷一次，轮询只做兜底。
async fn events(State(state): State<Arc<AppState>>) -> Response {
    let mut rx = state.events_tx.subscribe();
    let stream = async_stream::stream! {
        loop {
            match rx.recv().await {
                Ok(v) => {
                    yield Ok::<_, std::convert::Infallible>(
                        Event::default().event("change").data(v.to_string()),
                    );
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(_) => break,
            }
        }
    };
    Sse::new(stream).keep_alive(KeepAlive::default()).into_response()
}

pub async fn serve(state: Arc<AppState>, port: u16) -> Result<(), String> {
    // 远程访问开关决定监听地址：关 = 只听本机；开 = 0.0.0.0（局域网 / Tailscale）。
    let remote = state
        .file
        .read()
        .map(|f| f.remote_access.unwrap_or(false))
        .unwrap_or(false);
    let bind_host = if remote { "0.0.0.0" } else { "127.0.0.1" };
    // 启动时把 JSONL 中尚未入索引的部分补齐；失败不阻塞服务，后续查询仍会重试。
    if let Err(e) = journal_index::sync(&state) {
        eprintln!("[journal-index] 初始化失败: {e}");
    }
    let app = Router::new()
        .route("/", get(index))
        .route("/ui", get(index))
        .route("/webui", get(index))
        .route("/webui.html", get(index))
        .route("/assets/webui.css", get(webui_css))
        .route("/assets/webui.js", get(webui_js))
        .route("/favicon.ico", get(favicon))
        .route("/health", get(health))
        .route("/healthz", get(health))
        .route("/events", get(events))
        .route("/ws", get(admin::ws))
        .route("/stats", get(admin::stats_handler))
        .route("/bootstrap", get(admin::bootstrap))
        .route("/stats/catalog", get(admin::stats_catalog))
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
        .route("/admin/remote", get(admin::remote_access).post(admin::set_remote_access))
        .route("/admin/relay", get(admin::relay_view).post(admin::relay_save))
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
        .route("/journal/db/status", get(journal_db_status))
        .route("/journal/db/sync", post(journal_db_sync))
        .route("/collector/status", get(collector::status))
        .route("/update/check", get(update::check))
        .route("/admin/journal", get(admin::journal_query))
        .route("/admin/journal/entry", get(admin::journal_entry))
        .route("/admin/history", get(admin::journal_query))
        .route("/v1/models", get(proxy::list_models))
        .route("/v1/chat/completions", post(proxy::chat_completions))
        .route("/v1/messages", post(anthropic::messages))
        .route("/agents", get(agents::list))
        .route("/agents/scan", get(agents::list))
        .route("/agents/configure", post(agents::configure))
        .route("/agents/restore", post(agents::restore))
        .route("/agents/repair", post(agents::repair))
        .route("/data/summary", get(transfer::summary))
        .route("/data/export", post(transfer::export_data))
        .route("/data/import", post(transfer::import_data))
        .route("/stats/series", get(insights::series))
        .route("/stats/today", get(insights::today))
        .route("/stats/cache-ledger", get(insights::cache_ledger))
        .route("/affinity/status", get(affinity::status_handler))
        .route("/affinity/clear", post(affinity::clear_handler))
        .route("/health/upstreams", get(health::report))
        .route("/health/upstreams/clear", post(health::clear))
        .route("/sessions/scan", get(sessions::scan))
        .route("/sessions/records", get(sessions::records))
        .route("/sessions/bases", get(sessions::bases))
        .route("/sessions/tools", get(sessions::tools))
        .layer(DefaultBodyLimit::max(64 * 1024 * 1024))
        .layer(CompressionLayer::new().compress_when(
            DefaultPredicate::new().and(NotForContentType::new("text/event-stream")),
        ))
        .with_state(state.clone());

    // 请求的端口被占（比如别的程序或旧实例）时，从 +1 开始往后找空闲端口，
    // 避免桌面版静默连到别人身上、出现「两个版本状态不一样」的错觉。
    let mut listener = None;
    let mut actual_port = port;
    for offset in 0..20u16 {
        let p = port + offset;
        match tokio::net::TcpListener::bind((bind_host, p)).await {
            Ok(l) => {
                listener = Some(l);
                actual_port = p;
                break;
            }
            Err(_) => continue,
        }
    }
    let listener = listener.ok_or_else(|| {
        format!("{bind_host}:{port} 起连续 20 个端口都被占用，请设置 LLM_ROUTER_PORT 换个端口")
    })?;
    if actual_port != port {
        eprintln!("[native] 端口 {port} 被占，已改用 {actual_port}");
    }
    // 无论是否漂移都要写入实际端口，start_backend 靠它返回真实地址。
    state
        .port
        .store(actual_port, std::sync::atomic::Ordering::Relaxed);

    // 统一采集器：一个后台循环预热会话扫描、工具排行与今日全量统计，页面共享缓存。
    collector::start(state.clone());
    update::start(state.clone());

    // 统计每 5 秒批量落盘一次，请求路径上不再同步写 stats.json。
    {
        let flush_state = state.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(std::time::Duration::from_secs(5));
            loop {
                ticker.tick().await;
                stats::flush(&flush_state);
            }
        });
    }

    // 边缘中继 agent：配置了 relay_url 才会真正连接。
    {
        let relay_state = state.clone();
        tokio::spawn(async move { relay::run(relay_state).await });
    }

    axum::serve(listener, app)
        .await
        .map_err(|e| format!("HTTP 服务退出: {e}"))
}
