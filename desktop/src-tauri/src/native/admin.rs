use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};
use tauri::Manager;

use super::journal;
use super::proxy;
use super::state::{snapshot, AppState, ModelRoute, RouteEntry, Upstream};
use super::stats;

fn ok(v: Value) -> Response {
    Json(v).into_response()
}

fn bad(msg: &str) -> Response {
    (StatusCode::BAD_REQUEST, Json(json!({"ok": false, "error": msg}))).into_response()
}

fn not_found(msg: &str) -> Response {
    (StatusCode::NOT_FOUND, Json(json!({"ok": false, "error": msg}))).into_response()
}

pub async fn stats_handler(
    State(state): State<Arc<AppState>>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let mut snap = snapshot(&state);
    let stats_data = stats::load(&state.stats_path());
    let range = q.get("range").cloned().unwrap_or_default();
    snap["total_requests"] = json!(stats_data
        .get("totals")
        .and_then(|v| v.get("calls"))
        .and_then(|v| v.as_i64())
        .unwrap_or(0));
    snap["stats"] = stats::meta(&stats_data);
    snap["stats_range"] = stats::render(&stats::range_bucket(&stats_data, &range), &range);
    ok(snap)
}

pub async fn upstream_add(State(state): State<Arc<AppState>>, Json(p): Json<Value>) -> Response {
    let name = p.get("name").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
    let base_url = p.get("base_url").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
    if base_url.is_empty() {
        return bad("地址不能为空");
    }
    let key = p.get("api_key").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
    let id = format!("u{}", &uuid::Uuid::new_v4().simple().to_string()[..8]);
    let up = Upstream {
        id: id.clone(),
        name: if name.is_empty() { base_url.clone() } else { name },
        base_url,
        api_keys: if key.is_empty() { vec![] } else { vec![key] },
        selected_models: Some(vec![]),
        enabled: true,
        ..Default::default()
    };
    {
        let mut file = state.file.write().unwrap();
        file.upstreams.push(up.clone());
    }
    let _ = state.save();
    ok(json!({"ok": true, "upstream": {"id": id}}))
}

pub async fn upstream_update(State(state): State<Arc<AppState>>, Json(p): Json<Value>) -> Response {
    let id = p.get("id").and_then(|v| v.as_str()).unwrap_or("");
    if id.is_empty() {
        return bad("缺少 id");
    }
    {
        let mut file = state.file.write().unwrap();
        let Some(up) = file.find_upstream_mut(id) else {
            return not_found("找不到这个上游");
        };
        if let Some(v) = p.get("name").and_then(|v| v.as_str()) {
            up.name = v.to_string();
        }
        if let Some(v) = p.get("base_url").and_then(|v| v.as_str()) {
            if !v.is_empty() {
                up.base_url = v.to_string();
            }
        }
        if let Some(v) = p.get("enabled").and_then(|v| v.as_bool()) {
            up.enabled = v;
        }
        if let Some(v) = p.get("api_mode").and_then(|v| v.as_str()) {
            up.api_mode = Some(v.to_string());
        }
        if let Some(v) = p.get("api_keys").and_then(|v| v.as_array()) {
            up.api_keys = v
                .iter()
                .filter_map(|x| x.get("value").and_then(|v| v.as_str()).map(|s| s.to_string()).or_else(|| x.as_str().map(|s| s.to_string())))
                .collect();
        }
        if let Some(v) = p.get("first_timeout_seconds").and_then(|v| v.as_f64()) {
            up.first_timeout_seconds = Some(v);
        }
        if let Some(v) = p.get("connect_timeout_seconds").and_then(|v| v.as_f64()) {
            up.connect_timeout_seconds = Some(v);
        }
        if let Some(v) = p.get("key_strategy").and_then(|v| v.as_str()) {
            up.key_strategy = v.to_string();
        }
        if let Some(v) = p.get("user_agent").and_then(|v| v.as_str()) {
            up.user_agent = v.to_string();
        }
    }
    let _ = state.save();
    ok(json!({"ok": true}))
}

