//! 会话粘性（session affinity）与 Prompt Cache 的配套实现。
//!
//! 默认策略是 `auto`：只有观察到这个会话在某个上游确实产生了缓存读，
//! 或连续命中同一上游两次，才把它固定到那个上游；没有缓存收益时继续按
//! priority / random / round_robin 正常分流。策略可被模型路由的
//! `affinity` 字段覆盖：auto / session / turn / off / strict。

use std::collections::HashMap;
use std::fs;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::state::{AppState, Upstream};

const STORE_VERSION: i64 = 1;
const KEEP_SECS: i64 = 30 * 24 * 3600;
const MAX_STICKS: usize = 4096;
const AUTO_MIN_CACHE: u64 = 1024;

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct Stick {
    pub upstream: String,
    pub model: String,
    pub last_used: i64,
    pub hits: u64,
    pub cache_read: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Store {
    #[serde(default = "default_version")]
    pub version: i64,
    #[serde(default)]
    pub sticks: HashMap<String, Stick>,
    #[serde(skip)]
    pub path: PathBuf,
}

fn default_version() -> i64 {
    STORE_VERSION
}

impl Default for Store {
    fn default() -> Self {
        Self {
            version: STORE_VERSION,
            sticks: HashMap::new(),
            path: PathBuf::new(),
        }
    }
}

pub fn path_for(state_path: &Path) -> PathBuf {
    let name = state_path
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "config.state.json".to_string());
    state_path.with_file_name(name.replace(".state.json", ".affinity.json"))
}

impl Store {
    pub fn load(path: PathBuf) -> Self {
        let mut store = fs::read_to_string(&path)
            .ok()
            .and_then(|text| serde_json::from_str::<Store>(&text).ok())
            .unwrap_or_default();
        store.path = path;
        store.version = STORE_VERSION;
        store.sticks.retain(|_, s| s.last_used > now() - KEEP_SECS);
        store
    }

    pub fn save(&self) {
        if self.path.as_os_str().is_empty() {
            return;
        }
        if let Some(dir) = self.path.parent() {
            let _ = fs::create_dir_all(dir);
        }
        if let Ok(text) = serde_json::to_string_pretty(self) {
            let _ = fs::write(&self.path, text);
        }
    }

    pub fn count(&self) -> usize {
        self.sticks.len()
    }

    pub fn preferred(&self, model: &str, session: &str, mode: &str) -> Option<String> {
        if session.is_empty() || mode == "off" {
            return None;
        }
        let key = stick_key(model, session);
        let stick = self.sticks.get(&key)?;
        let age = now() - stick.last_used;
        let ttl = match mode {
            "session" | "strict" => KEEP_SECS,
            "turn" => 600,
            _ => cache_cold_secs(model),
        };
        if age < 0 || age > ttl {
            return None;
        }
        if mode == "auto" && stick.cache_read < AUTO_MIN_CACHE && stick.hits < 2 {
            return None;
        }
        Some(stick.upstream.clone())
    }

    pub fn remember(&mut self, model: &str, session: &str, upstream: &str, cache_read: u64) {
        if session.is_empty() || upstream.is_empty() {
            return;
        }
        let key = stick_key(model, session);
        let entry = self.sticks.entry(key).or_insert_with(|| Stick {
            upstream: upstream.to_string(),
            model: model.to_string(),
            last_used: now(),
            hits: 0,
            cache_read: 0,
        });
        entry.upstream = upstream.to_string();
        entry.model = model.to_string();
        entry.last_used = now();
        entry.hits = entry.hits.saturating_add(1);
        entry.cache_read = entry.cache_read.saturating_add(cache_read);
        self.prune();
        self.save();
    }

    fn prune(&mut self) {
        let cutoff = now() - KEEP_SECS;
        self.sticks.retain(|_, s| s.last_used >= cutoff);
        if self.sticks.len() <= MAX_STICKS {
            return;
        }
        let mut ages: Vec<(String, i64)> = self
            .sticks
            .iter()
            .map(|(k, v)| (k.clone(), v.last_used))
            .collect();
        ages.sort_by_key(|(_, at)| *at);
        for (key, _) in ages.into_iter().take(self.sticks.len() - MAX_STICKS) {
            self.sticks.remove(&key);
        }
    }

