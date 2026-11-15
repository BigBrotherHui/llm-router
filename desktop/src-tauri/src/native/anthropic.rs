//! Anthropic /v1/messages 适配：Claude Code / chat-demo 等客户端把 ANTHROPIC_BASE_URL 指到本服务时，
//! 把请求转成 OpenAI chat/completions，再把响应（含 SSE）转回 Anthropic 格式。
//!
//! 覆盖范围：
//!   - 角色归一化：OpenAI 上游只认 system/assistant/user/tool/function；DeepSeek Chat Engine 的
//!     latest_reminder 等扩展角色会被合并进上一条 user 消息（而不是原样透传触发 400）。
//!   - tools 双向：Anthropic tools/tool_use/tool_result <-> OpenAI tools/tool_calls/role=tool。
//!   - thinking：请求侧 thinking.enabled -> reasoning_effort；响应侧 reasoning_content -> thinking 块（带 signature）。
//!   - 流式：边读上游 SSE 边转 Anthropic 事件逐字下发，不再攒齐再发。
//!   - 服务端工具：客户端声明的 deepseek_web_search / deepseek_read_url 由网关自己执行
//!     （Bing 抓取 + 网页抽取），并在正文之前下发 server_tool_use /
//!     web_search_tool_result / web_fetch_tool_result 块；正文里的 [n] 角标会转成
//!     citations_delta（web_search_result_location），客户端因此能渲染来源卡片与角标。
//!   - 附件：image/document（含 file_id）转 OpenAI 多模态部件或内联文本，PDF 在网关侧抽文本。

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use axum::body::{to_bytes, Body, Bytes};
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use futures_util::StreamExt;
use serde_json::{json, Value};
use uuid::Uuid;

use super::files;
use super::proxy;
use super::search;
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

/// 把一条消息的 content 归一成块数组（字符串型 content 视作单个 text 块）。
fn content_blocks(v: &Value) -> Vec<Value> {
    match v {
        Value::Array(a) => a.clone(),
        Value::String(s) => vec![json!({"type": "text", "text": s})],
        Value::Object(_) => vec![v.clone()],
        _ => Vec::new(),
    }
}

/// 单个内容块的纯文本（thinking / redacted_thinking / image 等返回空串，即不回灌给 OpenAI 上游）。
fn block_text(b: &Value) -> String {
    match b {
        Value::String(s) => s.clone(),
        Value::Object(_) => match b.get("type").and_then(|x| x.as_str()).unwrap_or("") {
            "text" => b
                .get("text")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string(),
            "tool_result" => flatten_content(b.get("content").unwrap_or(&Value::Null)),
            "thinking" | "redacted_thinking" | "image" | "document" => String::new(),
            _ => b
                .get("text")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string(),
        },
        _ => String::new(),
    }
}

fn estimate_tokens(s: &str) -> u64 {
    ((s.chars().count() as u64) + 3) / 4
}

// ───────────────── 附件（image / document）→ OpenAI 多模态部件 ─────────────────
//
// 原先 image/document 块被 block_text 丢掉（返回空串），于是「贴图问颜色」这类请求
// 到了上游只剩文字，模型只能答「我没收到图片」——而实测同一个上游用 OpenAI 的
// image_url 部件是能看图算图的（usage.prompt_tokens_details.image_tokens 非零）。
// 所以这里把 Anthropic 的附件块翻译成 OpenAI 多模态部件：
//   image   base64/url          -> {"type":"image_url","image_url":{"url":...}}
//   image   file_id             -> 从 Files 目录读出，转 data URL
//   document text/content/base64(text) -> 直接内联成文本部件
//   document pdf                -> 网关侧抽文本（上游明确不支持 PDF 输入）
// 抽不出来的（扫描件 PDF、加密 PDF）返回 Err，由调用方明确告知客户端，而不是静默丢空。

fn text_part(s: &str) -> Value {
    json!({"type": "text", "text": s})
}

fn image_url_part(url: &str) -> Value {
    json!({"type": "image_url", "image_url": {"url": url}})
}

fn inline_text(header: &str, body: &str) -> Value {
    text_part(&format!("{header}\n{body}"))
}

/// 文本类附件与 PDF 走这里：能读成文本就内联，读不出就报错（不再静默丢弃）。
fn text_from_bytes(bytes: &[u8], mime: &str, filename: &str) -> Result<Value, String> {
    if mime == "application/pdf" || filename.to_lowercase().ends_with(".pdf") {
        return match super::pdf::extract_text(bytes) {
            Some(t) => Ok(inline_text(
                &format!("【附件 {filename}（PDF 抽取文本）】"),
                &t,
            )),
            None => Err(format!(
                "PDF 无法抽取文本（可能是扫描件/加密或子集字体）：{filename}"
            )),
        };
    }
    let texty = mime.starts_with("text/")
        || mime == "application/json"
        || mime == "application/xml"
        || mime.ends_with("+json")
        || mime.ends_with("+xml");
    if !texty {
        return Err(format!("不支持的附件类型: {filename} ({mime})"));
    }
    match String::from_utf8(bytes.to_vec()) {
        Ok(t) => Ok(inline_text(&format!("【附件 {filename}】"), &t)),
        Err(_) => Err(format!("附件不是合法 UTF-8 文本: {filename}")),
    }
}

fn media_source_part(src: &Value, dir: Option<&Path>, kind: &str) -> Result<Value, String> {
    let stype = src
        .get("type")
        .and_then(|v| v.as_str())
        .unwrap_or("base64");
    match stype {
        "base64" => {
            let data = src
                .get("data")
                .and_then(|v| v.as_str())
                .ok_or_else(|| "base64 源缺少 data".to_string())?;
            let mime = src
                .get("media_type")
                .and_then(|v| v.as_str())
                .unwrap_or(if kind == "image" {
                    "image/png"
                } else {
                    "application/pdf"
                });
            if mime.starts_with("image/") {
                Ok(image_url_part(&format!("data:{mime};base64,{data}")))
            } else {
                let bytes = base64::Engine::decode(
                    &base64::engine::general_purpose::STANDARD,
                    data,
                )
                .map_err(|e| format!("base64 解码失败: {e}"))?;
                text_from_bytes(&bytes, mime, "inline")
            }
        }
        "url" => {
            let url = src
                .get("url")
                .and_then(|v| v.as_str())
                .ok_or_else(|| "url 源缺少 url".to_string())?;
            if kind == "image" {
                Ok(image_url_part(url))
            } else {
                Err(format!("远程 document 尚未支持: {url}"))
            }
        }
        "text" => {
            let t = src
                .get("data")
                .and_then(|v| v.as_str())
                .ok_or_else(|| "text 源缺少 data".to_string())?;
            Ok(inline_text("【附件】", t))
        }
        "content" => {
            let t = flatten_content(src.get("content").unwrap_or(&Value::Null));
            if t.trim().is_empty() {
                Err("content 源为空".to_string())
            } else {
                Ok(inline_text("【附件】", &t))
            }
        }
        "file" => {
            let id = src
                .get("file_id")
                .and_then(|v| v.as_str())
                .ok_or_else(|| "file 源缺少 file_id".to_string())?;
            let dir = dir.ok_or_else(|| "附件目录不可用".to_string())?;
            let f = files::load(dir, id).ok_or_else(|| format!("附件不存在或已删除: {id}"))?;
            if f.is_image() {
                Ok(image_url_part(&f.data_url()))
            } else {
                text_from_bytes(&f.bytes, &f.mime, &f.filename)
            }
        }
        other => Err(format!("不支持的 source 类型: {other}")),
    }
}