pub async fn upstream_delete(State(state): State<Arc<AppState>>, Json(p): Json<Value>) -> Response {
    let id = p.get("id").and_then(|v| v.as_str()).unwrap_or("");
    {
        let mut file = state.file.write().unwrap();
        file.upstreams.retain(|u| u.id != id);
        for route in file.models.values_mut() {
            route.entries.retain(|e| e.upstream != id);
        }
        file.models.retain(|_, r| !r.entries.is_empty());
    }
    let _ = state.save();
    ok(json!({"ok": true}))
}

pub async fn fetch_models(State(state): State<Arc<AppState>>, Json(p): Json<Value>) -> Response {
    let ids: Vec<String> = p
        .get("ids")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|x| x.as_str().map(|s| s.to_string())).collect())
        .unwrap_or_default();
    let targets: Vec<Upstream> = {
        let file = state.file.read().unwrap();
        file.upstreams
            .iter()
            .filter(|u| ids.is_empty() || ids.contains(&u.id))
            .cloned()
            .collect()
    };
    let mut results = Vec::new();
    for up in targets {
        let url = super::proxy::models_url(&up.base_url);
        let timeout = up.first_timeout_seconds.unwrap_or(30.0).clamp(5.0, 60.0);
        let mut req = state
            .client
            .get(&url)
            .timeout(std::time::Duration::from_secs_f64(timeout))
            .header("Accept", "application/json");
        if let Some(k) = up.api_keys.first() {
            req = req.header("authorization", format!("Bearer {k}"));
        }
        let result = match req.send().await {
            Ok(resp) => {
                let status = resp.status();
                let text = resp.text().await.unwrap_or_default();
                if status.is_success() {
                    match serde_json::from_str::<Value>(&text) {
                        Ok(v) => {
                            let mut models: Vec<String> = Vec::new();
                            if let Some(arr) = v.get("data").and_then(|x| x.as_array()) {
                                for item in arr {
                                    if let Some(id) = item.get("id").and_then(|x| x.as_str()) {
                                        models.push(id.to_string());
                                    }
                                }
                            }
                            models.sort();
                            models.dedup();
                            {
                                let mut file = state.file.write().unwrap();
                                if let Some(u) = file.find_upstream_mut(&up.id) {
                                    u.available_models = models.clone();
                                    u.extra.insert("models_fetched_at".into(), json!(now_secs()));
                                    u.extra.insert("models_error".into(), Value::Null);
                                    let selected = u.selected();
                                    let keep: Vec<String> = selected.into_iter().filter(|m| models.contains(m)).collect();
                                    u.selected_models = Some(keep);
                                }
                            }
                            let _ = state.save();
                            json!({"id": up.id, "name": up.name, "ok": true, "models": models, "count": models.len()})
                        }
                        Err(e) => json!({"id": up.id, "name": up.name, "ok": false, "error": format!("返回不是合法 JSON: {e}")}),
                    }
                } else {
                    json!({"id": up.id, "name": up.name, "ok": false, "status": status.as_u16(), "error": format!("HTTP {}: {}", status, truncate(&text, 300))})
                }
            }
            Err(e) => json!({"id": up.id, "name": up.name, "ok": false, "error": e.to_string()}),
        };
        results.push(result);
    }
    ok(json!({"ok": true, "results": results}))
}

pub async fn select_models(State(state): State<Arc<AppState>>, Json(p): Json<Value>) -> Response {
    let id = p.get("id").and_then(|v| v.as_str()).unwrap_or("");
    let all = p.get("all").and_then(|v| v.as_bool()).unwrap_or(false);
    let none = p.get("none").and_then(|v| v.as_bool()).unwrap_or(false);
    let models: Vec<String> = p
        .get("models")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|x| x.as_str().map(|s| s.to_string())).collect())
        .unwrap_or_default();
    {
        let mut file = state.file.write().unwrap();
        let Some(up) = file.find_upstream_mut(id) else {
            return not_found("找不到这个上游");
        };
        let allowed: Vec<String> = if all {
            up.available_models.clone()
        } else if none {
            vec![]
        } else {
            models.into_iter().filter(|m| up.available_models.contains(m)).collect()
        };
        up.selected_models = Some(allowed.clone());
        let _ = allowed;
    }
    let _ = state.save();
    ok(json!({"ok": true}))
}

