//! 上游健康看板：从落盘流水聚合成功率 / P50 / P95 / 首字延迟 / 冷却倒计时，
//! 并把汇总按模型拆成明细，另提供手动清除熔断。数据全部来自最近的流水尾部缓存，不额外落盘。

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use chrono::Timelike;
use serde_json::{json, Value};

use super::state::AppState;

fn ok(v: Value) -> Response {
    Json(v).into_response()
}

fn bad(msg: &str) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({ "ok": false, "error": msg })),
    )
        .into_response()
}

fn f64_of(v: Option<&Value>) -> Option<f64> {
    v.and_then(|x| x.as_f64())
}

fn pct(sorted: &[f64], p: f64) -> Option<f64> {
    if sorted.is_empty() {
        return None;
    }
    let idx = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    Some(sorted[idx.min(sorted.len() - 1)])
}

fn round3(v: f64) -> f64 {
    (v * 1000.0).round() / 1000.0
}

#[derive(Default)]
struct Agg {
    calls: i64,
    ok: i64,
    failed: i64,
    attempts: i64,
    stream: i64,
    cache_read: i64,
    cache_write: i64,
    input: i64,
    output: i64,
    latencies: Vec<f64>,
    first_chunks: Vec<f64>,
    last_error: String,
    last_at: f64,
    by_model: BTreeMap<String, Agg>,
}

impl Agg {
    /// 把另一个聚合的计数与分位样本并进来（用于总览）。
    fn merge(&mut self, o: &Agg) {
        self.calls += o.calls;
        self.ok += o.ok;
        self.failed += o.failed;
        self.attempts += o.attempts;
        self.stream += o.stream;
        self.cache_read += o.cache_read;
        self.cache_write += o.cache_write;
        self.input += o.input;
        self.output += o.output;
        self.latencies.extend_from_slice(&o.latencies);
        self.first_chunks.extend_from_slice(&o.first_chunks);
        if o.last_at > self.last_at {
            self.last_at = o.last_at;
        }
    }

    fn add(&mut self, e: &Value, model: &str) {
        self.calls += 1;
        if e.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
            self.ok += 1;
        } else {
            self.failed += 1;
            if self.last_error.is_empty() {
                self.last_error = e
                    .get("error")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .chars()
                    .take(200)
                    .collect();
            }
        }
        self.attempts += e
            .get("attempts")
            .and_then(|v| v.as_i64())
            .unwrap_or(1)
            .max(1);
        if e.get("stream").and_then(|v| v.as_bool()).unwrap_or(false) {
            self.stream += 1;
        }
        let u = e.get("usage");
        self.cache_read += u
            .and_then(|x| x.get("cache_read"))
            .and_then(|x| x.as_i64())
            .unwrap_or(0);
        self.cache_write += u
            .and_then(|x| x.get("cache_write"))
            .and_then(|x| x.as_i64())
            .unwrap_or(0);
        self.input += u
            .and_then(|x| x.get("input"))
            .and_then(|x| x.as_i64())
            .unwrap_or(0);
        self.output += u
            .and_then(|x| x.get("output"))
            .and_then(|x| x.as_i64())
            .unwrap_or(0);
        if let Some(t) = f64_of(e.get("elapsed_seconds")) {
            if t.is_finite() && t >= 0.0 {
                self.latencies.push(t);
            }
        }
        if let Some(t) = f64_of(e.get("first_chunk_seconds")) {
            if t.is_finite() && t >= 0.0 {
                self.first_chunks.push(t);
            }
        }
        let at = e.get("at").and_then(|v| v.as_f64()).unwrap_or(0.0);
        if at > self.last_at {
            self.last_at = at;
        }
        if !model.is_empty() {
            self.by_model.entry(model.to_string()).or_default().add(e, "");
        }
    }

    fn json(&self) -> Value {
        let mut lat = self.latencies.clone();
        lat.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let mut fc = self.first_chunks.clone();
        fc.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let models: Vec<Value> = self
            .by_model
            .iter()
            .map(|(name, a)| {
                let mut v = a.json();
                if let Some(obj) = v.as_object_mut() {
                    obj.insert("model".to_string(), json!(name));
                    obj.remove("models");
                }
                v
            })
            .collect();
        let success_rate = if self.calls > 0 {
            Some(round3(self.ok as f64 / self.calls as f64 * 100.0))
        } else {
            None
        };
        json!({
            "calls": self.calls,
            "ok": self.ok,
            "failed": self.failed,
            "attempts": self.attempts,
            "stream": self.stream,
            "success_rate": success_rate,
            "p50_seconds": pct(&lat, 0.50).map(round3),
            "p95_seconds": pct(&lat, 0.95).map(round3),
            "p99_seconds": pct(&lat, 0.99).map(round3),
            "max_seconds": lat.last().copied().map(round3),
            "first_chunk_p50_seconds": pct(&fc, 0.50).map(round3),
            "first_chunk_p95_seconds": pct(&fc, 0.95).map(round3),
            "cache_read": self.cache_read,
            "cache_write": self.cache_write,
            "input": self.input,
            "output": self.output,
            "last_error": if self.last_error.is_empty() { Value::Null } else { json!(self.last_error) },
            "last_at": if self.last_at > 0.0 { json!(self.last_at) } else { Value::Null },
            "models": models,
        })
    }
}