fn media_part(b: &Value, dir: Option<&Path>) -> Result<Value, String> {
    let kind = b.get("type").and_then(|v| v.as_str()).unwrap_or("");
    if kind == "image" {
        return media_source_part(
            b.get("source").unwrap_or(&Value::Null),
            dir,
            "image",
        );
    }
    // document：优先 source，其次顶层 data 形式
    if let Some(src) = b.get("source") {
        return media_source_part(src, dir, "document");
    }
    if let Some(d) = b.get("data").and_then(|v| v.as_str()) {
        return Ok(inline_text("【附件】", d));
    }
    Err("document 块既没有 source 也没有 data".to_string())
}

/// 追加一条「带附件的」user 消息：上一条也是 user 时合并进它的部件数组。
fn push_user_parts(messages: &mut Vec<Value>, parts: Vec<Value>) {
    if let Some(last) = messages.last_mut() {
        if last.get("role").and_then(|r| r.as_str()) == Some("user") {
            let mut merged: Vec<Value> = match last.get("content") {
                Some(Value::String(s)) if !s.trim().is_empty() => vec![text_part(s)],
                Some(Value::Array(a)) => a.clone(),
                _ => Vec::new(),
            };
            merged.extend(parts);
            last["content"] = json!(merged);
            return;
        }
    }
    messages.push(json!({"role": "user", "content": parts}));
}

/// thinking 块的签名。上游 OpenAI 兼容接口没有这个概念，这里给一个可回传的稳定占位符；
/// 客户端把它原样带回来时我们会连同 thinking 块一起忽略，不影响后续轮次。
fn fake_signature() -> String {
    format!("llmrouter{}", Uuid::new_v4().simple())
}

fn tools_to_openai(v: &Value) -> Vec<Value> {
    v.as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|t| {
                    // 服务端工具（web_search_20250305 / deepseek_web_search / deepseek_read_url /
                    // computer_* …）带 type 字段，执行方是 Anthropic 服务端本尊，网关没有对应能力。
                    // 当成普通函数转给上游，模型会照着一个空 parameters 的假工具发起调用，
                    // 而客户端（chat-demo 之类）根本不执行客户端工具 —— 这一轮就停在半句话上。
                    // 所以这里直接跳过：模型退回凭自身知识作答，至少能给出完整回答。
                    match t.get("type").and_then(|x| x.as_str()) {
                        None | Some("custom") => {}
                        Some(_) => return None,
                    }
                    // Anthropic 的自定义工具是 {name, description, input_schema}。
                    let name = t.get("name").and_then(|x| x.as_str())?;
                    if name.is_empty() {
                        return None;
                    }
                    let mut f = json!({ "name": name });
                    if let Some(d) = t.get("description").and_then(|x| x.as_str()) {
                        f["description"] = json!(d);
                    }
                    f["parameters"] = t
                        .get("input_schema")
                        .cloned()
                        .unwrap_or_else(|| json!({"type": "object", "properties": {}}));
                    Some(json!({"type": "function", "function": f}))
                })
                .collect()
        })
        .unwrap_or_default()
}

fn tool_choice_to_openai(v: &Value) -> Option<Value> {
    match v.get("type").and_then(|x| x.as_str())? {
        "auto" => Some(json!("auto")),
        "any" => Some(json!("required")),
        "none" => Some(json!("none")),
        "tool" => Some(
            v.get("name")
                .and_then(|n| n.as_str())
                .map(|n| json!({"type": "function", "function": {"name": n}}))
                .unwrap_or_else(|| json!("auto")),
        ),
        _ => None,
    }
}

/// thinking.enabled -> OpenAI 的 reasoning_effort（按 budget_tokens 分档）。
fn reasoning_effort(v: &Value) -> Option<&'static str> {
    if v.get("type").and_then(|x| x.as_str())? != "enabled" {
        return None;
    }
    let budget = v.get("budget_tokens").and_then(|x| x.as_u64()).unwrap_or(0);
    Some(if budget >= 8192 {
        "high"
    } else if budget >= 2048 {
        "medium"
    } else {
        "low"
    })
}

/// 追加一条 user 文本：上一条也是 user 时合并进它（Anthropic 允许连续 user，OpenAI 上游更认合并后的单条，
/// 同时保证 latest_reminder 这类提醒上下文不丢）。
fn push_user(messages: &mut Vec<Value>, text: &str) {
    if let Some(last) = messages.last_mut() {
        if last.get("role").and_then(|r| r.as_str()) == Some("user") {
            let merged = match last.get("content").and_then(|c| c.as_str()) {
                Some(prev) if !prev.is_empty() => format!("{prev}\n\n{text}"),
                _ => text.to_string(),
            };
            last["content"] = json!(merged);
            return;
        }
    }
    messages.push(json!({"role": "user", "content": text}));
}

// ───────────────── 服务端工具（联网）─────────────────
//
// 客户端声明的是「服务端工具」：deepseek_web_search / deepseek_read_url（以及标准名
// web_search_*、web_fetch_*）。按协议执行方是服务端，而上游没有这些能力（实测：把
// web_search 当普通函数工具转给上游会被 400 拒掉：'function' is a required property）。
// 所以网关自己执行三步：
//   1. 抓取结果 → 组装 server_tool_use + web_search_tool_result / web_fetch_tool_result
//      块，在正文之前下发（客户端据此渲染「已联网搜索 / 来源」卡片）；
//   2. 结果按编号注入最后一轮 user，模型据此作答；
//   3. 正文里的 [n] 角标 → citations_delta（web_search_result_location），
//      客户端据此渲染句末来源角标。
// 搜索/抓取失败时不静默降级：照样下发工具块 + *_tool_result_error，让界面显示联网失败。

#[derive(Default, Clone)]
struct WebContext {
    /// 正文之前要下发的块（server_tool_use / *_tool_result）。
    prelude: Vec<Value>,
    /// 注入最后一轮 user 的联网上下文。
    prompt: Option<String>,
    /// 角标编号(1 起) -> (url, title)。
    sources: Vec<(String, Option<String>)>,
}

impl WebContext {
    #[allow(dead_code)]
    fn is_empty(&self) -> bool {
        self.prelude.is_empty() && self.prompt.is_none()
    }
}

