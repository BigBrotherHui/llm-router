use std::sync::Arc;

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use serde_json::{json, Map, Value};

use super::state::{AppState, Upstream};

const CHECKIN_PATHS: [&str; 3] = [
    "/api/user/self/checkin",
    "/api/user/checkin",
    "/api/v1/user/check-in",
];

pub fn site_root(base_url: &str) -> String {
    let raw = base_url.trim();
    let with_scheme = if raw.starts_with("http://") || raw.starts_with("https://") {
        raw.to_string()
    } else {
        format!("https://{raw}")
    };
    match reqwest::Url::parse(&with_scheme) {
        Ok(u) => {
            let host = u.host_str().unwrap_or("");
            let port = u.port().map(|p| format!(":{p}")).unwrap_or_default();
            format!("{}://{}{}", u.scheme(), host, port)
        }
        Err(_) => raw.trim_end_matches('/').to_string(),
    }
}

fn checkin_cfg(up: &Upstream) -> Map<String, Value> {
    up.extra
        .get("checkin")
        .and_then(|v| v.as_object())
        .cloned()
        .unwrap_or_default()
}

fn token_of(up: &Upstream) -> String {
    checkin_cfg(up)
        .get("token")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string()
}

fn jwt_exp(token: &str) -> Option<i64> {
    let payload = token.split('.').nth(1)?;
    let bytes = URL_SAFE_NO_PAD.decode(payload).ok()?;
    let value: Value = serde_json::from_slice(&bytes).ok()?;
    value.get("exp").and_then(|v| v.as_i64())
}

fn token_expired(token: &str) -> bool {
    jwt_exp(token)
        .map(|exp| exp < chrono::Utc::now().timestamp())
        .unwrap_or(false)
}

fn state_of(up: &Upstream) -> Value {
    up.extra
        .get("checkin_state")
        .cloned()
        .unwrap_or(Value::Null)
}

pub fn rows(state: &AppState) -> Vec<Value> {
    let file = state.file.read().unwrap();
    file.upstreams
        .iter()
        .map(|up| {
            let cfg = checkin_cfg(up);
            let token = token_of(up);
            let enabled = cfg.get("enabled").and_then(|v| v.as_bool()).unwrap_or(false);
            json!({
                "id": up.id,
                "name": up.name,
                "base_url": up.base_url,
                "site_root": site_root(&up.base_url),
                "enabled": up.enabled,
                "configured": !token.is_empty(),
                "checkin_enabled": enabled,
                "token_hint": if token.is_empty() { String::new() } else { super::state::mask_key(&token) },
                "token_expires_at": jwt_exp(&token),
                "token_expired": token_expired(&token),
                "user_id": cfg.get("user_id").cloned().unwrap_or_else(|| json!("")),
                "path": cfg.get("path").cloned().unwrap_or_else(|| json!("")),
                "token_looks_wrong": token.to_ascii_lowercase().starts_with("sk-"),
                "state": state_of(up),
            })
        })
        .collect()
}

pub fn has_targets(state: &AppState) -> bool {
    let file = state.file.read().unwrap();
    file.upstreams.iter().any(|u| {
        !token_of(u).is_empty()
            && checkin_cfg(u)
                .get("enabled")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
    })
}

/// 今天还没签过的已配置上游；自动调度用它避免重复 POST。
pub fn pending_ids(state: &AppState) -> Vec<String> {
    let today = chrono::Local::now().format("%Y-%m-%d").to_string();
    let file = state.file.read().unwrap();
    file.upstreams
        .iter()
        .filter(|u| {
            if token_of(u).is_empty()
                || !checkin_cfg(u)
                    .get("enabled")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false)
            {
                return false;
            }
            let st = state_of(u);
            let day = st.get("day").and_then(|v| v.as_str()).unwrap_or("");
            let kind = st.get("kind").and_then(|v| v.as_str()).unwrap_or("");
            !(day == today && (kind == "ok" || kind == "already"))
        })
        .map(|u| u.id.clone())
        .collect()
}

pub fn settings(state: &AppState) -> Value {    let file = state.file.read().unwrap();
    let s = file.checkin_settings.as_object().cloned().unwrap_or_default();
    json!({
        "enabled": s.get("enabled").and_then(|v| v.as_bool()).unwrap_or(false),
        "at": s.get("at").and_then(|v| v.as_str()).unwrap_or("08:00"),
        "random_minutes": s.get("random_minutes").and_then(|v| v.as_i64()).unwrap_or(30),
    })
}

fn checkin_entries(state: &AppState) -> Vec<Value> {
    let path = state.checkin_path();
    let Ok(text) = std::fs::read_to_string(path) else {
        return vec![];
    };
    let Ok(v) = serde_json::from_str::<Value>(&text) else {
        return vec![];
    };
    v.get("entries")
        .and_then(|x| x.as_array())
        .cloned()
        .unwrap_or_default()
}

