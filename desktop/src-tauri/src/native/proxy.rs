use std::sync::{Arc, Mutex};
use std::time::Instant;

use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use futures_util::StreamExt;
use serde_json::{json, Value};

use super::state::{mask_key, AppState, Upstream};

/// 流式响应期间累积的用量与分块信息。
#[derive(Default)]
struct StreamUsage {
    /// 上游 SSE 里出现过的所有 usage 字段合并结果（保留原始键名）。
    usage: Value,
    chunks: usize,
    first_chunk_seconds: Option<f64>,
}

/// 把后到的 usage 对象合并进已有对象；嵌套对象递归合并。
/// message_start / message_delta 这类分段回报的字段靠这个拼回完整 usage。
fn merge_usage(dst: &mut Value, src: &Value) {
    if let (Some(d), Some(s)) = (dst.as_object_mut(), src.as_object()) {
        for (k, v) in s {
            if v.is_object() {
                let slot = d.entry(k.clone()).or_insert_with(|| json!({}));
                merge_usage(slot, v);
            } else {
                d.insert(k.clone(), v.clone());
            }
        }
    }
}

/// 从一行 SSE 里抽取 usage / 内容块，并记录首块到达时间。
fn observe_sse_line(line: &str, shared: &Arc<Mutex<StreamUsage>>, started_all: Instant) {
    let trimmed = line.trim_start();
    let Some(rest) = trimmed.strip_prefix("data:") else {
        return;
    };
    let payload = rest.trim();
    if payload.is_empty() || payload == "[DONE]" {
        return;
    }
    let Ok(v) = serde_json::from_str::<Value>(payload) else {
        return;
    };
    let mut acc = shared.lock().unwrap();
    let mut usage = v.get("usage").cloned();
    if usage.is_none() {
        usage = v.pointer("/message/usage").cloned();
    }
    if let Some(u) = usage {
        if u.is_object() {
            merge_usage(&mut acc.usage, &u);
        }
    }
    let has_delta = v
        .pointer("/choices/0/delta/content")
        .and_then(|c| c.as_str())
        .map(|s| !s.is_empty())
        .unwrap_or(false)
        || v.pointer("/choices/0/delta/reasoning_content")
            .and_then(|c| c.as_str())
            .map(|s| !s.is_empty())
            .unwrap_or(false)
        || v.pointer("/choices/0/delta/tool_calls")
            .and_then(|c| c.as_array())
            .map(|a| !a.is_empty())
            .unwrap_or(false)
        || matches!(
            v.get("type").and_then(|x| x.as_str()),
            Some("content_block_delta")
        );
    if has_delta {
        acc.chunks += 1;
        if acc.first_chunk_seconds.is_none() {
            acc.first_chunk_seconds = Some(started_all.elapsed().as_secs_f64());
        }
    }
}

/// 生成一条与 Python 版对齐的流水记录。
/// elapsed_seconds = 整个请求耗时（含重试、含流式输出全过程）；
/// first_chunk_seconds = 首个内容块到达时间（首字延迟，非流式时等于 elapsed_seconds）。
#[allow(clippy::too_many_arguments)]
fn journal_entry(
    up: &Upstream,
    requested: &str,
    real_model: &str,
    status: u16,
    ok: bool,
    stream: bool,
    attempts: &[Value],
    error: &str,
    usage: &Value,
    elapsed: f64,
    first_chunk: Option<f64>,
    chunks: usize,
    session_id: &str,
    session_source: &str,
) -> Value {
    let mut e = json!({
        "at": chrono::Utc::now().timestamp() as f64,
        "time": chrono::Local::now().format("%H:%M:%S").to_string(),
        "upstream_id": up.id,
        "upstream": up.name,
        "key": mask_key(up.api_keys.first().map(|s| s.as_str()).unwrap_or("")),
        "model": requested,
        "effective_model": real_model,
        "status": status,
        "elapsed_seconds": elapsed,
        "ok": ok,
        "stream": stream,
        "attempts": attempts.len(),
        "route_source": "配置",
        "session_id": session_id,
        "session_source": session_source,
        "error": error,
        "usage": super::stats::with_normalized_usage(usage),
        "trail": attempts,
    });
    e["first_chunk_seconds"] = first_chunk.map(|v| json!(v)).unwrap_or(Value::Null);
    e["chunks"] = json!(chunks);
    e
}

