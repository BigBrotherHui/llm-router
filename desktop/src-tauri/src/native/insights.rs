//! 图表数据：把请求流水聚合成前端能直接画的时间序列、延迟直方图和模型/上游分布。
//!
//! 只读 `config.history.jsonl` 的尾部缓存（与健康看板共用 journal::recent），
//! 聚合结果按请求窗口返回；不写任何文件。

use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::{Query, State};
use axum::response::{IntoResponse, Response};
use axum::Json;
use chrono::Timelike;
use serde::Deserialize;
use serde_json::{json, Value};

use super::state::AppState;
use super::stats::normalize_usage;

#[derive(Deserialize, Default)]
pub struct SeriesQuery {
    /// today / all；缺省时看「今天」
    pub range: Option<String>,
    /// 滚动小时窗口；传了就以它为准
    pub hours: Option<f64>,
    /// 可选：强制分桶秒数
    pub bucket: Option<i64>,
}

#[derive(Deserialize, Default)]
pub struct LedgerQuery {
    pub range: Option<String>,
    pub hours: Option<f64>,
}

struct Bucket {
    start: f64,
    calls: u64,
    ok: u64,
    failed: u64,
    input: u64,
    output: u64,
    cache_read: u64,
    cache_write: u64,
    elapsed: Vec<f64>,
    first: Vec<f64>,
}

impl Bucket {
    fn new(start: f64) -> Self {
        Self {
            start,
            calls: 0,
            ok: 0,
            failed: 0,
            input: 0,
            output: 0,
            cache_read: 0,
            cache_write: 0,
            elapsed: Vec::new(),
            first: Vec::new(),
        }
    }

    fn json(&self, bucket_secs: i64) -> Value {
        let avg_ms = if self.elapsed.is_empty() {
            0.0
        } else {
            self.elapsed.iter().sum::<f64>() / self.elapsed.len() as f64 * 1000.0
        };
        json!({
            "at": self.start,
            "label": bucket_label(self.start, bucket_secs),
            "calls": self.calls,
            "ok": self.ok,
            "failed": self.failed,
            "success_rate": if self.calls > 0 { (self.ok as f64 / self.calls as f64 * 1000.0).round() / 10.0 } else { 0.0 },
            "input": self.input,
            "output": self.output,
            "cache_read": self.cache_read,
            "cache_write": self.cache_write,
            "avg_ms": (avg_ms * 10.0).round() / 10.0,
            "p50_ms": percentile_ms(&self.elapsed, 0.50),
            "p95_ms": percentile_ms(&self.elapsed, 0.95),
            "first_p50_ms": percentile_ms(&self.first, 0.50),
        })
    }
}

fn bucket_label(start: f64, bucket_secs: i64) -> String {
    let Some(dt) = chrono::DateTime::from_timestamp(start as i64, 0) else {
        return String::new();
    };
    let local = dt.with_timezone(&chrono::Local);
    if bucket_secs >= 86400 {
        local.format("%m-%d").to_string()
    } else {
        local.format("%H:%M").to_string()
    }
}