pub async fn set_group(State(state): State<Arc<AppState>>, Json(p): Json<Value>) -> Response {
    let id = p.get("id").and_then(|v| v.as_str()).unwrap_or("");
    let Some(mapping) = p.get("mapping").and_then(|v| v.as_object()) else {
        return bad("mapping 必须是对象");
    };
    {
        let mut file = state.file.write().unwrap();
        let Some(up) = file.find_upstream_mut(id) else {
            return not_found("找不到这个上游");
        };
        for (k, v) in mapping {
            if let Some(g) = v.as_str() {
                if g.is_empty() || g == k {
                    up.model_groups.remove(k);
                } else {
                    up.model_groups.insert(k.clone(), g.to_string());
                }
            }
        }
    }
    let _ = state.save();
    ok(json!({"ok": true}))
}

pub async fn model_save(State(state): State<Arc<AppState>>, Json(p): Json<Value>) -> Response {
    let name = p.get("name").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
    if name.is_empty() {
        return bad("缺少模型名");
    }
    let strategy = p.get("strategy").and_then(|v| v.as_str()).unwrap_or("priority").to_string();
    let entries: Vec<RouteEntry> = p
        .get("entries")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|e| {
                    let up = e.get("upstream").and_then(|v| v.as_str())?.to_string();
                    let model = e.get("model").and_then(|v| v.as_str()).unwrap_or(&name).to_string();
                    Some(RouteEntry {
                        upstream: up,
                        model,
                        enabled: e.get("enabled").and_then(|v| v.as_bool()).unwrap_or(true),
                        quota: e.get("quota").and_then(|v| v.as_i64()),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    if entries.is_empty() {
        return bad("entries 不能为空");
    }
    {
        let mut file = state.file.write().unwrap();
        file.models.insert(name.clone(), ModelRoute { strategy, entries, orderings: Default::default(), extra: Default::default() });
    }
    let _ = state.save();
    ok(json!({"ok": true, "model": {"name": name}}))
}

pub async fn model_delete(State(state): State<Arc<AppState>>, Json(p): Json<Value>) -> Response {
    let name = p.get("name").and_then(|v| v.as_str()).unwrap_or("");
    {
        let mut file = state.file.write().unwrap();
        file.models.remove(name);
    }
    let _ = state.save();
    ok(json!({"ok": true}))
}

pub async fn model_quota(State(state): State<Arc<AppState>>, Json(p): Json<Value>) -> Response {
    let name = p.get("name").and_then(|v| v.as_str()).unwrap_or("");
    let items = p.get("entries").and_then(|v| v.as_array()).cloned().unwrap_or_default();
    {
        let mut file = state.file.write().unwrap();
        let Some(route) = file.models.get_mut(name) else {
            return not_found("找不到这个分组");
        };
        for item in items {
            let up = item.get("upstream").and_then(|v| v.as_str()).unwrap_or("");
            let model = item.get("model").and_then(|v| v.as_str());
            for e in route.entries.iter_mut() {
                if e.upstream == up && model.map(|m| m == e.model).unwrap_or(true) {
                    e.quota = item.get("quota").and_then(|v| v.as_i64());
                }
            }
        }
    }
    let _ = state.save();
    ok(json!({"ok": true}))
}

pub async fn model_orderings(State(state): State<Arc<AppState>>, Json(p): Json<Value>) -> Response {
    let name = p.get("name").and_then(|v| v.as_str()).unwrap_or("");
    let file = state.file.read().unwrap();
    let Some(route) = file.models.get(name) else {
        return not_found("找不到这个分组");
    };
    let orderings: Vec<Value> = route.orderings.iter().map(|(label, members)| json!({"label": label, "members": members})).collect();
    ok(json!({"ok": true, "name": name, "active": "", "orderings": orderings, "entries": route.entries}))
}

pub async fn order_save(State(state): State<Arc<AppState>>, Json(p): Json<Value>) -> Response {
    let name = p.get("name").and_then(|v| v.as_str()).unwrap_or("");
    let label = p.get("label").and_then(|v| v.as_str()).unwrap_or("");
    if label.is_empty() {
        return bad("缺少排序名");
    }
    {
        let mut file = state.file.write().unwrap();
        let Some(route) = file.models.get_mut(name) else {
            return not_found("找不到这个分组");
        };
        if let Some(order) = p.get("order").and_then(|v| v.as_array()) {
            let mut new_entries = Vec::new();
            for item in order {
                if let (Some(up), Some(model)) = (
                    item.get("upstream").and_then(|v| v.as_str()),
                    item.get("model").and_then(|v| v.as_str()),
                ) {
                    if let Some(e) = route.entries.iter().find(|e| e.upstream == up && e.model == model) {
                        new_entries.push(e.clone());
                    }
                }
            }
            if !new_entries.is_empty() {
                route.entries = new_entries;
            }
        }
        let members: Vec<Value> = route.entries.iter().map(|e| json!({"upstream": e.upstream, "model": e.model})).collect();
        route.orderings.insert(label.to_string(), json!(members));
    }
    let _ = state.save();
    ok(json!({"ok": true}))
}

pub async fn order_apply(State(state): State<Arc<AppState>>, Json(p): Json<Value>) -> Response {
    let name = p.get("name").and_then(|v| v.as_str()).unwrap_or("");
    let label = p.get("label").and_then(|v| v.as_str()).unwrap_or("");
    {
        let mut file = state.file.write().unwrap();
        let Some(route) = file.models.get_mut(name) else {
            return not_found("找不到这个分组");
        };
        let Some(members) = route.orderings.get(label).cloned() else {
            return not_found("没有这套排序");
        };
        let mut new_entries = Vec::new();
        if let Some(arr) = members.as_array() {
            for item in arr {
                if let (Some(up), Some(model)) = (
                    item.get("upstream").and_then(|v| v.as_str()),
                    item.get("model").and_then(|v| v.as_str()),
                ) {
                    if let Some(e) = route.entries.iter().find(|e| e.upstream == up && e.model == model) {
                        new_entries.push(e.clone());
                    }
                }
            }
        }
        if !new_entries.is_empty() {
            route.entries = new_entries;
        }
    }
    let _ = state.save();
    ok(json!({"ok": true}))
}

pub async fn order_delete(State(state): State<Arc<AppState>>, Json(p): Json<Value>) -> Response {
    let name = p.get("name").and_then(|v| v.as_str()).unwrap_or("");
    let label = p.get("label").and_then(|v| v.as_str()).unwrap_or("");
    {
        let mut file = state.file.write().unwrap();
        if let Some(route) = file.models.get_mut(name) {
            route.orderings.remove(label);
        }
    }
    let _ = state.save();
    ok(json!({"ok": true}))
}

pub async fn window_behavior(State(state): State<Arc<AppState>>) -> Response {
    // webui 加载时会调用这里；此时窗口一定已经创建，顺手强制无边框，
    // 避免个别 Windows/打包组合忽略启动配置里的 decorations=false。
    if let Some(w) = main_window(&state) {
        #[cfg(windows)]
        crate::force_borderless(&w);
        #[cfg(not(windows))]
        let _ = w.set_decorations(false);
    }
    let behavior = state
        .file
        .read()
        .unwrap()
        .window_behavior
        .clone()
        .unwrap_or_else(|| "tray".to_string());
    let (config_windows, open_windows) = state
        .app
        .lock()
        .ok()
        .and_then(|app| app.as_ref().map(|a| {
            let cfg = a
                .config()
                .app
                .windows
                .iter()
                .map(|w| json!({"label": w.label, "decorations": w.decorations}))
                .collect::<Vec<_>>();
            (cfg, a.webview_windows().len())
        }))
        .unwrap_or((vec![], 0));
    ok(json!({"ok": true, "behavior": behavior, "native": true,
        "found": main_window(&state).is_some(),
        "config_windows": config_windows, "open_windows": open_windows}))
}

fn main_window(state: &AppState) -> Option<tauri::WebviewWindow> {
    state
        .app
        .lock()
        .ok()
        .and_then(|app| app.as_ref().and_then(|a| a.get_webview_window("main")))
}

pub async fn window_minimize(State(state): State<Arc<AppState>>) -> Response {
    let found = main_window(&state).is_some();
    if let Some(w) = main_window(&state) {
        let _ = w.minimize();
    }
    ok(json!({"ok": true, "found": found}))
}

pub async fn window_toggle_maximize(State(state): State<Arc<AppState>>) -> Response {
    if let Some(w) = main_window(&state) {
        if w.is_maximized().unwrap_or(false) {
            let _ = w.unmaximize();
        } else {
            let _ = w.maximize();
        }
    }
    ok(json!({"ok": true}))
}

pub async fn window_drag(State(state): State<Arc<AppState>>) -> Response {
    if let Some(w) = main_window(&state) {
        let _ = w.start_dragging();
    }
    ok(json!({"ok": true}))
}

pub async fn window_close(State(state): State<Arc<AppState>>) -> Response {
    if let Some(w) = main_window(&state) {
        let _ = w.close();
    }
    ok(json!({"ok": true}))
}

pub async fn set_window_behavior(
    State(state): State<Arc<AppState>>,
    Json(p): Json<Value>,
) -> Response {
    let behavior = p.get("behavior").and_then(|v| v.as_str()).unwrap_or("");
    if behavior != "tray" && behavior != "minimize" && behavior != "close" {
        return bad("behavior 只能是 tray、minimize 或 close");
    }
    {
        let mut file = state.file.write().unwrap();
        file.window_behavior = Some(behavior.to_string());
    }
    let _ = state.save();
    ok(json!({"ok": true, "behavior": behavior}))
}

pub async fn defaults(State(state): State<Arc<AppState>>, Json(p): Json<Value>) -> Response {    {
        let mut file = state.file.write().unwrap();
        if !file.defaults.is_object() {
            file.defaults = json!({});
        }
        if let Some(v) = p.get("first_timeout_seconds") {
            file.defaults["first_timeout_seconds"] = v.clone();
        }
        if let Some(v) = p.get("connect_timeout_seconds") {
            file.defaults["connect_timeout_seconds"] = v.clone();
        }
    }
    let _ = state.save();
    ok(json!({"ok": true}))
}

pub async fn speedtest(State(state): State<Arc<AppState>>, Json(p): Json<Value>) -> Response {
    let tests: Vec<(String, Option<String>)> = p
        .get("tests")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|t| {
                    let id = t.get("id").and_then(|v| v.as_str())?.to_string();
                    Some((id, t.get("model").and_then(|v| v.as_str()).map(|s| s.to_string())))
                })
                .collect()
        })
        .unwrap_or_default();
    let ids: Vec<String> = p
        .get("ids")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|x| x.as_str().map(|s| s.to_string())).collect())
        .unwrap_or_default();
    let targets: Vec<(Upstream, Option<String>)> = {
        let file = state.file.read().unwrap();
        file.upstreams
            .iter()
            .filter(|u| {
                if !tests.is_empty() {
                    tests.iter().any(|(id, _)| id == &u.id)
                } else if !ids.is_empty() {
                    ids.contains(&u.id)
                } else {
                    u.enabled
                }
            })
            .map(|u| {
                let model = tests.iter().find(|(id, _)| id == &u.id).and_then(|(_, m)| m.clone());
                (u.clone(), model)
            })
            .collect()
    };
    let mut results = Vec::new();
    for (up, model) in targets {
        let m = model
            .or_else(|| up.selected().into_iter().next())
            .unwrap_or_else(|| up.available_models.first().cloned().unwrap_or_default());
        if m.is_empty() {
            results.push(json!({"id": up.id, "name": up.name, "ok": false, "error": "没有可用模型"}));
            continue;
        }
        results.push(proxy::ping_upstream(&state, &up, &m).await);
    }
    ok(json!({"ok": true, "results": results}))
}