/// 流式请求结束时（或客户端提前断开时）写流水的守卫。
/// async_stream 的 generator 被 drop 时局部变量一起 drop，保证不会漏记。
struct StreamJournalGuard {
    state: Arc<AppState>,
    up: Upstream,
    requested: String,
    real_model: String,
    attempts: Vec<Value>,
    started_all: Instant,
    shared: Arc<Mutex<StreamUsage>>,
    written: bool,
    session_id: String,
    session_source: String,
}

impl Drop for StreamJournalGuard {
    fn drop(&mut self) {
        if self.written {
            return;
        }
        self.written = true;
        let acc = self.shared.lock().unwrap();
        let e = journal_entry(
            &self.up,
            &self.requested,
            &self.real_model,
            200,
            true,
            true,
            &self.attempts,
            "",
            &acc.usage,
            self.started_all.elapsed().as_secs_f64(),
            acc.first_chunk_seconds,
            acc.chunks,
            &self.session_id,
            &self.session_source,
        );
        super::state::append_journal(&self.state, e);
        let usage = super::stats::normalize_usage(&acc.usage);
        let cache_read = usage.get("cache_read").and_then(Value::as_u64).unwrap_or(0);
        super::affinity::remember(&self.state, &self.requested, &self.session_id, &self.up.id, cache_read);
    }
}

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
        _ => {
            // priority：可用的排前面（保持配置顺序），冷却中的垫底——
            // 和 Python 版 _order() 同语义，保证严格按用户排的顺序优先调用。
            let (ready, cooling): (Vec<_>, Vec<_>) = pairs
                .into_iter()
                .partition(|(up, _)| super::state::upstream_available(state, &up.id));
            ready.into_iter().chain(cooling).collect()
        }
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

