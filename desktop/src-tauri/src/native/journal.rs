use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::sync::Arc;

use serde_json::{json, Value};

use super::state::AppState;

fn read_tail(path: &std::path::Path, max_bytes: u64) -> Vec<Value> {
    let Ok(mut file) = std::fs::File::open(path) else {
        return vec![];
    };
    let len = file.metadata().map(|m| m.len()).unwrap_or(0);
    let start = len.saturating_sub(max_bytes);
    if file.seek(SeekFrom::Start(start)).is_err() {
        return vec![];
    }
    let mut buf = Vec::new();
    if file.read_to_end(&mut buf).is_err() {
        return vec![];
    }
    let text = String::from_utf8_lossy(&buf);
    let mut lines: Vec<&str> = text.lines().collect();
    if start > 0 && !lines.is_empty() {
        lines.remove(0); // 可能截断半行
    }
    lines
        .into_iter()
        .filter_map(|l| serde_json::from_str::<Value>(l.trim()).ok())
        .collect()
}

/// 健康看板取最近的流水条目（尾部缓存，按文件大小+mtime 失效）。
pub fn recent(state: &AppState) -> Arc<Vec<Value>> {
    tail_cached(state)
}

/// 8MB 尾巴只解析一次：index / query / entry 共用，按文件长度+mtime 失效。
fn tail_cached(state: &AppState) -> Arc<Vec<Value>> {
    let path = state.history_path();
    let meta = std::fs::metadata(&path).ok();
    let len = meta.as_ref().map(|m| m.len()).unwrap_or(0);
    let modified = meta.and_then(|m| m.modified().ok());
    if let Ok(cache) = state.journal_cache.lock() {
        if let Some((cached_len, cached_mtime, entries)) = cache.as_ref() {
            if *cached_len == len && *cached_mtime == modified {
                return entries.clone();
            }
        }
    }
    let entries = Arc::new(read_tail(&path, 8 * 1024 * 1024));
    if let Ok(mut cache) = state.journal_cache.lock() {
        *cache = Some((len, modified, entries.clone()));
    }
    entries
}

/// 列表页不需要大文本；详情走 /admin/journal/entry。推送也用这个瘦身版。
pub fn strip_heavy(mut entry: Value) -> Value {
    if let Some(obj) = entry.as_object_mut() {
        for key in ["request_text", "response_text", "reasoning_text"] {
            obj.remove(key);
        }
    }
    entry
}

fn date_of(entry: &Value) -> String {
    let at = entry.get("at").and_then(|v| v.as_f64()).unwrap_or(0.0);
    if let Some(dt) = chrono::DateTime::from_timestamp(at as i64, 0) {
        dt.format("%Y-%m-%d").to_string()
    } else {
        String::new()
    }
}

fn count_map_to_list(map: HashMap<String, usize>) -> Vec<Value> {
    let mut items: Vec<(String, usize)> = map.into_iter().filter(|(k, _)| !k.is_empty()).collect();
    items.sort_by(|a, b| b.1.cmp(&a.1));
    items.into_iter().map(|(k, v)| json!({"key": k, "count": v})).collect()
}

pub fn index(state: &AppState) -> Value {
    let entries = tail_cached(state);
    let mut days: HashMap<String, usize> = HashMap::new();
    let mut months: HashMap<String, usize> = HashMap::new();
    let mut groups: HashMap<String, usize> = HashMap::new();
    for e in entries.iter() {
        let d = date_of(e);
        if !d.is_empty() {
            *days.entry(d.clone()).or_default() += 1;
            *months.entry(d[..7].to_string()).or_default() += 1;
        }
        let g = e.get("model").and_then(|v| v.as_str()).unwrap_or("").to_string();
        *groups.entry(g).or_default() += 1;
    }
    json!({
        "total": entries.len(),
        "days": count_map_to_list(days),
        "months": count_map_to_list(months),
        "groups": count_map_to_list(groups),
    })
}

pub fn query(state: &AppState, q: &HashMap<String, String>) -> Value {
    let entries = tail_cached(state);
    let day = q.get("day").cloned().unwrap_or_default();
    let month = q.get("month").cloned().unwrap_or_default();
    let group = q.get("group").cloned().unwrap_or_default();
    let upstream = q.get("upstream").cloned().unwrap_or_default();
    let model = q.get("model").cloned().unwrap_or_default();

    let mut keep: Vec<Value> = entries
        .iter()
        .filter(|e| {
            let d = date_of(e);
            if !day.is_empty() && d != day {
                return false;
            }
            if !month.is_empty() && !d.starts_with(&month) {
                return false;
            }
            if !group.is_empty() && e.get("model").and_then(|v| v.as_str()).unwrap_or("") != group {
                return false;
            }
            if !upstream.is_empty() {
                let matches = e.get("upstream").and_then(|v| v.as_str()).map(|u| u == upstream).unwrap_or(false)
                    || e.get("trail").and_then(|v| v.as_array()).map(|a| a.iter().any(|s| s.get("upstream").and_then(|v| v.as_str()) == Some(upstream.as_str()))).unwrap_or(false);
                if !matches {
                    return false;
                }
            }
            if !model.is_empty() {
                let matches = e.get("effective_model").and_then(|v| v.as_str()).map(|m| m == model).unwrap_or(false)
                    || e.get("trail").and_then(|v| v.as_array()).map(|a| a.iter().any(|s| s.get("model").and_then(|v| v.as_str()) == Some(model.as_str()))).unwrap_or(false);
                if !matches {
                    return false;
                }
            }
            true
        })
        .cloned()
        .collect();
    keep.sort_by(|a, b| {
        b.get("at").and_then(|v| v.as_f64()).unwrap_or(0.0)
            .partial_cmp(&a.get("at").and_then(|v| v.as_f64()).unwrap_or(0.0))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let total = keep.len();
    let offset = q.get("offset").and_then(|v| v.parse::<usize>().ok()).unwrap_or(0);
    let limit = q.get("limit").and_then(|v| v.parse::<usize>().ok()).unwrap_or(100).min(1000);
    let page: Vec<Value> = keep
        .into_iter()
        .skip(offset)
        .take(limit)
        .map(strip_heavy)
        .collect();
    json!({
        "entries": page,
        "total": total,
        "offset": offset,
        "limit": limit,
        "days": [],
        "groups": [],
        "upstreams": [],
    })
}

/// 单条详情（带 request_text / response_text / reasoning_text）。
pub fn entry(state: &AppState, q: &HashMap<String, String>) -> Value {
    let at = q
        .get("at")
        .and_then(|v| v.parse::<f64>().ok())
        .unwrap_or(-1.0);
    let upstream = q.get("upstream").cloned().unwrap_or_default();
    let entries = tail_cached(state);
    let found = entries.iter().rev().find(|e| {
        let same_at = (e.get("at").and_then(|v| v.as_f64()).unwrap_or(-1.0) - at).abs() < 0.001;
        let same_up = upstream.is_empty()
            || e.get("upstream").and_then(|v| v.as_str()) == Some(upstream.as_str());
        same_at && same_up
    });
    json!({"entry": found.cloned().unwrap_or(Value::Null)})
}