fn percentile_ms(samples: &[f64], p: f64) -> f64 {
    if samples.is_empty() {
        return 0.0;
    }
    let mut v: Vec<f64> = samples.iter().copied().filter(|x| *x > 0.0).collect();
    if v.is_empty() {
        return 0.0;
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let idx = ((v.len() as f64 - 1.0) * p).round() as usize;
    (v[idx.min(v.len() - 1)] * 1000.0 * 10.0).round() / 10.0
}

fn auto_bucket(span: f64) -> i64 {
    if span <= 2.0 * 3600.0 {
        300
    } else if span <= 26.0 * 3600.0 {
        if span <= 8.0 * 3600.0 {
            900
        } else {
            1800
        }
    } else if span <= 4.0 * 86400.0 {
        3 * 3600
    } else if span <= 8.0 * 86400.0 {
        6 * 3600
    } else {
        86400
    }
}

fn usage_of(entry: &Value) -> Value {
    normalize_usage(&entry.get("usage").cloned().unwrap_or_else(|| json!({})))
}

fn n(v: &Value, key: &str) -> u64 {
    v.get(key).and_then(|x| x.as_u64()).unwrap_or(0)
}

pub async fn series(State(state): State<Arc<AppState>>, Query(q): Query<SeriesQuery>) -> Response {
    let now = chrono::Local::now();
    let now_ts = now.timestamp() as f64 + now.time().nanosecond() as f64 / 1_000_000_000.0;
    let mut range = q
        .range
        .as_deref()
        .map(|s| s.trim().to_ascii_lowercase())
        .unwrap_or_default();
    if range.is_empty() && q.hours.is_none() {
        range = "today".to_string();
    }

    let (since, range_label) = match range.as_str() {
        "today" => {
            let secs = now.time().num_seconds_from_midnight() as f64
                + now.time().nanosecond() as f64 / 1_000_000_000.0;
            (now_ts - secs, "today".to_string())
        }
        "all" => (0.0, "all".to_string()),
        _ => {
            let h = q.hours.unwrap_or(24.0).max(0.0);
            if h <= 0.0 {
                (0.0, "all".to_string())
            } else {
                (now_ts - h * 3600.0, format!("{}h", h))
            }
        }
    };

    let span = (now_ts - since).max(60.0);
    let bucket_secs = q.bucket.unwrap_or_else(|| auto_bucket(span)).max(60);
    let cache_key = format!("series|{}|{}|{}", range, q.hours.unwrap_or(0.0), bucket_secs);
    if let Some(v) = super::collector::get(&cache_key, 8) {
        return Json(v).into_response();
    }
    // 对齐到桶边界，图表的 x 轴刻度更自然。
    let start = (since / bucket_secs as f64).floor() * bucket_secs as f64;
    let count = (((now_ts - start) / bucket_secs as f64).ceil() as usize + 1).clamp(1, 240);

    let mut buckets: Vec<Bucket> = (0..count)
        .map(|i| Bucket::new(start + i as f64 * bucket_secs as f64))
        .collect();
    let mut total_elapsed: Vec<f64> = Vec::new();
    let mut total_first: Vec<f64> = Vec::new();
    let mut totals = Bucket::new(start);
    let mut models: HashMap<String, (u64, u64, u64, Vec<f64>)> = HashMap::new();
    let mut upstreams: HashMap<String, (u64, u64, u64, Vec<f64>, Vec<f64>)> = HashMap::new();

    let entries = super::journal_index::entries(&state, since, None);
    let mut earliest = f64::MAX;
    let mut latest: f64 = 0.0;

    for e in entries.iter() {
        let at = e.get("at").and_then(|v| v.as_f64()).unwrap_or(0.0);
        if at <= 0.0 || at < since {
            continue;
        }
        earliest = earliest.min(at);
        latest = latest.max(at);
        let ok = e.get("ok").and_then(|v| v.as_bool()).unwrap_or(false);
        let usage = usage_of(e);
        let input = n(&usage, "input");
        let output = n(&usage, "output");
        let cache_read = n(&usage, "cache_read");
        let cache_write = n(&usage, "cache_write");
        let elapsed = e.get("elapsed_seconds").and_then(|v| v.as_f64()).unwrap_or(0.0);
        let first = e
            .get("first_chunk_seconds")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0);
        let model = e
            .get("effective_model")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .or_else(|| e.get("model").and_then(|v| v.as_str()))
            .unwrap_or("")
            .to_string();
        let upstream = e
            .get("upstream")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        let idx = (((at - start) / bucket_secs as f64).floor() as isize)
            .clamp(0, buckets.len() as isize - 1) as usize;
        for b in [&mut buckets[idx], &mut totals] {
            b.calls += 1;
            if ok {
                b.ok += 1;
            } else {
                b.failed += 1;
            }
            b.input += input;
            b.output += output;
            b.cache_read += cache_read;
            b.cache_write += cache_write;
            if elapsed > 0.0 {
                b.elapsed.push(elapsed);
            }
            if first > 0.0 {
                b.first.push(first);
            }
        }
        if elapsed > 0.0 {
            total_elapsed.push(elapsed);
        }
        if first > 0.0 {
            total_first.push(first);
        }
        if !model.is_empty() {
            let m = models
                .entry(model)
                .or_insert_with(|| (0, 0, 0, Vec::new()));
            m.0 += 1;
            m.1 += input;
            m.2 += output;
            if elapsed > 0.0 {
                m.3.push(elapsed);
            }
        }
        if !upstream.is_empty() {
            let u = upstreams
                .entry(upstream)
                .or_insert_with(|| (0, 0, 0, Vec::new(), Vec::new()));
            u.0 += 1;
            if ok {
                u.1 += 1;
            } else {
                u.2 += 1;
            }
            if elapsed > 0.0 {
                u.3.push(elapsed);
            }
            if first > 0.0 {
                u.4.push(first);
            }
        }
    }

    // 固定延迟直方图（秒）：给「大多数请求落在哪」一个直观分布。
    let edges = [0.5_f64, 1.0, 2.0, 5.0, 10.0, 30.0, 60.0];
    let labels = [
        "≤0.5s", "0.5–1s", "1–2s", "2–5s", "5–10s", "10–30s", "30–60s", ">60s",
    ];
    let mut hist = vec![0_u64; labels.len()];
    for v in &total_elapsed {
        let mut idx = edges.len();
        for (i, edge) in edges.iter().enumerate() {
            if *v <= *edge {
                idx = i;
                break;
            }
        }
        hist[idx] += 1;
    }
    let histogram: Vec<Value> = labels
        .iter()
        .enumerate()
        .map(|(i, label)| json!({"label": label, "count": hist[i]}))
        .collect();

    let mut model_rows: Vec<Value> = models
        .into_iter()
        .map(|(model, (calls, input, output, elapsed))| {
            let avg = if elapsed.is_empty() {
                0.0
            } else {
                elapsed.iter().sum::<f64>() / elapsed.len() as f64 * 1000.0
            };
            json!({
                "model": model,
                "calls": calls,
                "input": input,
                "output": output,
                "total": input + output,
                "avg_ms": (avg * 10.0).round() / 10.0,
            })
        })
        .collect();
    model_rows.sort_by_key(|r| -r["calls"].as_i64().unwrap_or(0));

    let mut upstream_rows: Vec<Value> = upstreams
        .into_iter()
        .map(|(upstream, (calls, ok, failed, elapsed, first))| {
            json!({
                "upstream": upstream,
                "calls": calls,
                "ok": ok,
                "failed": failed,
                "success_rate": if calls > 0 { (ok as f64 / calls as f64 * 1000.0).round() / 10.0 } else { 0.0 },
                "p50_ms": percentile_ms(&elapsed, 0.50),
                "p95_ms": percentile_ms(&elapsed, 0.95),
                "first_p50_ms": percentile_ms(&first, 0.50),
            })
        })
        .collect();
    upstream_rows.sort_by_key(|r| -r["calls"].as_i64().unwrap_or(0));

    let totals_json = json!({
        "calls": totals.calls,
        "ok": totals.ok,
        "failed": totals.failed,
        "success_rate": if totals.calls > 0 { (totals.ok as f64 / totals.calls as f64 * 1000.0).round() / 10.0 } else { 0.0 },
        "input": totals.input,
        "output": totals.output,
        "cache_read": totals.cache_read,
        "cache_write": totals.cache_write,
        "p50_ms": percentile_ms(&total_elapsed, 0.50),
        "p95_ms": percentile_ms(&total_elapsed, 0.95),
        "first_p50_ms": percentile_ms(&total_first, 0.50),
    });

    let out = json!({
        "ok": true,
        "generated_at": now.format("%Y-%m-%d %H:%M:%S").to_string(),
        "range": range_label,
        "since": since,
        "bucket_seconds": bucket_secs,
        "buckets": buckets.iter().map(|b| b.json(bucket_secs)).collect::<Vec<_>>(),
        "histogram": histogram,
        "models": model_rows,
        "upstreams": upstream_rows,
        "totals": totals_json,
        "partial": earliest != f64::MAX && earliest > start + bucket_secs as f64,
        "earliest": if earliest == f64::MAX { 0.0 } else { earliest },
        "latest": latest,
    });
    super::collector::put(cache_key, out.clone());
    Json(out).into_response()
}