/// 声明了哪些服务端工具：(需要联网搜索, 需要读链接)。
fn server_tool_intent(req: &Value) -> (bool, bool) {
    let mut want_search = false;
    let mut want_read = false;
    if let Some(arr) = req.get("tools").and_then(|v| v.as_array()) {
        for t in arr {
            // 服务端工具带 type（客户端自定义工具没有 type，或显式 type == "custom"）
            let is_server = t
                .get("type")
                .and_then(|x| x.as_str())
                .map(|k| k != "custom")
                .unwrap_or(false);
            if !is_server {
                continue;
            }
            let name = t.get("name").and_then(|x| x.as_str()).unwrap_or("");
            if name == "deepseek_web_search" || name.starts_with("web_search") {
                want_search = true;
            }
            if name == "deepseek_read_url" || name.starts_with("web_fetch") {
                want_read = true;
            }
        }
    }
    (want_search, want_read)
}

/// 最后一轮 user 的纯文本（搜索词与链接识别都基于它）。
fn last_user_text(req: &Value) -> String {
    let Some(arr) = req.get("messages").and_then(|v| v.as_array()) else {
        return String::new();
    };
    for m in arr.iter().rev() {
        if m.get("role").and_then(|v| v.as_str()) != Some("user") {
            continue;
        }
        let text = flatten_content(m.get("content").unwrap_or(&Value::Null));
        if !text.trim().is_empty() {
            return text;
        }
    }
    String::new()
}

/// 与官方形状一致的工具调用 id：srvtoolu_{s|rl}_{序号}_{32 位 hex}。
fn server_tool_id(kind: &str) -> String {
    let tag = if kind == "read_link" { "rl" } else { "s" };
    format!("srvtoolu_{tag}_1_{}", Uuid::new_v4().simple())
}

fn server_tool_use_block(name: &str, kind: &str, input: Value) -> Value {
    json!({
        "type": "server_tool_use",
        "id": server_tool_id(kind),
        "name": name,
        "input": input,
        "caller": {"type": "direct"},
        "kind": kind,
    })
}

fn search_result_block(tool_use_id: &str, content: Value) -> Value {
    json!({
        "type": "web_search_tool_result",
        "tool_use_id": tool_use_id,
        "kind": "search",
        "content": content,
    })
}

fn search_error_content() -> Value {
    json!([{"type": "web_search_tool_result_error", "error_code": "unavailable"}])
}

/// 把用户那一整句话压成搜索词：去掉指令性尾巴、寒暄前缀与疑问脚手架，必要时补上当天日期。
///
/// 实测（Bing cn 版）：`最近有什么 AI 新闻 2026年11月15日` 只会返回「最近」这个词的歌曲/词典页，
/// 而 `AI 新闻 2026年11月15日` 能返回「AI 技术日报 - 2026-11-15」这类真结果——
/// 所以疑问词与时效副词一律删掉，时效诉求改由日期表达。
fn search_query(text: &str) -> String {
    let first = text.lines().next().unwrap_or("").trim();
    let mut q = first.to_string();
    // 1) 砍到第一个句末标点（用户常在后面追加「用一句话总结」之类的指令）
    let mut cut: Option<usize> = None;
    for sep in ['。', '？', '?', '！', '!', '；', ';'] {
        if let Some(pos) = q.find(sep) {
            cut = Some(cut.map_or(pos, |c: usize| c.min(pos)));
        }
    }
    if let Some(pos) = cut {
        q.truncate(pos);
    }
    // 2) 逗号后如果是指令性小句，砍掉
    for sep in [
        "，请", "，帮", "，并", "，用", "，要", "，不要", "，然后", "，顺带", "，另外",
        ",请", ",帮", ",并", ",用", ",要", ",不要",
    ] {
        if let Some(pos) = q.find(sep) {
            q.truncate(pos);
        }
    }
    // 3) 去掉寒暄/请求前缀
    let mut q = q
        .trim()
        .trim_start_matches(['请', '帮', '能', '你', '我'])
        .trim()
        .to_string();
    for p in [
        "麻烦", "帮我搜一下", "帮我查一下", "帮我搜", "帮我查", "搜索一下", "搜索", "搜一下",
        "搜一搜", "查一下", "查一查", "查找", "查下", "查询", "找一下", "找找", "看一下",
        "看看", "告诉我", "请问", "我想知道", "想知道", "了解一下", "了解", "介绍一下",
    ] {
        if let Some(rest) = q.strip_prefix(p) {
            q = rest.trim().to_string();
        }
    }
    let had_recency = ["最近", "最新", "今天", "今日", "目前", "现在", "近期", "今年", "当下"]
        .iter()
        .any(|w| q.contains(w));
    // 4) 去掉疑问脚手架与时效副词（时效改由日期承担）
    for w in ["有什么新的", "有什么新", "有哪些新的", "有哪些新", "有什么", "有哪些", "有啥", "是什么", "干嘛"] {
        q = q.replace(w, " ");
    }
    let mut q = q
        .trim_end_matches(['吗', '呢', '吧', '啊', '呀', '。', '？', '?', '！', '!', '，', ','])
        .trim()
        .to_string();
    for w in ["最近", "最新", "今天", "今日", "目前", "现在", "近期", "当下"] {
        while let Some(rest) = q.strip_prefix(w) {
            q = rest.trim().to_string();
        }
        while let Some(rest) = q.strip_suffix(w) {
            q = rest.trim().to_string();
        }
    }
    let q = q.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut out = search::truncate_chars(&q, 120);
    // 5) 有时效诉求却没写日期 -> 补当天日期
    let has_year = out
        .split(|c: char| !c.is_ascii_digit())
        .any(|s| s.len() == 4);
    if had_recency && !has_year {
        let now = chrono::Local::now();
        out.push_str(&format!(
            " {}年{}月{}日",
            now.format("%Y"),
            now.format("%-m"),
            now.format("%-d")
        ));
    }
    if out.chars().count() < 2 {
        search::truncate_chars(first, 120)
    } else {
        out
    }
}

