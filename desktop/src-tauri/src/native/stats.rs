use serde_json::{json, Map, Value};

const USAGE_KEYS: [&str; 5] = ["input", "output", "total", "cache_read", "cache_write"];

fn num(v: Option<&Value>) -> i64 {
    v.and_then(|x| x.as_i64()).unwrap_or(0)
}

fn float(v: Option<&Value>) -> f64 {
    v.and_then(|x| x.as_f64()).unwrap_or(0.0)
}

fn usage_of(bucket: &Value) -> Value {
    let mut out = Map::new();
    for k in USAGE_KEYS {
        let v = num(bucket.get(k));
        if v != 0 {
            out.insert(k.to_string(), json!(v));
        }
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

pub fn meta(stats: &Value) -> Value {
    let days: Vec<String> = stats
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