pub async fn journal_index(State(state): State<Arc<AppState>>) -> Response {
    ok(journal::index(&state))
}

pub async fn journal_query(
    State(state): State<Arc<AppState>>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    ok(journal::query(&state, &q))
}

pub async fn proxy_save(State(state): State<Arc<AppState>>, Json(p): Json<Value>) -> Response {
    {
        let mut file = state.file.write().unwrap();
        file.proxy_settings = p;
    }
    let _ = state.save();
    ok(json!({"ok": true}))
}

pub async fn proxy_detect() -> Response {
    ok(json!({"ok": true, "found": [], "hint": "原生版暂未实现本机代理探测"}))
}

pub async fn net_bench() -> Response {
    ok(json!({"ok": true, "results": []}))
}

pub async fn net_auto() -> Response {
    ok(json!({"ok": true, "changed": [], "kept": [], "results": []}))
}

pub async fn balance() -> Response {
    ok(json!({"ok": true, "results": []}))
}

pub async fn checkin_view(State(state): State<Arc<AppState>>) -> Response {
    ok(json!({
        "rows": super::checkin::rows(&state),
        "recent": super::checkin::recent(&state, 200),
        "settings": super::checkin::settings(&state),
        "summary": super::checkin::summary(&state),
    }))
}

pub async fn checkin_run(
    State(state): State<Arc<AppState>>,
    Json(p): Json<Value>,
) -> Response {
    let ids: Vec<String> = p
        .get("ids")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|x| x.as_str().map(|s| s.to_string())).collect())
        .unwrap_or_default();
    let probe = p.get("probe").and_then(|v| v.as_bool()).unwrap_or(false);
    let results = super::checkin::run(state.clone(), ids, !probe).await;
    ok(json!({
        "ok": true,
        "results": results,
        "probe": probe,
        "summary": super::checkin::summary(&state),
        "recent": super::checkin::recent(&state, 200),
    }))
}

