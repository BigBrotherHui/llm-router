use serde_json::{json, Map, Value};

const USAGE_KEYS: [&str; 5] = ["input", "output", "total", "cache_read", "cache_write"];

fn num(v: Option<&Value>) -> i64 {
    v.and_then(|x| x.as_i64()).unwrap_or(0)
}

/// 上游 usage 字段百花齐放：OpenAI 原生 prompt_tokens / completion_tokens、
/// Anthropic 的 cache_creation_input_tokens + claude_cache_creation_5_m_tokens、
/// 中转站塞进 billing_usage.claude_usage.*、opencode 塞进 opencode.tokens.*。
/// 统一归一成 5 个键，否则「缓存读/写」永远显示 0 或 —。
fn as_num(v: Option<&Value>) -> i64 {
    match v {
        Some(Value::Number(n)) => n
            .as_i64()
            .unwrap_or_else(|| n.as_f64().unwrap_or(0.0) as i64),
        Some(Value::String(s)) => s.parse::<f64>().unwrap_or(0.0) as i64,
        _ => 0,
    }
}

fn at_path<'a>(usage: &'a Value, path: &[&str]) -> Option<&'a Value> {
    let mut cur = usage;
    for seg in path {
        cur = cur.get(*seg)?;
    }
    Some(cur)
}

/// 别名组：取第一个非零值（同一个量的多种写法，不能相加）。
fn first_of(usage: &Value, paths: &[&[&str]]) -> i64 {
    for p in paths {
        let n = as_num(at_path(usage, p));
        if n != 0 {
            return n;
        }
    }
    0
}

/// 可叠加的多桶（1h / 5m 缓存写、多个 creation 明细）要相加。
fn sum_of(usage: &Value, paths: &[&[&str]]) -> i64 {
    paths.iter().map(|p| as_num(at_path(usage, p))).sum()
}

pub fn normalize_usage(raw: &Value) -> Value {
    let input = first_of(
        raw,
        &[
            &["input"],
            &["prompt_tokens"],
            &["input_tokens"],
            &["billing_usage", "claude_usage", "input_tokens"],
            &["opencode", "tokens", "input"],
        ],
    );
    let output = first_of(
        raw,
        &[
            &["output"],
            &["completion_tokens"],
            &["output_tokens"],
            &["billing_usage", "claude_usage", "output_tokens"],
            &["opencode", "tokens", "output"],
        ],
    );
    let total_raw = first_of(raw, &[&["total"], &["total_tokens"]]);
    let total = if total_raw != 0 { total_raw } else { input + output };
    let cache_read = first_of(
        raw,
        &[
            &["cache_read"],
            &["cache_read_input_tokens"],
            &["prompt_tokens_details", "cached_tokens"],
            &["input_tokens_details", "cached_tokens"],
            &["prompt_cache_hit_tokens"],
            &["cached_tokens"],
            &["billing_usage", "claude_usage", "cache_read_input_tokens"],
            &["opencode", "tokens", "cache_read"],
        ],
    );
    let cache_write = first_of(
        raw,
        &[
            &["cache_write"],
            &["cache_creation_input_tokens"],
            &["prompt_cache_miss_tokens"],
        ],
    ) + sum_of(
        raw,
        &[
            &["billing_usage", "claude_usage", "cache_creation_input_tokens"],
            &["claude_cache_creation_1_h_tokens"],
            &["claude_cache_creation_5_m_tokens"],
            &["prompt_tokens_details", "cache_creation_tokens"],
            &["cache_creation", "ephemeral_1h_input_tokens"],
            &["cache_creation", "ephemeral_5m_input_tokens"],
            &["opencode", "tokens", "cache_write_1h"],
            &["opencode", "tokens", "cache_write_5m"],
        ],
    );
    json!({
        "input": input,
        "output": output,
        "total": total,
        "cache_read": cache_read,
        "cache_write": cache_write,
    })
}

/// 写进流水时保留上游原始字段，同时补上 5 个标准键（覆盖同名字段），
/// 这样前端 usageCells 和统计累计都读同一套键，不会一半显示、一半「—」。
pub fn with_normalized_usage(raw: &Value) -> Value {
    let mut v = raw.as_object().cloned().unwrap_or_default();
    let n = normalize_usage(raw);
    for k in USAGE_KEYS {
        v.insert(k.to_string(), n[k].clone());
    }
    Value::Object(v)
}

