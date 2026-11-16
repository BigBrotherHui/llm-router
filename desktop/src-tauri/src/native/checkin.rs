use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Map, Value};
use tauri::{Manager, WebviewUrl, WebviewWindow, WebviewWindowBuilder};
use tokio_tungstenite::tungstenite::Message;

use super::state::{AppState, Upstream};

const CHECKIN_PATHS: [&str; 3] = [
    "/api/user/self/checkin",
    "/api/user/checkin",
    "/api/v1/user/check-in",
];

const CHROME_UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/138.0.0.0 Safari/537.36";

/// 登录页上优先匹配 Linux.do 的 OAuth 入口；可用 checkin.oauth_hint 覆盖
/// （值按正则片段处理，比如 "github|oauth"）。
const OAUTH_HINT: &str = r"linux[\s._-]*do|connect\.linux\.do|/api/oauth/linux";

/// 浏览器签到窗口要一个个来：多个 WebView2 同时登录同一站点容易互相抢 cookie，
/// 也会把内存顶上去。token 模式仍然走原来的 6 并发。
static BROWSER_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[derive(Clone)]
struct Provider {
    name: String,
    login_path: String,
    sign_in_path: Option<String>,
    auto_trigger: bool,
    user_info_path: String,
    api_user_key: String,
    #[allow(dead_code)]
    waf_cookies: Vec<String>,
}

struct UserInfo {
    ok: bool,
    status: u16,
    id: String,
    quota: f64,
    used: f64,
    message: String,
    blocked: bool,
}

struct SignInResp {
    status: u16,
    ok: bool,
    already: bool,
    blocked: bool,
    message: String,
}

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

fn cfg_str(cfg: &Map<String, Value>, key: &str) -> String {
    cfg.get(key)
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string()
}

fn cfg_raw(cfg: &Map<String, Value>, key: &str) -> String {
    cfg.get(key)
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string()
}

fn token_of(up: &Upstream) -> String {
    cfg_str(&checkin_cfg(up), "token")
}

fn auth_of(cfg: &Map<String, Value>) -> String {
    match cfg_str(cfg, "auth").as_str() {
        "browser" => "browser".to_string(),
        "chrome" => "chrome".to_string(),
        _ => "token".to_string(),
    }
}

fn configured(up: &Upstream) -> bool {
    let cfg = checkin_cfg(up);
    match auth_of(&cfg).as_str() {
        "browser" => true,
        "chrome" => !stored_cookies(&cfg).is_empty(),
        _ => !cfg_str(&cfg, "token").is_empty(),
    }
}