    pub fn clear(&mut self) -> usize {
        let n = self.sticks.len();
        self.sticks.clear();
        self.save();
        n
    }
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn stick_key(model: &str, session: &str) -> String {
    format!("{model}\u{1}{session}")
}

fn cache_cold_secs(model: &str) -> i64 {
    let m = model.to_ascii_lowercase();
    if m.contains("gpt") || m.contains("deepseek") {
        3600
    } else if m.contains("claude") {
        300
    } else {
        300
    }
}

pub fn mode_for(state: &AppState, requested: &str) -> String {
    let file = state.file.read().unwrap();
    file.models
        .get(requested)
        .and_then(|r| r.extra.get("affinity"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .unwrap_or("auto")
        .to_string()
}

/// 把会话 id 从 header / body 里取出来。优先级和 Magpie 一致：
/// gateway 自定义 header > 各 Agent 的 session/thread id > prompt_cache_key >
/// system+first-user 的稳定哈希（给不传 session 的客户端兜底）。
pub fn session_key(headers: &HeaderMap, payload: &Value) -> (Option<String>, String) {
    for (name, source) in [
        ("x-llm-router-session", "header:x-llm-router-session"),
        ("x-magpie-session", "header:x-magpie-session"),
        ("x-session-id", "header:x-session-id"),
        ("x-claude-code-session", "header:x-claude-code-session"),
        ("x-codex-thread-id", "header:x-codex-thread-id"),
        ("x-thread-id", "header:x-thread-id"),
    ] {
        if let Some(v) = headers.get(name).and_then(|v| v.to_str().ok()) {
            let v = v.trim();
            if !v.is_empty() {
                return (Some(v.to_string()), source.to_string());
            }
        }
    }

    for key in [
        "session_id",
        "sessionId",
        "conversation_id",
        "thread_id",
        "prompt_cache_key",
    ] {
        if let Some(v) = payload.get(key).and_then(Value::as_str) {
            let v = v.trim();
            if !v.is_empty() {
                return (Some(v.to_string()), format!("body:{key}"));
            }
        }
    }
    if let Some(meta) = payload.get("metadata") {
        for key in ["session_id", "sessionId", "conversation_id", "thread_id"] {
            if let Some(v) = meta.get(key).and_then(Value::as_str) {
                let v = v.trim();
                if !v.is_empty() {
                    return (Some(v.to_string()), format!("metadata:{key}"));
                }
            }
        }
    }

    let messages = payload.get("messages").and_then(Value::as_array);
    let Some(messages) = messages else {
        return (None, String::new());
    };
    let system = messages
        .iter()
        .find(|m| m.get("role").and_then(Value::as_str) == Some("system"))
        .map(|m| content_text(m.get("content")))
        .unwrap_or_default();
    let user = messages
        .iter()
        .find(|m| m.get("role").and_then(Value::as_str) == Some("user"))
        .map(|m| content_text(m.get("content")))
        .unwrap_or_default();
    if system.is_empty() && user.is_empty() {
        return (None, String::new());
    }
    let mut h = std::collections::hash_map::DefaultHasher::new();
    system.chars().take(4000).collect::<String>().hash(&mut h);
    user.chars().take(4000).collect::<String>().hash(&mut h);
    (Some(format!("prompt:{:016x}", h.finish())), "prompt".to_string())
}

fn content_text(v: Option<&Value>) -> String {
    let Some(v) = v else {
        return String::new();
    };
    if let Some(s) = v.as_str() {
        return s.to_string();
    }
    if let Some(arr) = v.as_array() {
        let mut out = String::new();
        for part in arr {
            if let Some(s) = part.as_str() {
                out.push_str(s);
            } else if let Some(s) = part.get("text").and_then(Value::as_str) {
                out.push_str(s);
            }
        }
        return out;
    }
    String::new()
}

/// 按 affinity 策略重排候选上游；返回 (新候选, 命中的 sticky upstream id)。
pub fn apply(
    state: &AppState,
    model: &str,
    session: &str,
    mode: &str,
    mut pairs: Vec<(Upstream, String)>,
) -> (Vec<(Upstream, String)>, Option<String>) {
    let preferred = {
        let store = state.affinity.lock().unwrap();
        store.preferred(model, session, mode)
    };
    let Some(id) = preferred else {
        return (pairs, None);
    };
    if let Some(pos) = pairs.iter().position(|(u, _)| u.id == id) {
        let p = pairs.remove(pos);
        pairs.insert(0, p);
        (pairs, Some(id))
    } else {
        (pairs, None)
    }
}

pub fn remember(state: &AppState, model: &str, session: &str, upstream: &str, cache_read: u64) {
    let mut store = state.affinity.lock().unwrap();
    store.remember(model, session, upstream, cache_read);
}

pub fn status(state: &AppState) -> Value {
    let store = state.affinity.lock().unwrap();
    let now = now();
    let mut sessions: Vec<Value> = store
        .sticks
        .iter()
        .map(|(k, s)| {
            let session = k.split_once('\u{1}').map(|(_, v)| v).unwrap_or(k);
            serde_json::json!({
                "session": session,
                "model": s.model,
                "upstream": s.upstream,
                "age_seconds": (now - s.last_used).max(0),
                "hits": s.hits,
                "cache_read": s.cache_read,
            })
        })
        .collect();
    sessions.sort_by_key(|v| v["age_seconds"].as_i64().unwrap_or(i64::MAX));
    sessions.truncate(100);
    serde_json::json!({
        "ok": true,
        "path": store.path.to_string_lossy(),
        "sticks": store.count(),
        "auto_min_cache": AUTO_MIN_CACHE,
        "keep_days": KEEP_SECS / 86400,
        "sessions": sessions,
    })
}

pub fn clear(state: &AppState) -> usize {
    let mut store = state.affinity.lock().unwrap();
    store.clear()
}

pub async fn status_handler(State(state): State<Arc<AppState>>) -> Response {
    Json(status(&state)).into_response()
}

pub async fn clear_handler(State(state): State<Arc<AppState>>) -> Response {
    let n = clear(&state);
    Json(json!({"ok": true, "cleared": n})).into_response()
}