fn float(v: Option<&Value>) -> f64 {
    v.and_then(|x| x.as_f64()).unwrap_or(0.0)
}

fn usage_of(bucket: &Value) -> Value {
    // 5 个键全给，0 也写明：前端 fmtNum(0) 显示 0，而不是缺字段显示「—」，
    // 用户才分得清「上游没报」和「真的没缓存命中」。
    let mut out = Map::new();
    for k in USAGE_KEYS {
        out.insert(k.to_string(), json!(num(bucket.get(k))));
    }
    Value::Object(out)
}

fn light(bucket: &Value) -> Value {
    let mut out = Map::new();
    for k in ["calls", "ok", "failed", "attempts"] {
        out.insert(k.to_string(), json!(num(bucket.get(k))));
    }
    for k in USAGE_KEYS {
        out.insert(k.to_string(), json!(num(bucket.get(k))));
    }
    if bucket.get("attempts").is_none() {
        let attempts = bucket
            .get("upstreams")
            .and_then(|v| v.as_object())
            .map(|m| m.values().map(|u| num(u.get("calls"))).sum::<i64>())
            .unwrap_or(0);
        out.insert("attempts".to_string(), json!(attempts));
    }
    Value::Object(out)
}

pub fn render(bucket: &Value, range: &str) -> Value {
    let empty = json!({});
    let models = bucket.get("models").and_then(|v| v.as_object()).unwrap_or_else(|| empty.as_object().unwrap());
    let mut groups: Vec<Value> = Vec::new();
    for (name, m) in models {
        let mut ups: Vec<Value> = Vec::new();
        if let Some(upmap) = m.get("upstreams").and_then(|v| v.as_object()) {
            for (un, u) in upmap {
                let mut sub_models: Vec<Value> = Vec::new();
                if let Some(mm) = u.get("models").and_then(|v| v.as_object()) {
                    for (mn, mb) in mm {
                        sub_models.push(json!({
                            "model": mn,
                            "calls": num(mb.get("calls")),
                            "ok": num(mb.get("ok")),
                            "failed": num(mb.get("failed")),
                            "usage": usage_of(mb),
                        }));
                    }
                    sub_models.sort_by_key(|x| -x["calls"].as_i64().unwrap_or(0));
                }
                ups.push(json!({
                    "upstream": un,
                    "calls": num(u.get("calls")),
                    "ok": num(u.get("ok")),
                    "failed": num(u.get("failed")),
                    "usage": usage_of(u),
                    "models": sub_models,
                }));
            }
        }
        ups.sort_by_key(|x| -x["calls"].as_i64().unwrap_or(0));
        let calls = num(m.get("calls"));
        groups.push(json!({
            "group": name,
            "calls": calls,
            "ok": num(m.get("ok")),
            "failed": num(m.get("failed")),
            "attempts": ups.iter().map(|u| num(u.get("calls"))).sum::<i64>(),
            "avg_elapsed": if calls > 0 { (float(m.get("elapsed")) / calls as f64 * 1000.0).round() / 1000.0 } else { 0.0 },
            "usage": usage_of(m),
            "upstreams": ups,
            "upstream_count": ups.len(),
        }));
    }
    groups.sort_by_key(|x| -x["calls"].as_i64().unwrap_or(0));

    let mut ups: Vec<Value> = Vec::new();
    if let Some(upmap) = bucket.get("upstreams").and_then(|v| v.as_object()) {
        for (n, u) in upmap {
            ups.push(json!({
                "upstream": n,
                "calls": num(u.get("calls")),
                "ok": num(u.get("ok")),
                "failed": num(u.get("failed")),
            }));
        }
    }
    ups.sort_by_key(|x| -x["calls"].as_i64().unwrap_or(0));
    let mut totals = light(bucket);
    totals["attempts"] = json!(ups.iter().map(|u| num(u.get("calls"))).sum::<i64>());
    json!({
        "groups": groups,
        "totals": totals,
        "upstreams": ups,
        "history_limit": 500,
        "range": range,
    })
}

pub fn load(path: &std::path::Path) -> Value {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .unwrap_or_else(|| json!({"totals": {}, "days": {}}))
}

