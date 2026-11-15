//! Files API 最小实现（Anthropic / DeepSeek Chat Engine 方言）。
//!
//! 为什么需要它：DeepSeek 官方前端（chat-demo 这类）上传附件时打的是
//! `POST {base}/files`（multipart，字段 `purpose` 必须排在 `file` 前），拿到 `file_id` 后
//! 在 messages 里以 `{type:"file", file_id}` / `{type:"image", source:{type:"file",file_id}}`
//! 引用。网关原先没有这个路由，浏览器拿到空 404，界面只显示「上传失败」。
//!
//! 这里不引 multer（避免为一个上传接口拉依赖），而是手工解析 multipart：body 上限
//! 由外层 DefaultBodyLimit 控制（64MB），附件本体与元数据落到 `<state 目录>/files/`。
//!
//! 形态：
//!   POST   /v1/files        -> {"id":"file_..","type":"file","filename":..,"mime_type":..,"size":..,"created_at":..}
//!   GET    /v1/files/{id}   -> 同上（元数据）
//!   DELETE /v1/files/{id}   -> {"id":"..","type":"file_deleted","deleted":true}

use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Path as AxumPath, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use serde_json::{json, Value};
use uuid::Uuid;

use super::state::AppState;

/// 附件目录：与 config.state.json 同级的 files/（随 LLM_ROUTER_STATE 一起搬家）。
pub fn files_dir(state: &AppState) -> PathBuf {
    state
        .state_path
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."))
        .join("files")
}

pub fn new_id() -> String {
    format!("file_{}", Uuid::new_v4().simple())
}

fn now_secs() -> u64 {
    chrono::Utc::now().timestamp().max(0) as u64
}

fn blob_path(dir: &Path, id: &str) -> PathBuf {
    dir.join(format!("{id}.bin"))
}

fn meta_path(dir: &Path, id: &str) -> PathBuf {
    dir.join(format!("{id}.json"))
}

/// 落盘的附件。`bytes` 在读盘时填充。
pub struct StoredFile {
    pub id: String,
    pub filename: String,
    pub mime: String,
    pub size: u64,
    pub created_at: u64,
    pub bytes: Vec<u8>,
}

impl StoredFile {
    pub fn is_image(&self) -> bool {
        self.mime.starts_with("image/")
    }

    pub fn is_pdf(&self) -> bool {
        self.mime == "application/pdf" || self.filename.to_lowercase().ends_with(".pdf")
    }

    /// data URL（给 OpenAI 兼容上游的 image_url 用）。
    pub fn data_url(&self) -> String {
        format!(
            "data:{};base64,{}",
            self.mime,
            base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &self.bytes)
        )
    }

    /// 纯文本类附件（txt/md/csv/json/代码）直接当文本读出。
    pub fn as_text(&self) -> Option<String> {
        let texty = self.mime.starts_with("text/")
            || self.mime == "application/json"
            || self.mime == "application/xml"
            || self.mime.ends_with("+json")
            || self.mime.ends_with("+xml")
            || self.mime.is_empty();
        if !texty && !self.is_pdf() {
            return None;
        }
        if self.is_pdf() {
            return super::pdf::extract_text(&self.bytes);
        }
        String::from_utf8(self.bytes.clone()).ok()
    }
}

fn write_meta(dir: &Path, f: &StoredFile) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    std::fs::write(blob_path(dir, &f.id), &f.bytes).map_err(|e| e.to_string())?;
    let meta = json!({
        "id": f.id,
        "filename": f.filename,
        "mime_type": f.mime,
        "size": f.size,
        "created_at": f.created_at,
    });
    std::fs::write(
        meta_path(dir, &f.id),
        serde_json::to_string_pretty(&meta).unwrap_or_default(),
    )
    .map_err(|e| e.to_string())
}

