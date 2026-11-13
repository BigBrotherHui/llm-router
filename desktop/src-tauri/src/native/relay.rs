//! 边缘中继 agent：桌面端主动连 Cloudflare Worker 的 Durable Object，
//! 把「移动端快照 + 增量」推上去；手机连同一个 DO，就近拿数据。

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::sync::mpsc::UnboundedSender;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;

use super::state::AppState;

/// 手机端只需要「摘要」：统计、上游概览、模型路由概览、最近 50 条记录。
pub fn snapshot(state: &AppState) -> Value {
    let stats = super::stats::load_runtime(state);
    // 只要汇总数字：不要把 totals 里嵌套的 models/upstreams 全量历史推给手机。
    let totals = super::stats::meta(&stats)
        .get("totals")
        .cloned()
        .unwrap_or_else(|| json!({}));

    let mut upstreams: Vec<Value> = Vec::new();
    {
        let file = state.file.read().unwrap();
        for up in &file.upstreams {
            let selected = up.selected().len();
            upstreams.push(json!({
                "id": up.id,
                "name": up.name,
                "enabled": up.enabled,
                "selected": selected,
                "available": up.available_models.len(),
                "cooling": super::state::upstream_cooling_remaining(state, &up.id) > 0,
            }));
        }
    }

    let mut models: Vec<Value> = Vec::new();
    {
        let file = state.file.read().unwrap();
        for (name, route) in &file.models {
            let enabled = route.entries.iter().filter(|e| e.enabled).count();
            models.push(json!({
                "name": name,
                "entries": route.entries.len(),
                "enabled": enabled,
            }));
        }
    }
    models.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));

    let mut q: HashMap<String, String> = HashMap::new();
    q.insert("limit".to_string(), "20".to_string());
    let page = super::journal::query(state, &q);
    let journal = page.get("entries").cloned().unwrap_or_else(|| json!([]));
    let journal_total = page.get("total").cloned().unwrap_or_else(|| json!(0));

    json!({
        "type": "snapshot",
        "at": chrono::Utc::now().timestamp() as f64,
        "totals": totals,
        "upstreams": upstreams,
        "models": models,
        "journal": journal,
        "journal_total": journal_total,
    })
}

