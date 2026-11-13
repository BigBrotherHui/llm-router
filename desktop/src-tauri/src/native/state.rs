use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::time::{Instant, SystemTime};

use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

pub const VERSION: &str = "2.10.0-native";

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Upstream {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub base_url: String,
    #[serde(default)]
    pub api_keys: Vec<String>,
    #[serde(default)]
    pub selected_models: Option<Vec<String>>,
    #[serde(default)]
    pub available_models: Vec<String>,
    #[serde(default)]
    pub model_groups: HashMap<String, String>,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub balance: Value,
    #[serde(default)]
    pub last_check: Value,
    #[serde(default)]
    pub last_checks: Value,
    #[serde(default)]
    pub checks_day: String,
    #[serde(default)]
    pub first_timeout_seconds: Option<f64>,
    #[serde(default)]
    pub connect_timeout_seconds: Option<f64>,
    #[serde(default)]
    pub key_strategy: String,
    #[serde(default, alias = "api", alias = "protocol", alias = "wire_api")]
    pub api_mode: Option<String>,
    #[serde(default)]
    pub user_agent: String,
    #[serde(default)]
    pub net_mode: Option<String>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl Upstream {
    pub fn selected(&self) -> Vec<String> {
        match &self.selected_models {
            Some(v) => v.clone(),
            None => self.available_models.clone(),
        }
    }
    pub fn group_of(&self, real: &str) -> String {
        self.model_groups
            .get(real)
            .cloned()
            .unwrap_or_else(|| real.to_string())
    }
    pub fn real_models_in(&self, group: &str) -> Vec<String> {
        self.selected()
            .into_iter()
            .filter(|m| self.group_of(m) == group)
            .collect()
    }
    pub fn is_offered(&self, model: &str) -> bool {
        self.available_models.iter().any(|m| m == model)
    }
    pub fn uses_responses(&self) -> bool {
        matches!(
            self.api_mode.as_deref().unwrap_or("chat").to_ascii_lowercase().as_str(),
            "responses" | "openai_responses" | "response"
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RouteEntry {
    #[serde(default)]
    pub upstream: String,
    #[serde(default)]
    pub model: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub quota: Option<i64>,
}

impl RouteEntry {
    pub fn skipped(&self) -> bool {
        self.quota == Some(0)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ModelRoute {
    #[serde(default = "default_strategy")]
    pub strategy: String,
    #[serde(default)]
    pub entries: Vec<RouteEntry>,
    #[serde(default)]
    pub orderings: HashMap<String, Value>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

fn default_strategy() -> String {
    "priority".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct StateFile {
    #[serde(default)]
    pub version: i64,
    #[serde(default)]
    pub upstreams: Vec<Upstream>,
    #[serde(default)]
    pub models: HashMap<String, ModelRoute>,
    #[serde(default)]
    pub suppressed_models: Vec<String>,
    #[serde(default)]
    pub defaults: Value,
    #[serde(default)]
    pub proxy_settings: Value,
    #[serde(default)]
    pub checkin_settings: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_behavior: Option<String>,
    /// 允许局域网 / Tailscale 访问（绑定 0.0.0.0，否则只听 127.0.0.1）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_access: Option<bool>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl StateFile {
    pub fn find_upstream(&self, id: &str) -> Option<&Upstream> {
        self.upstreams.iter().find(|u| u.id == id)
    }
    pub fn find_upstream_mut(&mut self, id: &str) -> Option<&mut Upstream> {
        self.upstreams.iter_mut().find(|u| u.id == id)
    }

    /// 按「勾选 + 分组」重建模型路由，保留已有的人工顺序。
    /// 与 Python 版 sync_routes 同语义：每个分组一条路由，成员 = 所有上游里
    /// 勾选了且归到这个分组的真实模型；已有成员保持相对位置，新成员追加末尾；
    /// 没有成员的分组删掉；显式删除过的分组（suppressed）不自动建回。
    pub fn sync_routes(&mut self, default_strategy: &str) -> (Vec<String>, Vec<String>) {
        let mut target: std::collections::BTreeMap<String, Vec<(String, String)>> = Default::default();
        for up in &self.upstreams {
            for real in up.selected() {
                let group = up.group_of(&real);
                if self.suppressed_models.contains(&group) {
                    continue;
                }
                target.entry(group).or_default().push((up.id.clone(), real));
            }
        }

        let mut created = Vec::new();
        let mut removed = Vec::new();

        let groups_to_remove: Vec<String> = self
            .models
            .keys()
            .filter(|g| !target.contains_key(*g))
            .cloned()
            .collect();
        for group in groups_to_remove {
            self.models.remove(&group);
            removed.push(group);
        }

        for (group, members) in target {
            match self.models.get_mut(&group) {
                None => {
                    self.models.insert(
                        group.clone(),
                        ModelRoute {
                            strategy: default_strategy.to_string(),
                            entries: members
                                .iter()
                                .map(|(uid, real)| RouteEntry {
                                    upstream: uid.clone(),
                                    model: real.clone(),
                                    enabled: true,
                                    quota: None,
                                })
                                .collect(),
                            ..Default::default()
                        },
                    );
                    created.push(group);
                }
                Some(route) => {
                    let wanted: std::collections::HashSet<(String, String)> =
                        members.iter().cloned().collect();
                    let mut kept: Vec<RouteEntry> = route
                        .entries
                        .iter()
                        .filter(|e| wanted.contains(&(e.upstream.clone(), e.model.clone())))
                        .cloned()
                        .collect();
                    let have: std::collections::HashSet<(String, String)> =
                        kept.iter().map(|e| (e.upstream.clone(), e.model.clone())).collect();
                    for (uid, real) in &members {
                        if !have.contains(&(uid.clone(), real.clone())) {
                            kept.push(RouteEntry {
                                upstream: uid.clone(),
                                model: real.clone(),
                                enabled: true,
                                quota: None,
                            });
                        }
                    }
                    route.entries = kept;
                }
            }
        }
        (created, removed)
    }
}

/// 上游运行时健康状态（不落盘，进程内）。
#[derive(Debug, Clone, Default)]
pub struct UpstreamHealth {
    pub consecutive_failures: u32,
    /// 冷却截止时刻（UNIX 秒）；0 = 未冷却。
    pub cooldown_until: i64,
    pub last_error: String,
    pub last_used: Option<i64>,
}

pub struct AppState {
    pub file: RwLock<StateFile>,
    pub state_path: PathBuf,
    pub started: Instant,
    pub client: reqwest::Client,
    pub total_requests: AtomicU64,
    pub rr: Mutex<HashMap<String, usize>>,
    pub checkin_plan: Mutex<Option<(String, i64)>>,
    pub checkin_last_day: Mutex<String>,
    pub app: Mutex<Option<tauri::AppHandle>>,
    pub health: Mutex<HashMap<String, UpstreamHealth>>,
    /// 实际监听端口（端口漂移时由 serve 更新）。
    pub port: std::sync::atomic::AtomicU16,
    /// /stats 快照缓存（1 秒），避免每次轮询都重建 4000+ 分组的 catalog。
    pub snapshot_cache: Mutex<Option<(Instant, Value)>>,
    /// 状态版本号：每次保存自增，前端据此判断要不要重新拉 catalog。
    pub catalog_rev: AtomicU64,
    /// catalog 缓存（按版本号）。
    pub catalog_cache: Mutex<Option<(u64, Value)>>,
    /// 历史文件尾部解析缓存：(文件长度, mtime, entries)。
    pub journal_cache: Mutex<Option<(u64, Option<SystemTime>, Arc<Vec<Value>>)>>,
    /// stats.json 读改写串行化。
    pub stats_lock: Mutex<()>,
    /// state.json 并发保存串行化（并行拉模型时会同时 save）。
    pub save_lock: Mutex<()>,
    /// 统计内存态：请求完成只改内存，定时批量落盘。
    pub stats_mem: Mutex<Option<Value>>,
    pub stats_dirty: AtomicBool,
    /// SSE 事件源：请求完成 / 状态保存后推给前端。
    pub events_tx: tokio::sync::broadcast::Sender<Value>,
    /// 流水写入队列：请求路径不碰磁盘，独立线程顺序 append。
    pub journal_tx: OnceLock<std::sync::mpsc::Sender<Value>>,
}

impl AppState {
    pub fn new(state_path: PathBuf, app: Option<tauri::AppHandle>) -> Self {
        let mut file = StateFile::default();
        if let Ok(text) = std::fs::read_to_string(&state_path) {
            if let Ok(parsed) = serde_json::from_str::<StateFile>(&text) {
                file = parsed;
            }
        }
        let client = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(10))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self {
            file: RwLock::new(file),
            state_path,
            started: Instant::now(),
            client,
            total_requests: AtomicU64::new(0),
            rr: Mutex::new(HashMap::new()),
            checkin_plan: Mutex::new(None),
            checkin_last_day: Mutex::new(String::new()),
            app: Mutex::new(app),
            health: Mutex::new(HashMap::new()),
            port: std::sync::atomic::AtomicU16::new(0),
            snapshot_cache: Mutex::new(None),
            catalog_rev: AtomicU64::new(1),
            catalog_cache: Mutex::new(None),
            journal_cache: Mutex::new(None),
            stats_lock: Mutex::new(()),
            save_lock: Mutex::new(()),
            stats_mem: Mutex::new(None),
            stats_dirty: AtomicBool::new(false),
            events_tx: tokio::sync::broadcast::channel(128).0,
            journal_tx: OnceLock::new(),
        }
    }

    pub fn stats_path(&self) -> PathBuf {
        let name = self
            .state_path
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "config.state.json".to_string());
        let stats_name = name.replace(".state.json", ".stats.json");
        self.state_path.with_file_name(stats_name)
    }

    pub fn history_path(&self) -> PathBuf {
        let name = self
            .state_path
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "config.state.json".to_string());
        let hist = name.replace(".state.json", ".history.jsonl");
        self.state_path.with_file_name(hist)
    }

    pub fn checkin_path(&self) -> PathBuf {
        let name = self
            .state_path
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "config.state.json".to_string());
        let ck = name.replace(".state.json", ".checkin.json");
        self.state_path.with_file_name(ck)
    }

    pub fn save(&self) -> Result<(), String> {
        let _guard = self.save_lock.lock().unwrap();
        if let Ok(mut cache) = self.snapshot_cache.lock() {
            *cache = None;
        }
        let file = self.file.read().unwrap();
        let text = serde_json::to_string_pretty(&*file).map_err(|e| e.to_string())?;
        if let Some(dir) = self.state_path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        let tmp = self.state_path.with_extension("json.tmp");
        std::fs::write(&tmp, text).map_err(|e| e.to_string())?;
        std::fs::rename(&tmp, &self.state_path).map_err(|e| e.to_string())?;
        let _ = self.events_tx.send(json!({"type": "state"}));
        Ok(())
    }

    /// 只有影响模型候选表（上游/勾选/分组/路由）的改动才需要让前端重拉 catalog。
    pub fn bump_catalog(&self) {
        self.catalog_rev.fetch_add(1, Ordering::Relaxed);
        if let Ok(mut cache) = self.catalog_cache.lock() {
            *cache = None;
        }
    }

    pub fn bump_total(&self) {
        self.total_requests.fetch_add(1, Ordering::Relaxed);
    }
}

pub const FAILURE_THRESHOLD: u32 = 3;
pub const COOLDOWN_SECONDS: i64 = 60;

pub fn record_upstream_result(state: &AppState, uid: &str, ok: bool, error: &str) {
    let mut health = state.health.lock().unwrap();
    let entry = health.entry(uid.to_string()).or_default();
    if ok {
        entry.consecutive_failures = 0;
        entry.cooldown_until = 0;
        entry.last_error.clear();
    } else {
        entry.consecutive_failures += 1;
        entry.last_error = error.chars().take(200).collect();
        if entry.consecutive_failures >= FAILURE_THRESHOLD {
            entry.cooldown_until = chrono::Utc::now().timestamp() + COOLDOWN_SECONDS;
            entry.consecutive_failures = 0;
        }
    }
    entry.last_used = Some(chrono::Utc::now().timestamp());
}

/// true = 不在冷却期。
pub fn upstream_available(state: &AppState, uid: &str) -> bool {
    let health = state.health.lock().unwrap();
    match health.get(uid) {
        Some(h) => chrono::Utc::now().timestamp() >= h.cooldown_until,
        None => true,
    }
}

pub fn upstream_last_error(state: &AppState, uid: &str) -> String {
    let health = state.health.lock().unwrap();
    health
        .get(uid)
        .map(|h| h.last_error.clone())
        .unwrap_or_default()
}

pub fn upstream_cooling_remaining(state: &AppState, uid: &str) -> i64 {
    let health = state.health.lock().unwrap();
    health
        .get(uid)
        .map(|h| {
            let now = chrono::Utc::now().timestamp();
            (h.cooldown_until - now).max(0)
        })
        .unwrap_or(0)
}

pub fn upstream_failure_count(state: &AppState, uid: &str) -> u32 {
    let health = state.health.lock().unwrap();
    health.get(uid).map(|h| h.consecutive_failures).unwrap_or(0)
}

/// 追加一条请求流水（JSONL，与 Python 版格式对齐，UI 靠 trail 显示尝试链）。
pub fn append_journal(state: &AppState, entry: Value) {
    super::stats::record(state, &entry);
    let _ = state.events_tx.send(json!({
        "type": "journal",
        "at": entry.get("at").cloned().unwrap_or(Value::Null),
    }));
    // 独立线程顺序落盘：请求路径上不阻塞 async 运行时。
    let tx = state.journal_tx.get_or_init(|| {
        let (tx, rx) = std::sync::mpsc::channel::<Value>();
        let path = state.history_path();
        std::thread::spawn(move || {
            if let Some(dir) = path.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            while let Ok(item) = rx.recv() {
                if let Ok(mut f) = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&path)
                {
                    use std::io::Write;
                    if let Ok(line) = serde_json::to_string(&item) {
                        let _ = writeln!(f, "{line}");
                    }
                }
            }
        });
        tx
    });
    let _ = tx.send(entry);
}

pub fn mask_key(key: &str) -> String {
    let chars: Vec<char> = key.chars().collect();
    if chars.is_empty() {
        return String::new();
    }
    if chars.len() <= 10 {
        return format!("{}{}", chars[..2].iter().collect::<String>(), "*".repeat(chars.len().saturating_sub(2)));
    }
    format!(
        "{}…{}",
        chars[..6].iter().collect::<String>(),
        chars[chars.len() - 4..].iter().collect::<String>()
    )
}

pub fn snapshot_cached(state: &AppState) -> Value {
    if let Ok(cache) = state.snapshot_cache.lock() {
        if let Some((at, value)) = cache.as_ref() {
            if at.elapsed() < std::time::Duration::from_secs(1) {
                return value.clone();
            }
        }
    }
    let value = snapshot(state);
    if let Ok(mut cache) = state.snapshot_cache.lock() {
        *cache = Some((Instant::now(), value.clone()));
    }
    value
}

pub fn snapshot(state: &AppState) -> Value {
    let file = state.file.read().unwrap();
    let defaults = &file.defaults;
    let first_timeout = defaults
        .get("first_timeout_seconds")
        .and_then(|v| v.as_f64())
        .unwrap_or(30.0);
    let connect_timeout = defaults
        .get("connect_timeout_seconds")
        .and_then(|v| v.as_f64())
        .unwrap_or(10.0);

    let upstreams: Vec<Value> = file
        .upstreams
        .iter()
        .map(|u| {
            let selected = u.selected();
            let mut groups: HashMap<String, Vec<String>> = HashMap::new();
            for m in &selected {
                groups.entry(u.group_of(m)).or_default().push(m.clone());
            }
            let mut groups_sorted: Vec<(String, Vec<String>)> = groups.into_iter().collect();
            for (_, v) in groups_sorted.iter_mut() {
                v.sort();
            }
            groups_sorted.sort_by(|a, b| a.0.cmp(&b.0));
            let groups_val: Map<String, Value> = groups_sorted
                .into_iter()
                .map(|(k, v)| (k, json!(v)))
                .collect();
            let consec_fail = upstream_failure_count(state, &u.id);
            let cool_remaining = upstream_cooling_remaining(state, &u.id);
            let is_available = upstream_available(state, &u.id);
            let last_err_raw = upstream_last_error(state, &u.id);
            let last_err_val = if last_err_raw.is_empty() {
                Value::Null
            } else {
                json!(last_err_raw)
            };
            json!({
                "id": u.id,
                "name": u.name,
                "base_url": u.base_url,
                "mirrors": [],
                "base_urls": [u.base_url.clone()],
                "mirror_count": 0,
                "mirror_strategy": "priority",
                "last_mirror": null,
                "enabled": u.enabled,
                "key_hint": u.api_keys.first().map(|k| mask_key(k)).unwrap_or_default(),
                "has_key": !u.api_keys.is_empty(),
                "key_count": u.api_keys.len(),
                "key_hints": u.api_keys.iter().map(|k| mask_key(k)).collect::<Vec<_>>(),
                "key_names": [],
                "key_disabled": [],
                "first_timeout_seconds": u.first_timeout_seconds.unwrap_or(first_timeout),
                "connect_timeout_seconds": u.connect_timeout_seconds.unwrap_or(connect_timeout),
                "key_strategy": if u.key_strategy.is_empty() { "round_robin" } else { u.key_strategy.as_str() },
                "available_models": u.available_models,
                "selected_models": selected,
                "selected_count": selected.len(),
                "available_count": u.available_models.len(),
                "model_groups": u.model_groups,
                "groups": groups_val,
                "models_fetched_at": u.extra.get("models_fetched_at").cloned().unwrap_or(Value::Null),
                "models_error": u.extra.get("models_error").cloned().unwrap_or(Value::Null),
                "requests": 0,
                "successes": 0,
                "failures": 0,
                "consecutive_failures": consec_fail,
                "cooldown_remaining_seconds": cool_remaining,
                "available": is_available,
                "last_error": last_err_val,
                "avg_latency_seconds": 0,
                "last_used": Value::Null,
                "last_check": u.last_check,
                "last_checks": u.last_checks,
                "checks_day": if u.checks_day.is_empty() { Value::Null } else { json!(u.checks_day) },
                "balance": u.balance,
                "user_agent": u.user_agent,
                "api_mode": u.api_mode.clone().unwrap_or_else(|| "chat".to_string()),
                "api_mode_label": if u.uses_responses() { "Responses" } else { "Chat Completions" },
                "net_mode": u.net_mode.clone().unwrap_or_else(|| "inherit".to_string()),
                "net_mode_effective": u.net_mode.clone().unwrap_or_else(|| "rule".to_string()),
                "net_source": "默认",
                "stream_only_models": u.extra.get("stream_only_models").cloned().unwrap_or_else(|| json!([])),
            })
        })
        .collect();

    let mut models = Map::new();
    for (name, route) in &file.models {
        let members: Vec<Value> = route
            .entries
            .iter()
            .map(|e| {
                let up_name = file
                    .find_upstream(&e.upstream)
                    .map(|u| u.name.clone())
                    .unwrap_or_else(|| e.upstream.clone());
                json!({
                    "upstream": e.upstream,
                    "upstream_name": up_name,
                    "model": e.model,
                    "renamed": e.model != *name,
                    "enabled": e.enabled,
                    "quota": e.quota,
                })
            })
            .collect();
        let upstream_names: Vec<String> = route
            .entries
            .iter()
            .map(|e| {
                file.find_upstream(&e.upstream)
                    .map(|u| u.name.clone())
                    .unwrap_or_else(|| e.upstream.clone())
            })
            .collect();
        let mut real_models: Vec<String> = route.entries.iter().map(|e| e.model.clone()).collect();
        real_models.sort();
        real_models.dedup();
        let resolved = route
            .entries
            .iter()
            .filter(|e| {
                e.enabled
                    && !e.skipped()
                    && file
                        .find_upstream(&e.upstream)
                        .map(|u| u.enabled)
                        .unwrap_or(false)
            })
            .count();
        let mut val = json!({
            "strategy": route.strategy,
            "entries": route.entries,
            "members": members,
            "upstream_names": upstream_names,
            "real_models": real_models,
            "resolved_count": resolved,
            "active_ordering": "",
        });
        if !route.orderings.is_empty() {
            val["orderings"] = json!(route.orderings);
        }
        models.insert(name.clone(), val);
    }

    json!({
        "version": VERSION,
        "http_error_hints": {},
        "config_path": state.state_path.to_string_lossy(),
        "defaults": {"first_timeout_seconds": first_timeout, "connect_timeout_seconds": connect_timeout},
        "proxy": {"enabled": false, "default_mode": "rule", "urls": {}, "upstreams": {}},
        "state_path": state.state_path.to_string_lossy(),
        "journal": journal_info(state),
        "uptime_seconds": state.started.elapsed().as_secs_f64(),
        "total_requests": state.total_requests.load(Ordering::Relaxed),
        "failure_threshold": 3,
        "cooldown_seconds": 60,
        "default_strategy": "priority",
        "suppressed_models": file.suppressed_models,
        "upstreams": upstreams,
        "models": models,
        "catalog_rev": state.catalog_rev.load(Ordering::Relaxed),
        "call_stats": {"groups": [], "totals": {"calls":0,"ok":0,"failed":0,"attempts":0,"input":0,"output":0,"total":0,"cache_read":0,"cache_write":0}, "upstreams": []},
    })
}

/// 模型候选总表（体积大），单独接口按需加载；状态没变时复用缓存。
pub fn catalog_cached(state: &AppState) -> Value {
    let rev = state.catalog_rev.load(Ordering::Relaxed);
    if let Ok(cache) = state.catalog_cache.lock() {
        if let Some((cached_rev, catalog)) = cache.as_ref() {
            if *cached_rev == rev {
                return json!({"ok": true, "rev": rev, "catalog": catalog});
            }
        }
    }
    let catalog = {
        let file = state.file.read().unwrap();
        build_catalog(&file)
    };
    if let Ok(mut cache) = state.catalog_cache.lock() {
        *cache = Some((rev, json!(catalog)));
    }
    json!({"ok": true, "rev": rev, "catalog": catalog})
}

fn catalog_entry<'a>(map: &'a mut HashMap<String, Value>, group: &str) -> &'a mut Value {
    map.entry(group.to_string()).or_insert_with(|| {
        json!({
            "name": group,
            "upstreams": [],
            "real_models": [],
            "offered_only": [],
            "configured": false,
            "strategy": Value::Null,
        })
    })
}

fn build_catalog(file: &StateFile) -> Vec<Value> {
    let mut map: HashMap<String, Value> = HashMap::new();
    let order: HashMap<String, usize> = file
        .upstreams
        .iter()
        .enumerate()
        .map(|(i, u)| (u.id.clone(), i))
        .collect();

    for up in &file.upstreams {
        let selected = up.selected();
        for real in &up.available_models {
            let group = up.group_of(real);
            let e = catalog_entry(&mut map, &group);
            if selected.iter().any(|m| m == real) {
                e["upstreams"].as_array_mut().unwrap().push(json!({
                    "id": up.id, "name": up.name, "enabled": up.enabled,
                    "model": real, "renamed": real != &group
                }));
                let rms = e["real_models"].as_array_mut().unwrap();
                if !rms.iter().any(|x| x == real) {
                    rms.push(json!(real));
                }
            } else {
                let offered = e["offered_only"].as_array_mut().unwrap();
                if !offered.iter().any(|x| x["id"] == up.id) {
                    offered.push(json!({"id": up.id, "name": up.name}));
                }
            }
        }
    }

    for (group, route) in &file.models {
        let e = catalog_entry(&mut map, group);
        e["configured"] = json!(true);
        e["strategy"] = json!(route.strategy);
        for ent in &route.entries {
            if let Some(up) = file.find_upstream(&ent.upstream) {
                let ups = e["upstreams"].as_array_mut().unwrap();
                if !ups.iter().any(|x| x["id"] == up.id && x["model"] == ent.model) {
                    ups.push(json!({
                        "id": up.id, "name": up.name, "enabled": up.enabled,
                        "model": ent.model, "renamed": ent.model != *group
                    }));
                }
                let rms = e["real_models"].as_array_mut().unwrap();
                if !rms.iter().any(|x| x == &ent.model) {
                    rms.push(json!(ent.model));
                }
            }
        }
    }

    let mut out: Vec<Value> = map.into_values().collect();
    for e in out.iter_mut() {
        let mut ups = e["upstreams"].as_array().cloned().unwrap_or_default();
        ups.sort_by_key(|u| order.get(u["id"].as_str().unwrap_or("")).copied().unwrap_or(999));
        e["upstream_count"] = json!(ups.len());
        let offered = e["offered_only"].as_array().cloned().unwrap_or_default();
        let ids: Vec<&str> = ups.iter().filter_map(|u| u["id"].as_str()).collect();
        let filtered: Vec<Value> = offered
            .into_iter()
            .filter(|o| !ids.contains(&o["id"].as_str().unwrap_or("")))
            .collect();
        e["offered_only_count"] = json!(filtered.len());
        e["offered_only"] = json!(filtered);
        e["upstreams"] = json!(ups);
        let rms = e["real_models"].as_array().cloned().unwrap_or_default();
        e["aliased"] = json!(rms.len() > 1 || rms.first().map(|v| v.as_str() != Some(e["name"].as_str().unwrap_or(""))).unwrap_or(false));
    }
    out.sort_by(|a, b| {
        a["name"].as_str().unwrap_or("").to_lowercase().cmp(&b["name"].as_str().unwrap_or("").to_lowercase())
    });
    out
}

pub fn journal_info(state: &AppState) -> Value {
    let path = state.history_path();
    let bytes = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    // 大文件不为了一个数字全读一遍；按平均行宽估算即可。
    let lines = if bytes > 4 * 1024 * 1024 {
        (bytes / 220) as usize
    } else {
        std::fs::read_to_string(&path)
            .map(|s| s.lines().count())
            .unwrap_or(0)
    };
    json!({"current_lines": lines, "files": 1, "bytes": bytes, "rotate_at": 10000})
}