fn merge_bucket(dst: &mut Value, src: &Value) {
    for key in ["calls", "ok", "failed", "attempts", "input", "output", "total", "cache_read", "cache_write"] {
        let a = num(dst.get(key));
        let b = num(src.get(key));
        dst[key] = json!(a + b);
    }
    dst["elapsed"] = json!(float(dst.get("elapsed")) + float(src.get("elapsed")));
    for nest in ["upstreams", "models"] {
        if let Some(src_map) = src.get(nest).and_then(|v| v.as_object()) {
            if dst.get(nest).is_none() {
                dst[nest] = json!({});
            }
            for (name, sub) in src_map {
                let entry = dst[nest]
                    .as_object_mut()
                    .unwrap()
                    .entry(name.clone())
                    .or_insert_with(|| json!({}));
                merge_bucket(entry, sub);
            }
        }
    }
}

pub fn range_bucket(stats: &Value, range: &str) -> Value {
    let days = stats.get("days").and_then(|v| v.as_object());
    if let Some(day) = range.strip_prefix("day:") {
        return days
            .and_then(|m| m.get(day))
            .cloned()
            .unwrap_or_else(|| json!({}));
    }
    if let Some(month) = range.strip_prefix("month:") {
        let mut merged = json!({});
        if let Some(m) = days {
            for (d, b) in m {
                if d.starts_with(month) {
                    merge_bucket(&mut merged, b);
                }
            }
        }
        return merged;
    }
    stats.get("totals").cloned().unwrap_or_else(|| json!({}))
}

/// 原生代理每完成一次客户端请求就更新 stats.json（与旧 Python 版同一份结构）。
pub fn record(state: &super::state::AppState, entry: &Value) {
    let _guard = state.stats_lock.lock().unwrap();
    let at = entry
        .get("at")
        .and_then(|v| v.as_f64())
        .unwrap_or_else(|| chrono::Utc::now().timestamp() as f64);
    let ok = entry.get("ok").and_then(|v| v.as_bool()).unwrap_or(false);
    let model = entry.get("model").and_then(|v| v.as_str()).unwrap_or("");
    let upstream = entry.get("upstream").and_then(|v| v.as_str()).unwrap_or("");
    let elapsed = entry
        .get("elapsed_seconds")
        .and_then(|v| v.as_f64())
        .unwrap_or(0.0);
    // 流水里的 usage 可能是上游原始格式，先归一成 5 键再累计。
    let raw_usage = entry.get("usage").cloned().unwrap_or_else(|| json!({}));
    let usage = normalize_usage(&raw_usage);
    let trail = entry
        .get("trail")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let attempts = trail.len().max(
        entry
            .get("attempts")
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as usize,
    );

    let mut delta = json!({
        "calls": 1,
        "ok": if ok { 1 } else { 0 },
        "failed": if ok { 0 } else { 1 },
        "attempts": attempts,
        "elapsed": elapsed,
        "upstreams": {},
        "models": {},
    });
    copy_usage(&mut delta, &usage);

    let mut model_bucket = json!({
        "calls": 1,
        "ok": if ok { 1 } else { 0 },
        "failed": if ok { 0 } else { 1 },
        "attempts": attempts,
        "elapsed": elapsed,
        "upstreams": {},
    });
    copy_usage(&mut model_bucket, &usage);

    let items: Vec<Value> = if trail.is_empty() {
        vec![json!({
            "upstream": upstream, "model": entry.get("effective_model").and_then(|v| v.as_str()).unwrap_or(model),
            "ok": ok, "elapsed_seconds": elapsed,
        })]
    } else {
        trail
    };
    for item in items {
        let un = item.get("upstream").and_then(|v| v.as_str()).unwrap_or("");
        let real = item.get("model").and_then(|v| v.as_str()).unwrap_or(model);
        if un.is_empty() {
            continue;
        }
        let item_ok = item.get("ok").and_then(|v| v.as_bool()).unwrap_or(false);
        let item_elapsed = item
            .get("elapsed_seconds")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0);
        let attempt = json!({
            "calls": 1,
            "ok": if item_ok { 1 } else { 0 },
            "failed": if item_ok { 0 } else { 1 },
            "elapsed": item_elapsed,
            "models": {
                real: {
                    "calls": 1,
                    "ok": if item_ok { 1 } else { 0 },
                    "failed": if item_ok { 0 } else { 1 },
                    "elapsed": item_elapsed,
                }
            }
        });
        merge_bucket(child(child(&mut delta, "upstreams"), un), &attempt);
        merge_bucket(child(child(&mut model_bucket, "upstreams"), un), &attempt);
    }
    child(&mut delta, "models")[model] = model_bucket;

    let mut stats = load_runtime(state);
    if !stats.is_object() {
        stats = json!({});
    }
    let mut totals = stats.get("totals").cloned().unwrap_or_else(|| json!({}));
    merge_bucket(&mut totals, &delta);
    stats["totals"] = totals;

    let day = chrono::DateTime::from_timestamp(at as i64, 0)
        .map(|dt| dt.with_timezone(&chrono::Local).format("%Y-%m-%d").to_string())
        .unwrap_or_default();
    if !day.is_empty() {
        if !stats.get("days").map(|v| v.is_object()).unwrap_or(false) {
            stats["days"] = json!({});
        }
        let mut day_bucket = stats["days"]
            .get(&day)
            .cloned()
            .unwrap_or_else(|| json!({}));
        merge_bucket(&mut day_bucket, &delta);
        stats["days"][day] = day_bucket;
    }

    if let Ok(mut mem) = state.stats_mem.lock() {
        *mem = Some(stats);
    }
    state.stats_dirty.store(true, std::sync::atomic::Ordering::Relaxed);
}