/// 读回一个附件（元数据 + 字节）。
pub fn load(dir: &Path, id: &str) -> Option<StoredFile> {
    if id.is_empty() || id.contains("..") || id.contains('/') || id.contains('\\') {
        return None;
    }
    let meta: Value =
        serde_json::from_str(&std::fs::read_to_string(meta_path(dir, id)).ok()?).ok()?;
    let bytes = std::fs::read(blob_path(dir, id)).ok()?;
    Some(StoredFile {
        id: id.to_string(),
        filename: meta
            .get("filename")
            .and_then(|v| v.as_str())
            .unwrap_or("file")
            .to_string(),
        mime: meta
            .get("mime_type")
            .and_then(|v| v.as_str())
            .unwrap_or("application/octet-stream")
            .to_string(),
        size: meta.get("size").and_then(|v| v.as_u64()).unwrap_or(bytes.len() as u64),
        created_at: meta.get("created_at").and_then(|v| v.as_u64()).unwrap_or(0),
        bytes,
    })
}

fn meta_json(f: &StoredFile) -> Value {
    json!({
        "id": f.id,
        "type": "file",
        "filename": f.filename,
        "mime_type": f.mime,
        "size": f.size,
        "created_at": f.created_at,
    })
}

fn err_response(status: StatusCode, kind: &str, message: &str) -> Response {
    (
        status,
        axum::Json(json!({
            "type": "error",
            "error": {"type": kind, "message": message},
        })),
    )
        .into_response()
}

// ───────────────────────── multipart ─────────────────────────

fn find_sub(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || hay.len() < needle.len() {
        return None;
    }
    let first = needle[0];
    let last = hay.len() - needle.len();
    let mut i = 0;
    while i <= last {
        if hay[i] == first && &hay[i..i + needle.len()] == needle {
            return Some(i);
        }
        i += 1;
    }
    None
}

struct Part {
    name: String,
    filename: Option<String>,
    content_type: Option<String>,
    data: Vec<u8>,
}

fn header_value(head: &str, key: &str) -> Option<String> {
    for line in head.split("\r\n").chain(head.split('\n')) {
        let (k, v) = match line.split_once(':') {
            Some(kv) => kv,
            None => continue,
        };
        if k.trim().eq_ignore_ascii_case(key) {
            return Some(v.trim().to_string());
        }
    }
    None
}

fn param_value(head: &str, key: &str) -> Option<String> {
    let lower = head.to_lowercase();
    let at = lower.find(&format!("{key}="))?;
    let rest = &head[at + key.len() + 1..];
    let rest = rest.strip_prefix('"').unwrap_or(rest);
    match rest.find('"') {
        Some(end) => Some(rest[..end].to_string()),
        None => {
            let end = rest.find(';').unwrap_or(rest.len());
            Some(rest[..end].trim().to_string())
        }
    }
}

/// 手工解析 multipart/form-data。容忍多余空白与 CRLF/LF 混用，不追求 RFC 全覆盖。
fn parse_multipart(body: &[u8], boundary: &str) -> Vec<Part> {
    let mut parts = Vec::new();
    let delim = format!("--{boundary}");
    let delim_bytes = delim.as_bytes();
    let sep = format!("\r\n--{boundary}");
    let sep_bytes = sep.as_bytes();
    let mut pos = 0usize;
    while let Some(rel) = find_sub(&body[pos..], delim_bytes) {
        let mut start = pos + rel + delim_bytes.len();
        if body[start..].starts_with(b"--") {
            break;
        }
        if body[start..].starts_with(b"\r\n") {
            start += 2;
        } else if body[start..].starts_with(b"\n") {
            start += 1;
        }
        let end = match find_sub(&body[start..], sep_bytes) {
            Some(j) => start + j,
            None => body.len(),
        };
        let seg = &body[start..end];
        if let Some(h) = find_sub(seg, b"\r\n\r\n").or_else(|| find_sub(seg, b"\n\n")) {
            let skip = if seg[h..].starts_with(b"\r\n\r\n") { 4 } else { 2 };
            let head = String::from_utf8_lossy(&seg[..h]).to_string();
            let data = seg[h + skip..].to_vec();
            let disposition = header_value(&head, "content-disposition").unwrap_or_default();
            parts.push(Part {
                name: param_value(&disposition, "name").unwrap_or_default(),
                filename: param_value(&disposition, "filename"),
                content_type: header_value(&head, "content-type"),
                data,
            });
        }
        pos = end + 2;
        if pos > body.len() {
            break;
        }
    }
    parts
}