pub async fn checkin_config(
    State(state): State<Arc<AppState>>,
    Json(p): Json<Value>,
) -> Response {
    let id = p.get("id").and_then(|v| v.as_str()).unwrap_or("");
    if id.is_empty() {
        return bad("缺少 id");
    }
    {
        let mut file = state.file.write().unwrap();
        let Some(up) = file.find_upstream_mut(id) else {
            return not_found("找不到这个上游");
        };
        let mut cfg = up
            .extra
            .get("checkin")
            .and_then(|v| v.as_object())
            .cloned()
            .unwrap_or_default();
        let old_token = cfg.get("token").and_then(|v| v.as_str()).unwrap_or("").to_string();
        if let Some(token) = p.get("token").and_then(|v| v.as_str()) {
            if token != "__keep__" {
                let token = token.trim().to_string();
                if token != old_token {
                    if token.is_empty() {
                        cfg.remove("token");
                    } else {
                        cfg.insert("token".to_string(), json!(token));
                    }
                    cfg.remove("path");
                    up.extra.remove("checkin_state");
                }
            }
        }
        if let Some(uid) = p.get("user_id").and_then(|v| v.as_str()) {
            cfg.insert("user_id".to_string(), json!(uid.trim()));
        }
        let has_token = cfg
            .get("token")
            .and_then(|v| v.as_str())
            .map(|s| !s.is_empty())
            .unwrap_or(false);
        if let Some(enabled) = p.get("enabled").and_then(|v| v.as_bool()) {
            cfg.insert("enabled".to_string(), json!(enabled && has_token));
        } else if has_token && !cfg.contains_key("enabled") {
            cfg.insert("enabled".to_string(), json!(true));
        }
        if !has_token {
            cfg.insert("enabled".to_string(), json!(false));
        }
        up.extra.insert("checkin".to_string(), Value::Object(cfg));
    }
    let _ = state.save();
    ok(json!({"ok": true, "rows": super::checkin::rows(&state)}))
}

