use std::sync::Arc;

use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use futures_util::StreamExt;
use serde_json::{json, Value};

use super::state::{mask_key, AppState, Upstream};

fn join_url(base: &str, endpoint: &str) -> String {
    let mut b = base.trim_end_matches('/').to_string();
    if b.ends_with("/v1") {
        b.push_str(endpoint);
    } else if b.contains("/v1/") {
        b.push_str(endpoint);
    } else {
        b.push_str("/v1");
        b.push_str(endpoint);
    }
    b
}

pub fn models_url(base: &str) -> String {
    join_url(base, "/models")
}

pub fn resolve(state: &AppState, requested: &str) -> Vec<(Upstream, String)> {
    let file = state.file.read().unwrap();
    let mut pairs: Vec<(Upstream, String)> = Vec::new();

    if let Some(route) = file.models.get(requested) {
        for e in &route.entries {
            if !e.enabled || e.skipped() {
                continue;
            }
            if let Some(up) = file.find_upstream(&e.upstream) {
                if up.enabled {
                    pairs.push((up.clone(), if e.model.is_empty() { requested.to_string() } else { e.model.clone() }));
                }
            }
        }
        if !pairs.is_empty() {
            return order(pairs, &route.strategy, requested, state);
        }
    }

    for up in &file.upstreams {
        if !up.enabled {
            continue;
        }
        for real in up.real_models_in(requested) {
            pairs.push((up.clone(), real));
        }
    }
    if !pairs.is_empty() {
        return pairs;
    }
    for up in &file.upstreams {
        if up.enabled && up.is_offered(requested) {
            pairs.push((up.clone(), requested.to_string()));
        }
    }
    if !pairs.is_empty() {
        return pairs;
    }
    file.upstreams
        .iter()
        .filter(|u| u.enabled)
        .map(|u| (u.clone(), requested.to_string()))
        .collect()
}

fn order(
    mut pairs: Vec<(Upstream, String)>,
    strategy: &str,
    model: &str,
    state: &AppState,
) -> Vec<(Upstream, String)> {
    match strategy {
        "random" => {
            use rand::seq::SliceRandom;
            pairs.shuffle(&mut rand::thread_rng());
            pairs
        }
        "round_robin" => {
            let mut rr = state.rr.lock().unwrap();
            let pos = rr.entry(model.to_string()).or_insert(0);
            let n = pairs.len();
            if n > 0 {
                let start = *pos % n;
                *pos = start + 1;
                pairs.rotate_left(start);
            }
            pairs
        }
        _ => pairs,
    }
}

fn auth_headers(up: &Upstream) -> HeaderMap {
    let mut headers = HeaderMap::new();
    if let Some(key) = up.api_keys.first() {
        if let Ok(v) = format!("Bearer {key}").parse() {
            headers.insert("authorization", v);
        }
    }
    headers.insert("content-type", "application/json".parse().unwrap());
    headers
}

fn error_response(status: StatusCode, message: &str, attempts: &[Value]) -> Response {
    (
        status,
        axum::Json(json!({
            "error": {
                "message": message,
                "type": "router_error",
                "attempts": attempts,
            }
        })),
    )
        .into_response()
}