fn checkin_enabled(up: &Upstream) -> bool {
    checkin_cfg(up)
        .get("enabled")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

fn checkin_site_root(up: &Upstream, cfg: &Map<String, Value>) -> String {
    let custom = cfg_str(cfg, "site_root");
    if custom.is_empty() {
        site_root(&up.base_url)
    } else {
        site_root(&custom)
    }
}

fn provider_of(up: &Upstream, cfg: &Map<String, Value>) -> Provider {
    let root = checkin_site_root(up, cfg);
    let host = tauri::Url::parse(&root)
        .ok()
        .and_then(|u| u.host_str().map(|s| s.to_ascii_lowercase()))
        .unwrap_or_default();
    let explicit = cfg_str(cfg, "provider").to_ascii_lowercase();
    let mut p = if explicit == "agentrouter" || (explicit.is_empty() && host.contains("agentrouter")) {
        Provider {
            name: "agentrouter".to_string(),
            login_path: "/login".to_string(),
            sign_in_path: None,
            auto_trigger: true,
            user_info_path: "/api/user/self".to_string(),
            api_user_key: "new-api-user".to_string(),
            waf_cookies: vec!["acw_tc".to_string()],
        }
    } else if explicit == "anyrouter" || (explicit.is_empty() && host.contains("anyrouter")) {
        Provider {
            name: "anyrouter".to_string(),
            login_path: "/login".to_string(),
            sign_in_path: Some("/api/user/sign_in".to_string()),
            auto_trigger: false,
            user_info_path: "/api/user/self".to_string(),
            api_user_key: "new-api-user".to_string(),
            waf_cookies: vec![
                "acw_tc".to_string(),
                "cdn_sec_tc".to_string(),
                "acw_sc__v2".to_string(),
            ],
        }
    } else {
        Provider {
            name: if explicit.is_empty() {
                "custom".to_string()
            } else {
                explicit
            },
            login_path: "/login".to_string(),
            sign_in_path: None,
            auto_trigger: false,
            user_info_path: "/api/user/self".to_string(),
            api_user_key: "new-api-user".to_string(),
            waf_cookies: Vec::new(),
        }
    };
    if let Some(v) = cfg.get("login_path").and_then(|v| v.as_str()) {
        let v = v.trim();
        if !v.is_empty() {
            p.login_path = v.to_string();
        }
    }
    if let Some(v) = cfg.get("user_info_path").and_then(|v| v.as_str()) {
        let v = v.trim();
        if !v.is_empty() {
            p.user_info_path = v.to_string();
        }
    }
    if let Some(v) = cfg.get("api_user_key").and_then(|v| v.as_str()) {
        let v = v.trim();
        if !v.is_empty() {
            p.api_user_key = v.to_string();
        }
    }
    if let Some(v) = cfg.get("sign_in_path").and_then(|v| v.as_str()) {
        let v = v.trim();
        p.sign_in_path = if v.is_empty() {
            None
        } else {
            Some(v.to_string())
        };
        if cfg.get("auto_trigger").and_then(|x| x.as_bool()).is_none() {
            p.auto_trigger = p.sign_in_path.is_none() && p.auto_trigger;
        }
    }
    if let Some(v) = cfg.get("auto_trigger").and_then(|x| x.as_bool()) {
        p.auto_trigger = v;
    }
    p
}

fn sign_in_candidates(prov: &Provider, cfg: &Map<String, Value>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    if let Some(p) = &prov.sign_in_path {
        out.push(p.clone());
    }
    let learned = cfg_str(cfg, "path");
    if !learned.is_empty() && !out.contains(&learned) {
        out.push(learned);
    }
    if prov.name == "custom" || (prov.sign_in_path.is_none() && !prov.auto_trigger) {
        for p in CHECKIN_PATHS {
            if !out.iter().any(|x| x == p) {
                out.push(p.to_string());
            }
        }
    }
    out
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

/// 只有显式加入（extra 里有 checkin 配置）的上游才出现在签到页。
/// 不支持签到的上游可以不加入；加入后也能随时移除（只删签到配置，不动上游）。
fn listed(up: &Upstream) -> bool {
    up.extra.contains_key("checkin")
}

pub fn available(state: &AppState) -> Vec<Value> {
    let file = state.file.read().unwrap();
    file.upstreams
        .iter()
        .filter(|up| !listed(up))
        .map(|up| {
            json!({
                "id": up.id,
                "name": up.name,
                "base_url": up.base_url,
                "site_root": site_root(&up.base_url),
            })
        })
        .collect()
}

pub fn rows(state: &AppState) -> Vec<Value> {
    let file = state.file.read().unwrap();
    file.upstreams
        .iter()
        .filter(|up| listed(up))
        .map(|up| {
            let cfg = checkin_cfg(up);
            let token = token_of(up);
            let auth = auth_of(&cfg);
            let browser = auth == "browser";
            let chrome = auth == "chrome";
            let has_cookies = !stored_cookies(&cfg).is_empty();
            let configured = match auth.as_str() {
                "browser" => true,
                "chrome" => has_cookies,
                _ => !token.is_empty(),
            };
            let enabled = cfg.get("enabled").and_then(|v| v.as_bool()).unwrap_or(false);
            let prov = provider_of(up, &cfg);
            let st = state_of(up);
            json!({
                "id": up.id,
                "name": up.name,
                "base_url": up.base_url,
                "site_root": checkin_site_root(up, &cfg),
                "enabled": up.enabled,
                "configured": configured,
                "auth": auth,
                "provider": prov.name,
                "site_root_custom": cfg_str(&cfg, "site_root"),
                "oauth_hint": cfg_str(&cfg, "oauth_hint"),
                "email": cfg_str(&cfg, "email"),
                "has_password": !cfg_raw(&cfg, "password").is_empty(),
                "has_cookies": has_cookies,
                "cookie_at": cfg.get("cookie_at").cloned().unwrap_or_else(|| json!(0)),
                "checkin_enabled": enabled,
                "token_hint": if browser || chrome || token.is_empty() { String::new() } else { super::state::mask_key(&token) },
                "token_expires_at": if browser || chrome { Value::Null } else { json!(jwt_exp(&token)) },
                "token_expired": !browser && !chrome && token_expired(&token),
                "user_id": cfg.get("user_id").cloned().unwrap_or_else(|| json!("")),
                "path": cfg.get("path").cloned().unwrap_or_else(|| json!("")),
                "token_looks_wrong": !browser && !chrome && token.to_ascii_lowercase().starts_with("sk-"),
                "login_at": st.get("login_at").cloned().unwrap_or_else(|| json!(0)),
                "state": st,
            })
        })
        .collect()
}

pub fn has_targets(state: &AppState) -> bool {
    let file = state.file.read().unwrap();
    file.upstreams
        .iter()
        .any(|u| configured(u) && checkin_enabled(u))
}

/// 今天还没签过的已配置上游；自动调度用它避免重复 POST。
pub fn pending_ids(state: &AppState) -> Vec<String> {
    let today = chrono::Local::now().format("%Y-%m-%d").to_string();
    let file = state.file.read().unwrap();
    file.upstreams
        .iter()
        .filter(|u| {
            if !configured(u) || !checkin_enabled(u) {
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

pub fn settings(state: &AppState) -> Value {
    let file = state.file.read().unwrap();
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
                let wanted = ids.is_empty() || ids.contains(&u.id);
                configured(u) && (checkin_enabled(u) || !ids.is_empty()) && wanted
            })
            .cloned()
            .collect()
    };
    let mut out = Vec::new();
    let results: Vec<(Upstream, Value)> = futures_util::stream::iter(targets)
        .map(|up| {
            let state = state.clone();
            let up2 = up.clone();
            async move {
                let result = run_one(&state, &up2, press_checkin).await;
                (up, result)
            }
        })
        .buffer_unordered(6)
        .collect()
        .await;
    for (up, result) in results {
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
    let cfg = checkin_cfg(up);
    match auth_of(&cfg).as_str() {
        "browser" => run_browser(state, up, press).await,
        "chrome" => run_one_chrome(state, up, press).await,
        _ => run_one_token(state, up, press).await,
    }
}

async fn run_one_token(state: &AppState, up: &Upstream, press: bool) -> Value {
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
    let timeout = Duration::from_secs(15);
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
                &format!(
                    "HTTP {}：{}",
                    status.as_u16(),
                    if msg.is_empty() { "站点拒绝请求" } else { &msg }
                ),
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
                return result(
                    "none",
                    false,
                    "站点没开启签到功能",
                    json!({"path": path, "status": status.as_u16()}),
                );
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
            return result(
                "already",
                true,
                if msg.is_empty() { "今天已经签过了" } else { &msg },
                json!({"path": path, "status": status.as_u16()}),
            );
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

async fn run_one_chrome(state: &AppState, up: &Upstream, press: bool) -> Value {
    let cfg = checkin_cfg(up);
    let cookies = stored_cookies(&cfg);
    if cookies.is_empty() {
        return result(
            "login",
            false,
            "还没有用 Chrome 登录：点行内「登录」打开专用 Chrome 登录一次",
            json!({"need_login": true, "chrome": true}),
        );
    }
    let root = checkin_site_root(up, &cfg);
    if root.is_empty() {
        return result("error", false, "上游没填地址", Value::Null);
    }
    let prov = provider_of(up, &cfg);
    let cfg_user = cfg_str(&cfg, "user_id");
    let cookie = cookie_header(&cookies);
    let mut info = http_user_info(state, &root, &prov, &cookie, &cfg_user).await;
    if !info.ok {
        let (kind, message) = if info.blocked {
            (
                "blocked",
                "站点要求人机验证，Chrome 保存的 Cookie 不够用，请重新登录".to_string(),
            )
        } else if info.status == 0 {
            (
                "error",
                if info.message.is_empty() {
                    format!("连不上 {root}")
                } else {
                    info.message.clone()
                },
            )
        } else {
            (
                "login",
                "Chrome 保存的 Cookie 已失效，请点「登录」重新登录".to_string(),
            )
        };
        return result(
            kind,
            false,
            &message,
            json!({"need_login": true, "status": info.status, "chrome": true}),
        );
    }
    let api_user = if !info.id.is_empty() {
        info.id.clone()
    } else {
        cfg_user
    };
    let per_unit = cfg
        .get("quota_per_unit")
        .and_then(|v| v.as_f64())
        .filter(|x| *x > 0.0)
        .unwrap_or(500000.0);
    let base = json!({
        "provider": prov.name,
        "path": prov.sign_in_path.clone().unwrap_or_default(),
        "status": info.status,
        "quota": info.quota,
        "used_quota": info.used,
        "quota_usd": round2(info.quota / per_unit),
        "login_at": cfg.get("cookie_at").cloned().unwrap_or_else(|| json!(0)),
        "chrome": true,
    });
    if !press {
        return result("ok", true, "Chrome Cookie 可用，可以签到", base);
    }
    if prov.auto_trigger {
        let after = http_user_info(state, &root, &prov, &cookie, &api_user).await;
        if !after.ok {
            return result("error", false, "拉取用户信息触发签到失败", base);
        }
        let reward = (after.quota + after.used) - (info.quota + info.used);
        let mut out = result("ok", true, "签到完成（拉取用户信息时自动触发）", base);
        out["reward"] = json!(reward.max(0.0));
        if reward > 0.0 {
            out["reward_text"] = json!(format!("${:.2}", reward / per_unit));
        }
        return out;
    }
    let mut last = result("none", false, "站点没有可用的签到接口", base.clone());
    let mut saw_auth = false;
    for path in sign_in_candidates(&prov, &cfg) {
        let r = http_sign_in(state, &root, &path, &prov, &cookie, &api_user).await;
        if r.blocked {
            return result(
                "blocked",
                false,
                "站点要求人机验证，请重新用 Chrome 登录",
                json!({"blocked": "cf", "status": r.status, "path": path}),
            );
        }
        if r.status == 401 || r.status == 403 {
            saw_auth = true;
            last = result(
                "auth",
                false,
                "站点拒绝了 Chrome Cookie（401/403），请重新登录",
                json!({"path": path, "status": r.status}),
            );
            continue;
        }
        if r.status == 404 || r.status == 405 {
            last = result(
                "none",
                false,
                &format!("站点没有 {path}（HTTP {}）", r.status),
                json!({"path": path, "status": r.status}),
            );
            continue;
        }
        if r.ok || r.already {
            let after = http_user_info(state, &root, &prov, &cookie, &api_user).await;
            let reward = if after.ok {
                (after.quota + after.used) - (info.quota + info.used)
            } else {
                0.0
            };
            let mut out = result(
                if r.already { "already" } else { "ok" },
                true,
                if r.message.is_empty() {
                    if r.already {
                        "今天已经签过了"
                    } else {
                        "签到成功"
                    }
                } else {
                    &r.message
                },
                json!({
                    "path": path,
                    "status": r.status,
                    "checked_in_today": r.already,
                    "quota": info.quota,
                    "used_quota": info.used,
                    "quota_usd": round2(info.quota / per_unit),
                    "login_at": base["login_at"].clone(),
                    "provider": prov.name,
                    "chrome": true,
                }),
            );
            if reward > 0.0 {
                out["reward"] = json!(reward);
                out["reward_text"] = json!(format!("${:.2}", reward / per_unit));
            }
            return out;
        }
        last = result(
            "error",
            false,
            if r.message.is_empty() {
                "站点没有接受这次签到"
            } else {
                &r.message
            },
            json!({"path": path, "status": r.status}),
        );
    }
    if saw_auth {
        return result(
            "auth",
            false,
            "Chrome Cookie 被站点拒绝（401/403），请重新登录",
            base,
        );
    }
    last
}

/// 打开（或复用）一个可见的站点登录窗口。登录后 cookie 会留在该上游专属的
/// WebView2 profile 里，之后自动签到直接复用，不用再登。
pub fn open_login_window(state: &AppState, up: &Upstream) -> Result<String, String> {
    let app = state
        .app
        .lock()
        .unwrap()
        .clone()
        .ok_or_else(|| "浏览器登录只在桌面版可用".to_string())?;
    let cfg = checkin_cfg(up);
    let prov = provider_of(up, &cfg);
    let root = checkin_site_root(up, &cfg);
    if root.is_empty() {
        return Err("上游没填地址".to_string());
    }
    let label = format!("ck-{}", safe_label(&up.id));
    let login_url = format!("{root}{}", prov.login_path);
    if let Some(w) = app.get_webview_window(&label) {
        if let Ok(url) = tauri::Url::parse(&login_url) {
            let _ = w.navigate(url);
        }
        let _ = w.set_skip_taskbar(false);
        let _ = w.show();
        let _ = w.set_focus();
        return Ok(label);
    }
    let w = build_browser_window(&app, state, &label, &up.name, &login_url, true)?;
    let _ = w.set_focus();
    Ok(label)
}

async fn run_browser(state: &AppState, up: &Upstream, press: bool) -> Value {
    let cfg = checkin_cfg(up);
    let prov = provider_of(up, &cfg);
    let root = checkin_site_root(up, &cfg);
    if root.is_empty() {
        return result("error", false, "上游没填地址", Value::Null);
    }
    let app = state.app.lock().unwrap().clone();
    let Some(app) = app else {
        return result("error", false, "浏览器签到只在桌面版可用", Value::Null);
    };
    let label = format!("ck-{}", safe_label(&up.id));
    let _guard = BROWSER_LOCK.lock().await;
    let existing = app.get_webview_window(&label);
    let visible = existing
        .as_ref()
        .and_then(|w| w.is_visible().ok())
        .unwrap_or(false);
    let win = match existing {
        Some(w) => w,
        None => match build_browser_window(&app, state, &label, &up.name, &root, false) {
            Ok(w) => w,
            Err(e) => {
                return result(
                    "error",
                    false,
                    &format!("打开签到浏览器失败：{e}"),
                    Value::Null,
                )
            }
        },
    };

    // 可见窗口可能是用户正在手动登录，别打断它，直接复用现有 cookie。
    if !visible {
        if let Ok(url) = tauri::Url::parse(&root) {
            let _ = win.navigate(url);
        }
        let _ = wait_page_ready(&win, 30).await;
        tokio::time::sleep(Duration::from_millis(1200)).await;
    }

    let mut cookies = cookies_now(&win, &root).await;
    let mut login_at = state_of(up)
        .get("login_at")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    let mut login_note = String::new();
    let cfg_user = cfg_str(&cfg, "user_id");
    let mut info = http_user_info(state, &root, &prov, &cookie_header(&cookies), &cfg_user).await;
    // 不强制依赖 cookie 名叫 session：先拿用户信息验证会话。失败且站点有响应时，
    // 再用邮箱密码自动登录一次（凭证没配就是 Ok(false)，直接跳过）。
    if !info.ok && info.status != 0 && !info.blocked {
        match try_browser_login(state, &win, &root, &prov, &cfg).await {
            Ok(true) => {
                login_at = chrono::Utc::now().timestamp();
                cookies = cookies_now(&win, &root).await;
                info = http_user_info(
                    state,
                    &root,
                    &prov,
                    &cookie_header(&cookies),
                    &cfg_user,
                )
                .await;
                login_note = "已自动登录；".to_string();
            }
            Ok(false) => {}
            Err(e) => login_note = format!("自动登录未完成（{e}）；"),
        }
    }
    if !info.ok {
        if !visible {
            let _ = win.close();
        }
        if info.blocked {
            return result(
                "blocked",
                false,
                if info.message.is_empty() {
                    "站点要求人机验证（Cloudflare/Turnstile），请打开登录窗口手动通过"
                } else {
                    &info.message
                },
                json!({"need_login": true, "status": info.status}),
            );
        }
        if info.status == 0 {
            let message = if info.message.is_empty() {
                format!("连不上 {root}")
            } else {
                info.message.clone()
            };
            return result("error", false, &message, json!({"status": 0}));
        }
        let message = if login_note.is_empty() {
            "浏览器还没登录：请在签到页点「登录」，在弹出的窗口里登录一次".to_string()
        } else {
            format!("{login_note}需要登录：请在签到页点「登录」完成站点登录")
        };
        return result(
            "login",
            false,
            &message,
            json!({"need_login": true, "status": info.status}),
        );
    }

    let api_user = if !info.id.is_empty() {
        info.id.clone()
    } else {
        cfg_user
    };
    let per_unit = cfg
        .get("quota_per_unit")
        .and_then(|v| v.as_f64())
        .filter(|x| *x > 0.0)
        .unwrap_or(500000.0);
    let base = json!({
        "provider": prov.name,
        "path": prov.sign_in_path.clone().unwrap_or_default(),
        "status": info.status,
        "quota": info.quota,
        "used_quota": info.used,
        "quota_usd": round2(info.quota / per_unit),
        "login_at": login_at,
    });
    if !press {
        if !visible {
            let _ = win.close();
        }
        return result(
            "ok",
            true,
            if login_note.is_empty() {
                "浏览器已登录，可以签到"
            } else {
                "已自动登录，可以签到"
            },
            base,
        );
    }

    if prov.auto_trigger {
        let after = http_user_info(
            state,
            &root,
            &prov,
            &cookie_header(&cookies),
            &api_user,
        )
        .await;
        if !visible {
            let _ = win.close();
        }
        if !after.ok {
            return result("error", false, "拉取用户信息触发签到失败", base);
        }
        let reward = (after.quota + after.used) - (info.quota + info.used);
        let mut out = result(
            "ok",
            true,
            &format!("{login_note}签到完成（拉取用户信息时自动触发）"),
            base,
        );
        out["reward"] = json!(reward.max(0.0));
        if reward > 0.0 {
            out["reward_text"] = json!(format!("${:.2}", reward / per_unit));
        }
        return out;
    }

    let mut last = result("none", false, "站点没有可用的签到接口", base.clone());
    let mut saw_auth = false;
    for path in sign_in_candidates(&prov, &cfg) {
        let r = http_sign_in(
            state,
            &root,
            &path,
            &prov,
            &cookie_header(&cookies),
            &api_user,
        )
        .await;
        if r.blocked {
            if !visible {
                let _ = win.close();
            }
            return result(
                "blocked",
                false,
                "站点要求人机验证（Cloudflare/Turnstile），浏览器会话可能已失效",
                json!({"blocked": "cf", "status": r.status, "path": path}),
            );
        }
        if r.status == 401 || r.status == 403 {
            saw_auth = true;
            last = result(
                "auth",
                false,
                &format!("站点拒绝了浏览器会话（HTTP {}），请重新登录", r.status),
                json!({"path": path, "status": r.status}),
            );
            continue;
        }
        if r.status == 404 || r.status == 405 {
            last = result(
                "none",
                false,
                &format!("站点没有 {path}（HTTP {}）", r.status),
                json!({"path": path, "status": r.status}),
            );
            continue;
        }
        if r.ok || r.already {
            let after = http_user_info(
                state,
                &root,
                &prov,
                &cookie_header(&cookies),
                &api_user,
            )
            .await;
            let reward = if after.ok {
                (after.quota + after.used) - (info.quota + info.used)
            } else {
                0.0
            };
            if !visible {
                let _ = win.close();
            }
            let mut out = result(
                if r.already { "already" } else { "ok" },
                true,
                if r.message.is_empty() {
                    if r.already {
                        "今天已经签过了"
                    } else {
                        "签到成功"
                    }
                } else {
                    &r.message
                },
                json!({
                    "path": path,
                    "status": r.status,
                    "checked_in_today": r.already,
                    "quota": info.quota,
                    "used_quota": info.used,
                    "quota_usd": round2(info.quota / per_unit),
                    "login_at": login_at,
                    "provider": prov.name,
                }),
            );
            if reward > 0.0 {
                out["reward"] = json!(reward);
                out["reward_text"] = json!(format!("${:.2}", reward / per_unit));
            }
            return out;
        }
        last = result(
            "error",
            false,
            if r.message.is_empty() {
                "站点没有接受这次签到"
            } else {
                &r.message
            },
            json!({"path": path, "status": r.status}),
        );
    }
    if !visible {
        let _ = win.close();
    }
    if saw_auth {
        return result(
            "auth",
            false,
            "浏览器会话被站点拒绝（401/403），请重新登录",
            base,
        );
    }
    last
}

/// 方案 1：用专用 Chrome 登录，CDP 自动抓该站 Cookie。
/// Chrome 用独立 profile（<state目录>/checkin-chrome-profile），不影响日常 Chrome。
pub struct ChromeLogin {
    pub up: Upstream,
    pub root: String,
    pub port: u16,
    pub ws_path: String,
}

static CHROME_TASKS: OnceLock<Mutex<HashMap<String, Arc<AtomicBool>>>> = OnceLock::new();

fn chrome_tasks() -> &'static Mutex<HashMap<String, Arc<AtomicBool>>> {
    CHROME_TASKS.get_or_init(|| Mutex::new(HashMap::new()))
}

pub fn prepare_chrome_login(state: &AppState, up: &Upstream) -> Result<ChromeLogin, String> {
    let cfg = checkin_cfg(up);
    if auth_of(&cfg) != "chrome" {
        return Err("这个上游不是 Chrome 登录模式".to_string());
    }
    let root = checkin_site_root(up, &cfg);
    if root.is_empty() {
        return Err("上游没填地址".to_string());
    }
    let prov = provider_of(up, &cfg);
    let login_url = format!("{root}{}", prov.login_path);
    let profile = chrome_profile_dir(state);
    let (port, ws_path) = ensure_chrome(&profile, &login_url)?;
    Ok(ChromeLogin {
        up: up.clone(),
        root,
        port,
        ws_path,
    })
}

pub fn spawn_chrome_capture(state: Arc<AppState>, prep: ChromeLogin) {
    let uid = prep.up.id.clone();
    let running = {
        let mut tasks = chrome_tasks().lock().unwrap();
        if let Some(flag) = tasks.get(&uid) {
            if flag.load(Ordering::Relaxed) {
                return;
            }
        }
        let flag = Arc::new(AtomicBool::new(true));
        tasks.insert(uid.clone(), flag.clone());
        flag
    };
    let pending = result(
        "login",
        false,
        "已打开 Chrome：请在窗口里登录，成功后会自动保存 Cookie",
        json!({"chrome": true, "pending": true}),
    );
    set_state(&state, &uid, &pending);
    let _ = state.save();
    let _ = tauri::async_runtime::spawn(async move {
        capture_chrome_login(state, prep).await;
        running.store(false, Ordering::Relaxed);
    });
}

async fn capture_chrome_login(state: Arc<AppState>, prep: ChromeLogin) {
    let cfg = checkin_cfg(&prep.up);
    let prov = provider_of(&prep.up, &cfg);
    let host = host_of(&prep.root);
    let api_user = cfg_str(&cfg, "user_id");
    let deadline = std::time::Instant::now() + Duration::from_secs(900);
    let mut client: Option<Cdp> = None;
    let mut connect_fail = 0u32;
    loop {
        if std::time::Instant::now() > deadline {
            break;
        }
        if client.is_none() {
            match Cdp::connect(prep.port, &prep.ws_path).await {
                Ok(c) => {
                    client = Some(c);
                    connect_fail = 0;
                }
                Err(_) => {
                    connect_fail += 1;
                    // 调试端口在 Chrome 启动时就有；连续 30 秒连不上基本是窗口被关了。
                    if connect_fail >= 15 {
                        break;
                    }
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    continue;
                }
            }
        }
        let cookies = match client.as_mut().unwrap().site_cookies(&host).await {
            Ok(v) => v,
            Err(_) => {
                client = None;
                continue;
            }
        };
        if !cookies.is_empty() {
            let info = http_user_info(
                &state,
                &prep.root,
                &prov,
                &cookie_header(&cookies),
                &api_user,
            )
            .await;
            if info.ok {
                save_chrome_cookies(&state, &prep.up.id, &cookies, &info.id);
                return;
            }
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    let fail = result(
        "login",
        false,
        "Chrome 登录未完成或窗口已关闭，请重试",
        json!({"chrome": true}),
    );
    set_state(&state, &prep.up.id, &fail);
    let _ = state.save();
}

fn save_chrome_cookies(state: &AppState, uid: &str, cookies: &[(String, String)], api_user: &str) {
    let now = chrono::Utc::now().timestamp();
    let mut file = state.file.write().unwrap();
    if let Some(up) = file.find_upstream_mut(uid) {
        let mut cfg = checkin_cfg(up);
        let mut obj = Map::new();
        for (k, v) in cookies {
            obj.insert(k.clone(), json!(v));
        }
        cfg.insert("cookies".to_string(), Value::Object(obj));
        cfg.insert("cookie_at".to_string(), json!(now));
        if !api_user.is_empty() && cfg_str(&cfg, "user_id").is_empty() {
            cfg.insert("user_id".to_string(), json!(api_user));
        }
        up.extra.insert("checkin".to_string(), Value::Object(cfg));
        let ok = result(
            "ok",
            true,
            "Chrome 登录成功，已保存 Cookie",
            json!({"login_at": now, "chrome": true, "cookie_count": cookies.len()}),
        );
        up.extra.insert("checkin_state".to_string(), ok);
    }
    drop(file);
    let _ = state.save();
}

fn stored_cookies(cfg: &Map<String, Value>) -> Vec<(String, String)> {
    cfg.get("cookies")
        .and_then(|v| v.as_object())
        .map(|m| {
            m.iter()
                .filter_map(|(k, v)| {
                    v.as_str()
                        .filter(|s| !s.is_empty())
                        .map(|s| (k.clone(), s.to_string()))
                })
                .collect()
        })
        .unwrap_or_default()
}

fn host_of(url: &str) -> String {
    tauri::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(|s| s.to_ascii_lowercase()))
        .unwrap_or_default()
}

fn domain_matches(domain: &str, host: &str) -> bool {
    let d = domain.trim_start_matches('.').to_ascii_lowercase();
    let h = host.to_ascii_lowercase();
    if d.is_empty() || h.is_empty() {
        return false;
    }
    h == d || h.ends_with(&format!(".{d}"))
}

fn extract_site_cookies(cookies: &[Value], host: &str) -> Vec<(String, String)> {
    let now = chrono::Utc::now().timestamp() as f64;
    let mut map: HashMap<String, String> = HashMap::new();
    for c in cookies {
        let Some(name) = c.get("name").and_then(|v| v.as_str()) else {
            continue;
        };
        let Some(value) = c.get("value").and_then(|v| v.as_str()) else {
            continue;
        };
        if name.is_empty() {
            continue;
        }
        let domain = c.get("domain").and_then(|v| v.as_str()).unwrap_or("");
        if !domain_matches(domain, host) {
            continue;
        }
        let expires = c.get("expires").and_then(|v| v.as_f64()).unwrap_or(-1.0);
        if expires > 0.0 && expires < now {
            continue;
        }
        map.insert(name.to_string(), value.to_string());
    }
    map.into_iter().collect()
}

type CdpSocket = tokio_tungstenite::WebSocketStream<
    tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
>;

struct Cdp {
    ws: CdpSocket,
    next: u64,
}

impl Cdp {
    async fn connect(port: u16, ws_path: &str) -> Result<Self, String> {
        let url = format!("ws://127.0.0.1:{port}{ws_path}");
        let (ws, _) = tokio_tungstenite::connect_async(url.as_str())
            .await
            .map_err(|e| e.to_string())?;
        Ok(Self { ws, next: 1 })
    }

    async fn call(&mut self, method: &str, params: Value) -> Result<Value, String> {
        let id = self.next;
        self.next += 1;
        let payload = json!({"id": id, "method": method, "params": params});
        self.ws
            .send(Message::Text(payload.to_string()))
            .await
            .map_err(|e| e.to_string())?;
        loop {
            let item = tokio::time::timeout(Duration::from_secs(15), self.ws.next())
                .await
                .map_err(|_| "CDP 响应超时".to_string())?;
            match item {
                Some(Ok(Message::Text(text))) => {
                    let v: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
                    if v.get("id").and_then(|x| x.as_u64()) == Some(id) {
                        if let Some(err) = v.get("error") {
                            return Err(err.to_string());
                        }
                        return Ok(v.get("result").cloned().unwrap_or(Value::Null));
                    }
                }
                Some(Ok(Message::Ping(p))) => {
                    let _ = self.ws.send(Message::Pong(p)).await;
                }
                Some(Ok(Message::Close(_))) | None => {
                    return Err("CDP 连接已关闭".to_string())
                }
                Some(Err(e)) => return Err(e.to_string()),
                _ => {}
            }
        }
    }

    async fn site_cookies(&mut self, host: &str) -> Result<Vec<(String, String)>, String> {
        let result = match self.call("Storage.getCookies", json!({})).await {
            Ok(v) => v,
            Err(_) => self.call("Network.getAllCookies", json!({})).await?,
        };
        let list = result
            .get("cookies")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        Ok(extract_site_cookies(&list, host))
    }
}

fn chrome_profile_dir(state: &AppState) -> PathBuf {
    state
        .state_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("checkin-chrome-profile")
}

fn chrome_exe() -> Result<PathBuf, String> {
    if let Ok(p) = std::env::var("CHROME_PATH") {
        let p = PathBuf::from(p);
        if p.exists() {
            return Ok(p);
        }
    }
    #[cfg(windows)]
    {
        let mut candidates = vec![
            PathBuf::from(r"C:\Program Files\Google\Chrome\Application\chrome.exe"),
            PathBuf::from(r"C:\Program Files (x86)\Google\Chrome\Application\chrome.exe"),
        ];
        if let Ok(local) = std::env::var("LOCALAPPDATA") {
            candidates.push(PathBuf::from(local).join(r"Google\Chrome\Application\chrome.exe"));
        }
        for p in candidates {
            if p.exists() {
                return Ok(p);
            }
        }
    }
    Err("没有找到 Google Chrome；可以设置 CHROME_PATH 指向 chrome.exe".to_string())
}

fn launch_chrome(profile: &Path, login_url: &str) -> Result<(), String> {
    let exe = chrome_exe()?;
    let mut cmd = std::process::Command::new(exe);
    cmd.arg(format!("--user-data-dir={}", profile.display()))
        .arg("--remote-debugging-port=0")
        .arg("--remote-allow-origins=*")
        .arg("--no-first-run")
        .arg("--no-default-browser-check")
        .arg("--new-window")
        .arg(login_url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    cmd.spawn().map_err(|e| format!("启动 Chrome 失败：{e}"))?;
    Ok(())
}

fn read_devtools_port(path: &Path) -> Option<(u16, String)> {
    let text = std::fs::read_to_string(path).ok()?;
    let mut lines = text.lines();
    let port = lines.next()?.trim().parse().ok()?;
    let ws_path = lines.next()?.trim().to_string();
    if ws_path.is_empty() {
        None
    } else {
        Some((port, ws_path))
    }
}

fn port_open(port: u16) -> bool {
    std::net::TcpStream::connect_timeout(
        &std::net::SocketAddr::from(([127, 0, 0, 1], port)),
        Duration::from_millis(400),
    )
    .is_ok()
}

fn ensure_chrome(profile: &Path, login_url: &str) -> Result<(u16, String), String> {
    std::fs::create_dir_all(profile).map_err(|e| format!("创建 Chrome 配置目录失败：{e}"))?;
    let port_file = profile.join("DevToolsActivePort");
    if let Some((port, ws_path)) = read_devtools_port(&port_file) {
        if port_open(port) {
            // Chrome 已经在跑：再启动一次只是把登录页开成新窗口/标签，端口沿用。
            let _ = launch_chrome(profile, login_url);
            return Ok((port, ws_path));
        }
    }
    let _ = std::fs::remove_file(&port_file);
    launch_chrome(profile, login_url)?;
    for _ in 0..120 {
        std::thread::sleep(Duration::from_millis(250));
        if let Some((port, ws_path)) = read_devtools_port(&port_file) {
            if port > 0 && port_open(port) {
                return Ok((port, ws_path));
            }
        }
    }
    Err("Chrome 启动后没有开放调试端口。若专用 Chrome 已经开着（没有调试端口），请先关掉它再试。".to_string())
}

fn build_browser_window(
    app: &tauri::AppHandle,
    state: &AppState,
    label: &str,
    name: &str,
    url: &str,
    visible: bool,
) -> Result<WebviewWindow, String> {
    let parsed = tauri::Url::parse(url).map_err(|e| e.to_string())?;
    let profile = profile_dir(state, label);
    std::fs::create_dir_all(&profile).map_err(|e| e.to_string())?;
    let profile = if profile.is_absolute() {
        profile
    } else {
        std::env::current_dir()
            .map(|dir| dir.join(&profile))
            .unwrap_or(profile)
    };
    WebviewWindowBuilder::new(app, label, WebviewUrl::External(parsed))
        .title(if visible {
            format!("签到登录 · {name}")
        } else {
            format!("签到 · {name}")
        })
        .inner_size(980.0, 760.0)
        .visible(visible)
        .skip_taskbar(!visible)
        .data_directory(profile)
        .incognito(false)
        .build()
        .map_err(|e| e.to_string())
}

fn profile_dir(state: &AppState, label: &str) -> PathBuf {
    state
        .state_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("checkin-profiles")
        .join(label)
}

fn safe_label(id: &str) -> String {
    let mut out = String::new();
    for ch in id.chars() {
        if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
            out.push(ch);
        } else {
            out.push('-');
        }
        if out.len() >= 40 {
            break;
        }
    }
    let trimmed = out.trim_matches('-').to_string();
    if trimmed.is_empty() {
        "up".to_string()
    } else {
        trimmed
    }
}

async fn cookies_now(win: &WebviewWindow, root: &str) -> Vec<(String, String)> {
    let win = win.clone();
    let root = root.to_string();
    tokio::task::spawn_blocking(move || read_cookies(&win, &root))
        .await
        .ok()
        .and_then(|r| r.ok())
        .unwrap_or_default()
}

fn read_cookies(win: &WebviewWindow, root: &str) -> Result<Vec<(String, String)>, String> {
    let url = tauri::Url::parse(root).map_err(|e| e.to_string())?;
    let list = win.cookies_for_url(url).map_err(|e| e.to_string())?;
    Ok(list
        .into_iter()
        .map(|c| (c.name().to_string(), c.value().to_string()))
        .collect())
}

fn cookie_header(cookies: &[(String, String)]) -> String {
    cookies
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("; ")
}

async fn try_browser_login(
    state: &AppState,
    win: &WebviewWindow,
    root: &str,
    prov: &Provider,
    cfg: &Map<String, Value>,
) -> Result<bool, String> {
    let email = cfg_str(cfg, "email");
    let password = cfg_raw(cfg, "password");
    let has_password = !email.is_empty() && !password.is_empty();

    let login_url = format!("{root}{}", prov.login_path);
    let parsed = tauri::Url::parse(&login_url).map_err(|e| e.to_string())?;
    win.navigate(parsed).map_err(|e| e.to_string())?;
    let _ = wait_page_ready(win, 40).await;
    tokio::time::sleep(Duration::from_millis(1200)).await;

    let api_user = cfg_str(cfg, "user_id");
    let mut last_error = String::new();

    if has_password {
        let mut outcome = Value::Null;
        for attempt in 0..5 {
            outcome = eval_json(win, &login_script(&email, &password))
                .await
                .unwrap_or(Value::Null);
            if outcome.get("ok").and_then(|v| v.as_bool()) == Some(true) {
                break;
            }
            if attempt < 4 {
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }
        if outcome.get("ok").and_then(|v| v.as_bool()) == Some(true) {
            match wait_logged_in(state, win, root, prov, &api_user, 35, false).await {
                Ok(true) => return Ok(true),
                Ok(false) => last_error = "邮箱登录后没有拿到有效会话".to_string(),
                Err(e) => last_error = e,
            }
            // 密码登录失败后回登录页，给 OAuth 一次机会。
            if let Ok(url) = tauri::Url::parse(&login_url) {
                let _ = win.navigate(url);
                let _ = wait_page_ready(win, 20).await;
                tokio::time::sleep(Duration::from_millis(800)).await;
            }
        } else {
            last_error = outcome
                .get("reason")
                .and_then(|v| v.as_str())
                .unwrap_or("登录表单没有提交成功")
                .to_string();
        }
    }

    // 没邮箱密码也走这里：Linux.do 等 OAuth 站点只要浏览器 profile 里还有
    // 平台登录态，点一下 OAuth 入口就能自动回到站点并刷新会话。
    let hint = cfg_str(cfg, "oauth_hint");
    let pattern = if hint.is_empty() {
        OAUTH_HINT.to_string()
    } else {
        hint
    };
    let mut outcome = Value::Null;
    for attempt in 0..8 {
        outcome = eval_json(win, &oauth_login_script(&pattern))
            .await
            .unwrap_or(Value::Null);
        if outcome.get("ok").and_then(|v| v.as_bool()) == Some(true) {
            break;
        }
        if attempt < 7 {
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }
    if outcome.get("ok").and_then(|v| v.as_bool()) == Some(true) {
        return wait_logged_in(state, win, root, prov, &api_user, 30, true).await;
    }
    let reason = outcome
        .get("reason")
        .and_then(|v| v.as_str())
        .unwrap_or("没有找到 Linux.do / OAuth 登录入口");
    if last_error.is_empty() {
        Err(reason.to_string())
    } else {
        Err(format!("{last_error}；OAuth：{reason}"))
    }
}

async fn wait_logged_in(
    state: &AppState,
    win: &WebviewWindow,
    root: &str,
    prov: &Provider,
    api_user: &str,
    timeout_secs: u64,
    oauth: bool,
) -> Result<bool, String> {
    let site_host = tauri::Url::parse(root)
        .ok()
        .and_then(|u| u.host_str().map(|s| s.to_ascii_lowercase()))
        .unwrap_or_default();
    let started = std::time::Instant::now();
    let mut tick = 0u32;
    loop {
        // 每 2 秒问一次站点「我是谁」，比死盯 cookie 名（session/其它）可靠。
        if tick % 2 == 0 {
            let cookies = cookies_now(win, root).await;
            let info = http_user_info(state, root, prov, &cookie_header(&cookies), api_user).await;
            if info.ok {
                return Ok(true);
            }
        }
        let elapsed = started.elapsed().as_secs();
        if oauth && elapsed >= 12 {
            let url = current_url(win).await;
            let host = tauri::Url::parse(&url)
                .ok()
                .and_then(|u| u.host_str().map(|s| s.to_ascii_lowercase()))
                .unwrap_or_default();
            if !host.is_empty()
                && !site_host.is_empty()
                && host != site_host
                && !host.ends_with(&format!(".{site_host}"))
            {
                return Err(format!(
                    "已跳转到 {host}，需要先在登录窗口完成该平台（如 Linux.do）的登录"
                ));
            }
        }
        if elapsed >= timeout_secs {
            return Ok(false);
        }
        tick += 1;
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

async fn current_url(win: &WebviewWindow) -> String {
    eval_json(win, "location.href")
        .await
        .and_then(|v| v.as_str().map(|s| s.to_string()))
        .unwrap_or_default()
}

fn oauth_login_script(pattern: &str) -> String {
    let pattern = serde_json::to_string(pattern).unwrap_or_else(|_| "\"linux\"".to_string());
    format!(
        r#"(() => {{
  const re = (() => {{ try {{ return new RegExp({pattern}, 'i'); }} catch (e) {{ return /linux/i; }} }})();
  // OAuth 按钮有的用 window.open 开弹窗；塞回本窗口，才能把 session 写进同一个 cookie 罐。
  try {{ window.open = (u) => {{ if (u) location.href = u; return null; }}; }} catch (e) {{}}
  const vis = (el) => {{
    if (!el || !el.isConnected) return false;
    const st = window.getComputedStyle(el);
    if (st.display === 'none' || st.visibility === 'hidden' || parseFloat(st.opacity) === 0) return false;
    const r = el.getBoundingClientRect();
    return r.width > 0 && r.height > 0;
  }};
  const nodes = Array.from(document.querySelectorAll('a[href], button, [role="button"], [role="link"], [onclick]'));
  const desc = (el) => {{
    const img = el.querySelector('img');
    return [
      el.innerText || '',
      el.getAttribute('aria-label') || '',
      el.getAttribute('title') || '',
      el.getAttribute('href') || '',
      typeof el.className === 'string' ? el.className : '',
      img ? (img.getAttribute('alt') || '') : ''
    ].join(' ').replace(/\s+/g, ' ').trim();
  }};
  const match = nodes.find(el => vis(el) && re.test(desc(el)));
  if (!match) return {{ok: false, reason: '没有找到 Linux.do / OAuth 登录入口'}};
  const href = match.getAttribute('href') || '';
  try {{
    if (match.tagName === 'A' && href && href !== '#' && !/^javascript:/i.test(href)) {{
      match.target = '_self';
      location.href = match.href;
    }} else {{
      match.click();
    }}
  }} catch (e) {{
    return {{ok: false, reason: '点击 OAuth 入口失败：' + String(e).slice(0, 80)}};
  }}
  return {{ok: true, hit: desc(match).slice(0, 100)}};
}})()"#,
        pattern = pattern
    )
}

async fn wait_page_ready(win: &WebviewWindow, timeout_secs: u64) -> Value {
    let deadline = std::time::Instant::now() + Duration::from_secs(timeout_secs);
    loop {
        if let Some(v) = eval_json(win, PAGE_STATE_JS).await {
            if v.get("blocked").and_then(|x| x.as_bool()) == Some(true) {
                return v;
            }
            if v.get("ready").and_then(|x| x.as_bool()) == Some(true) {
                return v;
            }
        }
        if std::time::Instant::now() > deadline {
            return Value::Null;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

async fn eval_json(win: &WebviewWindow, js: &str) -> Option<Value> {
    let (tx, rx) = tokio::sync::oneshot::channel::<String>();
    let tx = std::sync::Mutex::new(Some(tx));
    let script = js.to_string();
    win.eval_with_callback(script, move |s| {
        if let Some(tx) = tx.lock().unwrap().take() {
            let _ = tx.send(s);
        }
    })
    .ok()?;
    let raw = tokio::time::timeout(Duration::from_secs(20), rx)
        .await
        .ok()?
        .ok()?;
    serde_json::from_str(&raw).ok()
}

async fn http_user_info(
    state: &AppState,
    root: &str,
    prov: &Provider,
    cookie: &str,
    api_user: &str,
) -> UserInfo {
    let url = format!("{root}{}", prov.user_info_path);
    let mut req = state
        .client
        .get(&url)
        .header("User-Agent", CHROME_UA)
        .header("Accept", "application/json, text/plain, */*")
        .header("Accept-Language", "zh-CN,zh;q=0.9,en;q=0.8")
        .header("Referer", format!("{root}/"))
        .header("Origin", root)
        .timeout(Duration::from_secs(25));
    if !cookie.is_empty() {
        req = req.header("Cookie", cookie);
    }
    if !api_user.is_empty() && !prov.api_user_key.is_empty() {
        req = req.header(&prov.api_user_key, api_user);
    }
    let resp = match req.send().await {
        Ok(r) => r,
        Err(e) => {
            return UserInfo {
                ok: false,
                status: 0,
                id: String::new(),
                quota: 0.0,
                used: 0.0,
                message: format!("连不上 {root}：{e}"),
                blocked: false,
            }
        }
    };
    let status = resp.status().as_u16();
    let text = resp.text().await.unwrap_or_default();
    if looks_blocked(&text) {
        return UserInfo {
            ok: false,
            status,
            id: String::new(),
            quota: 0.0,
            used: 0.0,
            message: "站点要求人机验证（Cloudflare/Turnstile），浏览器会话没有通过".to_string(),
            blocked: true,
        };
    }
    let payload: Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(_) => {
            return UserInfo {
                ok: false,
                status,
                id: String::new(),
                quota: 0.0,
                used: 0.0,
                message: format!("用户信息接口返回的不是 JSON（HTTP {status}），登录可能已失效"),
                blocked: false,
            }
        }
    };
    let success = payload
        .get("success")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let data = payload.get("data").cloned().unwrap_or(Value::Null);
    let obj = if data.is_object() { data } else { payload.clone() };
    let id = obj
        .get("id")
        .map(|v| {
            v.as_str()
                .map(|s| s.to_string())
                .unwrap_or_else(|| v.to_string())
        })
        .unwrap_or_default();
    let quota = obj.get("quota").and_then(|v| v.as_f64()).unwrap_or(0.0);
    let used = obj
        .get("used_quota")
        .or_else(|| obj.get("used"))
        .and_then(|v| v.as_f64())
        .unwrap_or(0.0);
    let message = payload
        .get("msg")
        .or_else(|| payload.get("message"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if !success && id.is_empty() {
        return UserInfo {
            ok: false,
            status,
            id,
            quota,
            used,
            message: if message.is_empty() {
                format!("读取用户信息失败（HTTP {status}）")
            } else {
                message
            },
            blocked: false,
        };
    }
    UserInfo {
        ok: true,
        status,
        id,
        quota,
        used,
        message,
        blocked: false,
    }
}

async fn http_sign_in(
    state: &AppState,
    root: &str,
    path: &str,
    prov: &Provider,
    cookie: &str,
    api_user: &str,
) -> SignInResp {
    let url = format!("{root}{path}");
    let mut req = state
        .client
        .post(&url)
        .header("User-Agent", CHROME_UA)
        .header("Accept", "application/json, text/plain, */*")
        .header("Accept-Language", "zh-CN,zh;q=0.9,en;q=0.8")
        .header("Content-Type", "application/json")
        .header("X-Requested-With", "XMLHttpRequest")
        .header("Referer", format!("{root}/"))
        .header("Origin", root)
        .timeout(Duration::from_secs(25));
    if !cookie.is_empty() {
        req = req.header("Cookie", cookie);
    }
    if !api_user.is_empty() && !prov.api_user_key.is_empty() {
        req = req.header(&prov.api_user_key, api_user);
    }
    let resp = match req.send().await {
        Ok(r) => r,
        Err(e) => {
            return SignInResp {
                status: 0,
                ok: false,
                already: false,
                blocked: false,
                message: format!("连不上 {root}：{e}"),
            }
        }
    };
    let status = resp.status().as_u16();
    let text = resp.text().await.unwrap_or_default();
    if looks_blocked(&text) {
        return SignInResp {
            status,
            ok: false,
            already: false,
            blocked: true,
            message: "站点要求人机验证（Cloudflare/Turnstile）".to_string(),
        };
    }
    let payload: Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(_) => {
            let ok = text.to_ascii_lowercase().contains("success");
            return SignInResp {
                status,
                ok,
                already: false,
                blocked: false,
                message: if ok {
                    "签到成功".to_string()
                } else {
                    format!("签到接口返回的不是 JSON（HTTP {status}）")
                },
            };
        }
    };
    let msg = payload
        .get("msg")
        .or_else(|| payload.get("message"))
        .or_else(|| {
            payload
                .get("data")
                .and_then(|d| d.get("msg").or_else(|| d.get("message")))
        })
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let success = payload.get("ret").and_then(as_i64_like) == Some(1)
        || payload.get("code").and_then(as_i64_like) == Some(0)
        || payload.get("success").and_then(|v| v.as_bool()) == Some(true)
        || payload
            .get("data")
            .and_then(|d| d.get("success"))
            .and_then(|v| v.as_bool())
            == Some(true);
    let low = msg.to_ascii_lowercase();
    let already = status == 409
        || low.contains("already")
        || msg.contains("已经签到")
        || msg.contains("已签到")
        || msg.contains("重复签到");
    SignInResp {
        status,
        ok: success && !already,
        already,
        blocked: false,
        message: msg,
    }
}

fn as_i64_like(v: &Value) -> Option<i64> {
    v.as_i64()
        .or_else(|| v.as_str().and_then(|s| s.parse::<i64>().ok()))
}

fn looks_blocked(text: &str) -> bool {
    let low = text.to_ascii_lowercase();
    low.contains("just a moment")
        || low.contains("__cf_chl_")
        || low.contains("turnstile")
        || low.contains("attention required")
        || low.contains("access denied")
        || low.contains("verify you are human")
        || text.contains("请进行验证")
        || text.contains("访问受限")
        || text.contains("为了更好的访问体验")
}

fn round2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
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

const PAGE_STATE_JS: &str = r#"(() => {
  const vis = (el) => {
    if (!el || !el.isConnected) return false;
    const st = window.getComputedStyle(el);
    if (st.display === 'none' || st.visibility === 'hidden' || parseFloat(st.opacity) === 0) return false;
    const r = el.getBoundingClientRect();
    return r.width > 0 && r.height > 0;
  };
  const text = (document.body && document.body.innerText || '');
  const blocked = /请进行验证|为了更好的访问体验|访问受限|Access denied|verify you are human/i.test(text);
  const hasUser = ['#username','input[name="username"]','input[name="email"]','input[type="email"]'].some(s => vis(document.querySelector(s)));
  const ready = document.readyState !== 'loading' &&
    (hasUser || !!document.querySelector('.semi-card, form, button') || text.trim().length > 30);
  return {ready, blocked, hasUser, url: location.href, title: document.title || '', text: text.slice(0, 200)};
})()"#;

fn login_script(email: &str, password: &str) -> String {
    let email = serde_json::to_string(email).unwrap_or_else(|_| "\"\"".to_string());
    let password = serde_json::to_string(password).unwrap_or_else(|_| "\"\"".to_string());
    format!(
        r#"(() => {{
  const email = {email};
  const password = {password};
  const vis = (el) => {{
    if (!el || !el.isConnected) return false;
    const st = window.getComputedStyle(el);
    if (st.display === 'none' || st.visibility === 'hidden' || parseFloat(st.opacity) === 0) return false;
    const r = el.getBoundingClientRect();
    return r.width > 0 && r.height > 0;
  }};
  const setVal = (el, v) => {{
    const setter = Object.getOwnPropertyDescriptor(window.HTMLInputElement.prototype, 'value')?.set;
    if (setter) setter.call(el, v); else el.value = v;
    el.dispatchEvent(new Event('input', {{bubbles: true}}));
    el.dispatchEvent(new Event('change', {{bubbles: true}}));
  }};
  const userSels = ['#username', 'input[name="username"]', 'input[name="email"]', 'input[type="email"]'];
  const pwdSels = ['#password', 'input[name="password"]', 'input[type="password"]'];
  const findUser = () => {{
    for (const s of userSels) {{
      const el = document.querySelector(s);
      if (vis(el)) return el;
    }}
    return null;
  }};
  for (const s of ['.semi-modal-close', '.ant-modal-close', '[aria-label="Close"]']) {{
    document.querySelectorAll(s).forEach(b => {{ if (vis(b)) {{ try {{ b.click(); }} catch (e) {{}} }} }});
  }}
  try {{ document.dispatchEvent(new KeyboardEvent('keydown', {{key: 'Escape', bubbles: true}})); }} catch (e) {{}}
  if (!findUser()) {{
    const entries = [
      '.semi-card button:has(.semi-icon-mail)',
      '.semi-card button:has([aria-label="mail"])',
      'button:has(.semi-icon-mail)'
    ];
    for (const s of entries) {{
      for (const b of document.querySelectorAll(s)) {{
        if (!vis(b)) continue;
        try {{ b.click(); }} catch (e) {{}}
        if (findUser()) break;
      }}
      if (findUser()) break;
    }}
    if (!findUser()) {{
      for (const t of document.querySelectorAll('.semi-card .semi-tabs-tab')) {{
        if (!vis(t)) continue;
        try {{ t.click(); }} catch (e) {{}}
        if (findUser()) break;
      }}
    }}
  }}
  const user = findUser();
  if (!user) return {{ok: false, reason: '没有找到邮箱输入框（可能仍在人机验证，或页面没加载完）'}};
  let pwd = null;
  for (const s of pwdSels) {{
    const el = document.querySelector(s);
    if (vis(el)) {{ pwd = el; break; }}
  }}
  if (!pwd) return {{ok: false, reason: '没有找到密码输入框'}};
  setVal(user, email);
  setVal(pwd, password);
  const form = user.closest('form') || document;
  let submit = form.querySelector('button[type="submit"]');
  if (!submit) {{
    const re = /^(登录|登 录|Login|Sign in|Sign In|继续|Continue)$/;
    submit = [...document.querySelectorAll('button')].find(b => vis(b) && re.test((b.innerText || '').trim()));
  }}
  if (!submit) return {{ok: false, reason: '没有找到登录按钮'}};
  submit.click();
  return {{ok: true}};
}})()"#,
        email = email,
        password = password
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn up(base: &str) -> Upstream {
        Upstream {
            base_url: base.to_string(),
            ..Default::default()
        }
    }

    fn cfg(v: Value) -> Map<String, Value> {
        v.as_object().cloned().unwrap_or_default()
    }

    #[test]
    fn provider_detects_builtin_sites() {
        let empty = Map::new();
        let any = provider_of(&up("https://anyrouter.top/v1"), &empty);
        assert_eq!(any.name, "anyrouter");
        assert_eq!(any.sign_in_path.as_deref(), Some("/api/user/sign_in"));
        assert!(!any.auto_trigger);
        assert_eq!(any.user_info_path, "/api/user/self");
        assert_eq!(any.api_user_key, "new-api-user");

        let agent = provider_of(&up("https://agentrouter.org"), &empty);
        assert_eq!(agent.name, "agentrouter");
        assert!(agent.auto_trigger);
        assert!(agent.sign_in_path.is_none());
    }

    #[test]
    fn provider_honours_overrides_and_site_root() {
        let u = up("https://api.example.com/v1");
        let c = cfg(json!({
            "provider": "custom",
            "site_root": "https://panel.example.com/",
            "sign_in_path": "/api/user/sign_in",
            "login_path": "/signin",
            "api_user_key": "X-User"
        }));
        let p = provider_of(&u, &c);
        assert_eq!(checkin_site_root(&u, &c), "https://panel.example.com");
        assert_eq!(p.login_path, "/signin");
        assert_eq!(p.sign_in_path.as_deref(), Some("/api/user/sign_in"));
        assert_eq!(p.api_user_key, "X-User");
    }

    #[test]
    fn browser_mode_is_configured_without_token() {
        let mut u = up("https://anyrouter.top");
        u.extra.insert(
            "checkin".to_string(),
            json!({"auth": "browser", "enabled": true}),
        );
        assert!(configured(&u));
        assert!(checkin_enabled(&u));
        assert_eq!(auth_of(&checkin_cfg(&u)), "browser");
        assert!(!configured(&up("https://anyrouter.top")));
    }

    #[test]
    fn sign_in_candidates_builtin_vs_custom() {
        let empty = Map::new();
        let any = provider_of(&up("https://anyrouter.top"), &empty);
        assert_eq!(
            sign_in_candidates(&any, &empty),
            vec!["/api/user/sign_in".to_string()]
        );

        let custom = provider_of(&up("https://example.com"), &empty);
        let got = sign_in_candidates(&custom, &empty);
        assert!(got.contains(&"/api/user/self/checkin".to_string()));
        assert!(got.contains(&"/api/user/checkin".to_string()));

        let learned = cfg(json!({"path": "/api/user/self/checkin"}));
        let got = sign_in_candidates(&custom, &learned);
        assert_eq!(
            got.first().map(String::as_str),
            Some("/api/user/self/checkin")
        );
    }

    #[test]
    fn numeric_and_block_detection_helpers() {
        assert_eq!(as_i64_like(&json!(1)), Some(1));
        assert_eq!(as_i64_like(&json!("0")), Some(0));
        assert_eq!(as_i64_like(&json!(null)), None);
        assert!(looks_blocked("<title>Just a moment...</title>"));
        assert!(looks_blocked("Access denied"));
        assert!(!looks_blocked("{\"success\":true}"));
        assert_eq!(round2(1.2345), 1.23);
    }

    #[test]
    fn membership_is_explicit() {
        let mut u = up("https://anyrouter.top");
        assert!(!listed(&u));
        u.extra.insert("checkin".to_string(), json!({}));
        assert!(listed(&u));
    }

    #[test]
    fn cookie_header_and_labels() {
        assert_eq!(
            cookie_header(&[
                ("acw_tc".to_string(), "1".to_string()),
                ("session".to_string(), "abc".to_string())
            ]),
            "acw_tc=1; session=abc"
        );
        assert_eq!(safe_label("a/b:c d"), "a-b-c-d");
        assert_eq!(safe_label(""), "up");
    }

    #[test]
    fn chrome_mode_needs_cookies() {
        let mut u = up("https://anyrouter.top");
        u.extra.insert(
            "checkin".to_string(),
            json!({"auth": "chrome", "enabled": true}),
        );
        assert_eq!(auth_of(&checkin_cfg(&u)), "chrome");
        assert!(!configured(&u));
        u.extra.insert(
            "checkin".to_string(),
            json!({"auth": "chrome", "enabled": true, "cookies": {"session": "x"}}),
        );
        assert!(configured(&u));
        assert_eq!(stored_cookies(&checkin_cfg(&u)).len(), 1);
    }

    #[test]
    fn chrome_cookie_extraction_filters_site_and_expiry() {
        assert!(domain_matches(".anyrouter.top", "anyrouter.top"));
        assert!(domain_matches(".anyrouter.top", "www.anyrouter.top"));
        assert!(!domain_matches(".linux.do", "anyrouter.top"));
        let cookies = vec![
            json!({"name":"session","value":"s","domain":".anyrouter.top","path":"/","expires":-1}),
            json!({"name":"acw_tc","value":"a","domain":"anyrouter.top","path":"/","expires":0}),
            json!({"name":"linuxdo","value":"l","domain":".linux.do","path":"/","expires":-1}),
            json!({"name":"old","value":"o","domain":".anyrouter.top","path":"/","expires":1}),
        ];
        let got = extract_site_cookies(&cookies, "anyrouter.top");
        assert!(got.iter().any(|(k, v)| k == "session" && v == "s"));
        assert!(got.iter().any(|(k, _)| k == "acw_tc"));
        assert!(!got.iter().any(|(k, _)| k == "linuxdo"));
        assert!(!got.iter().any(|(k, _)| k == "old"));
    }

    #[test]
    fn chrome_host_and_cookie_storage() {
        assert_eq!(host_of("https://anyrouter.top/v1"), "anyrouter.top");
        assert_eq!(host_of("not a url"), "");
        let c = cfg(json!({"cookies":{"session":"s","acw_tc":"a"}}));
        assert_eq!(stored_cookies(&c).len(), 2);
    }
}