pub fn summary(state: &AppState) -> Value {
    let entries = checkin_entries(state);
    let today = chrono::Local::now().format("%Y-%m-%d").to_string();
    let today_checked = entries
        .iter()
        .filter(|e| {
            e.get("day").and_then(|v| v.as_str()) == Some(today.as_str())
                && matches!(
                    e.get("kind").and_then(|v| v.as_str()),
                    Some("ok") | Some("already")
                )
        })
        .count();
    json!({"total": entries.len(), "today_checked": today_checked, "today": today})
}

pub fn recent(state: &AppState, limit: usize) -> Vec<Value> {
    let entries = checkin_entries(state);
    entries.iter().rev().take(limit).cloned().collect()
}

fn append_entry(state: &AppState, entry: Value) {
    let path = state.checkin_path();
    let mut entries = checkin_entries(state);
    entries.push(entry);
    if entries.len() > 2000 {
        let keep = entries.len() - 2000;
        entries.drain(0..keep);
    }
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(
        &path,
        serde_json::to_string_pretty(&json!({"entries": entries})).unwrap_or_default(),
    );
}

fn set_state(state: &AppState, uid: &str, result: &Value) {
    let mut file = state.file.write().unwrap();
    if let Some(up) = file.find_upstream_mut(uid) {
        let mut cfg = checkin_cfg(up);
        if let Some(path) = result.get("path").and_then(|v| v.as_str()) {
            if !path.is_empty() {
                cfg.insert("path".to_string(), json!(path));
            }
        }
        up.extra.insert("checkin".to_string(), Value::Object(cfg));
        up.extra.insert("checkin_state".to_string(), result.clone());
    }
}

pub async fn run(state: Arc<AppState>, ids: Vec<String>, press_checkin: bool) -> Vec<Value> {
    let targets: Vec<Upstream> = {
        let file = state.file.read().unwrap();
        file.upstreams
            .iter()
            .filter(|u| {
                let token = token_of(u);
                let enabled = checkin_cfg(u)
                    .get("enabled")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                let wanted = ids.is_empty() || ids.contains(&u.id);
                !token.is_empty() && (enabled || !ids.is_empty()) && wanted
            })
            .cloned()
            .collect()
    };
    let mut out = Vec::new();
    for up in targets {
        let result = run_one(&state, &up, press_checkin).await;
        set_state(&state, &up.id, &result);
        if press_checkin && result.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
            let mut entry = result.clone();
            entry["id"] = json!(up.id);
            entry["name"] = json!(up.name);
            append_entry(&state, entry);
        }
        out.push(result);
    }
    let _ = state.save();
    out
}