async fn collect_web_context(
    state: &AppState,
    req: &Value,
    want_search: bool,
    want_read: bool,
) -> WebContext {
    let mut ctx = WebContext::default();
    let text = last_user_text(req);
    if text.trim().is_empty() {
        return ctx;
    }
    let mut sections: Vec<String> = Vec::new();

    if want_search {
        let query = search_query(&text);
        let use_block = server_tool_use_block(
            "deepseek_web_search",
            "search",
            json!({"action": "search", "query": query, "queries": [query]}),
        );
        let id = use_block["id"].as_str().unwrap_or("").to_string();
        ctx.prelude.push(use_block);
        match search::web_search(&state.client, &query, 8).await {
            Ok(hits) if !hits.is_empty() => {
                let items: Vec<Value> = hits.iter().map(search::hit_to_result).collect();
                ctx.prelude.push(search_result_block(&id, json!(items)));
                for h in &hits {
                    ctx.sources.push((
                        h.url.clone(),
                        if h.title.is_empty() {
                            None
                        } else {
                            Some(h.title.clone())
                        },
                    ));
                }
                sections.push(search::render_context(&query, &hits));
            }
            Ok(_) => {
                ctx.prelude.push(search_result_block(&id, search_error_content()));
                sections.push(format!(
                    "【联网搜索结果】搜索词「{query}」没有返回结果，请如实说明未检索到。"
                ));
            }
            Err(e) => {
                eprintln!("[anthropic] 联网搜索失败: {e}");
                ctx.prelude.push(search_result_block(&id, search_error_content()));
                sections.push(format!(
                    "【联网搜索结果】网关联网失败（{e}），请如实说明无法联网核实。"
                ));
            }
        }
    }

    if want_read {
        if let Some(url) = search::first_url(&text) {
            let use_block = server_tool_use_block(
                "deepseek_read_url",
                "read_link",
                json!({"action": "read_link", "url": url}),
            );
            let id = use_block["id"].as_str().unwrap_or("").to_string();
            ctx.prelude.push(use_block);
            match search::read_page(&state.client, &url).await {
                Ok((title, body)) => {
                    ctx.prelude.push(json!({
                        "type": "web_fetch_tool_result",
                        "tool_use_id": id,
                        "kind": "read_link",
                        "content": {
                            "type": "web_fetch_result",
                            "url": url,
                            "title": title,
                            "content": {
                                "type": "document",
                                "source": {
                                    "type": "text",
                                    "media_type": "text/plain",
                                    "data": body,
                                },
                            },
                            "published_at": Value::Null,
                        },
                    }));
                    sections.push(format!("【已读取网页 {url}（正文截断至 2 万字符）】\n{body}"));
                }
                Err(e) => {
                    eprintln!("[anthropic] 读取网页失败: {e}");
                    ctx.prelude.push(json!({
                        "type": "web_fetch_tool_result",
                        "tool_use_id": id,
                        "kind": "read_link",
                        "content": {
                            "type": "web_fetch_tool_result_error",
                            "error_code": "url_not_accessible",
                        },
                    }));
                    sections.push(format!("【读取网页 {url} 失败：{e}】"));
                }
            }
        }
    }

    if !sections.is_empty() {
        ctx.prompt = Some(sections.join("\n\n"));
    }
    ctx
}

struct BuiltRequest {
    body: Value,
    model: String,
    stream: bool,
    id: String,
    input_tokens: u64,
    /// 客户端是否显式要求思维链（thinking.type == "enabled"）。
    thinking_enabled: bool,
    /// 正文之前下发的服务端工具块（server_tool_use / *_tool_result）。
    prelude: Vec<Value>,
    /// 角标编号(1 起) -> (url, title)，用于把正文里的 [n] 转成 citations。
    sources: Vec<(String, Option<String>)>,
}