/// GET /health/upstreams?hours=24&limit=4000
/// hours=0 表示不按时间过滤（仅受尾部缓存 8MB 限制）。
pub async fn report(
    State(state): State<Arc<AppState>>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let hours: f64 = q
        .get("hours")
        .and_then(|s| s.parse::<f64>().ok())
        .unwrap_or(24.0)
        .max(0.0);
    let has_hours = q.contains_key("hours");
    let mut range = q
        .get("range")
        .map(|s| s.trim().to_ascii_lowercase())
        .unwrap_or_default();
    // 默认看「今天」：既没传 range 也没传 hours 时按本机时区当天 00:00 起算。
    if range.is_empty() && !has_hours {
        range = "today".to_string();
    }
    let limit: usize = q
        .get("limit")
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(4000)
        .min(20000);
    // range=today 按本机时区当天 00:00 起算；range=all 不按时间过滤；否则用滚动小时窗。
    let (cutoff, range_label) = match range.as_str() {
        "today" => {
            let now = chrono::Local::now();
            let secs = now.time().num_seconds_from_midnight() as f64
                + now.time().nanosecond() as f64 / 1_000_000_000.0;
            (now.timestamp() as f64 - secs, "today".to_string())
        }
        "all" => (0.0, "all".to_string()),
        _ => (
            if hours > 0.0 {
                chrono::Utc::now().timestamp() as f64 - hours * 3600.0
            } else {
                0.0
            },
            format!("{hours}h"),
        ),
    };

    let entries = super::journal::recent(&state);
    let mut scanned = 0usize;
    let mut kept = 0usize;
    let mut by_upstream: BTreeMap<String, Agg> = BTreeMap::new();
    // 倒序遍历：尾部缓存是时间正序，倒着取最近的 limit 条即可。
    for e in entries.iter().rev() {
        scanned += 1;
        let at = e.get("at").and_then(|v| v.as_f64()).unwrap_or(0.0);
        if at > 0.0 && at < cutoff {
            continue;
        }
        if kept >= limit {
            break;
        }
        kept += 1;
        // 每次尝试都算一次上游调用：trail 里逐条聚合，成功率按尝试粒度更真实。
        if let Some(trail) = e.get("trail").and_then(|v| v.as_array()) {
            if !trail.is_empty() {
                // 整条请求的 usage / 首字只属于「真正把这次请求服务完」的那次尝试（最后一次成功，
                // 全失败时取最后一次），否则按尝试平摊会把缓存读写算丢。
                let mut served = trail.len() - 1;
                for (i, t) in trail.iter().enumerate() {
                    if t.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
                        served = i;
                    }
                }
                for (i, t) in trail.iter().enumerate() {
                    let name = t
                        .get("upstream")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let model = t
                        .get("model")
                        .and_then(|v| v.as_str())
                        .unwrap_or_else(|| e.get("model").and_then(|v| v.as_str()).unwrap_or(""));
                    let mut one = json!({
                        "ok": t.get("ok").and_then(|v| v.as_bool()).unwrap_or(false),
                        "attempts": 1,
                        "error": t.get("error").cloned().unwrap_or(Value::Null),
                        "elapsed_seconds": t.get("elapsed_seconds").cloned().unwrap_or(Value::Null),
                        "at": e.get("at").cloned().unwrap_or(Value::Null),
                    });
                    if t.get("status").and_then(|v| v.as_u64()).unwrap_or(200) >= 400 {
                        one["ok"] = json!(false);
                    }
                    if i == served {
                        one["usage"] = e.get("usage").cloned().unwrap_or(Value::Null);
                        one["first_chunk_seconds"] = e
                            .get("first_chunk_seconds")
                            .cloned()
                            .unwrap_or(Value::Null);
                        one["stream"] = e.get("stream").cloned().unwrap_or(Value::Null);
                    }
                    by_upstream.entry(name.clone()).or_default().add(&one, model);
                }
                continue;
            }
        }
        let name = e
            .get("upstream")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let model = e.get("model").and_then(|v| v.as_str()).unwrap_or("");
        by_upstream.entry(name).or_default().add(e, model);
    }

    // 总览用合并后的聚合（分位数要基于原始样本，不能从每行的 P50 再平均）。
    let mut total_agg = Agg::default();
    for a in by_upstream.values() {
        total_agg.merge(a);
    }

    let file = state.file.read().unwrap();
    let mut rows: Vec<Value> = Vec::new();
    // 所有已配置上游都要出现，哪怕这段时间没有流水。
    for u in &file.upstreams {
        let mut base = by_upstream.remove(&u.name).unwrap_or_default().json();
        let failed = super::state::upstream_failure_count(&state, &u.id);
        let cooling = super::state::upstream_cooling_remaining(&state, &u.id);
        let last_err = super::state::upstream_last_error(&state, &u.id);
        if let Some(obj) = base.as_object_mut() {
            obj.insert("id".to_string(), json!(u.id));
            obj.insert("upstream".to_string(), json!(u.name));
            obj.insert("base_url".to_string(), json!(u.base_url));
            obj.insert("enabled".to_string(), json!(u.enabled));
            obj.insert("available".to_string(), json!(super::state::upstream_available(&state, &u.id)));
            obj.insert("consecutive_failures".to_string(), json!(failed));
            obj.insert("cooldown_remaining_seconds".to_string(), json!(cooling));
            obj.insert("last_error_runtime".to_string(), if last_err.is_empty() { Value::Null } else { json!(last_err) });
            obj.insert("key_count".to_string(), json!(u.api_keys.len()));
            obj.insert("selected_count".to_string(), json!(u.selected().len()));
        }
        rows.push(base);
    }
    // 流水里出现但配置已删除的上游也列出来，标记 orphan。
    for (name, agg) in by_upstream {
        let mut v = agg.json();
        if let Some(obj) = v.as_object_mut() {
            obj.insert("id".to_string(), Value::Null);
            obj.insert("upstream".to_string(), json!(name));
            obj.insert("enabled".to_string(), json!(false));
            obj.insert("available".to_string(), json!(true));
            obj.insert("orphan".to_string(), json!(true));
        }
        rows.push(v);
    }
    rows.sort_by(|a, b| {
        let af = a.get("failed").and_then(|v| v.as_i64()).unwrap_or(0);
        let bf = b.get("failed").and_then(|v| v.as_i64()).unwrap_or(0);
        bf.cmp(&af).then_with(|| {
            let ac = a.get("calls").and_then(|v| v.as_i64()).unwrap_or(0);
            let bc = b.get("calls").and_then(|v| v.as_i64()).unwrap_or(0);
            bc.cmp(&ac)
        })
    });

    ok(json!({
        "ok": true,
        "hours": hours,
        "range": range_label,
        "since": if cutoff > 0.0 { json!(cutoff) } else { Value::Null },
        "scanned": scanned,
        "kept": kept,
        "generated_at": chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
        "totals": total_agg.json(),
        "threshold": super::state::FAILURE_THRESHOLD,
        "cooldown_seconds": super::state::COOLDOWN_SECONDS,
        "upstreams": rows,
    }))
}

/// POST /health/upstreams/clear  body: {"id": "..."} 或 {"all": true}
/// 清掉连续失败计数与冷却，让被熔断的上游立刻重新参与路由。
pub async fn clear(State(state): State<Arc<AppState>>, Json(p): Json<Value>) -> Response {
    let all = p.get("all").and_then(|v| v.as_bool()).unwrap_or(false);
    let id = p.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
    if !all && id.is_empty() {
        return bad("需要 id（上游 id）或 all: true");
    }
    let mut cleared: Vec<String> = Vec::new();
    let mut health = state.health.lock().unwrap();
    if all {
        cleared = health.keys().cloned().collect();
        health.clear();
    } else if let Some(h) = health.get_mut(&id) {
        h.consecutive_failures = 0;
        h.cooldown_until = 0;
        h.last_error.clear();
        cleared.push(id.clone());
    }
    drop(health);
    let _ = state.events_tx.send(json!({ "type": "state", "reason": "health_clear" }));
    ok(json!({ "ok": true, "cleared": cleared }))
}
