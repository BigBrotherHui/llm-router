//! 边缘中继 agent：桌面端主动连 Cloudflare Worker 的 Durable Object，
//! 把「移动端快照 + 增量」推上去；手机连同一个 DO，就近拿数据。

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;

use super::state::AppState;

/// 手机端只需要「摘要」：统计、上游概览、模型路由概览、最近 50 条记录。
pub fn snapshot(state: &AppState) -> Value {
    let stats = super::stats::load_runtime(state);
    let totals = stats
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
            let mut names: Vec<String> = route
                .entries
                .iter()
                .map(|e| {
                    file.upstreams
                        .iter()
                        .find(|u| u.id == e.upstream)
                        .map(|u| u.name.clone())
                        .unwrap_or_else(|| e.upstream.clone())
                })
                .collect();
            names.dedup();
            models.push(json!({
                "name": name,
                "entries": route.entries.len(),
                "enabled": enabled,
                "upstreams": names,
            }));
        }
    }
    models.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));

    let mut q: HashMap<String, String> = HashMap::new();
    q.insert("limit".to_string(), "50".to_string());
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
            inbound = ws.next() => match inbound {
                Some(Ok(Message::Text(text))) => {
                    if text.contains("\"pull\"") {
                        let snap = snapshot(state);
                        if ws.send(Message::Text(snap.to_string())).await.is_err() {
                            return Ok(());
                        }
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