fn ledger_window(q: &LedgerQuery) -> (f64, String) {
    let now = chrono::Local::now();
    let now_ts = now.timestamp() as f64 + now.time().nanosecond() as f64 / 1_000_000_000.0;
    let mut range = q
        .range
        .as_deref()
        .map(|s| s.trim().to_ascii_lowercase())
        .unwrap_or_default();
    if range.is_empty() && q.hours.is_none() {
        range = "today".to_string();
    }
    match range.as_str() {
        "today" => {
            let secs = now.time().num_seconds_from_midnight() as f64
                + now.time().nanosecond() as f64 / 1_000_000_000.0;
            (now_ts - secs, "today".to_string())
        }
        "all" => (0.0, "all".to_string()),
        _ => {
            let h = q.hours.unwrap_or(24.0).max(0.0);
            if h <= 0.0 {
                (0.0, "all".to_string())
            } else {
                (now_ts - h * 3600.0, format!("{}h", h))
            }
        }
    }
}

/// Prompt Cache 账本：按模型 / 上游 / 会话统计缓存读写与命中率，
/// 同时把粘性路由的会话数带出来，方便对照「固定上游后缓存是否更稳」。
pub async fn cache_ledger(State(state): State<Arc<AppState>>, Query(q): Query<LedgerQuery>) -> Response {
    let (since, range_label) = ledger_window(&q);
    let cache_key = format!("ledger|{}|{}", range_label, q.hours.unwrap_or(0.0));
    if let Some(v) = super::collector::get(&cache_key, 15) {
        return Json(v).into_response();
    }
    let port = {
        let p = state.port.load(std::sync::atomic::Ordering::Relaxed);
        if p == 0 { 8010 } else { p }
    };
    // ledger_window() 返回的是秒；session_cache_ledger() 的 since_ms 需要毫秒，
    // 否则「今天」会把全部历史会话都算进来（最近 100 条也会错）。
    let since_ms = if since <= 0.0 { 0 } else { (since * 1000.0).round() as i64 };
    let mut out = super::sessions::session_cache_ledger(port, since_ms);
    out["ok"] = json!(true);
    out["generated_at"] = json!(chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string());
    out["range"] = json!(range_label);
    out["since"] = json!(since);
    out["affinity_sticks"] = json!(state.affinity.lock().map(|s| s.count()).unwrap_or(0));
    super::collector::put(cache_key, out.clone());
    Json(out).into_response()
}