pub async fn checkin_settings(
    State(state): State<Arc<AppState>>,
    Json(p): Json<Value>,
) -> Response {
    {
        let mut file = state.file.write().unwrap();
        let mut s = file
            .checkin_settings
            .as_object()
            .cloned()
            .unwrap_or_default();
        s.insert(
            "enabled".to_string(),
            json!(p.get("enabled").and_then(|v| v.as_bool()).unwrap_or(false)),
        );
        s.insert(
            "at".to_string(),
            json!(p.get("at").and_then(|v| v.as_str()).unwrap_or("08:00")),
        );
        s.insert(
            "random_minutes".to_string(),
            json!(p
                .get("random_minutes")
                .and_then(|v| v.as_i64())
                .unwrap_or(30)
                .clamp(0, 240)),
        );
        file.checkin_settings = Value::Object(s);
    }
    // 改了签到时间：今天的随机计划作废，下一拍按新时刻重算。
    *state.checkin_plan.lock().unwrap() = None;
    let _ = state.save();
    ok(json!({"ok": true, "settings": super::checkin::settings(&state)}))
}

pub async fn versions() -> Response {
    ok(json!({"ok": true, "versions": []}))
}

pub async fn admin_log() -> Response {
    ok(json!({"ok": true, "log": []}))
}