/// 常驻任务：有配置就连，断了按 5 秒重连。
pub async fn run(state: Arc<AppState>) {
    loop {
        let (base, token) = {
            let file = state.file.read().unwrap();
            (
                file.relay_url.clone().unwrap_or_default(),
                file.relay_token.clone().unwrap_or_default(),
            )
        };
        if base.is_empty() {
            tokio::time::sleep(Duration::from_secs(10)).await;
            continue;
        }
        let url = format!("{}/agent", base.trim_end_matches('/'));
        // tungstenite 只认 ws/wss 前缀；用户填的是 https 地址。
        let url = url
            .replacen("https://", "wss://", 1)
            .replacen("http://", "ws://", 1);
        if let Err(e) = session(&state, &url, token.as_str()).await {
            eprintln!("[relay] {e}");
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

async fn session(state: &Arc<AppState>, url: &str, token: &str) -> Result<(), String> {
    let mut request = url
        .into_client_request()
        .map_err(|e| format!("构造请求失败: {e}"))?;
    if !token.is_empty() {
        request.headers_mut().insert(
            "authorization",
            format!("Bearer {token}")
                .parse()
                .map_err(|_| "token 非法".to_string())?,
        );
    }
    let (mut ws, _) = tokio_tungstenite::connect_async(request)
        .await
        .map_err(|e| format!("连接边缘失败: {e}"))?;

    let snap = snapshot(state);
    ws.send(Message::Text(snap.to_string()))
        .await
        .map_err(|e| format!("发送快照失败: {e}"))?;

    let mut rx = state.events_tx.subscribe();
    let (tx, mut outbox) = tokio::sync::mpsc::unbounded_channel::<String>();
    loop {
        tokio::select! {
            event = rx.recv() => match event {
                Ok(v) => {
                    let payload = if v.get("type").and_then(|t| t.as_str()) == Some("journal") {
                        json!({"type": "journal", "entry": v.get("entry").cloned().unwrap_or(Value::Null)})
                    } else {
                        v
                    };
                    if ws.send(Message::Text(payload.to_string())).await.is_err() {
                        return Ok(());
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(_) => return Ok(()),
            },
            outbound = outbox.recv() => {
                if let Some(text) = outbound {
                    if ws.send(Message::Text(text)).await.is_err() {
                        return Ok(());
                    }
                }
            },
            inbound = ws.next() => match inbound {
                Some(Ok(Message::Text(text))) => {
                    let parsed: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
                    match parsed.get("type").and_then(|t| t.as_str()) {
                        Some("pull") => {
                            let snap = snapshot(state);
                            if ws.send(Message::Text(snap.to_string())).await.is_err() {
                                return Ok(());
                            }
                        }
                        Some("req") => {
                            // 完整控制台的 API 调用：并发转发到本机 8010，结果再回传边缘。
                            let state = state.clone();
                            let tx = tx.clone();
                            tokio::spawn(async move { handle_rpc(state, tx, parsed).await; });
                        }
                        _ => {}
                    }
                }
                Some(Ok(Message::Ping(p))) => {
                    let _ = ws.send(Message::Pong(p)).await;
                }
                Some(Ok(Message::Close(_))) | None => return Ok(()),
                Some(Err(e)) => return Err(format!("长连接断开: {e}")),
                _ => {}
            },
        }
    }
}

/// 把边缘转发来的 HTTP 请求打到本机服务，再把状态/头/体（base64）回传。
async fn handle_rpc(state: Arc<AppState>, tx: UnboundedSender<String>, req: Value) {
    let id = req.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let method = req.get("method").and_then(|v| v.as_str()).unwrap_or("GET");
    let path = req.get("path").and_then(|v| v.as_str()).unwrap_or("/");
    let port = state
        .port
        .load(std::sync::atomic::Ordering::Relaxed);
    let port = if port == 0 { 8010 } else { port };
    let url = format!("http://127.0.0.1:{port}{path}");

    let mut builder = state
        .client
        .request(
            method.parse().unwrap_or(reqwest::Method::GET),
            &url,
        )
        .timeout(Duration::from_secs(30));
    if let Some(headers) = req.get("headers").and_then(|v| v.as_object()) {
        for (k, v) in headers {
            if let Some(s) = v.as_str() {
                builder = builder.header(k, s);
            }
        }
    }
    if let Some(body) = req.get("body").and_then(|v| v.as_str()) {
        if !body.is_empty() {
            if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(body) {
                builder = builder.body(bytes);
            }
        }
    }

    let payload = match builder.send().await {
        Ok(resp) => {
            let status = resp.status().as_u16();
            let mut headers = serde_json::Map::new();
            for (k, v) in resp.headers() {
                let name = k.as_str().to_lowercase();
                if ["content-length", "content-encoding", "transfer-encoding"]
                    .contains(&name.as_str())
                {
                    continue;
                }
                if let Ok(s) = v.to_str() {
                    headers.insert(name, json!(s));
                }
            }
            let bytes = resp.bytes().await.unwrap_or_default();
            json!({
                "type": "res",
                "id": id,
                "status": status,
                "headers": headers,
                "body": base64::engine::general_purpose::STANDARD.encode(&bytes),
            })
        }
        Err(e) => json!({
            "type": "res",
            "id": id,
            "status": 502,
            "headers": {"content-type": "application/json"},
            "body": base64::engine::general_purpose::STANDARD.encode(
                json!({"error": e.to_string()}).to_string()
            ),
        }),
    };
    let _ = tx.send(payload.to_string());
}