async fn run_one(state: &AppState, up: &Upstream, press: bool) -> Value {
    let token = token_of(up);
    if token.is_empty() {
        return result("nocfg", false, "还没配置访问令牌", Value::Null);
    }
    if token.to_ascii_lowercase().starts_with("sk-") {
        return result(
            "nocfg",
            false,
            "这里要填站点的「访问令牌」，不是 sk- 开头的推理 Key",
            Value::Null,
        );
    }
    if token_expired(&token) {
        return result(
            "auth",
            false,
            "访问令牌已过期（JWT exp 已过），请到站点重新生成访问令牌后保存",
            Value::Null,
        );
    }
    let root = site_root(&up.base_url);
    if root.is_empty() {
        return result("error", false, "上游没填地址", Value::Null);
    }
    let cfg = checkin_cfg(up);
    let known = cfg.get("path").and_then(|v| v.as_str()).unwrap_or("");
    let mut candidates: Vec<&str> = Vec::new();
    if CHECKIN_PATHS.contains(&known) {
        candidates.push(known);
    }
    for p in CHECKIN_PATHS {
        if !candidates.contains(&p) {
            candidates.push(p);
        }
    }
    let user_id = cfg.get("user_id").and_then(|v| v.as_str()).unwrap_or("");
    let timeout = std::time::Duration::from_secs(15);
    let mut last = result("none", false, "站点没有可用的签到接口", Value::Null);
    let mut saw_auth = false;
    for path in candidates {
        let url = format!("{root}{path}");
        let mut req = if press {
            state.client.post(&url)
        } else {
            state.client.get(&url)
        };
        req = req
            .header("Accept", "application/json")
            .header("Authorization", format!("Bearer {token}"))
            .timeout(timeout);
        if !user_id.is_empty() {
            req = req.header("New-Api-User", user_id);
        }
        let resp = match req.send().await {
            Ok(r) => r,
            Err(e) => {
                last = result("error", false, &format!("连不上 {root}：{e}"), Value::Null);
                continue;
            }
        };
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        let low = text.to_ascii_lowercase();
        if low.contains("just a moment")
            || low.contains("__cf_chl_")
            || low.contains("turnstile")
            || low.contains("attention required")
        {
            return result(
                "blocked",
                false,
                "站点要求人机验证（Cloudflare/Turnstile），请在站点上用浏览器手动签到",
                json!({"blocked": "cf", "status": status.as_u16(), "path": path}),
            );
        }
        if status.as_u16() == 401 || status.as_u16() == 403 {
            saw_auth = true;
            last = result(
                "auth",
                false,
                &format!("站点拒绝了访问令牌（HTTP {}）", status.as_u16()),
                Value::Null,
            );
            continue;
        }
        if status.as_u16() == 404 || status.as_u16() == 405 {
            last = result(
                "none",
                false,
                &format!("站点没有 {path}（HTTP {}）", status.as_u16()),
                Value::Null,
            );
            continue;
        }
        let payload: Option<Value> = serde_json::from_str(&text).ok();
        let Some(payload) = payload else {
            last = result(
                "none",
                false,
                &format!("{path} 返回的不是 JSON（多半是首页）"),
                Value::Null,
            );
            continue;
        };
        if status.as_u16() >= 400 {
            let msg = payload
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            if status.as_u16() == 409
                || msg.to_ascii_lowercase().contains("already")
                || msg.contains("已签到")
            {
                return result(
                    "already",
                    true,
                    if msg.is_empty() { "今天已经签过了" } else { &msg },
                    json!({"path": path, "checked_in_today": true, "status": status.as_u16()}),
                );
            }
            last = result(
                "error",
                false,
                &format!("HTTP {}：{}", status.as_u16(), if msg.is_empty() { "站点拒绝请求" } else { &msg }),
                json!({"status": status.as_u16()}),
            );
            continue;
        }
        if !press {
            let data = payload.get("data").cloned().unwrap_or(Value::Null);
            let checked = data
                .get("checked_in_today")
                .or_else(|| data.get("stats").and_then(|s| s.get("checked_in_today")))
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let enabled = data.get("enabled").and_then(|v| v.as_bool()).unwrap_or(true);
            if !enabled {
                return result("none", false, "站点没开启签到功能", json!({"path": path, "status": status.as_u16()}));
            }
            return result(
                if checked { "already" } else { "ok" },
                true,
                if checked { "今天已经签过了" } else { "可以签到" },
                json!({
                    "path": path, "status": status.as_u16(),
                    "checked_in_today": checked,
                    "total_checkins": data.get("stats").and_then(|s| s.get("total_checkins")).cloned().unwrap_or(json!(0)),
                    "total_quota": data.get("stats").and_then(|s| s.get("total_quota")).cloned().unwrap_or(json!(0)),
                }),
            );
        }
        let data = payload.get("data").cloned().unwrap_or(Value::Null);
        let success = payload.get("success").and_then(|v| v.as_bool()).unwrap_or(false)
            || data.get("success").and_then(|v| v.as_bool()).unwrap_or(false)
            || payload.get("code").and_then(|v| v.as_i64()).unwrap_or(-1) == 0;
        let msg = payload
            .get("message")
            .and_then(|v| v.as_str())
            .or_else(|| data.get("message").and_then(|v| v.as_str()))
            .unwrap_or("")
            .to_string();
        if success {
            return result(
                "ok",
                true,
                if msg.is_empty() { "签到成功" } else { &msg },
                json!({
                    "path": path, "status": status.as_u16(),
                    "reward": data.get("quota_awarded").or_else(|| data.get("reward")).cloned().unwrap_or(json!(0)),
                }),
            );
        }
        if msg.to_ascii_lowercase().contains("already") || msg.contains("已签到") {
            return result("already", true, if msg.is_empty() { "今天已经签过了" } else { &msg }, json!({"path": path, "status": status.as_u16()}));
        }
        last = result(
            "error",
            false,
            if msg.is_empty() { "站点没有接受这次签到" } else { &msg },
            json!({"path": path, "status": status.as_u16()}),
        );
    }
    if saw_auth && last.get("kind").and_then(|v| v.as_str()) == Some("none") {
        return result("auth", false, "站点拒绝了访问令牌", Value::Null);
    }
    last
}

fn result(kind: &str, ok: bool, message: &str, extra: Value) -> Value {
    let mut v = json!({
        "ok": ok,
        "kind": kind,
        "at": chrono::Utc::now().timestamp() as f64,
        "day": chrono::Local::now().format("%Y-%m-%d").to_string(),
        "message": message,
    });
    if let (Some(obj), Some(extra_obj)) = (v.as_object_mut(), extra.as_object()) {
        for (k, val) in extra_obj {
            obj.insert(k.clone(), val.clone());
        }
    }
    v
}