fn attach_headers(mut resp: Response, headers: &HeaderMap) -> Response {
    resp.headers_mut().extend(headers.clone());
    resp
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
    headers: HeaderMap,
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
    let (session, session_source) = super::affinity::session_key(&headers, &payload);
    let session_id = session.unwrap_or_default();
    let affinity_mode = super::affinity::mode_for(&state, &requested);
    let raw_candidates = resolve(&state, &requested);
    let (candidates, sticky_upstream) =
        super::affinity::apply(&state, &requested, &session_id, &affinity_mode, raw_candidates);
    if candidates.is_empty() {
        return error_response(StatusCode::SERVICE_UNAVAILABLE, "没有可用的上游", &[]);
    }
    let mut response_headers = HeaderMap::new();
    if !session_id.is_empty() {
        if let Ok(v) = session_id.parse() {
            response_headers.insert("x-llm-router-session", v);
        }
    }
    if let Some(id) = &sticky_upstream {
        if let Ok(v) = id.parse() {
            response_headers.insert("x-llm-router-affinity", v);
        }
    }

    let mut attempts: Vec<Value> = Vec::new();
    let mut last_status = StatusCode::BAD_GATEWAY;
    let mut last_message = "所有上游都失败了".to_string();
    let started_all = std::time::Instant::now();

    // 每次尝试记录进 journal（trail 字段），UI 的尝试链才有完整顺序。
    macro_rules! push_attempt {
        ($up:expr, $model:expr, $ok:expr, $status:expr, $elapsed:expr, $err:expr) => {
            attempts.push(json!({
                "upstream": $up.name, "upstream_id": $up.id, "model": $model,
                "ok": $ok, "status": $status,
                "elapsed_seconds": $elapsed,
                "error": $err, "reason": $err,
            }));
        };
    }
    for (up, real_model) in &candidates {
        let url = join_url(&up.base_url, "/chat/completions");
        let mut out = payload.clone();
        out["model"] = json!(real_model);
        if stream {
            // 让上游在最后一块回报 usage；否则流式请求永远统计不到 token / 缓存读写。
            if !out.get("stream_options").map(|v| v.is_object()).unwrap_or(false) {
                out["stream_options"] = json!({ "include_usage": true });
            } else if out["stream_options"].get("include_usage").is_none() {
                out["stream_options"]["include_usage"] = json!(true);
            }
        }
        let started = Instant::now();
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
                        // 先读到首包再决定是否提交：首包就失败/为空的，换下一个上游，
                        // 而不是把一个半截的 200 流交给客户端。
                        let mut source = resp.bytes_stream();
                        let first = match source.next().await {
                            Some(Ok(bytes)) => bytes,
                            Some(Err(e)) => {
                                super::state::record_upstream_result(&state, &up.id, false, &e.to_string());
                                push_attempt!(up, real_model, false, status.as_u16(), started.elapsed().as_secs_f64(), e.to_string());
                                last_message = format!("上游 {} 首包读取失败: {e}", up.name);
                                continue;
                            }
                            None => {
                                super::state::record_upstream_result(&state, &up.id, false, "首包为空");
                                push_attempt!(up, real_model, false, status.as_u16(), started.elapsed().as_secs_f64(), "首包为空");
                                last_message = format!("上游 {} 返回空响应", up.name);
                                continue;
                            }
                        };
                        super::state::record_upstream_result(&state, &up.id, true, "");
                        push_attempt!(up, real_model, true, status.as_u16(), started.elapsed().as_secs_f64(), "");
                        state.bump_total();
                        let requested_name = requested.clone();
                        let shared = Arc::new(Mutex::new(StreamUsage {
                            usage: json!({}),
                            chunks: 0,
                            first_chunk_seconds: None,
                        }));
                        let observe = shared.clone();
                        let guard = StreamJournalGuard {
                            state: state.clone(),
                            up: up.clone(),
                            requested: requested.clone(),
                            real_model: real_model.clone(),
                            attempts: attempts.clone(),
                            started_all,
                            shared,
                            written: false,
                            session_id: session_id.clone(),
                            session_source: session_source.clone(),
                        };
                        let body_stream = async_stream::stream! {
                            // guard 被 generator 持有，流结束/客户端断开时 Drop 里补写完整流水。
                            let _guard = guard;
                            let mut buffer = String::new();
                            buffer.push_str(&String::from_utf8_lossy(&first));
                            while let Some(pos) = buffer.find('\n') {
                                let line = buffer[..=pos].to_string();
                                buffer.drain(..=pos);
                                observe_sse_line(&line, &observe, started_all);
                                yield Ok::<_, std::io::Error>(Bytes::from(rewrite_sse_line(&line, &requested_name)));
                            }
                            while let Some(chunk) = source.next().await {
                                match chunk {
                                    Ok(bytes) => {
                                        buffer.push_str(&String::from_utf8_lossy(&bytes));
                                        while let Some(pos) = buffer.find('\n') {
                                            let line = buffer[..=pos].to_string();
                                            buffer.drain(..=pos);
                                            observe_sse_line(&line, &observe, started_all);
                                            yield Ok::<_, std::io::Error>(Bytes::from(rewrite_sse_line(&line, &requested_name)));
                                        }
                                        // 单行异常长（没有换行）时先吐出去，防止内存无限增长
                                        if buffer.len() > 1024 * 1024 {
                                            observe_sse_line(&buffer, &observe, started_all);
                                            yield Ok::<_, std::io::Error>(Bytes::from(rewrite_sse_line(&buffer, &requested_name)));
                                            buffer.clear();
                                        }
                                    }
                                    Err(e) => {
                                        yield Err(std::io::Error::new(std::io::ErrorKind::Other, e));
                                        return;
                                    }
                                }
                            }
                            if !buffer.is_empty() {
                                observe_sse_line(&buffer, &observe, started_all);
                                yield Ok::<_, std::io::Error>(Bytes::from(rewrite_sse_line(&buffer, &requested_name)));
                            }
                        };
                        let resp = Response::builder()
                            .status(StatusCode::OK)
                            .header("content-type", content_type)
                            .header("cache-control", "no-cache")
                            .body(Body::from_stream(body_stream))
                            .unwrap();
                        return attach_headers(resp, &response_headers);
                    }
                    let bytes = match resp.bytes().await {
                        Ok(b) => b,
                        Err(e) => {
                            super::state::record_upstream_result(&state, &up.id, false, &e.to_string());
                            push_attempt!(up, real_model, false, status.as_u16(), started.elapsed().as_secs_f64(), e.to_string());
                            last_message = format!("读取上游响应失败: {e}");
                            continue;
                        }
                    };
                    let usage = serde_json::from_slice::<Value>(&bytes)
                        .ok()
                        .and_then(|v| v.get("usage").cloned())
                        .unwrap_or_else(|| json!({}));
                    let body = rewrite_model(&bytes, &requested).unwrap_or(bytes);
                    super::state::record_upstream_result(&state, &up.id, true, "");
                    push_attempt!(up, real_model, true, status.as_u16(), started.elapsed().as_secs_f64(), "");
                    let elapsed = started_all.elapsed().as_secs_f64();
                    let normalized = super::stats::normalize_usage(&usage);
                    super::state::append_journal(
                        &state,
                        journal_entry(
                            up,
                            &requested,
                            real_model,
                            200,
                            true,
                            false,
                            &attempts,
                            "",
                            &usage,
                            elapsed,
                            Some(elapsed),
                            1,
                            &session_id,
                            &session_source,
                        ),
                    );
                    super::affinity::remember(
                        &state,
                        &requested,
                        &session_id,
                        &up.id,
                        normalized.get("cache_read").and_then(Value::as_u64).unwrap_or(0),
                    );
                    state.bump_total();
                    let resp = Response::builder()
                        .status(StatusCode::OK)
                        .header("content-type", content_type)
                        .body(Body::from(body))
                        .unwrap();
                    return attach_headers(resp, &response_headers);
                }
                let text = resp.text().await.unwrap_or_default();
                super::state::record_upstream_result(&state, &up.id, false, &truncate(&text, 200));
                push_attempt!(up, real_model, false, status.as_u16(), started.elapsed().as_secs_f64(), truncate(&text, 200));
                last_status = StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
                last_message = format!("上游 {} 返回 HTTP {}: {}", up.name, status, truncate(&text, 400));
            }
            Err(e) => {
                super::state::record_upstream_result(&state, &up.id, false, &e.to_string());
                push_attempt!(up, real_model, false, Value::Null, started.elapsed().as_secs_f64(), e.to_string());
                last_message = format!("连接上游 {} 失败: {e}", up.name);
            }
        }
    }
    state.bump_total();
    // 全部失败：最后一家当记录主体，trail 里保留完整尝试链。
    let (final_up, final_model) = candidates
        .last()
        .cloned()
        .map(|(u, m)| (u, m))
        .unwrap_or_else(|| {
            (
                Upstream { id: "-".into(), name: "-".to_string(), ..Default::default() },
                requested.clone(),
            )
        });
    let elapsed = started_all.elapsed().as_secs_f64();
    super::state::append_journal(
        &state,
        journal_entry(
            &final_up,
            &requested,
            &final_model,
            last_status.as_u16(),
            false,
            stream,
            &attempts,
            &last_message,
            &json!({}),
            elapsed,
            None,
            0,
            &session_id,
            &session_source,
        ),
    );
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
