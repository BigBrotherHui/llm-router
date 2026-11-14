//! Anthropic /v1/messages 适配：Claude Code 等 Agent 把 ANTHROPIC_BASE_URL 指到本服务时，
//! 把请求转成 OpenAI chat/completions，再把响应（含 SSE）转回 Anthropic 格式。

use std::sync::Arc;

use axum::body::{to_bytes, Body, Bytes};
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use serde_json::{json, Value};
use uuid::Uuid;

use super::proxy;
use super::state::AppState;

const MAX_BODY: usize = 64 * 1024 * 1024;

fn anthropic_error(status: StatusCode, message: &str) -> Response {
    (
        status,
        axum::Json(json!({
            "type": "error",
            "error": {"type": "api_error", "message": message},
        })),
    )
        .into_response()
}

fn flatten_content(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Array(arr) => {
            let mut parts = Vec::new();
            for item in arr {
                match item {
                    Value::String(s) => parts.push(s.clone()),
                    Value::Object(_) => {
                        let kind = item.get("type").and_then(|x| x.as_str()).unwrap_or("");
                        if kind == "text" {
                            if let Some(t) = item.get("text").and_then(|x| x.as_str()) {
                                parts.push(t.to_string());
                            }
                        } else if kind == "tool_result" {
                            parts.push(flatten_content(item.get("content").unwrap_or(&Value::Null)));
                        } else if let Some(t) = item.get("text").and_then(|x| x.as_str()) {
                            parts.push(t.to_string());
                        }
                    }
                    _ => {}
                }
            }
            parts.join("\n")
        }
        Value::Object(_) => v
            .get("text")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string(),
        _ => String::new(),
    }
}

fn estimate_tokens(s: &str) -> u64 {
    ((s.chars().count() as u64) + 3) / 4
}

fn build_openai_request(req: &Value) -> Result<(Value, String, bool, String), String> {
    let model = req
        .get("model")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if model.is_empty() {
        return Err("请求里缺少 model".to_string());
    }
    let stream = req.get("stream").and_then(|v| v.as_bool()).unwrap_or(false);
    let mut messages: Vec<Value> = Vec::new();
    if let Some(system) = req.get("system") {
        let text = flatten_content(system);
        if !text.is_empty() {
            messages.push(json!({"role": "system", "content": text}));
        }
    }
    if let Some(arr) = req.get("messages").and_then(|v| v.as_array()) {
        for m in arr {
            let role = m.get("role").and_then(|v| v.as_str()).unwrap_or("user");
            let content = flatten_content(m.get("content").unwrap_or(&Value::Null));
            messages.push(json!({"role": role, "content": content}));
        }
    }
    if messages.is_empty() {
        return Err("请求里没有可转发的 messages".to_string());
    }
    let mut out = json!({
        "model": model.clone(),
        "messages": messages,
        "max_tokens": req.get("max_tokens").and_then(|v| v.as_u64()).unwrap_or(1024),
        "stream": stream,
    });
    if let Some(t) = req.get("temperature") {
        out["temperature"] = t.clone();
    }
    if let Some(t) = req.get("top_p") {
        out["top_p"] = t.clone();
    }
    if let Some(s) = req.get("stop_sequences") {
        out["stop"] = s.clone();
    }
    let id = format!("msg_{}", Uuid::new_v4().simple());
    Ok((out, model, stream, id))
}

fn sse_event(event: &str, data: &Value) -> String {
    format!(
        "event: {event}\ndata: {}\n\n",
        serde_json::to_string(data).unwrap_or_else(|_| "{}".to_string())
    )
}