/// 托盘迷你 HUD 用的今日摘要：只读流水尾部，字段足够画四格小面板。
fn route_view(entry: &Value, bases: &HashMap<String, String>) -> Value {
    let mut routes: Vec<Value> = Vec::new();
    if let Some(trail) = entry.get("trail").and_then(Value::as_array) {
        for t in trail {
            let upstream = t.get("upstream").and_then(Value::as_str).unwrap_or("");
            routes.push(json!({
                "upstream": upstream,
                "base_url": bases.get(upstream).cloned().unwrap_or_default(),
                "model": t.get("model").cloned().unwrap_or(Value::Null),
                "ok": t.get("ok").and_then(Value::as_bool).unwrap_or(false),
                "status": t.get("status").cloned().unwrap_or(Value::Null),
                "elapsed_seconds": t.get("elapsed_seconds").cloned().unwrap_or(Value::Null),
                "error": t.get("error").cloned().unwrap_or(Value::Null),
            }));
        }
    }
    if routes.is_empty() {
        let upstream = entry.get("upstream").and_then(Value::as_str).unwrap_or("");
        routes.push(json!({
            "upstream": upstream,
            "base_url": bases.get(upstream).cloned().unwrap_or_default(),
            "model": entry.get("effective_model").or_else(|| entry.get("model")).cloned().unwrap_or(Value::Null),
            "ok": entry.get("ok").and_then(Value::as_bool).unwrap_or(false),
            "status": entry.get("status").cloned().unwrap_or(Value::Null),
            "elapsed_seconds": entry.get("elapsed_seconds").cloned().unwrap_or(Value::Null),
            "error": entry.get("error").cloned().unwrap_or(Value::Null),
        }));
    }
    json!({
        "at": entry.get("at").cloned().unwrap_or(Value::Null),
        "time": entry.get("time").cloned().unwrap_or(Value::Null),
        "model": entry.get("model").cloned().unwrap_or(Value::Null),
        "effective_model": entry.get("effective_model").cloned().unwrap_or(Value::Null),
        "ok": entry.get("ok").and_then(Value::as_bool).unwrap_or(false),
        "elapsed_seconds": entry.get("elapsed_seconds").cloned().unwrap_or(Value::Null),
        "routes": routes,
    })
}

pub async fn today(State(state): State<Arc<AppState>>) -> Response {
    Json(today_summary(&state)).into_response()
}