fn boundary_of(headers: &HeaderMap) -> Option<String> {
    let ct = headers.get("content-type")?.to_str().ok()?;
    let lower = ct.to_lowercase();
    let at = lower.find("boundary=")?;
    let rest = ct[at + "boundary=".len()..].trim_matches('"');
    let end = rest.find(';').unwrap_or(rest.len());
    Some(rest[..end].trim().trim_matches('"').to_string())
}

/// 按扩展名猜 MIME（有些客户端上传时不给 content-type）。
fn guess_mime(filename: &str, given: Option<&str>) -> String {
    if let Some(ct) = given {
        if !ct.is_empty() && ct != "application/octet-stream" {
            return ct.to_string();
        }
    }
    let ext = filename.rsplit('.').next().unwrap_or("").to_lowercase();
    match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        "pdf" => "application/pdf",
        "txt" => "text/plain",
        "md" => "text/markdown",
        "csv" => "text/csv",
        "json" => "application/json",
        "html" | "htm" => "text/html",
        "xml" => "application/xml",
        "js" | "mjs" | "cjs" => "text/javascript",
        "ts" | "tsx" | "jsx" => "text/plain",
        "rs" => "text/plain",
        "py" => "text/plain",
        "go" => "text/plain",
        "java" => "text/plain",
        "c" | "h" | "cpp" | "hpp" => "text/plain",
        "sh" => "text/plain",
        "yml" | "yaml" => "application/x-yaml",
        "log" => "text/plain",
        _ => "application/octet-stream",
    }
    .to_string()
}

// ───────────────────────── handlers ─────────────────────────

pub async fn upload(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let boundary = match boundary_of(&headers) {
        Some(b) => b,
        None => {
            return err_response(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                "缺少 multipart boundary：Files API 需要 multipart/form-data",
            )
        }
    };
    let parts = parse_multipart(&body, &boundary);
    let purpose = parts
        .iter()
        .find(|p| p.name == "purpose")
        .map(|p| String::from_utf8_lossy(&p.data).trim().to_string())
        .unwrap_or_else(|| "file".to_string());
    let file_part = parts
        .iter()
        .find(|p| p.name == "file")
        .or_else(|| parts.iter().find(|p| p.filename.is_some()));
    let file_part = match file_part {
        Some(p) => p,
        None => {
            return err_response(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                "multipart 里没有 file 字段",
            )
        }
    };
    if file_part.data.is_empty() {
        return err_response(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "上传的 file 为空",
        );
    }
    let filename = file_part
        .filename
        .clone()
        .unwrap_or_else(|| format!("upload-{}", purpose));
    let mime = guess_mime(&filename, file_part.content_type.as_deref());
    let dir = files_dir(&state);
    let stored = StoredFile {
        id: new_id(),
        filename,
        mime,
        size: file_part.data.len() as u64,
        created_at: now_secs(),
        bytes: file_part.data.clone(),
    };
    if let Err(e) = write_meta(&dir, &stored) {
        return err_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "api_error",
            &format!("附件落盘失败: {e}"),
        );
    }
    eprintln!(
        "[files] 已保存 {} ({}, {} bytes)",
        stored.id, stored.mime, stored.size
    );
    axum::Json(meta_json(&stored)).into_response()
}

pub async fn retrieve(
    State(state): State<Arc<AppState>>,
    AxumPath(id): AxumPath<String>,
) -> Response {
    match load(&files_dir(&state), &id) {
        Some(f) => axum::Json(meta_json(&f)).into_response(),
        None => err_response(
            StatusCode::NOT_FOUND,
            "not_found_error",
            &format!("没有这个文件: {id}"),
        ),
    }
}

pub async fn delete(State(state): State<Arc<AppState>>, AxumPath(id): AxumPath<String>) -> Response {
    let dir = files_dir(&state);
    let existed = blob_path(&dir, &id).exists();
    let _ = std::fs::remove_file(blob_path(&dir, &id));
    let _ = std::fs::remove_file(meta_path(&dir, &id));
    if existed {
        axum::Json(json!({"id": id, "type": "file_deleted", "deleted": true})).into_response()
    } else {
        // 客户端（chat-demo）删附件时并不检查结果，返回 200 更省事，但保留真实语义：
        err_response(
            StatusCode::NOT_FOUND,
            "not_found_error",
            &format!("没有这个文件: {id}"),
        )
    }
}