pub async fn messages(
    State(state): State<Arc<AppState>>,
    _headers: HeaderMap,
    body: Bytes,
) -> Response {
    let req: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => {
            return anthropic_error(
                StatusCode::BAD_REQUEST,
                &format!("请求体不是合法 JSON: {e}"),
            )
        }
    };
    let (openai_req, model, stream, msg_id) = match build_openai_request(&req) {
        Ok(v) => v,
        Err(e) => return anthropic_error(StatusCode::BAD_REQUEST, &e),
    };
    let input_tokens = estimate_tokens(
        &serde_json::to_string(&openai_req["messages"]).unwrap_or_default(),
    );
    let openai_body = match serde_json::to_vec(&openai_req) {
        Ok(b) => Bytes::from(b),
        Err(e) => {
            return anthropic_error(
                StatusCode::BAD_REQUEST,
                &format!("构造转发请求失败: {e}"),
            )
        }
    };
    let resp = proxy::chat_completions(State(state.clone()), HeaderMap::new(), openai_body).await;
    let status = resp.status();
    let bytes = match to_bytes(resp.into_body(), MAX_BODY).await {
        Ok(b) => b,
        Err(e) => {
            return anthropic_error(
                StatusCode::BAD_GATEWAY,
                &format!("读取上游响应失败: {e}"),
            )
        }
    };
    if !status.is_success() {
        let text = String::from_utf8_lossy(&bytes);
        let message = serde_json::from_slice::<Value>(&bytes)
            .ok()
            .and_then(|v| {
                v.get("error")
                    .and_then(|e| e.get("message"))
                    .and_then(|m| m.as_str())
                    .map(|s| s.to_string())
            })
            .unwrap_or_else(|| text.to_string());
        return anthropic_error(status, &message);
    }

    if !stream {
        let v: Value = serde_json::from_slice(&bytes).unwrap_or_else(|_| json!({}));
        let text = v["choices"][0]["message"]["content"]
            .as_str()
            .unwrap_or("")
            .to_string();
        let finish = v["choices"][0]["finish_reason"].as_str().unwrap_or("stop");
        let stop_reason = if finish == "length" {
            "max_tokens"
        } else {
            "end_turn"
        };
        let usage = v.get("usage");
        let in_tok = usage
            .and_then(|u| u.get("prompt_tokens"))
            .and_then(|x| x.as_u64())
            .unwrap_or(input_tokens);
        let out_tok = usage
            .and_then(|u| u.get("completion_tokens"))
            .and_then(|x| x.as_u64())
            .unwrap_or_else(|| estimate_tokens(&text));
        return axum::Json(json!({
            "id": msg_id,
            "type": "message",
            "role": "assistant",
            "model": model.clone(),
            "content": [{"type": "text", "text": text}],
            "stop_reason": stop_reason,
            "stop_sequence": Value::Null,
            "usage": {"input_tokens": in_tok, "output_tokens": out_tok},
        }))
        .into_response();
    }

    // 流式：把整段 OpenAI SSE 收齐后转成 Anthropic 事件流。
    // 本地路由上游响应量不大，缓冲后一次性发出仍是合法 SSE，Claude Code 可正常解析。
    let raw = String::from_utf8_lossy(&bytes);
    let mut text = String::new();
    let mut finish = "stop".to_string();
    for line in raw.lines() {
        let t = line.trim_start();
        let Some(payload) = t.strip_prefix("data:") else {
            continue;
        };
        let payload = payload.trim();
        if payload == "[DONE]" || payload.is_empty() {
            continue;
        }
        let Ok(chunk) = serde_json::from_str::<Value>(payload) else {
            continue;
        };
        if let Some(delta) = chunk["choices"][0]["delta"]["content"].as_str() {
            text.push_str(delta);
        }
        if let Some(f) = chunk["choices"][0]["finish_reason"].as_str() {
            finish = f.to_string();
        }
    }
    let stop_reason = if finish == "length" {
        "max_tokens"
    } else {
        "end_turn"
    };
    let out_tok = estimate_tokens(&text);
    let mut sse = String::new();
    sse.push_str(&sse_event(
        "message_start",
        &json!({
            "type": "message_start",
            "message": {
                "id": msg_id,
                "type": "message",
                "role": "assistant",
                "model": model.clone(),
                "content": [],
                "stop_reason": Value::Null,
                "stop_sequence": Value::Null,
                "usage": {"input_tokens": input_tokens, "output_tokens": 0},
            },
        }),
    ));
    sse.push_str(&sse_event(
        "content_block_start",
        &json!({
            "type": "content_block_start",
            "index": 0,
            "content_block": {"type": "text", "text": ""},
        }),
    ));
    if !text.is_empty() {
        sse.push_str(&sse_event(
            "content_block_delta",
            &json!({
                "type": "content_block_delta",
                "index": 0,
                "delta": {"type": "text_delta", "text": text.clone()},
            }),
        ));
    }
    sse.push_str(&sse_event(
        "content_block_stop",
        &json!({"type": "content_block_stop", "index": 0}),
    ));
    sse.push_str(&sse_event(
        "message_delta",
        &json!({
            "type": "message_delta",
            "delta": {"stop_reason": stop_reason, "stop_sequence": Value::Null},
            "usage": {"output_tokens": out_tok},
        }),
    ));
    sse.push_str(&sse_event(
        "message_stop",
        &json!({"type": "message_stop"}),
    ));
    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "text/event-stream")
        .header("cache-control", "no-cache")
        .body(Body::from(sse))
        .unwrap()
}