pub fn today_summary(state: &AppState) -> Value {
    let now = chrono::Local::now();
    let now_ts = now.timestamp() as f64 + now.time().nanosecond() as f64 / 1_000_000_000.0;
    let secs = now.time().num_seconds_from_midnight() as f64
        + now.time().nanosecond() as f64 / 1_000_000_000.0;
    let since = now_ts - secs;
    let port = {
        let p = state.port.load(std::sync::atomic::Ordering::Relaxed);
        if p == 0 { 8010 } else { p }
    };
    let totals = super::sessions::today_totals(port);
    let session_total = totals.total;
    let session_routed = totals.routed;

    let mut calls: u64 = 0;
    let mut ok: u64 = 0;
    let mut failed: u64 = 0;
    let mut input: u64 = 0;
    let mut output: u64 = 0;
    let mut cache_read: u64 = 0;
    let mut cache_write: u64 = 0;
    let mut elapsed: Vec<f64> = Vec::new();
    let mut first: Vec<f64> = Vec::new();
    let bases: HashMap<String, String> = state
        .file
        .read()
        .map(|f| {
            f.upstreams
                .iter()
                .map(|u| (u.name.clone(), u.base_url.clone()))
                .collect()
        })
        .unwrap_or_default();
    let entries = super::journal_index::entries(state, 0.0, None);
    let route_entries: Vec<&Value> = entries
        .iter()
        .filter(|e| e.get("at").and_then(Value::as_f64).unwrap_or(0.0) > 0.0)
        .collect();
    let last_route = route_entries
        .last()
        .map(|e| route_view(e, &bases))
        .unwrap_or(Value::Null);
    let prev_route = if route_entries.len() >= 2 {
        route_view(route_entries[route_entries.len() - 2], &bases)
    } else {
        Value::Null
    };
    let latest = super::sessions::last_round(port);
    let route = if latest.is_null() { last_route.clone() } else { latest };

    for e in entries.iter() {
        let at = e.get("at").and_then(|v| v.as_f64()).unwrap_or(0.0);
        if at <= 0.0 || at < since {
            continue;
        }
        calls += 1;
        if e.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
            ok += 1;
        } else {
            failed += 1;
        }
        let usage = usage_of(e);
        input += n(&usage, "input");
        output += n(&usage, "output");
        cache_read += n(&usage, "cache_read");
        cache_write += n(&usage, "cache_write");
        if let Some(v) = e.get("elapsed_seconds").and_then(|v| v.as_f64()) {
            if v > 0.0 {
                elapsed.push(v);
            }
        }
        if let Some(v) = e.get("first_chunk_seconds").and_then(|v| v.as_f64()) {
            if v > 0.0 {
                first.push(v);
            }
        }
    }

    let session_extra = session_total.saturating_sub(session_routed);
    let all_calls = calls + session_extra;
    let all_input = input + totals.input.saturating_sub(totals.routed_input);
    let all_output = output + totals.output.saturating_sub(totals.routed_output);
    let all_cache_read = cache_read + totals.cache_read.saturating_sub(totals.routed_cache_read);
    let all_cache_write = cache_write + totals.cache_write.saturating_sub(totals.routed_cache_write);
    let all_ok = session_extra + ok;
    let all_failed = failed;
    let mut all_elapsed = elapsed.clone();
    all_elapsed.extend(totals.durations.iter().copied());
    json!({
        "all_calls": all_calls,
        "all_ok": all_ok,
        "all_failed": all_failed,
        "all_success_rate": if all_calls > 0 { (all_ok as f64 / all_calls as f64 * 1000.0).round() / 10.0 } else { 0.0 },
        "all_input": all_input,
        "all_output": all_output,
        "all_cache_read": all_cache_read,
        "all_cache_write": all_cache_write,
        "all_p50_ms": percentile_ms(&all_elapsed, 0.50),
        "all_p95_ms": percentile_ms(&all_elapsed, 0.95),
        "routed_calls": calls,
        "calls": calls,
        "ok": ok,
        "failed": failed,
        "success_rate": if calls > 0 { (ok as f64 / calls as f64 * 1000.0).round() / 10.0 } else { 0.0 },
        "input": input,
        "output": output,
        "cache_read": cache_read,
        "cache_write": cache_write,
        "p50_ms": percentile_ms(&elapsed, 0.50),
        "p95_ms": percentile_ms(&elapsed, 0.95),
        "first_p50_ms": percentile_ms(&first, 0.50),
        "last": last_route,
        "prev": prev_route,
        "route": route,
        "updated": now.format("%H:%M:%S").to_string(),
    })
}