fn build_openai_request(
    req: &Value,
    files_dir: Option<&Path>,
    web: &WebContext,
) -> Result<BuiltRequest, String> {
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
    // 解析不了、被丢掉的附件（用于「整轮都是附件且全丢」时给出明确报错）
    let mut dropped: Vec<String> = Vec::new();

    if let Some(system) = req.get("system") {
        let text = flatten_content(system);
        if !text.is_empty() {
            messages.push(json!({"role": "system", "content": text}));
        }
    }

    if let Some(arr) = req.get("messages").and_then(|v| v.as_array()) {
        let last_index = arr.len().saturating_sub(1);
        for (idx, m) in arr.iter().enumerate() {
            let is_last = idx == last_index;
            let raw_role = m.get("role").and_then(|v| v.as_str()).unwrap_or("user");
            let blocks = content_blocks(m.get("content").unwrap_or(&Value::Null));
            match raw_role {
                "assistant" => {
                    let mut text = String::new();
                    let mut tool_calls: Vec<Value> = Vec::new();
                    for b in &blocks {
                        match b.get("type").and_then(|x| x.as_str()).unwrap_or("") {
                            "tool_use" => {
                                let name = b.get("name").and_then(|x| x.as_str()).unwrap_or("");
                                if name.is_empty() {
                                    continue;
                                }
                                let id = b
                                    .get("id")
                                    .and_then(|x| x.as_str())
                                    .filter(|s| !s.is_empty())
                                    .map(|s| s.to_string())
                                    .unwrap_or_else(|| format!("call_{}", Uuid::new_v4().simple()));
                                let args = b.get("input").cloned().unwrap_or_else(|| json!({}));
                                tool_calls.push(json!({
                                    "id": id,
                                    "type": "function",
                                    "function": {
                                        "name": name,
                                        "arguments": serde_json::to_string(&args).unwrap_or_else(|_| "{}".to_string()),
                                    },
                                }));
                            }
                            _ => {
                                let t = block_text(b);
                                if !t.is_empty() {
                                    if !text.is_empty() {
                                        text.push('\n');
                                    }
                                    text.push_str(&t);
                                }
                            }
                        }
                    }
                    if text.is_empty() && tool_calls.is_empty() {
                        continue;
                    }
                    let mut msg = json!({"role": "assistant", "content": text});
                    if !tool_calls.is_empty() {
                        msg["tool_calls"] = json!(tool_calls);
                    }
                    messages.push(msg);
                }
                "user" => {
                    let mut text = String::new();
                    let mut tool_results: Vec<Value> = Vec::new();
                    let mut media: Vec<Value> = Vec::new();
                    for b in &blocks {
                        match b.get("type").and_then(|x| x.as_str()).unwrap_or("") {
                            "tool_result" => {
                                let id = b
                                    .get("tool_use_id")
                                    .and_then(|x| x.as_str())
                                    .unwrap_or("")
                                    .to_string();
                                let body = flatten_content(b.get("content").unwrap_or(&Value::Null));
                                tool_results.push(json!({
                                    "role": "tool",
                                    "tool_call_id": id,
                                    "content": body,
                                }));
                            }
                            "image" | "document" => match media_part(b, files_dir) {
                                Ok(p) => media.push(p),
                                Err(e) => {
                                    eprintln!("[anthropic] 附件未转发: {e}");
                                    dropped.push(e);
                                }
                            },
                            _ => {
                                let t = block_text(b);
                                if !t.is_empty() {
                                    if !text.is_empty() {
                                        text.push('\n');
                                    }
                                    text.push_str(&t);
                                }
                            }
                        }
                    }
                    // OpenAI 要求 role=tool 紧跟带 tool_calls 的 assistant 消息，先发工具结果再发用户文本。
                    messages.extend(tool_results);
                    // 附件没解析成功时，把原因作为一条可见文本带上（只对最后一轮加，避免历史里反复出现），
                    // 免得模型和用户都以为「这条消息本来就没有附件」。
                    if !dropped.is_empty() && is_last {
                        let note = format!(
                            "【{} 个附件未转发：{}】",
                            dropped.len(),
                            dropped.join("；")
                        );
                        if text.is_empty() {
                            text = note;
                        } else {
                            text.push('\n');
                            text.push_str(&note);
                        }
                    }
                    if media.is_empty() {
                        if !text.is_empty() {
                            push_user(&mut messages, &text);
                        }
                    } else {
                        // 有附件：组装多模态部件数组（文本在前，图片/文档随后）
                        let mut parts: Vec<Value> = Vec::new();
                        if !text.trim().is_empty() {
                            parts.push(text_part(&text));
                        }
                        parts.extend(media);
                        push_user_parts(&mut messages, parts);
                    }
                }
                "system" | "tool" | "function" => {
                    let text = flatten_content(m.get("content").unwrap_or(&Value::Null));
                    if !text.is_empty() {
                        messages.push(json!({"role": raw_role, "content": text}));
                    }
                }
                _ => {
                    // latest_reminder 等扩展角色：合并进上一条 user，合并不了才新起一条。
                    let text = flatten_content(m.get("content").unwrap_or(&Value::Null));
                    if !text.trim().is_empty() {
                        push_user(&mut messages, &text);
                    }
                }
            }
        }
    }

    if messages.is_empty() {
        if !dropped.is_empty() {
            return Err(format!(
                "请求里没有可转发的 messages（附件均未解析成功：{}）",
                dropped.join("；")
            ));
        }
        return Err("请求里没有可转发的 messages".to_string());
    }

    // 联网上下文（搜索结果 / 网页正文）追加到最后一轮 user：模型据此作答并按编号引用。
    if let Some(note) = web.prompt.as_deref() {
        match messages
            .iter()
            .rposition(|m| m.get("role").and_then(|r| r.as_str()) == Some("user"))
        {
            Some(i) => {
                let content = messages[i].get("content").cloned().unwrap_or(Value::Null);
                messages[i]["content"] = match content {
                    Value::String(s) if s.trim().is_empty() => json!(note),
                    Value::String(mut s) => {
                        s.push_str("\n\n");
                        s.push_str(note);
                        json!(s)
                    }
                    Value::Array(mut parts) => {
                        parts.push(text_part(note));
                        json!(parts)
                    }
                    _ => json!(note),
                };
            }
            None => messages.push(json!({"role": "user", "content": note})),
        }
    }

    let mut out = json!({
        "model": model.clone(),
        "messages": messages,
        // Anthropic 客户端（如 chat-demo）常常省略 max_tokens；1024 会被上游的隐式思维链吃光，
        // 表现成「回答空白 + stop_reason=max_tokens」，所以缺省给一个更宽的预算。
        "max_tokens": req.get("max_tokens").and_then(|v| v.as_u64()).unwrap_or(4096),
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
    let tools = req.get("tools").map(tools_to_openai).unwrap_or_default();
    if !tools.is_empty() {
        out["tools"] = json!(tools);
        if let Some(choice) = req.get("tool_choice").and_then(tool_choice_to_openai) {
            out["tool_choice"] = choice;
        }
    }
    // 客户端到底要不要思维链：只有 thinking.type == "enabled" 才回吐 thinking 块。
    // 有些 OpenAI 兼容上游（如 modelscope 系）不管你要不要都会回 reasoning_content，
    // 若原样转成 thinking 块，客户端会在「深度思考」关闭时也把推理过程显示出来。
    let thinking_enabled = req
        .get("thinking")
        .and_then(|t| t.get("type"))
        .and_then(|v| v.as_str())
        == Some("enabled");
    if let Some(effort) = req.get("thinking").and_then(reasoning_effort) {
        out["reasoning_effort"] = json!(effort);
    } else {
        // 客户端不要思维链时，明确让上游别推理。否则上游照旧生成 reasoning_content，
        // 这些 token 照算进 max_tokens，却被网关按 thinking_enabled 过滤掉——
        // 用户看到的就是一段空白回答（实测带 PDF 上下文时 1024 预算全被推理吃掉）。
        out["enable_thinking"] = json!(false);
    }

    let input_tokens = estimate_tokens(
        &serde_json::to_string(&out["messages"]).unwrap_or_default(),
    );
    Ok(BuiltRequest {
        body: out,
        model,
        stream,
        id: format!("msg_{}", Uuid::new_v4().simple()),
        input_tokens,
        thinking_enabled,
        prelude: web.prelude.clone(),
        sources: web.sources.clone(),
    })
}

fn sse_event(event: &str, data: &Value) -> String {
    format!(
        "event: {event}\ndata: {}\n\n",
        serde_json::to_string(data).unwrap_or_else(|_| "{}".to_string())
    )
}

/// 上游 OpenAI 流 -> Anthropic 事件流的增量转换器。
struct Converter {
    model: String,
    id: String,
    input_tokens: u64,
    started: bool,
    next_index: u32,
    open_thinking: Option<u32>,
    open_text: Option<u32>,
    /// OpenAI 的 tool_calls[].index -> (Anthropic 块下标, 工具 id)
    tools: HashMap<i64, (u32, String)>,
    tool_order: Vec<i64>,
    finish: Option<String>,
    usage: Value,
    done: bool,
    /// 与 BuiltRequest::thinking_enabled 一致：关掉时丢弃上游的 reasoning_content。
    thinking_enabled: bool,
    /// 正文之前要下发的服务端工具块。
    prelude: Vec<Value>,
    prelude_done: bool,
    /// 角标编号(1 起) -> (url, title)。
    sources: Vec<(String, Option<String>)>,
    /// 待判定文本（可能含未闭合的 [n] 角标）。
    pending: String,
    /// 已下发的正文（用于 citations 的 cited_text，只保留尾部）。
    cited: String,
}

impl Converter {
    fn new(
        model: String,
        id: String,
        input_tokens: u64,
        thinking_enabled: bool,
        prelude: Vec<Value>,
        sources: Vec<(String, Option<String>)>,
    ) -> Self {
        Self {
            model,
            id,
            input_tokens,
            started: false,
            next_index: 0,
            open_thinking: None,
            open_text: None,
            tools: HashMap::new(),
            tool_order: Vec::new(),
            finish: None,
            usage: json!({}),
            done: false,
            thinking_enabled,
            prelude,
            prelude_done: false,
            sources,
            pending: String::new(),
            cited: String::new(),
        }
    }

    /// message_start + 正文之前要下发的服务端工具块（只发一次）。
    fn ensure_start(&mut self, out: &mut Vec<String>) {
        if !self.started {
            self.started = true;
            out.push(sse_event(
                "message_start",
                &json!({
                    "type": "message_start",
                    "message": {
                        "id": self.id,
                        "type": "message",
                        "role": "assistant",
                        "model": self.model,
                        "content": [],
                        "stop_reason": Value::Null,
                        "stop_sequence": Value::Null,
                        "usage": {"input_tokens": self.input_tokens, "output_tokens": 0},
                    },
                }),
            ));
        }
        if !self.prelude_done {
            self.prelude_done = true;
            for block in self.prelude.clone() {
                let idx = self.next_index;
                self.next_index += 1;
                out.push(sse_event(
                    "content_block_start",
                    &json!({
                        "type": "content_block_start",
                        "index": idx,
                        "content_block": block,
                    }),
                ));
                out.push(sse_event(
                    "content_block_stop",
                    &json!({"type": "content_block_stop", "index": idx}),
                ));
            }
        }
    }

    /// 已下发正文的尾部（citations 的 cited_text）。
    fn cited_tail(&self) -> String {
        let n = self.cited.chars().count();
        let tail: String = if n > 200 {
            self.cited.chars().skip(n - 200).collect()
        } else {
            self.cited.clone()
        };
        tail.trim().to_string()
    }

    fn open_text_block(&mut self, out: &mut Vec<String>) -> u32 {
        self.ensure_start(out);
        if let Some(i) = self.open_text {
            return i;
        }
        let idx = self.next_index;
        self.next_index += 1;
        self.open_text = Some(idx);
        out.push(sse_event(
            "content_block_start",
            &json!({
                "type": "content_block_start",
                "index": idx,
                "content_block": {"type": "text", "text": ""},
            }),
        ));
        idx
    }

    /// 下发一段正文（不处理角标，调用方已切好）。
    fn emit_text(&mut self, t: &str, out: &mut Vec<String>) {
        if t.is_empty() {
            return;
        }
        self.close_thinking(out);
        let idx = self.open_text_block(out);
        out.push(sse_event(
            "content_block_delta",
            &json!({
                "type": "content_block_delta",
                "index": idx,
                "delta": {"type": "text_delta", "text": t},
            }),
        ));
        self.cited.push_str(t);
        let n = self.cited.chars().count();
        if n > 400 {
            self.cited = self.cited.chars().skip(n - 400).collect();
        }
    }

    /// 下发一个引用角标：citations_delta 落在当前正文块，然后关块，
    /// 后续正文另起一块（官方行为：挂过引用的块之后正文另起新块，角标才会落在引用句之后）。
    fn emit_citation(
        &mut self,
        url: &str,
        title: Option<String>,
        cited_text: &str,
        out: &mut Vec<String>,
    ) {
        let idx = self.open_text_block(out);
        out.push(sse_event(
            "content_block_delta",
            &json!({
                "type": "content_block_delta",
                "index": idx,
                "delta": {
                    "type": "citations_delta",
                    "citation": {
                        "type": "web_search_result_location",
                        "url": url,
                        "title": title,
                        "cited_text": cited_text,
                        "encrypted_index": "",
                    },
                },
            }),
        ));
        out.push(sse_event(
            "content_block_stop",
            &json!({"type": "content_block_stop", "index": idx}),
        ));
        self.open_text = None;
    }

    /// 正文增量入口：把可能含 [n] 角标的文本切开，角标转成 citations_delta。
    fn push_text(&mut self, t: &str, out: &mut Vec<String>) {
        self.pending.push_str(t);
        self.drain_pending(out, false);
    }

    fn drain_pending(&mut self, out: &mut Vec<String>, eof: bool) {
        loop {
            let Some(start) = self.pending.find('[') else {
                let all = std::mem::take(&mut self.pending);
                self.emit_text(&all, out);
                return;
            };
            if start > 0 {
                let pre: String = self.pending.drain(..start).collect();
                self.emit_text(&pre, out);
            }
            // pending 现在以 '[' 开头；角标全是 ASCII，按字节扫描最直接。
            let bytes = self.pending.as_bytes();
            let scan_end = bytes.len().min(13);
            let scan = &bytes[1..scan_end];
            let close = scan.iter().position(|b| *b == b']');
            match close {
                Some(pos) => {
                    // 只校验 ']' 之前的那几个字节（scan 里可能还含 ']' 之后的正文）
                    let inner = &scan[..pos];
                    let shape_ok = !inner.is_empty()
                        && inner
                            .iter()
                            .all(|b| b.is_ascii_digit() || *b == b',' || *b == b' ');
                    if !shape_ok {
                        // 不是角标（例如「[注]」）：原样发出到 ']'（含），继续扫描后面的内容。
                        let literal: String = self.pending.drain(..pos + 2).collect();
                        self.emit_text(&literal, out);
                        continue;
                    }
                    let nums: Vec<usize> = self.pending[1..1 + pos]
                        .split(|c| c == ',' || c == ' ')
                        .filter(|s| !s.is_empty())
                        .filter_map(|s| s.parse::<usize>().ok())
                        .collect();
                    let valid = !nums.is_empty()
                        && nums.iter().all(|n| *n >= 1 && *n <= self.sources.len());
                    if !valid {
                        let literal: String = self.pending.drain(..pos + 2).collect();
                        self.emit_text(&literal, out);
                        continue;
                    }
                    self.pending.drain(..pos + 2);
                    let cited = self.cited_tail();
                    for n in nums {
                        if let Some((url, title)) = self.sources.get(n - 1).cloned() {
                            self.emit_citation(&url, title, &cited, out);
                        }
                    }
                }
                None => {
                    // 形如 "[12"：只有「全是数字/逗号/空格」才可能是没收完的角标，等一下个增量。
                    let maybe_incomplete = scan
                        .iter()
                        .all(|b| b.is_ascii_digit() || *b == b',' || *b == b' ');
                    if maybe_incomplete && !eof {
                        return;
                    }
                    let all = std::mem::take(&mut self.pending);
                    self.emit_text(&all, out);
                    return;
                }
            }
        }
    }

    fn close_thinking(&mut self, out: &mut Vec<String>) {
        if let Some(i) = self.open_thinking.take() {
            out.push(sse_event(
                "content_block_delta",
                &json!({
                    "type": "content_block_delta",
                    "index": i,
                    "delta": {"type": "signature_delta", "signature": fake_signature()},
                }),
            ));
            out.push(sse_event(
                "content_block_stop",
                &json!({"type": "content_block_stop", "index": i}),
            ));
        }
    }

    fn close_text(&mut self, out: &mut Vec<String>) {
        if let Some(i) = self.open_text.take() {
            out.push(sse_event(
                "content_block_stop",
                &json!({"type": "content_block_stop", "index": i}),
            ));
        }
    }

    fn close_all(&mut self, out: &mut Vec<String>) {
        self.close_thinking(out);
        self.close_text(out);
        for oi in self.tool_order.clone() {
            if let Some((idx, _)) = self.tools.get(&oi).cloned() {
                out.push(sse_event(
                    "content_block_stop",
                    &json!({"type": "content_block_stop", "index": idx}),
                ));
            }
        }
    }

    fn feed(&mut self, chunk: &Value, out: &mut Vec<String>) {
        if let Some(u) = chunk.get("usage") {
            if u.as_object().map(|o| !o.is_empty()).unwrap_or(false) {
                self.usage = u.clone();
            }
        }
        let choice = &chunk["choices"][0];
        if let Some(f) = choice.get("finish_reason").and_then(|x| x.as_str()) {
            self.finish = Some(f.to_string());
        }
        let delta = &choice["delta"];

        // thinking（DeepSeek 系 OpenAI 兼容上游把思维链放在 reasoning_content）
        // 客户端没开「深度思考」时直接丢弃，避免推理过程混进回答。
        if let Some(t) = delta
            .get("reasoning_content")
            .and_then(|x| x.as_str())
            .filter(|s| !s.is_empty())
            .filter(|_| self.thinking_enabled)
        {
            self.ensure_start(out);
            if self.open_thinking.is_none() {
                self.close_text(out);
                let idx = self.next_index;
                self.next_index += 1;
                self.open_thinking = Some(idx);
                out.push(sse_event(
                    "content_block_start",
                    &json!({
                        "type": "content_block_start",
                        "index": idx,
                        "content_block": {"type": "thinking", "thinking": ""},
                    }),
                ));
            }
            out.push(sse_event(
                "content_block_delta",
                &json!({
                    "type": "content_block_delta",
                    "index": self.open_thinking.unwrap_or(0),
                    "delta": {"type": "thinking_delta", "thinking": t},
                }),
            ));
        }

        // 正文（含 [n] 角标 -> citations_delta）
        if let Some(t) = delta
            .get("content")
            .and_then(|x| x.as_str())
            .filter(|s| !s.is_empty())
        {
            self.push_text(t, out);
        }

        // 工具调用
        if let Some(tcs) = delta.get("tool_calls").and_then(|x| x.as_array()) {
            for tc in tcs {
                self.ensure_start(out);
                let oi = tc.get("index").and_then(|x| x.as_i64()).unwrap_or(0);
                if !self.tools.contains_key(&oi) {
                    self.close_thinking(out);
                    self.close_text(out);
                    let id = tc
                        .get("id")
                        .and_then(|x| x.as_str())
                        .filter(|s| !s.is_empty())
                        .map(|s| s.to_string())
                        .unwrap_or_else(|| format!("toolu_{}", Uuid::new_v4().simple()));
                    let name = tc["function"]["name"].as_str().unwrap_or("").to_string();
                    let idx = self.next_index;
                    self.next_index += 1;
                    self.tools.insert(oi, (idx, id.clone()));
                    self.tool_order.push(oi);
                    out.push(sse_event(
                        "content_block_start",
                        &json!({
                            "type": "content_block_start",
                            "index": idx,
                            "content_block": {"type": "tool_use", "id": id, "name": name, "input": {}},
                        }),
                    ));
                }
                if let Some(args) = tc["function"]["arguments"].as_str() {
                    if !args.is_empty() {
                        if let Some((idx, _)) = self.tools.get(&oi) {
                            out.push(sse_event(
                                "content_block_delta",
                                &json!({
                                    "type": "content_block_delta",
                                    "index": *idx,
                                    "delta": {"type": "input_json_delta", "partial_json": args},
                                }),
                            ));
                        }
                    }
                }
            }
        }
    }

    /// 收尾：关闭打开的块 + message_delta + message_stop。上游中途断流时也走这里，
    /// 客户端因此能保留已收到的部分，而不是整轮流被当成 interrupted 丢掉。
    fn finish_events(&mut self, out: &mut Vec<String>) {
        if self.done {
            return;
        }
        self.done = true;
        // 流末：把还压在缓冲区里的正文（可能含半个角标）原样发出。
        self.drain_pending(out, true);
        self.ensure_start(out);
        if self.open_thinking.is_none()
            && self.open_text.is_none()
            && self.tools.is_empty()
            && self.prelude.is_empty()
        {
            let idx = self.next_index;
            out.push(sse_event(
                "content_block_start",
                &json!({
                    "type": "content_block_start",
                    "index": idx,
                    "content_block": {"type": "text", "text": ""},
                }),
            ));
            out.push(sse_event(
                "content_block_stop",
                &json!({"type": "content_block_stop", "index": idx}),
            ));
        }
        let has_tools = !self.tools.is_empty();
        self.close_all(out);
        let finish = self.finish.clone().unwrap_or_else(|| "stop".to_string());
        let stop_reason = if has_tools {
            "tool_use"
        } else if finish == "length" {
            "max_tokens"
        } else {
            "end_turn"
        };
        let out_tok = self
            .usage
            .get("completion_tokens")
            .and_then(|x| x.as_u64())
            .unwrap_or(0);
        out.push(sse_event(
            "message_delta",
            &json!({
                "type": "message_delta",
                "delta": {"stop_reason": stop_reason, "stop_sequence": Value::Null},
                "usage": {"output_tokens": out_tok},
            }),
        ));
        out.push(sse_event("message_stop", &json!({"type": "message_stop"})));
    }
}

fn parse_data_line(line: &str) -> Option<String> {
    let t = line.trim_start();
    let payload = t.strip_prefix("data:")?.trim();
    if payload.is_empty() {
        return None;
    }
    Some(payload.to_string())
}

/// 把上游 SSE 逐行转成 Anthropic 事件流。
fn stream_body(body: Body, built: BuiltRequest) -> Body {
    let upstream = body.into_data_stream();
    let mut conv = Converter::new(
        built.model.clone(),
        built.id.clone(),
        built.input_tokens,
        built.thinking_enabled,
        built.prelude.clone(),
        built.sources.clone(),
    );
    let stream = async_stream::stream! {
        futures_util::pin_mut!(upstream);
        let mut buffer = String::new();
        while let Some(chunk) = upstream.next().await {
            let bytes = match chunk {
                Ok(b) => b,
                Err(_) => {
                    // 上游断流：按正常结束收尾，保住已经流出去的内容。
                    let mut events: Vec<String> = Vec::new();
                    conv.finish_events(&mut events);
                    for e in events {
                        yield Ok::<_, std::io::Error>(Bytes::from(e));
                    }
                    return;
                }
            };
            buffer.push_str(&String::from_utf8_lossy(&bytes));
            while let Some(pos) = buffer.find('\n') {
                let line: String = buffer.drain(..=pos).collect();
                let Some(payload) = parse_data_line(&line) else {
                    continue;
                };
                let mut events: Vec<String> = Vec::new();
                if payload == "[DONE]" {
                    conv.finish_events(&mut events);
                    for e in events {
                        yield Ok::<_, std::io::Error>(Bytes::from(e));
                    }
                    return;
                }
                if let Ok(v) = serde_json::from_str::<Value>(&payload) {
                    conv.feed(&v, &mut events);
                }
                for e in events {
                    yield Ok::<_, std::io::Error>(Bytes::from(e));
                }
            }
        }
        let mut events: Vec<String> = Vec::new();
        conv.finish_events(&mut events);
        for e in events {
            yield Ok::<_, std::io::Error>(Bytes::from(e));
        }
    };
    Body::from_stream(stream)
}

/// 尾部 n 个字符（citations 的 cited_text）。
fn tail_of(s: &str, n: usize) -> String {
    let total = s.chars().count();
    let tail: String = if total > n {
        s.chars().skip(total - n).collect()
    } else {
        s.to_string()
    };
    tail.trim().to_string()
}

/// 非流式：把正文按 [n] 角标切成「文本块 + citations」的序列（角标本身不留在正文里）。
fn marked_text_blocks(text: &str, sources: &[(String, Option<String>)]) -> Vec<Value> {
    let mut blocks: Vec<Value> = Vec::new();
    let mut buf = String::new();
    let bytes = text.as_bytes();
    let mut i = 0usize;
    while i < text.len() {
        if bytes[i] == b'[' {
            let scan_end = text.len().min(i + 13);
            let scan = &bytes[i + 1..scan_end];
            if let Some(pos) = scan.iter().position(|b| *b == b']') {
                // 只校验 ']' 之前的那几个字节
                let inner = &scan[..pos];
                let shape_ok = !inner.is_empty()
                    && inner
                        .iter()
                        .all(|b| b.is_ascii_digit() || *b == b',' || *b == b' ');
                if shape_ok {
                    let nums: Vec<usize> = text[i + 1..i + 1 + pos]
                        .split(|c| c == ',' || c == ' ')
                        .filter(|s| !s.is_empty())
                        .filter_map(|s| s.parse::<usize>().ok())
                        .collect();
                    let valid =
                        !nums.is_empty() && nums.iter().all(|n| *n >= 1 && *n <= sources.len());
                    if valid {
                        let mut citations: Vec<Value> = Vec::new();
                        for n in nums {
                            if let Some((url, title)) = sources.get(n - 1) {
                                citations.push(json!({
                                    "type": "web_search_result_location",
                                    "url": url,
                                    "title": title,
                                    "cited_text": tail_of(&buf, 200),
                                    "encrypted_index": "",
                                }));
                            }
                        }
                        blocks.push(
                            json!({"type": "text", "text": buf.clone(), "citations": citations}),
                        );
                        buf.clear();
                        i += pos + 2;
                        continue;
                    }
                }
            }
        }
        let c = text[i..].chars().next().unwrap_or(' ');
        buf.push(c);
        i += c.len_utf8();
    }
    blocks.push(json!({"type": "text", "text": buf}));
    blocks
}

/// OpenAI 非流式响应 -> Anthropic message。
fn to_anthropic_message(v: &Value, built: &BuiltRequest) -> Value {
    let msg = &v["choices"][0]["message"];
    let finish = v["choices"][0]
        .get("finish_reason")
        .and_then(|x| x.as_str())
        .unwrap_or("stop")
        .to_string();
    let mut content: Vec<Value> = Vec::new();
    content.extend(built.prelude.iter().cloned());

    if built.thinking_enabled {
        if let Some(r) = msg
            .get("reasoning_content")
            .and_then(|x| x.as_str())
            .filter(|s| !s.is_empty())
        {
            content.push(
                json!({"type": "thinking", "thinking": r, "signature": fake_signature()}),
            );
        }
    }

    let text = msg.get("content").and_then(|x| x.as_str()).unwrap_or("");
    let tool_calls = msg
        .get("tool_calls")
        .and_then(|x| x.as_array())
        .cloned()
        .unwrap_or_default();
    if !text.is_empty() || tool_calls.is_empty() {
        content.extend(marked_text_blocks(text, &built.sources));
    }

    for tc in &tool_calls {
        let id = tc
            .get("id")
            .and_then(|x| x.as_str())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .unwrap_or_else(|| format!("toolu_{}", Uuid::new_v4().simple()));
        let name = tc["function"]["name"].as_str().unwrap_or("").to_string();
        let args = tc["function"]["arguments"].as_str().unwrap_or("{}");
        let input = serde_json::from_str::<Value>(args).unwrap_or_else(|_| json!({}));
        content.push(json!({"type": "tool_use", "id": id, "name": name, "input": input}));
    }

    let stop_reason = if !tool_calls.is_empty() {
        "tool_use"
    } else if finish == "length" {
        "max_tokens"
    } else {
        "end_turn"
    };
    let usage = v.get("usage");
    let in_tok = usage
        .and_then(|u| u.get("prompt_tokens"))
        .and_then(|x| x.as_u64())
        .unwrap_or(built.input_tokens);
    let out_tok = usage
        .and_then(|u| u.get("completion_tokens"))
        .and_then(|x| x.as_u64())
        .unwrap_or_else(|| estimate_tokens(text));

    json!({
        "id": built.id,
        "type": "message",
        "role": "assistant",
        "model": built.model,
        "content": content,
        "stop_reason": stop_reason,
        "stop_sequence": Value::Null,
        "usage": {"input_tokens": in_tok, "output_tokens": out_tok},
    })
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
    let files_dir = files::files_dir(&state);
    // 客户端声明了服务端工具就先在本机执行（搜索 / 读链接），再带着结果去问模型。
    let (want_search, want_read) = server_tool_intent(&req);
    let web = if want_search || want_read {
        collect_web_context(&state, &req, want_search, want_read).await
    } else {
        WebContext::default()
    };
    let built = match build_openai_request(&req, Some(files_dir.as_path()), &web) {
        Ok(v) => v,
        Err(e) => return anthropic_error(StatusCode::BAD_REQUEST, &e),
    };
    let openai_body = match serde_json::to_vec(&built.body) {
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

    if built.stream {
        // 流式：直接把上游字节流交给转换器逐字转事件；上游非 2xx 时先把错误体读出来再按错误回。
        if !status.is_success() {
            let bytes = match to_bytes(resp.into_body(), MAX_BODY).await {
                Ok(b) => b,
                Err(e) => {
                    return anthropic_error(
                        StatusCode::BAD_GATEWAY,
                        &format!("读取上游响应失败: {e}"),
                    )
                }
            };
            let message = upstream_error_message(&bytes);
            return anthropic_error(status, &message);
        }
        let body = stream_body(resp.into_body(), built);
        return Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "text/event-stream")
            .header("cache-control", "no-cache")
            .body(body)
            .unwrap();
    }

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
        let message = upstream_error_message(&bytes);
        return anthropic_error(status, &message);
    }
    let v: Value = serde_json::from_slice(&bytes).unwrap_or_else(|_| json!({}));
    axum::Json(to_anthropic_message(&v, &built)).into_response()
}

fn upstream_error_message(bytes: &Bytes) -> String {
    let text = String::from_utf8_lossy(bytes);
    serde_json::from_slice::<Value>(bytes)
        .ok()
        .and_then(|v| {
            v.get("error")
                .and_then(|e| e.get("message"))
                .and_then(|m| m.as_str())
                .map(|s| s.to_string())
        })
        .unwrap_or_else(|| text.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_query_strips_scaffolding() {
        let q = search_query("最近有什么 AI 新闻？用一句话总结，并在句末标注来源编号。");
        assert!(q.starts_with("AI 新闻"), "疑问词/时效词应被剥掉: {q}");
        assert!(!q.contains("最近"), "时效副词应被剥掉: {q}");
        assert!(!q.contains("用一句话总结"), "指令尾巴应被截掉: {q}");
        assert!(q.contains('年'), "有时效诉求应补当天日期: {q}");
    }

    #[test]
    fn search_query_keeps_topic() {
        let q = search_query("帮我查一下 Kimi K3.1 Pro 有哪些升级");
        assert!(q.contains("Kimi"), "{q}");
        assert!(!q.contains("帮我"), "{q}");
        assert!(!q.contains("有哪些"), "{q}");
    }

    #[test]
    fn search_query_keeps_explicit_date() {
        let q = search_query("2026年11月 全球 AI 新闻日报");
        assert!(q.contains("2026年11月"), "{q}");
        assert_eq!(q.matches('年').count(), 1, "不应重复补日期: {q}");
    }
}