/// 读运行时统计（内存优先），没有就加载 stats.json 并缓存。
pub fn load_runtime(state: &super::state::AppState) -> Value {
    if let Ok(mem) = state.stats_mem.lock() {
        if let Some(v) = mem.as_ref() {
            return v.clone();
        }
    }
    let loaded = load(&state.stats_path());
    if let Ok(mut mem) = state.stats_mem.lock() {
        *mem = Some(loaded.clone());
    }
    loaded
}

/// 定时把内存统计flush到 stats.json（5 秒一次，由 serve 里的后台任务调用）。
pub fn flush(state: &super::state::AppState) {
    if !state
        .stats_dirty
        .swap(false, std::sync::atomic::Ordering::Relaxed)
    {
        return;
    }
    let Some(stats) = state.stats_mem.lock().ok().and_then(|m| m.clone()) else {
        return;
    };
    let path = state.stats_path();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(text) = serde_json::to_string_pretty(&stats) {
        let tmp = path.with_extension("json.tmp");
        if std::fs::write(&tmp, text).is_ok() {
            let _ = std::fs::rename(&tmp, &path);
        } else {
            state
                .stats_dirty
                .store(true, std::sync::atomic::Ordering::Relaxed);
        }
    }
}

fn child<'a>(value: &'a mut Value, key: &str) -> &'a mut Value {
    if !value.is_object() {
        *value = json!({});
    }
    let obj = value.as_object_mut().unwrap();
    obj.entry(key.to_string()).or_insert_with(|| json!({}));
    obj.get_mut(key).unwrap()
}

fn usage_num(usage: &Value, key: &str) -> Option<i64> {
    let aliases: &[&str] = match key {
        "input" => &["input", "prompt_tokens"],
        "output" => &["output", "completion_tokens"],
        "total" => &["total", "total_tokens"],
        "cache_read" => &["cache_read", "cache_read_input_tokens"],
        "cache_write" => &["cache_write", "cache_creation_input_tokens"],
        _ => &[],
    };
    for alias in aliases {
        if let Some(n) = usage.get(*alias).and_then(|v| v.as_i64()) {
            return Some(n);
        }
    }
    if key == "cache_read" {
        return usage
            .get("prompt_tokens_details")
            .and_then(|d| d.get("cached_tokens"))
            .and_then(|v| v.as_i64());
    }
    None
}

fn copy_usage(dst: &mut Value, usage: &Value) {
    for k in USAGE_KEYS {
        if let Some(n) = usage_num(usage, k) {
            if n != 0 {
                dst[k] = json!(n);
            }
        }
    }
}

pub fn meta(stats: &Value) -> Value {    let days: Vec<String> = stats
        .get("days")
        .and_then(|v| v.as_object())
        .map(|m| m.keys().cloned().collect())
        .unwrap_or_default();
    let mut days_sorted = days.clone();
    days_sorted.sort_by(|a, b| b.cmp(a));
    let mut months: Vec<String> = days.iter().map(|d| d.chars().take(7).collect()).collect();
    months.sort();
    months.dedup();
    months.sort_by(|a, b| b.cmp(a));
    json!({
        "days": days_sorted,
        "months": months,
        "totals": light(stats.get("totals").unwrap_or(&json!({}))),
    })
}