pub async fn chat_completions(
    State(state): State<Arc<AppState>>,
    _headers: HeaderMap,
    body: Bytes,
) -> Response {
    let payload: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => return error_response(StatusCode::BAD_REQUEST, &format!("请求体不是合法 JSON: {e}"), &[]),
    };
    let requested = payload
        .get("model")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if requested.is_empty() {
        return error_response(StatusCode::BAD_REQUEST, "请求里缺少 model", &[]);
    }
    let stream = payload.get("stream").and_then(|v| v.as_bool()).unwrap_or(false);
    let candidates = resolve(&state, &requested);
    if candidates.is_empty() {
        return error_response(StatusCode::SERVICE_UNAVAILABLE, "没有可用的上游", &[]);
    }

    let mut attempts: Vec<Value> = Vec::new();
    let mut last_status = StatusCode::BAD_GATEWAY;
    let mut last_message = "所有上游都失败了".to_string();

    for (up, real_model) in candidates {
        let url = join_url(&up.base_url, "/chat/completions");
        let mut out = payload.clone();
        out["model"] = json!(real_model);
        let started = std::time::Instant::now();
        let resp = state
            .client
            .post(&url)
            .headers(auth_headers(&up))
            .json(&out)
            .send()
            .await;

        match resp {
            Ok(resp) => {
                let status = resp.status();
                if status.is_success() {
                    let content_type = resp
                        .headers()
                        .get("content-type")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("application/json")
                        .to_string();
                    if stream {
                        let requested_name = requested.clone();
                        let mut source = resp.bytes_stream();
                        let body_stream = async_stream::stream! {
                            let mut buffer = String::new();
                            while let Some(chunk) = source.next().await {
                                match chunk {
                                    Ok(bytes) => {
                                        buffer.push_str(&String::from_utf8_lossy(&bytes));
                                        while let Some(pos) = buffer.find('\n') {
                                            let line = buffer[..=pos].to_string();
                                            buffer.drain(..=pos);
                                            yield Ok::<_, std::io::Error>(Bytes::from(rewrite_sse_line(&line, &requested_name)));
                                        }
                                    }
                                    Err(e) => {
                                        yield Err(std::io::Error::new(std::io::ErrorKind::Other, e));
                                        return;
                                    }
                                }
                            }
                            if !buffer.is_empty() {
                                yield Ok::<_, std::io::Error>(Bytes::from(rewrite_sse_line(&buffer, &requested_name)));
                            }
                        };
                        return Response::builder()
                            .status(StatusCode::OK)
                            .header("content-type", content_type)
                            .header("cache-control", "no-cache")
                            .body(Body::from_stream(body_stream))
                            .unwrap();
                    }
                    let bytes = match resp.bytes().await {
                        Ok(b) => b,
                        Err(e) => {
                            last_message = format!("读取上游响应失败: {e}");
                            continue;
                        }
                    };
                    let body = rewrite_model(&bytes, &requested).unwrap_or(bytes);
                    attempts.push(json!({
                        "upstream": up.name, "upstream_id": up.id,
                        "model": real_model, "ok": true, "status": status.as_u16(),
                        "elapsed_seconds": started.elapsed().as_secs_f64(),
                    }));
                    state.bump_total();
                    return Response::builder()
                        .status(StatusCode::OK)
                        .header("content-type", content_type)
                        .body(Body::from(body))
                        .unwrap();
                }
                let text = resp.text().await.unwrap_or_default();
                last_status = StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
                last_message = format!("上游 {} 返回 HTTP {}: {}", up.name, status, truncate(&text, 400));
                attempts.push(json!({
                    "upstream": up.name, "upstream_id": up.id, "model": real_model,
                    "ok": false, "status": status.as_u16(),
                    "elapsed_seconds": started.elapsed().as_secs_f64(),
                    "error": truncate(&text, 400),
                }));
            }
            Err(e) => {
                last_message = format!("连接上游 {} 失败: {e}", up.name);
                attempts.push(json!({
                    "upstream": up.name, "upstream_id": up.id, "model": real_model,
                    "ok": false, "status": Value::Null,
                    "elapsed_seconds": started.elapsed().as_secs_f64(),
                    "error": e.to_string(),
                }));
            }
        }
    }
    state.bump_total();
    error_response(last_status, &last_message, &attempts)
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        s.chars().take(n).collect::<String>() + "…"
    }
}

fn rewrite_model(body: &[u8], requested: &str) -> Option<Bytes> {
    let mut v: Value = serde_json::from_slice(body).ok()?;
    if v.get("model").is_some() {
        v["model"] = json!(requested);
        serde_json::to_vec(&v).ok().map(Bytes::from)
    } else {
        None
    }
}

fn rewrite_sse_line(line: &str, requested: &str) -> String {
    let trimmed = line.trim_start();
    if let Some(rest) = trimmed.strip_prefix("data:") {
        let payload = rest.trim_start();
        if payload != "[DONE]" {
            if let Ok(mut v) = serde_json::from_str::<Value>(payload) {
                if v.get("model").is_some() {
                    v["model"] = json!(requested);
                    if let Ok(text) = serde_json::to_string(&v) {
                        return format!("data: {text}\n");
                    }
                }
            }
        }
    }
    line.to_string()
}

pub async fn list_models(State(state): State<Arc<AppState>>) -> Response {
    let file = state.file.read().unwrap();
    let mut data = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for up in &file.upstreams {
        if !up.enabled {
            continue;
        }
        for m in up.selected() {
            let group = up.group_of(&m);
            if seen.insert(group.clone()) {
                data.push(json!({
                    "id": group,
                    "object": "model",
                    "created": 0,
                    "owned_by": "llm-router",
                    "upstreams": [up.name],
                    "configured": file.models.contains_key(&group),
                }));
            }
        }
    }
    axum::Json(json!({"object": "list", "data": data})).into_response()
}

pub async fn ping_upstream(state: &AppState, up: &Upstream, model: &str) -> Value {
    let started = std::time::Instant::now();
    let url = join_url(&up.base_url, "/chat/completions");
    let body = json!({
        "model": model,
        "messages": [{"role": "user", "content": "ping"}],
        "max_tokens": 1,
        "stream": false,
    });
    match state
        .client
        .post(&url)
        .headers(auth_headers(up))
        .json(&body)
        .timeout(std::time::Duration::from_secs(
            up.first_timeout_seconds.unwrap_or(30.0) as u64,
        ))
        .send()
        .await
    {
        Ok(resp) => {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            json!({
                "id": up.id, "name": up.name, "model": model,
                "ok": status.is_success(),
                "status": status.as_u16(),
                "latency_seconds": started.elapsed().as_secs_f64(),
                "error": if status.is_success() { Value::Null } else { json!(truncate(&text, 300)) },
                "key_hint": up.api_keys.first().map(|k| mask_key(k)).unwrap_or_default(),
            })
        }
        Err(e) => json!({
            "id": up.id, "name": up.name, "model": model,
            "ok": false, "status": Value::Null,
            "latency_seconds": started.elapsed().as_secs_f64(),
            "error": e.to_string(),
        }),
    }
}
