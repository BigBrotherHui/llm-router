//! 统一采集器：后台只跑一个循环，预热会话对账 / 工具排行 / 今日全量统计。
//! 页面请求同一 query 时直接命中内存缓存，避免每页各扫一遍会话文件。

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use axum::extract::{Query, State};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};

use super::sessions::{self, ScanQuery, ToolQuery};
use super::state::AppState;

#[derive(Clone)]
struct Entry {
    at: Instant,
    value: Value,
}

fn cache() -> &'static Mutex<HashMap<String, Entry>> {
    static CACHE: OnceLock<Mutex<HashMap<String, Entry>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

static HITS: AtomicU64 = AtomicU64::new(0);
static MISSES: AtomicU64 = AtomicU64::new(0);
static RUNS: AtomicU64 = AtomicU64::new(0);
static LAST_RUN: AtomicU64 = AtomicU64::new(0);

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub fn get(key: &str, ttl_secs: u64) -> Option<Value> {
    let map = cache().lock().ok()?;
    match map.get(key) {
        Some(e) if e.at.elapsed() < Duration::from_secs(ttl_secs) => {
            HITS.fetch_add(1, Ordering::Relaxed);
            Some(e.value.clone())
        }
        _ => {
            MISSES.fetch_add(1, Ordering::Relaxed);
            None
        }
    }
}

pub fn put(key: String, value: Value) {
    if let Ok(mut map) = cache().lock() {
        map.insert(key, Entry { at: Instant::now(), value });
    }
}

pub fn invalidate(key: &str) {
    if let Ok(mut map) = cache().lock() {
        map.remove(key);
    }
}

fn norm_sources(s: Option<&str>) -> String {
    let mut v: Vec<String> = s
        .unwrap_or("dsh,claude,pi,codex")
        .split(',')
        .map(|x| x.trim().to_ascii_lowercase())
        .filter(|x| !x.is_empty())
        .collect();
    v.sort();
    v.dedup();
    v.join(",")
}

pub fn scan_key(q: &ScanQuery) -> String {
    let range = q
        .range
        .as_deref()
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| match q.hours {
            Some(h) => format!("{h}h"),
            None => "today".into(),
        });
    format!(
        "scan|{range}|{}|{}|{}|{}",
        norm_sources(q.sources.as_deref()),
        q.limit.unwrap_or(200),
        q.route.as_deref().unwrap_or("all").trim().to_ascii_lowercase(),
        q.metric.as_deref().unwrap_or("happycoding").trim().to_ascii_lowercase()
    )
}

pub fn tools_key(q: &ToolQuery) -> String {
    let range = q
        .range
        .as_deref()
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| match q.hours {
            Some(h) => format!("{h}h"),
            None => "today".into(),
        });
    format!(
        "tools|{range}|{}|{}",
        norm_sources(q.sources.as_deref()),
        q.limit.unwrap_or(200)
    )
}

/// 后台预热：固定默认查询（今天/四源/200 行），每 10 秒一轮。
pub fn start(state: Arc<AppState>) {
    tokio::spawn(async move {
        // 错开启动，避免和第一次页面加载抢 IO。
        tokio::time::sleep(Duration::from_secs(3)).await;
        loop {
            let scan_q = ScanQuery {
                hours: None,
                range: Some("today".into()),
                sources: Some("dsh,claude,pi,codex".into()),
                limit: Some(200),
                route: None,
                metric: None,
                day: None,
                month: None,
                group: None,
                upstream: None,
                model: None,
                offset: None,
            };
            invalidate(&scan_key(&scan_q));
            let _ = sessions::scan(State(state.clone()), Query(scan_q)).await;
            let tools_q = ToolQuery {
                hours: None,
                range: Some("today".into()),
                sources: Some("dsh,claude".into()),
                limit: Some(200),
            };
            invalidate(&tools_key(&tools_q));
            let _ = sessions::tools(State(state.clone()), Query(tools_q)).await;
            let port = {
                let p = state.port.load(Ordering::Relaxed);
                if p == 0 { 8010 } else { p }
            };
            let _ = sessions::today_totals(port);
            let _ = super::journal_index::sync(&state);
            RUNS.fetch_add(1, Ordering::Relaxed);
            LAST_RUN.store(now_secs(), Ordering::Relaxed);
            tokio::time::sleep(Duration::from_secs(10)).await;
        }
    });
}

pub async fn status(State(_state): State<Arc<AppState>>) -> Response {
    let map = cache().lock().map(|m| m.clone()).unwrap_or_default();
    let entries: Vec<Value> = map
        .iter()
        .map(|(k, e)| {
            json!({
                "key": k,
                "age_seconds": e.at.elapsed().as_secs(),
                "bytes": serde_json::to_string(&e.value).map(|s| s.len()).unwrap_or(0),
            })
        })
        .collect();
    Json(json!({
        "ok": true,
        "mode": "single-collector",
        "interval_seconds": 10,
        "runs": RUNS.load(Ordering::Relaxed),
        "last_run": LAST_RUN.load(Ordering::Relaxed),
        "hits": HITS.load(Ordering::Relaxed),
        "misses": MISSES.load(Ordering::Relaxed),
        "entries": entries,
    }))
    .into_response()
}