pub async fn journal_files() -> Response {
    ok(json!({"ok": true, "files": []}))
}

pub async fn auto_configure(State(state): State<Arc<AppState>>, Json(_p): Json<Value>) -> Response {
    let mut created = Vec::new();
    {
        let mut file = state.file.write().unwrap();
        let upstreams = file.upstreams.clone();
        for up in upstreams {
            for real in up.selected() {
                let group = up.group_of(&real);
                if file.models.contains_key(&group) {
                    continue;
                }
                let mut entries = Vec::new();
                for other in &file.upstreams {
                    for m in other.real_models_in(&group) {
                        entries.push(RouteEntry {
                            upstream: other.id.clone(),
                            model: m,
                            enabled: true,
                            quota: None,
                        });
                    }
                }
                if !entries.is_empty() {
                    file.models.insert(group.clone(), ModelRoute {
                        strategy: "priority".to_string(),
                        entries,
                        orderings: Default::default(),
                        extra: Default::default(),
                    });
                    created.push(group);
                }
            }
        }
    }
    let _ = state.save();
    ok(json!({"ok": true, "created": created, "skipped": []}))
}

pub async fn rename_group(State(state): State<Arc<AppState>>, Json(p): Json<Value>) -> Response {
    let from = p.get("from").and_then(|v| v.as_str()).unwrap_or("");
    let to = p.get("to").and_then(|v| v.as_str()).unwrap_or("");
    if from.is_empty() || to.is_empty() {
        return bad("from/to 不能为空");
    }
    {
        let mut file = state.file.write().unwrap();
        if let Some(route) = file.models.remove(from) {
            let entries = route.entries.iter().map(|e| RouteEntry {
                upstream: e.upstream.clone(),
                model: if e.model == from { to.to_string() } else { e.model.clone() },
                enabled: e.enabled,
                quota: e.quota,
            }).collect();
            file.models.insert(to.to_string(), ModelRoute {
                strategy: route.strategy,
                entries,
                orderings: route.orderings,
                extra: route.extra,
            });
        }
        for up in file.upstreams.iter_mut() {
            for value in up.model_groups.values_mut() {
                if value == from {
                    *value = to.to_string();
                }
            }
        }
    }
    let _ = state.save();
    ok(json!({"ok": true, "from": from, "to": to}))
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        s.chars().take(n).collect::<String>() + "…"
    }
}

fn now_secs() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}
