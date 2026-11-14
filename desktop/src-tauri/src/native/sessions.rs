//! 会话对账：直接读各家 Agent 自己写的会话账本（DeepSeek Harness / Claude Code / Pi / Codex），
//! 把「经过本机 llm-router」和「压根没经过本机路由」的调用分开统计。
//!
//! 为什么需要它：路由流水（config.history.jsonl）只记录经过 8010 的请求；
//! Agent 们各自还写着自己的账本（模型 / token / 时间），把它们读出来就能回答
//! 「哪些调用没走我的路由、被谁接走了」。
//!
//! 全部只读：不改任何 Agent 的配置，也不写任何文件。

use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use chrono::{TimeZone, Timelike};
use serde::Deserialize;
use serde_json::{json, Value};

use super::state::AppState;

/// 单个会话文件最多解压/读取的字节数（防止 100MB 级的 rollout 把内存吃满）。
const FILE_CAP: u64 = 48 * 1024 * 1024;
/// 单个来源最多处理的文件数。
const FILE_LIMIT: usize = 400;
/// 返回给前端的明细行上限。
const ROW_LIMIT: usize = 200;
/// 返回给前端的会话条目上限。
const SESSION_LIMIT: usize = 60;

/// HUD 用的今日会话总览缓存：会话文件扫描较慢，30 秒内复用。
static TODAY_CACHE: OnceLock<Mutex<Option<(Instant, TodayTotals)>>> = OnceLock::new();
static LAST_ROUND_CACHE: OnceLock<Mutex<Option<(Instant, Value)>>> = OnceLock::new();

/// Magpie 网关自报的每轮实测值（ttft_ms / ms），按「输入 + 输出 + 缓存读」指纹索引。
/// 我们的会话记录里没有「网关收包」时刻，只能靠它把 magpie 口径对齐到面板上的数。
struct MagpieSnap {
    at: Instant,
    map: HashMap<String, (f64, f64)>,
}
static MAGPIE_SNAP: OnceLock<Mutex<Option<MagpieSnap>>> = OnceLock::new();
static MAGPIE_BUSY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn magpie_key(input: u64, output: u64, cache_read: u64) -> String {
    format!("{input}|{output}|{cache_read}")
}

fn refresh_magpie() {
    let client = match reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(8))
        .build()
    {
        Ok(c) => c,
        Err(_) => return,
    };
    // 单页最多 500 条，用 offset 往回翻；最多翻 8 页（4000 条）够覆盖一两天的量。
    const PAGE: usize = 500;
    const MAX_PAGES: usize = 8;
    let mut all: Vec<Value> = Vec::new();
    for page in 0..MAX_PAGES {
        let url = format!(
            "http://127.0.0.1:3425/v1/magpie/usage/requests?limit={PAGE}&offset={}",
            page * PAGE
        );
        let resp = match client
            .get(&url)
            .header("Authorization", "Bearer magpie")
            .header("Accept-Encoding", "identity")
            .send()
        {
            Ok(r) => r,
            Err(_) => break,
        };
        if !resp.status().is_success() {
            break;
        }
        let text = match resp.text() {
            Ok(t) => t,
            Err(_) => break,
        };
        let v: Value = match serde_json::from_str(&text) {
            Ok(v) => v,
            Err(_) => break,
        };
        let rows = match v.get("rows").and_then(|r| r.as_array()) {
            Some(r) => r,
            None => break,
        };
        let empty = rows.is_empty();
        let short = rows.len() < PAGE;
        all.extend(rows.iter().cloned());
        if empty || short {
            break;
        }
        if let Some(total) = v.get("total").and_then(|t| t.as_u64()) {
            if all.len() as u64 >= total {
                break;
            }
        }
    }
    if all.is_empty() {
        return;
    }
    let mut map: HashMap<String, (f64, f64)> = HashMap::new();
    for r in all.iter() {
        let input = r.get("in").and_then(|x| x.as_u64());
        let output = r.get("out").and_then(|x| x.as_u64());
        let cache_read = r.get("cache_read").and_then(|x| x.as_u64()).unwrap_or(0);
        let ttft = r.get("ttft_ms").and_then(|x| x.as_f64());
        let ms = r.get("ms").and_then(|x| x.as_f64());
        if let (Some(i), Some(o), Some(t), Some(m)) = (input, output, ttft, ms) {
            if o > 0 {
                map.insert(magpie_key(i, o, cache_read), (t, m));
                if let Some(sid) = r.get("session").and_then(|x| x.as_str()) {
                    map.insert(format!("{sid}|{}", magpie_key(i, o, cache_read)), (t, m));
                }
            }
        }
    }
    if let Some(cell) = MAGPIE_SNAP.get() {
        if let Ok(mut g) = cell.lock() {
            *g = Some(MagpieSnap {
                at: Instant::now(),
                map,
            });
        }
    }
}

/// 返回 Magpie 实测值快照；过期就在后台线程刷一次（本机网关没开时返回空表，不报错）。
pub fn magpie_offsets() -> HashMap<String, (f64, f64)> {
    let cell = MAGPIE_SNAP.get_or_init(|| Mutex::new(None));
    let stale = {
        match cell.lock() {
            Ok(g) => match g.as_ref() {
                Some(s) => s.at.elapsed().as_secs() >= 30,
                None => true,
            },
            Err(_) => false,
        }
    };
    if stale {
        let cold = {
            match cell.lock() {
                Ok(g) => g.is_none(),
                Err(_) => false,
            }
        };
        if cold {
            // 冷启动（进程刚起来 / 还没任何快照）：同步拉一次，否则第一次扫描会全部退回 step 口径。
            // 一次 HTTP 到本机 3425，几十毫秒，可以接受。
            refresh_magpie();
        } else if !MAGPIE_BUSY.swap(true, std::sync::atomic::Ordering::SeqCst) {
            std::thread::spawn(|| {
                refresh_magpie();
                MAGPIE_BUSY.store(false, std::sync::atomic::Ordering::SeqCst);
            });
        }
    }
    match cell.lock() {
        Ok(g) => g.as_ref().map(|s| s.map.clone()).unwrap_or_default(),
        Err(_) => HashMap::new(),
    }
}

fn ok(v: Value) -> Response {
    Json(v).into_response()
}

fn bad(msg: &str) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({"ok": false, "error": msg})),
    )
        .into_response()
}

fn home() -> PathBuf {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn mtime_ms(path: &Path) -> i64 {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn iso_ms(s: &str) -> i64 {
    chrono::DateTime::parse_from_rfc3339(s)
        .map(|t| t.timestamp_millis())
        .unwrap_or(0)
}

#[derive(Deserialize, Default, Clone)]
pub struct ScanQuery {
    /// 时间窗（小时），0 或缺省后用默认 168 小时；负数表示不限时间
    pub hours: Option<i64>,
    /// today = 本机时区当天 00:00 起；all = 不限时间；其余按 hours
    pub range: Option<String>,
    /// 逗号分隔：dsh,claude,pi,codex
    pub sources: Option<String>,
    /// 明细行上限
    pub limit: Option<usize>,
    /// 路由范围：all（默认）/ routed（只走过本机路由）/ external（只未走本机路由）
    pub route: Option<String>,
    pub day: Option<String>,
    pub month: Option<String>,
    pub group: Option<String>,
    pub upstream: Option<String>,
    pub model: Option<String>,
    pub offset: Option<usize>,
    /// 首字 / 耗时口径：happycoding（默认）/ magpie / dsh
    pub metric: Option<String>,
}

/// HUD 用的“今日全部流量”快照：包含所有 Agent 会话，不只是经过 llm-router 的部分。
#[derive(Clone, Default)]
pub struct TodayTotals {
    pub total: u64,
    pub routed: u64,
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub routed_input: u64,
    pub routed_output: u64,
    pub routed_cache_read: u64,
    pub routed_cache_write: u64,
    pub durations: Vec<f64>,
    pub routed_durations: Vec<f64>,
}

struct Agg {
    source: String,
    agent: String,
    provider: String,
    model: String,
    base_url: String,
    routed: bool,
    rounds: u64,
    input: u64,
    output: u64,
    cache_read: u64,
    cache_write: u64,
    total: u64,
    first_ms: i64,
    last_ms: i64,
    durations: Vec<f64>,
    first_chunks: Vec<f64>,
    sessions: HashSet<String>,
}

impl Agg {
    fn new(source: &str, agent: &str, provider: &str, model: &str) -> Self {
        Agg {
            source: source.to_string(),
            agent: agent.to_string(),
            provider: provider.to_string(),
            model: model.to_string(),
            base_url: String::new(),
            routed: false,
            rounds: 0,
            input: 0,
            output: 0,
            cache_read: 0,
            cache_write: 0,
            total: 0,
            first_ms: 0,
            last_ms: 0,
            durations: Vec::new(),
            first_chunks: Vec::new(),
            sessions: HashSet::new(),
        }
    }

    fn add(
        &mut self,
        at_ms: i64,
        input: u64,
        output: u64,
        cache_read: u64,
        cache_write: u64,
        total: u64,
        duration_ms: Option<f64>,
        first_chunk_ms: Option<f64>,
    ) {
        if let Some(d) = duration_ms {
            if d > 0.0 {
                self.durations.push(d / 1000.0);
            }
        }
        if let Some(f) = first_chunk_ms {
            if f > 0.0 {
                self.first_chunks.push(f / 1000.0);
            }
        }
        self.rounds += 1;
        self.input += input;
        self.output += output;
        self.cache_read += cache_read;
        self.cache_write += cache_write;
        self.total += if total > 0 { total } else { input + output };
        if self.first_ms == 0 || (at_ms > 0 && at_ms < self.first_ms) {
            self.first_ms = at_ms;
        }
        if at_ms > self.last_ms {
            self.last_ms = at_ms;
        }
    }
}

/// 单个来源的扫描状态。
#[derive(Default, Clone)]
struct SrcStat {
    id: String,
    name: String,
    root: String,
    files: u64,
    sessions: u64,
    bytes: u64,
    rounds: u64,
    skipped_old: u64,
    truncated: bool,
    ms: u64,
    note: String,
}

/// 一个会话（一份账本文件）的汇总。
#[derive(Clone)]
struct Sess {
    source: String,
    agent: String,
    id: String,
    project: String,
    cwd: String,
    path: String,
    started_ms: i64,
    ended_ms: i64,
    rounds: u64,
    routed_rounds: u64,
    input: u64,
    output: u64,
    cache_read: u64,
    cache_write: u64,
    models: HashMap<String, u64>,
    all_models: HashMap<String, u64>,
    providers: HashSet<String>,
    external_top: String,
}

type AggMap = HashMap<String, Agg>;
type SessList = Vec<Sess>;

fn bump(
    aggs: &mut AggMap,
    key: &str,
    make: impl FnOnce() -> Agg,
    at_ms: i64,
    u: [u64; 5],
    duration_ms: Option<f64>,
    first_chunk_ms: Option<f64>,
) {
    let e = aggs
        .entry(key.to_string())
        .or_insert_with(|| make());
    e.add(at_ms, u[0], u[1], u[2], u[3], u[4], duration_ms, first_chunk_ms);
}

fn num(v: &Value) -> u64 {
    if let Some(n) = v.as_u64() {
        return n;
    }
    if let Some(n) = v.as_i64() {
        return n.max(0) as u64;
    }
    if let Some(n) = v.as_f64() {
        return n.max(0.0) as u64;
    }
    0
}

fn pick_num(v: &Value, keys: &[&str]) -> u64 {
    for k in keys {
        if let Some(x) = v.get(*k) {
            let n = num(x);
            if n > 0 {
                return n;
            }
        }
    }
    0
}

// ───────────────────────── provider → baseURL 映射 ─────────────────────────

fn is_router_url(url: &str, port: u16) -> bool {
    let u = url.to_ascii_lowercase();
    u.contains(&format!("127.0.0.1:{port}"))
        || u.contains(&format!("localhost:{port}"))
        || u.contains(&format!("[::1]:{port}"))
        || u.contains("llm-router")
        || u.contains("llmrouter")
}

fn is_router_name(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    n.contains("llm-router") || n.contains("llmrouter") || n.contains("llm_router")
}

fn insert_base(map: &mut HashMap<String, String>, name: &str, url: &str) {
    let name = name.trim().trim_matches('"').to_string();
    let url = url.trim().trim_matches('"').trim_matches('\'').to_string();
    if name.is_empty() || url.is_empty() {
        return;
    }
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return;
    }
    map.entry(name.clone()).or_insert_with(|| url.clone());
    map.entry(name.to_ascii_lowercase()).or_insert(url);
}

/// YAML 启发式：靠缩进栈把 `baseURL:` 归到最近的上一层映射键（provider 名）。
fn add_yaml_bases(text: &str, map: &mut HashMap<String, String>) {
    let mut stack: Vec<(usize, String)> = Vec::new();
    for raw in text.lines() {
        let line = raw.trim_end();
        let t = line.trim_start();
        if t.is_empty() || t.starts_with('#') || t.starts_with('-') && !t.contains(':') {
            continue;
        }
        let indent = line.len() - t.len();
        while let Some((i, _)) = stack.last() {
            if *i >= indent {
                stack.pop();
            } else {
                break;
            }
        }
        let Some((k, v)) = t.split_once(':') else {
            continue;
        };
        let key = k.trim().trim_start_matches("- ").trim();
        let val = v.trim();
        let lower = key.to_ascii_lowercase();
        if lower == "baseurl" || lower == "base_url" || lower == "anthropic_base_url" {
            let owner = stack
                .iter()
                .rev()
                .find(|(i, _)| *i < indent)
                .map(|(_, n)| n.clone())
                .unwrap_or_else(|| "(通用)".to_string());
            insert_base(map, &owner, val);
        }
        if val.is_empty() && !key.is_empty() {
            stack.push((indent, key.to_string()));
        }
    }
}

/// JSON 配置里的 provider → baseUrl（Pi / opencode / zcode / claude settings）。
fn add_json_bases(text: &str, map: &mut HashMap<String, String>) {
    let cleaned = super::agents::strip_jsonc(text.trim_start_matches('\u{feff}'));
    let Ok(v) = serde_json::from_str::<Value>(&cleaned) else {
        return;
    };
    // env.ANTHROPIC_BASE_URL
    if let Some(u) = v
        .get("env")
        .and_then(|e| e.get("ANTHROPIC_BASE_URL"))
        .and_then(|x| x.as_str())
    {
        insert_base(map, "anthropic", u);
    }
    for holder in ["providers", "provider"] {
        if let Some(obj) = v.get(holder).and_then(|x| x.as_object()) {
            for (name, pv) in obj {
                for key in ["baseUrl", "baseURL", "base_url", "url"] {
                    if let Some(u) = pv.get(key).and_then(|x| x.as_str()) {
                        insert_base(map, name, u);
                    }
                }
                if let Some(u) = pv
                    .get("options")
                    .and_then(|o| o.get("baseURL").or_else(|| o.get("baseUrl")))
                    .and_then(|x| x.as_str())
                {
                    insert_base(map, name, u);
                }
            }
        }
    }
    // claude settings.json 的 env 也直接挂在顶层
    if let Some(u) = v.get("ANTHROPIC_BASE_URL").and_then(|x| x.as_str()) {
        insert_base(map, "anthropic", u);
    }
}

fn add_toml_bases(text: &str, map: &mut HashMap<String, String>) {
    let Ok(v) = text.parse::<toml::Value>() else {
        return;
    };
    if let Some(mp) = v.get("model_providers").and_then(|x| x.as_table()) {
        for (name, pv) in mp {
            if let Some(u) = pv.get("base_url").and_then(|x| x.as_str()) {
                insert_base(map, name, u);
            }
        }
    }
    if let Some(mp) = v.get("providers").and_then(|x| x.as_table()) {
        for (name, pv) in mp {
            for key in ["base_url", "baseURL", "baseUrl"] {
                if let Some(u) = pv.get(key).and_then(|x| x.as_str()) {
                    insert_base(map, name, u);
                }
            }
        }
    }
}

/// 收集本机各 Agent 配置里的 provider → baseURL，用来判断一次调用有没有经过我们。
fn collect_bases(home: &Path) -> HashMap<String, String> {
    let mut map: HashMap<String, String> = HashMap::new();
    let read = |p: &Path| std::fs::read_to_string(p).unwrap_or_default();

    // DSH：settings.yaml + 每个 profile 的 patch 层
    add_yaml_bases(&read(&home.join(".dsh").join("settings.yaml")), &mut map);
    if let Ok(entries) = std::fs::read_dir(home.join(".dsh").join("profiles")) {
        for e in entries.flatten() {
            add_yaml_bases(
                &read(&e.path().join("cordis.patch.yml")),
                &mut map,
            );
        }
    }
    // Pi
    add_json_bases(&read(&home.join(".pi").join("agent").join("models.json")), &mut map);
    // Claude Code
    add_json_bases(&read(&home.join(".claude").join("settings.json")), &mut map);
    // opencode
    add_json_bases(
        &read(&home.join(".config").join("opencode").join("opencode.json")),
        &mut map,
    );
    // Codex
    add_toml_bases(&read(&home.join(".codex").join("config.toml")), &mut map);
    // Kimi Code
    add_toml_bases(&read(&home.join(".kimi-code").join("config.toml")), &mut map);
    map
}

// ───────────────────────── 文件遍历 / 行读取 ─────────────────────────

fn walk_files(root: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    if out.len() >= FILE_LIMIT || depth > 4 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    let mut dirs: Vec<PathBuf> = Vec::new();
    for e in entries.flatten() {
        let p = e.path();
        let Ok(ft) = e.file_type() else { continue };
        if ft.is_dir() {
            dirs.push(p);
        } else if ft.is_file() {
            out.push(p);
            if out.len() >= FILE_LIMIT {
                return;
            }
        }
    }
    dirs.sort();
    for d in dirs {
        walk_files(&d, depth + 1, out);
        if out.len() >= FILE_LIMIT {
            return;
        }
    }
}

/// 只读文件尾部若干字节逐行回调（大文件用），返回读了多少字节。
fn tail_lines(path: &Path, max_bytes: u64, f: &mut dyn FnMut(&str)) -> u64 {
    let Ok(mut file) = std::fs::File::open(path) else {
        return 0;
    };
    let len = file.metadata().map(|m| m.len()).unwrap_or(0);
    let start = len.saturating_sub(max_bytes);
    let mut buf = Vec::new();
    if file.seek(SeekFrom::Start(start)).is_ok() && file.read_to_end(&mut buf).is_ok() {
        let text = String::from_utf8_lossy(&buf);
        let mut lines = text.split('\n');
        if start > 0 {
            lines.next(); // 丢掉可能被截断的半行
        }
        for l in lines {
            let t = l.trim();
            if !t.is_empty() {
                f(t);
            }
        }
    }
    buf.len() as u64
}

// ───────────────────────── 各来源解析 ─────────────────────────

struct Ctx<'a> {
    bases: &'a HashMap<String, String>,
    port: u16,
    since_ms: i64,
    route_filter: u8,
    metric: u8,
    /// magpie 口径用：网关实测值快照，每次扫描只取一次
    magpie: HashMap<String, (f64, f64)>,
    aggs: &'a mut AggMap,
    sessions: &'a mut SessList,
    records: Option<&'a mut Vec<Value>>,
}

/// 0 = 全部，1 = 只走过本机路由，2 = 只未走本机路由。
pub fn route_filter(v: Option<&str>) -> u8 {
    match v.unwrap_or("").trim().to_ascii_lowercase().as_str() {
        "routed" | "router" | "llmrouter" | "yes" | "true" => 1,
        "external" | "unrouted" | "direct" | "no" | "false" => 2,
        _ => 0,
    }
}

/// 首字 / 耗时口径：0 = happycoding（默认），1 = magpie，2 = dsh（当前方案）
pub fn metric_mode(v: Option<&str>) -> u8 {
    match v.unwrap_or("").trim().to_ascii_lowercase().as_str() {
        "magpie" => 1,
        "dsh" | "step" | "native" | "current" => 2,
        _ => 0,
    }
}

impl<'a> Ctx<'a> {
    /// 记一轮调用：先按 provider 名查 baseURL，再用 baseURL 判断是否经过本机路由。
    fn round(
        &mut self,
        source: &str,
        agent: &str,
        sess: &mut Sess,
        provider: &str,
        model: &str,
        at_ms: i64,
        usage: [u64; 5],
        duration_ms: Option<f64>,
        first_chunk_ms: Option<f64>,
        header_ms: Option<f64>,
    ) {
        // magpie 口径：会话记录里没有「网关收包」时刻，但 Magpie 自己的用量日志有。
        // 走 Magpie 的请求按 token 指纹（输入 / 输出 / 缓存读）对账，对上就直接用网关实测值。
        let mut duration_ms = duration_ms;
        let mut first_chunk_ms = first_chunk_ms;
        let mut metric_source = "step";
        if self.metric == 1 && source == "dsh" {
            if let Some(h) = header_ms {
                metric_source = "header";
                if let Some(d) = duration_ms.as_mut() {
                    *d = (*d - h).max(0.0);
                }
                if let Some(f) = first_chunk_ms.as_mut() {
                    *f = (*f - h).max(0.0);
                }
            }
            if usage[1] > 0 {
                let key = magpie_key(usage[0], usage[1], usage[2]);
                let hit = self
                    .magpie
                    .get(&format!("{}|{key}", sess.id))
                    .or_else(|| self.magpie.get(&key))
                    .copied();
                if let Some((ttft, ms)) = hit {
                    first_chunk_ms = Some(ttft);
                    duration_ms = Some(ms);
                    metric_source = "magpie";
                }
            }
        }
        let provider = if provider.trim().is_empty() {
            "(未记录)"
        } else {
            provider.trim()
        };
        let model = if model.trim().is_empty() {
            "(未记录)"
        } else {
            model.trim()
        };
        let base = self
            .bases
            .get(provider)
            .or_else(|| self.bases.get(&provider.to_ascii_lowercase()))
            .cloned()
            .unwrap_or_default();
        let routed = is_router_name(provider)
            || (!base.is_empty() && is_router_url(&base, self.port));
        if (self.route_filter == 1 && !routed) || (self.route_filter == 2 && routed) {
            return;
        }
        let key = format!("{source}\u{1}{provider}\u{1}{model}");
        let label = format!("{provider}/{model}");
        bump(
            self.aggs,
            &key,
            || Agg::new(source, agent, provider, model),
            at_ms,
            usage,
            duration_ms,
            first_chunk_ms,
        );
        if let Some(e) = self.aggs.get_mut(&key) {
            e.routed = routed;
            if e.base_url.is_empty() {
                e.base_url = base.clone();
            }
            e.sessions.insert(sess.id.clone());
        }
        sess.rounds += 1;
        sess.input += usage[0];
        sess.output += usage[1];
        sess.cache_read += usage[2];
        sess.cache_write += usage[3];
        sess.providers.insert(provider.to_string());
        *sess.all_models.entry(model.to_string()).or_insert(0) += 1;
        if routed {
            sess.routed_rounds += 1;
        } else {
            *sess.models.entry(label.clone()).or_insert(0) += 1;
            if sess.external_top.is_empty() {
                sess.external_top = label;
            }
        }
        if at_ms > 0 {
            if sess.started_ms == 0 || at_ms < sess.started_ms {
                sess.started_ms = at_ms;
            }
            if at_ms > sess.ended_ms {
                sess.ended_ms = at_ms;
            }
        }
        if let Some(records) = self.records.as_deref_mut() {
            let time = chrono::Local
                .timestamp_millis_opt(at_ms)
                .single()
                .map(|t| t.format("%Y-%m-%d %H:%M:%S").to_string())
                .unwrap_or_default();
            let secs = duration_ms.map(|ms| ms / 1000.0).unwrap_or(0.0);
            records.push(json!({
                "at": at_ms as f64 / 1000.0,
                "time": time,
                "model": model,
                "effective_model": model,
                "upstream": provider,
                "upstream_id": base,
                "ok": true,
                "status": 200,
                "elapsed_seconds": if duration_ms.is_some() { json!(secs) } else { Value::Null },
                "first_chunk_seconds": first_chunk_ms.map(|ms| json!(ms / 1000.0)).unwrap_or(Value::Null),
                "request_header_seconds": header_ms.map(|ms| json!(ms / 1000.0)).unwrap_or(Value::Null),
                "metric_source": metric_source,
                "stream": false,
                "usage": {
                    "input": usage[0], "output": usage[1], "total": usage[4],
                    "cache_read": usage[2], "cache_write": usage[3]
                },
                "trail": [{
                    "upstream": provider, "model": model, "ok": true, "status": 200,
                    "elapsed_seconds": if duration_ms.is_some() { json!(secs) } else { Value::Null }
                }],
                "source": source, "agent": agent, "session_id": sess.id,
                "project": sess.project, "routed": routed
            }));
        }
    }
}

/// DeepSeek Harness：~/.dsh/sessions/<project>/<session>/session.v4.jsonl.zstd
/// 记录形如 {"type":"assistant/message","time":<ms>,"data":{"message":{"source":{provider,model}},"usage":{inputTokens,...}}}
fn blank_stat(id: &str, name: &str) -> SrcStat {
    SrcStat { id: id.to_string(), name: name.to_string(), ..Default::default() }
}

/// 今日所有 Agent 会话轮数，以及其中真正走本机 llm-router 的轮数。
/// 返回 (全部轮数, 过路由轮数)，后者是前者的子集。
/// 今日所有 Agent 会话的全量统计：token 总量、轮数、可测耗时样本。
/// 会话记录只保留成功轮，因此成功率仍由本机 8010 流水给出；这里负责
/// 展示“全部请求”的 token/耗时，避免 HUD 只看见经过 llm-router 的 38 轮。
pub fn today_totals(port: u16) -> TodayTotals {
    let cache = TODAY_CACHE.get_or_init(|| Mutex::new(None));
    {
        let guard = cache.lock().unwrap();
        if let Some((at, t)) = guard.as_ref() {
            if at.elapsed() < Duration::from_secs(30) {
                return t.clone();
            }
        }
    }

    let home = home();
    let bases = collect_bases(&home);
    let now = chrono::Local::now();
    let secs = now.time().num_seconds_from_midnight() as i64;
    let since_ms = (now.timestamp() - secs) * 1000;
    let mut aggs: AggMap = HashMap::new();
    let mut sessions: SessList = Vec::new();
    {
        let mut ctx = Ctx { bases: &bases, port, since_ms, route_filter: 0, metric: 0, magpie: HashMap::new(), aggs: &mut aggs, sessions: &mut sessions, records: None };
        let mut st = blank_stat("dsh", "DeepSeek Harness");
        scan_dsh(&mut ctx, &home, &mut st);
        let mut st = blank_stat("claude", "Claude Code");
        scan_claude(&mut ctx, &home, &mut st);
        let mut st = blank_stat("pi", "Pi");
        scan_pi(&mut ctx, &home, &mut st);
        let mut st = blank_stat("codex", "Codex");
        scan_codex(&mut ctx, &home, &mut st);
    }
    let mut t = TodayTotals::default();
    for a in aggs.values() {
        t.total += a.rounds;
        t.input += a.input;
        t.output += a.output;
        t.cache_read += a.cache_read;
        t.cache_write += a.cache_write;
        t.durations.extend(a.durations.iter().copied());
        if a.routed {
            t.routed += a.rounds;
            t.routed_input += a.input;
            t.routed_output += a.output;
            t.routed_cache_read += a.cache_read;
            t.routed_cache_write += a.cache_write;
            t.routed_durations.extend(a.durations.iter().copied());
        }
    }
    *cache.lock().unwrap() = Some((Instant::now(), t.clone()));
    t
}

#[derive(Default)]
struct LedgerAgg {
    calls: u64,
    input: u64,
    output: u64,
    cache_read: u64,
    cache_write: u64,
    routed_calls: u64,
    routed_input: u64,
    routed_output: u64,
    routed_cache_read: u64,
    routed_cache_write: u64,
    sessions: HashSet<String>,
    upstreams: HashSet<String>,
    last_at: f64,
    durations: Vec<f64>,
    first_chunks: Vec<f64>,
}

impl LedgerAgg {
    fn add_agg(&mut self, a: &Agg) {
        self.calls += a.rounds;
        self.input += a.input;
        self.output += a.output;
        self.cache_read += a.cache_read;
        self.cache_write += a.cache_write;
        if a.routed {
            self.routed_calls += a.rounds;
            self.routed_input += a.input;
            self.routed_output += a.output;
            self.routed_cache_read += a.cache_read;
            self.routed_cache_write += a.cache_write;
        }
        self.durations.extend_from_slice(&a.durations);
        self.first_chunks.extend_from_slice(&a.first_chunks);
        self.sessions.extend(a.sessions.iter().cloned());
        if !a.provider.is_empty() {
            self.upstreams.insert(a.provider.clone());
        }
        self.last_at = self.last_at.max(a.last_ms as f64 / 1000.0);
    }

    fn add_sess(&mut self, s: &Sess) {
        self.calls += s.rounds;
        self.input += s.input;
        self.output += s.output;
        self.cache_read += s.cache_read;
        self.cache_write += s.cache_write;
        self.sessions.insert(s.id.clone());
        self.upstreams.extend(s.providers.iter().cloned());
        self.last_at = self.last_at.max(s.ended_ms as f64 / 1000.0);
    }

    fn p50(&self, p: f64) -> Value {
        if self.durations.is_empty() {
            return Value::Null;
        }
        json!(pct_ms(&self.durations, p) / 1000.0)
    }

    fn p95(&self) -> Value {
        self.p50(0.95)
    }

    fn fc50(&self) -> Value {
        if self.first_chunks.is_empty() {
            return Value::Null;
        }
        json!(pct_ms(&self.first_chunks, 0.50) / 1000.0)
    }

    fn json(&self, label: &str) -> Value {
        let prompt = self.input + self.cache_read + self.cache_write;
        json!({
            "label": label,
            "calls": self.calls,
            "routed_calls": self.routed_calls,
            "input": self.input,
            "output": self.output,
            "cache_read": self.cache_read,
            "cache_write": self.cache_write,
            "routed_input": self.routed_input,
            "routed_output": self.routed_output,
            "routed_cache_read": self.routed_cache_read,
            "routed_cache_write": self.routed_cache_write,
            "prompt_tokens": prompt,
            "p50_seconds": self.p50(0.50),
            "p95_seconds": self.p95(),
            "first_chunk_p50_seconds": self.fc50(),
            "hit_rate": if prompt > 0 { (self.cache_read as f64 / prompt as f64 * 1000.0).round() / 10.0 } else { 0.0 },
            "saved_tokens": self.cache_read,
            "sessions": self.sessions.len(),
            "upstreams": self.upstreams.iter().cloned().collect::<Vec<_>>(),
            "last_at": self.last_at,
        })
    }
}

/// 所有 Agent 会话的缓存账本。和路由流水无关：DSH / Claude / Pi / Codex
/// 各自写的会话记录里只要有 usage，就进入这里；`routed` 只是其中一个子集。
pub fn session_cache_ledger(port: u16, since_ms: i64, route: u8, metric: u8) -> Value {
    let home = home();
    let bases = collect_bases(&home);
    let mut aggs: AggMap = HashMap::new();
    let mut sessions: SessList = Vec::new();
    {
        let mut ctx = Ctx {
            bases: &bases, port, since_ms, route_filter: route, metric,
            magpie: if metric == 1 { magpie_offsets() } else { HashMap::new() },
            aggs: &mut aggs, sessions: &mut sessions, records: None,
        };
        let mut st = blank_stat("dsh", "DeepSeek Harness");
        scan_dsh(&mut ctx, &home, &mut st);
        let mut st = blank_stat("claude", "Claude Code");
        scan_claude(&mut ctx, &home, &mut st);
        let mut st = blank_stat("pi", "Pi");
        scan_pi(&mut ctx, &home, &mut st);
        let mut st = blank_stat("codex", "Codex");
        scan_codex(&mut ctx, &home, &mut st);
    }

    let mut totals = LedgerAgg::default();
    let mut models: HashMap<String, LedgerAgg> = HashMap::new();
    let mut upstreams: HashMap<String, LedgerAgg> = HashMap::new();
    for a in aggs.values() {
        totals.add_agg(a);
        if !a.model.is_empty() {
            models.entry(a.model.clone()).or_default().add_agg(a);
        }
        if !a.provider.is_empty() {
            upstreams.entry(a.provider.clone()).or_default().add_agg(a);
        }
    }

    let mut model_rows: Vec<Value> = models.into_iter().map(|(k, v)| v.json(&k)).collect();
    model_rows.sort_by_key(|r| -r["cache_read"].as_i64().unwrap_or(0));
    let mut upstream_rows: Vec<Value> = upstreams.into_iter().map(|(k, v)| v.json(&k)).collect();
    upstream_rows.sort_by_key(|r| -r["cache_read"].as_i64().unwrap_or(0));

    let mut session_rows: Vec<Value> = sessions
        .iter()
        .map(|s| {
            let mut row = LedgerAgg::default();
            row.add_sess(s);
            let model = s
                .all_models
                .iter()
                .max_by_key(|(_, n)| **n)
                .map(|(m, _)| m.clone())
                .unwrap_or_default();
            let mut val = row.json(&s.id);
            val["source"] = json!(s.source);
            val["agent"] = json!(s.agent);
            val["project"] = json!(s.project);
            val["model"] = json!(model);
            val["switches"] = json!(s.providers.len().saturating_sub(1));
            val
        })
        .collect();
    // 「按会话（最近 100）」必须按最后活动时间倒序，不能按缓存读大小排。
    session_rows.sort_by(|a, b| {
        let at = a["last_at"].as_f64().unwrap_or(0.0);
        let bt = b["last_at"].as_f64().unwrap_or(0.0);
        bt.partial_cmp(&at).unwrap_or(std::cmp::Ordering::Equal)
    });

    let total_label = if since_ms <= 0 { "全部" } else { "今天 00:00 起" };
    json!({
        "totals": totals.json(total_label),
        "models": model_rows,
        "upstreams": upstream_rows,
        "sessions": session_rows,
    })
}


/// 逐轮会话记录：给「请求记录」页展示所有 Agent 的每次调用，
/// 而不是只看经过本机 8010 的 config.history.jsonl。
pub fn session_records(port: u16, since_ms: i64, route: u8, metric: u8, day: &str, month: &str, group: &str, upstream: &str, model: &str, limit: usize, offset: usize) -> Value {
    let home = home();
    let bases = collect_bases(&home);
    let mut records: Vec<Value> = Vec::new();
    let mut aggs: AggMap = HashMap::new();
    let mut sessions: SessList = Vec::new();
    {
        let mut ctx = Ctx {
            bases: &bases, port, since_ms, route_filter: route, metric,
            magpie: if metric == 1 { magpie_offsets() } else { HashMap::new() },
            aggs: &mut aggs, sessions: &mut sessions, records: Some(&mut records),
        };
        let mut st = blank_stat("dsh", "DeepSeek Harness");
        scan_dsh(&mut ctx, &home, &mut st);
        let mut st = blank_stat("claude", "Claude Code");
        scan_claude(&mut ctx, &home, &mut st);
        let mut st = blank_stat("pi", "Pi");
        scan_pi(&mut ctx, &home, &mut st);
        let mut st = blank_stat("codex", "Codex");
        scan_codex(&mut ctx, &home, &mut st);
    }
    let mut days: Vec<String> = records.iter().filter_map(|r| r["time"].as_str().map(|s| s.chars().take(10).collect())).collect();
    days.sort(); days.dedup();
    let mut months: Vec<String> = records.iter().filter_map(|r| r["time"].as_str().map(|s| s.chars().take(7).collect())).collect();
    months.sort(); months.dedup();
    if !day.is_empty() {
        records.retain(|r| r["time"].as_str().map(|s| s.starts_with(day)).unwrap_or(false));
    }
    if !month.is_empty() {
        records.retain(|r| r["time"].as_str().map(|s| s.starts_with(month)).unwrap_or(false));
    }
    if !group.is_empty() {
        records.retain(|r| {
            let p = r["upstream"].as_str().unwrap_or("");
            let m = r["model"].as_str().unwrap_or("");
            format!("{p}/{m}") == group
        });
    }
    if !upstream.is_empty() {
        records.retain(|r| r["upstream"].as_str() == Some(upstream));
    }
    if !model.is_empty() {
        records.retain(|r| r["model"].as_str() == Some(model));
    }
    records.sort_by(|a, b| {
        let at = a["at"].as_f64().unwrap_or(0.0);
        let bt = b["at"].as_f64().unwrap_or(0.0);
        bt.partial_cmp(&at).unwrap_or(std::cmp::Ordering::Equal)
    });
    let total = records.len();
    let mut calls = 0u64; let mut input = 0u64; let mut output = 0u64;
    let mut cache_read = 0u64; let mut cache_write = 0u64;
    let mut groups: HashMap<String, (u64, u64, u64, u64, u64, String)> = HashMap::new();
    let mut ups: HashMap<String, (u64, String)> = HashMap::new();
    for r in &records {
        calls += 1;
        let u = &r["usage"];
        input += u["input"].as_u64().unwrap_or(0);
        output += u["output"].as_u64().unwrap_or(0);
        cache_read += u["cache_read"].as_u64().unwrap_or(0);
        cache_write += u["cache_write"].as_u64().unwrap_or(0);
        let provider = r["upstream"].as_str().unwrap_or("");
        let model = r["model"].as_str().unwrap_or("");
        let gkey = format!("{provider}/{model}");
        let e = groups.entry(gkey.clone()).or_insert((0, 0, 0, 0, 0, String::new()));
        e.0 += 1;
        e.1 += u["input"].as_u64().unwrap_or(0);
        e.2 += u["output"].as_u64().unwrap_or(0);
        e.3 += u["cache_read"].as_u64().unwrap_or(0);
        e.4 += u["cache_write"].as_u64().unwrap_or(0);
        e.5 = provider.to_string();
        ups.entry(format!("{provider}\u{1}{model}")).or_insert((0, provider.to_string()));
        if let Some(x) = ups.get_mut(&format!("{provider}\u{1}{model}")) { x.0 += 1; }
    }
    let mut group_rows: Vec<Value> = groups.into_iter().map(|(k, (c, i, o, cr, cw, p))| json!({
        "group": k, "calls": c, "ok": c, "failed": 0, "attempts": c,
        "avg_elapsed": 0.0, "upstream_count": 1,
        "usage": {"input": i, "output": o, "cache_read": cr, "cache_write": cw},
        "upstreams": [{"upstream": p, "calls": c, "ok": c, "failed": 0,
                       "models": [{"model": k.splitn(2, '/').nth(1).unwrap_or(""), "calls": c, "ok": c, "failed": 0}]}]
    })).collect();
    group_rows.sort_by_key(|v| std::cmp::Reverse(v["calls"].as_u64().unwrap_or(0)));
    let upstream_rows: Vec<Value> = ups.into_iter().map(|(k, (c, p))| {
        let model = k.splitn(2, '\u{1}').nth(1).unwrap_or("");
        json!({"upstream": p, "model": model, "key": format!("{p} · {model}"), "failed": 0, "calls": c})
    }).collect();
    // 口径覆盖：magpie 口径下有多少轮真的取到了网关实测值，多少轮回退到 request/header。
    let mut magpie_hits = 0usize;
    let mut header_hits = 0usize;
    // 分母只算「这一轮确实走了 Magpie 网关」的，走 dli / primerelay 的不该拉低覆盖率
    let mut via_magpie = 0usize;
    for r in records.iter() {
        let base = r.get("upstream_id").and_then(|v| v.as_str()).unwrap_or("");
        let up = r.get("upstream").and_then(|v| v.as_str()).unwrap_or("");
        if base.contains(":3425") || up.starts_with("magpie") {
            via_magpie += 1;
        }
        match r.get("metric_source").and_then(|v| v.as_str()) {
            Some("magpie") => magpie_hits += 1,
            Some("header") => header_hits += 1,
            _ => {}
        }
    }
    let metric_label = match metric {
        1 => "magpie",
        2 => "dsh",
        _ => "happycoding",
    };
    let rounds_total = records.len();
    let entries: Vec<Value> = records.into_iter().skip(offset).take(limit).collect();
    json!({
        "scope": "all", "total": total, "offset": offset, "limit": limit,
        "entries": entries,
        "metric": {"mode": metric_label, "rounds": rounds_total,
                   "magpie": magpie_hits, "header": header_hits, "via_magpie": via_magpie,
                   "magpie_available": !self_magpie_empty(metric)},
        "totals": {"calls": calls, "ok": calls, "failed": Value::Null, "attempts": calls,
                   "input": input, "output": output, "cache_read": cache_read, "cache_write": cache_write},
        "groups": group_rows, "upstreams": upstream_rows,
        "days": days.iter().map(|d| json!({"key": d})).collect::<Vec<_>>(),
        "months": months.iter().map(|m| json!({"key": m})).collect::<Vec<_>>(),
    })
}

/// magpie 口径是否有可用的网关实测值（本机没开 Magpie 时为 false，前端据此提示）。
fn self_magpie_empty(metric: u8) -> bool {
    if metric != 1 {
        return false;
    }
    let cell = MAGPIE_SNAP.get_or_init(|| Mutex::new(None));
    match cell.lock() {
        Ok(g) => g.as_ref().map(|s| s.map.is_empty()).unwrap_or(true),
        Err(_) => true,
    }
}

fn epoch_ms(v: &Value) -> i64 {
    v.get("time")
        .and_then(|x| x.as_i64().or_else(|| x.as_f64().map(|f| f as i64)))
        .unwrap_or(0)
}

fn route_value(
    source: &str,
    provider: &str,
    model: &str,
    at_ms: i64,
    active: bool,
    port: u16,
    bases: &HashMap<String, String>,
    retry: Option<&(i64, String, String)>,
) -> Value {
    let base = bases.get(provider).cloned().unwrap_or_default();
    let routed = is_router_name(provider) || is_router_url(&base, port);
    let mut routes: Vec<Value> = Vec::new();
    if let Some((_, rp, err)) = retry {
        if rp != provider {
            routes.push(json!({
                "upstream": rp,
                "base_url": bases.get(rp).cloned().unwrap_or_default(),
                "model": "",
                "ok": false,
                "status": Value::Null,
                "elapsed_seconds": Value::Null,
                "error": err,
            }));
        }
    }
    routes.push(json!({
        "upstream": provider,
        "base_url": base,
        "model": model,
        "ok": true,
        "status": Value::Null,
        "elapsed_seconds": Value::Null,
        "error": "",
    }));
    let time = chrono::Local
        .timestamp_millis_opt(at_ms)
        .single()
        .map(|t| t.format("%H:%M:%S").to_string())
        .unwrap_or_default();
    json!({
        "source": source,
        "provider": provider,
        "model": model,
        "base_url": bases.get(provider).cloned().unwrap_or_default(),
        "routed": routed,
        "active": active,
        "at": at_ms,
        "time": time,
        "routes": routes,
    })
}

fn dsh_last_round(home: &Path, port: u16, bases: &HashMap<String, String>) -> Option<(i64, Value)> {
    let root = home.join(".dsh").join("sessions");
    let mut files: Vec<PathBuf> = Vec::new();
    if let Ok(projects) = std::fs::read_dir(&root) {
        for p in projects.flatten() {
            if p.path().is_dir() {
                walk_files(&p.path(), 0, &mut files);
            }
        }
    }
    files.sort_by_key(|f| std::cmp::Reverse(mtime_ms(f)));
    let path = files.first()?;
    let f = std::fs::File::open(path).ok()?;
    let dec = zstd::stream::read::Decoder::new(BufReader::new(f)).ok()?;
    let mut last_msg: Option<(i64, String, String, i64)> = None;
    let mut last_ctx: Option<(i64, String, String)> = None;
    let mut last_retry: Option<(i64, String, String)> = None;
    for line in BufReader::new(dec).lines().flatten() {
        let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
        let typ = v.get("type").and_then(Value::as_str).unwrap_or("");
        let data = v.get("data").unwrap_or(&Value::Null);
        let at = epoch_ms(&v);
        if typ == "assistant/message" {
            let msg = data.get("message").unwrap_or(&Value::Null);
            let src = msg.get("source").unwrap_or(&Value::Null);
            let provider = src.get("provider").and_then(Value::as_str).unwrap_or("");
            let model = src.get("model").and_then(Value::as_str).unwrap_or("");
            if !provider.is_empty() {
                last_msg = Some((at, provider.to_string(), model.to_string(), pick_num(data, &["turn"]) as i64));
            }
        } else if typ == "request/context" {
            let provider = data.get("provider").and_then(Value::as_str).unwrap_or("");
            let model = data.get("model").and_then(Value::as_str).unwrap_or("");
            if !provider.is_empty() {
                last_ctx = Some((at, provider.to_string(), model.to_string()));
            }
        } else if typ == "llm/retry" {
            let provider = data.get("provider").and_then(Value::as_str).unwrap_or("");
            let err = data
                .get("failure")
                .and_then(|f| f.get("message"))
                .and_then(Value::as_str)
                .unwrap_or("");
            if !provider.is_empty() {
                last_retry = Some((at, provider.to_string(), err.to_string()));
            }
        }
    }
    let (at, provider, model, active, retry) = match (&last_ctx, &last_msg) {
        (Some(c), Some(m)) if c.0 >= m.0 => (c.0, c.1.clone(), c.2.clone(), true, None),
        (_, Some(m)) => {
            let r = last_retry.as_ref().filter(|r| r.0 <= m.0 && r.0 >= m.0.saturating_sub(120_000) && r.1 != m.1);
            (m.0, m.1.clone(), m.2.clone(), false, r)
        }
        (Some(c), None) => (c.0, c.1.clone(), c.2.clone(), true, None),
        _ => return None,
    };
    let _ = last_msg.as_ref().map(|m| m.3);
    Some((at, route_value("dsh", &provider, &model, at, active, port, bases, retry)))
}

fn latest_file(root: &Path, depth: usize) -> Option<PathBuf> {
    let mut files: Vec<PathBuf> = Vec::new();
    walk_files(root, depth, &mut files);
    files.sort_by_key(|f| std::cmp::Reverse(mtime_ms(f)));
    files.into_iter().next()
}

fn claude_last_round(home: &Path, port: u16, bases: &HashMap<String, String>) -> Option<(i64, Value)> {
    let path = latest_file(&home.join(".claude").join("projects"), 3)?;
    let at = mtime_ms(&path);
    let mut found = None;
    tail_lines(&path, 4 * 1024 * 1024, &mut |line| {
        let Ok(v) = serde_json::from_str::<Value>(line) else { return };
        if v.get("type").and_then(Value::as_str) != Some("assistant") { return; }
        let msg = v.get("message").unwrap_or(&Value::Null);
        let model = msg.get("model").and_then(Value::as_str).unwrap_or("");
        if !model.is_empty() { found = Some(model.to_string()); }
    });
    found.map(|model| (at, route_value("claude", "anthropic", &model, at, false, port, bases, None)))
}

fn pi_last_round(home: &Path, port: u16, bases: &HashMap<String, String>) -> Option<(i64, Value)> {
    let path = latest_file(&home.join(".pi").join("agent").join("sessions"), 3)?;
    let at = mtime_ms(&path);
    let mut found = None;
    tail_lines(&path, 4 * 1024 * 1024, &mut |line| {
        let Ok(v) = serde_json::from_str::<Value>(line) else { return };
        if v.get("type").and_then(Value::as_str) != Some("message") { return; }
        let msg = v.get("message").unwrap_or(&Value::Null);
        if msg.get("role").and_then(Value::as_str) != Some("assistant") { return; }
        let provider = msg.get("provider").and_then(Value::as_str).unwrap_or("pi");
        let model = msg.get("model").and_then(Value::as_str).unwrap_or("");
        if !model.is_empty() { found = Some((provider.to_string(), model.to_string())); }
    });
    found.map(|(provider, model)| (at, route_value("pi", &provider, &model, at, false, port, bases, None)))
}

/// 最近一轮对话实际使用的路由（含没有经过 llm-router 的调用）。
/// `active` 为 true 表示会话里已经发出 request/context 但还没有对应回复，通常就是当前正在跑的请求。
pub fn last_round(port: u16) -> Value {
    let cache = LAST_ROUND_CACHE.get_or_init(|| Mutex::new(None));
    let mut guard = cache.lock().unwrap();
    if let Some((at, value)) = guard.as_ref() {
        if at.elapsed() < Duration::from_secs(3) {
            return value.clone();
        }
    }
    let home = home();
    let bases = collect_bases(&home);
    let mut found: Vec<(i64, Value)> = Vec::new();
    if let Some(v) = dsh_last_round(&home, port, &bases) { found.push(v); }
    if let Some(v) = claude_last_round(&home, port, &bases) { found.push(v); }
    if let Some(v) = pi_last_round(&home, port, &bases) { found.push(v); }
    found.sort_by_key(|(at, _)| std::cmp::Reverse(*at));
    let value = found.into_iter().next().map(|(_, v)| v).unwrap_or(Value::Null);
    *guard = Some((Instant::now(), value.clone()));
    value
}

fn scan_dsh(ctx: &mut Ctx, home: &Path, st: &mut SrcStat) {

    let root = home.join(".dsh").join("sessions");
    st.root = root.to_string_lossy().to_string();
    if !root.is_dir() {
        st.note = "没有找到 ~/.dsh/sessions".into();
        return;
    }
    let mut files: Vec<PathBuf> = Vec::new();
    if let Ok(projects) = std::fs::read_dir(&root) {
        for p in projects.flatten() {
            if p.path().is_dir() {
                walk_files(&p.path(), 0, &mut files);
            }
        }
    }
    files.retain(|f| {
        f.file_name()
            .map(|n| n.to_string_lossy().ends_with(".jsonl.zstd") || n.to_string_lossy().ends_with(".jsonl"))
            .unwrap_or(false)
    });
    // 新的先扫
    files.sort_by_key(|f| std::cmp::Reverse(mtime_ms(f)));

    for path in files {
        let mt = mtime_ms(&path);
        if ctx.since_ms > 0 && mt > 0 && mt < ctx.since_ms {
            st.skipped_old += 1;
            continue;
        }
        let project = path
            .parent()
            .and_then(|p| p.parent())
            .and_then(|p| p.file_name())
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        let mut sess = Sess {
            source: "dsh".into(),
            agent: "DeepSeek Harness".into(),
            id: path
                .parent()
                .and_then(|p| p.file_name())
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| "(未知会话)".into()),
            project,
            cwd: String::new(),
            path: path.to_string_lossy().to_string(),
            started_ms: 0,
            ended_ms: 0,
            rounds: 0,
            routed_rounds: 0,
            input: 0,
            output: 0,
            cache_read: 0,
            cache_write: 0,
            models: HashMap::new(),
            all_models: HashMap::new(),
            providers: HashSet::new(),
            external_top: String::new(),
        };
        st.files += 1;
        let started = std::time::Instant::now();
        let is_zstd = path.to_string_lossy().ends_with(".zstd");
        let lines: Box<dyn Read> = if is_zstd {
            match std::fs::File::open(&path)
                .map_err(|e| e.to_string())
                .and_then(|f| {
                    zstd::stream::read::Decoder::new(BufReader::new(f)).map_err(|e| e.to_string())
                }) {
                Ok(d) => Box::new(d.take(FILE_CAP)),
                Err(e) => {
                    st.note = format!("zstd 解压失败: {e}");
                    continue;
                }
            }
        } else {
            match std::fs::File::open(&path) {
                Ok(f) => Box::new(f.take(FILE_CAP)),
                Err(_) => continue,
            }
        };
        let mut read = 0u64;
        let mut first = true;
        let mut step_starts: HashMap<(i64, i64), i64> = HashMap::new();
        // request/header 不带 turn/step，只能按「最近一次 step/start」归属；
        // startsSeries=true 表示这条 header 开启一个新的配置序列（DSH 会去重复用）。
        let mut header_starts: HashMap<(i64, i64), i64> = HashMap::new();
        let mut pending: Option<(i64, i64)> = None;
        for line in BufReader::new(lines).lines() {
            let Ok(line) = line else { break };
            read += line.len() as u64 + 1;
            if first {
                first = false;
                if let Ok(v) = serde_json::from_str::<Value>(&line) {
                    if v.get("type").and_then(|t| t.as_str()) == Some("session") {
                        sess.cwd = v.get("cwd").and_then(|c| c.as_str()).unwrap_or("").into();
                        if let Some(id) = v.get("id").and_then(|c| c.as_str()) {
                            sess.id = id.to_string();
                        }
                    }
                }
            }
            if line.contains("step/start") {
                if let Ok(v) = serde_json::from_str::<Value>(&line) {
                    if v.get("type").and_then(|t| t.as_str()) == Some("step/start") {
                        let d = v.get("data").cloned().unwrap_or(Value::Null);
                        let turn = d.get("turn").and_then(|x| x.as_i64()).unwrap_or(0);
                        let step = d.get("step").and_then(|x| x.as_i64()).unwrap_or(0);
                        let at = v.get("time").and_then(|t| t.as_i64()).unwrap_or(0);
                        if turn > 0 && step > 0 && at > 0 {
                            step_starts.insert((turn, step), at);
                            pending = Some((turn, step));
                        }
                        continue;
                    }
                }
            }
            if line.contains("request/header") {
                if let (Some(key), Ok(v)) = (pending, serde_json::from_str::<Value>(&line)) {
                    if v.get("type").and_then(|t| t.as_str()) == Some("request/header") {
                        if let Some(at) = v.get("time").and_then(|t| t.as_i64()) {
                            // 同一轮可能在重试时写多条 header，取最早那条（第一条发出前的时刻）
                            header_starts.entry(key).or_insert(at);
                        }
                    }
                }
                continue;
            }
            if !line.contains("assistant/message") {
                continue;
            }
            let Ok(v) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if v.get("type").and_then(|t| t.as_str()) != Some("assistant/message") {
                continue;
            }
            let at = v.get("time").and_then(|t| t.as_i64()).unwrap_or(0);
            if ctx.since_ms > 0 && at > 0 && at < ctx.since_ms {
                continue;
            }
            let d = v.get("data").cloned().unwrap_or(Value::Null);
            let turn = d.get("turn").and_then(|x| x.as_i64()).unwrap_or(0);
            let step = d.get("step").and_then(|x| x.as_i64()).unwrap_or(0);
            let start = step_starts.remove(&(turn, step)).filter(|start| at > *start);
            let header_ms = match (start, header_starts.get(&(turn, step)).copied()) {
                (Some(start), Some(h)) if h >= start => Some((h - start) as f64),
                _ => None,
            };
            // 一帧流里有三类时间点，三种口径各取所需：
            //   frame     第一个 SSE 帧（含 role / block-start 这类空帧）
            //   content   第一个真正带内容的块（跳过 block-start / block-end / usage / finish）
            //   end       最后一个流块的时间（= 收到最后一个字节）
            let stream = d.get("stream").and_then(|s| s.as_array());
            let frame_time = stream
                .and_then(|arr| arr.iter().filter_map(|x| x.get("time").and_then(|t| t.as_i64())).min());
            let content_time = stream.and_then(|arr| {
                arr.iter()
                    .filter(|x| {
                        let t = x.get("chunk").and_then(|c| c.get("type")).and_then(|t| t.as_str());
                        !matches!(t, Some("block-start") | Some("block-end") | Some("usage") | Some("finish"))
                    })
                    .filter_map(|x| {
                        x.get("time")
                            .and_then(|t| t.as_i64())
                            .or_else(|| x.get("time0").and_then(|t| t.as_i64()))
                    })
                    .min()
            });
            let end_time = stream.and_then(|arr| arr.iter().filter_map(|x| x.get("time").and_then(|t| t.as_i64())).max());
            // 口径 0 = happycoding（New API）：首字取第一个 SSE 帧，耗时向下取整到整秒
            // 口径 1 = magpie：首字取第一个有内容的块，耗时到最后一个字节
            // 口径 2 = dsh（当前）：首字取第一个流帧，耗时到 assistant/message
            let (first_chunk, duration) = match start {
                Some(start) if at > start => match ctx.metric {
                    1 => (
                        content_time.filter(|t| *t >= start).map(|t| (t - start) as f64),
                        end_time.filter(|t| *t > start).map(|t| (t - start) as f64),
                    ),
                    2 => (
                        frame_time.filter(|t| *t >= start).map(|t| (t - start) as f64),
                        Some((at - start) as f64),
                    ),
                    _ => (
                        frame_time.filter(|t| *t >= start).map(|t| (t - start) as f64),
                        Some((((at - start) as f64) / 1000.0).floor() * 1000.0),
                    ),
                },
                _ => (None, None),
            };
            let src = d
                .get("message")
                .and_then(|m| m.get("source"))
                .cloned()
                .unwrap_or(Value::Null);
            let provider = src.get("provider").and_then(|x| x.as_str()).unwrap_or("");
            let model = src.get("model").and_then(|x| x.as_str()).unwrap_or("");
            let u = d.get("usage").cloned().unwrap_or(Value::Null);
            let usage = [
                pick_num(&u, &["inputTokens", "input", "prompt_tokens"]),
                pick_num(&u, &["outputTokens", "output"]),
                pick_num(&u, &["cacheReadTokens", "cache_read"]),
                pick_num(&u, &["cacheWriteTokens", "cache_write"]),
                pick_num(&u, &["totalTokens", "total"]),
            ];
            ctx.round("dsh", "DeepSeek Harness", &mut sess, provider, model, at, usage, duration, first_chunk, header_ms);
        }
        if read >= FILE_CAP {
            st.truncated = true;
        }
        st.bytes += read;
        st.ms += started.elapsed().as_millis() as u64;
        if sess.rounds > 0 {
            st.rounds += sess.rounds;
            st.sessions += 1;
            ctx.sessions.push(sess);
        }
    }
}

/// Claude Code：~/.claude/projects/<project>/<session>.jsonl
fn scan_claude(ctx: &mut Ctx, home: &Path, st: &mut SrcStat) {
    let root = home.join(".claude").join("projects");
    st.root = root.to_string_lossy().to_string();
    if !root.is_dir() {
        st.note = "没有找到 ~/.claude/projects".into();
        return;
    }
    let mut files: Vec<PathBuf> = Vec::new();
    walk_files(&root, 0, &mut files);
    files.retain(|f| {
        f.file_name()
            .map(|n| n.to_string_lossy().ends_with(".jsonl"))
            .unwrap_or(false)
    });
    files.sort_by_key(|f| std::cmp::Reverse(mtime_ms(f)));
    for path in files {
        let mt = mtime_ms(&path);
        if ctx.since_ms > 0 && mt > 0 && mt < ctx.since_ms {
            st.skipped_old += 1;
            continue;
        }
        let mut sess = Sess {
            source: "claude".into(),
            agent: "Claude Code".into(),
            id: path
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default(),
            project: path
                .parent()
                .and_then(|p| p.file_name())
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default(),
            cwd: String::new(),
            path: path.to_string_lossy().to_string(),
            started_ms: 0,
            ended_ms: 0,
            rounds: 0,
            routed_rounds: 0,
            input: 0,
            output: 0,
            cache_read: 0,
            cache_write: 0,
            models: HashMap::new(),
            all_models: HashMap::new(),
            providers: HashSet::new(),
            external_top: String::new(),
        };
        st.files += 1;
        let started = std::time::Instant::now();
        let n = tail_lines(&path, FILE_CAP, &mut |line| {
            if !line.contains("\"assistant\"") && !line.contains("\"api-request\"") {
                return;
            }
            let Ok(v) = serde_json::from_str::<Value>(line) else {
                return;
            };
            let t = v.get("type").and_then(|x| x.as_str()).unwrap_or("");
            let at = v
                .get("timestamp")
                .and_then(|x| x.as_str())
                .map(iso_ms)
                .unwrap_or(0);
            if ctx.since_ms > 0 && at > 0 && at < ctx.since_ms {
                return;
            }
            if sess.cwd.is_empty() {
                if let Some(c) = v.get("cwd").and_then(|x| x.as_str()) {
                    sess.cwd = c.to_string();
                }
            }
            if t == "assistant" {
                let msg = v.get("message").cloned().unwrap_or(Value::Null);
                let model = msg.get("model").and_then(|x| x.as_str()).unwrap_or("");
                let u = msg.get("usage").cloned().unwrap_or(Value::Null);
                let usage = [
                    pick_num(&u, &["input_tokens", "input"]),
                    pick_num(&u, &["output_tokens", "output"]),
                    pick_num(&u, &["cache_read_input_tokens", "cache_read"]),
                    pick_num(&u, &["cache_creation_input_tokens", "cache_write"]),
                    pick_num(&u, &["total_tokens", "total"]),
                ];
                let provider = v
                    .get("provider")
                    .and_then(|x| x.as_str())
                    .filter(|s| !s.is_empty())
                    .unwrap_or("anthropic");
                ctx.round("claude", "Claude Code", &mut sess, provider, model, at, usage, None, None, None);
            }
        });
        st.bytes += n;
        if n >= FILE_CAP {
            st.truncated = true;
        }
        st.ms += started.elapsed().as_millis() as u64;
        if sess.rounds > 0 {
            st.rounds += sess.rounds;
            st.sessions += 1;
            ctx.sessions.push(sess);
        }
    }
}

/// Pi：~/.pi/agent/sessions/<project>/<ts>_<id>.jsonl
fn scan_pi(ctx: &mut Ctx, home: &Path, st: &mut SrcStat) {
    let root = home.join(".pi").join("agent").join("sessions");
    st.root = root.to_string_lossy().to_string();
    if !root.is_dir() {
        st.note = "没有找到 ~/.pi/agent/sessions".into();
        return;
    }
    let mut files: Vec<PathBuf> = Vec::new();
    walk_files(&root, 0, &mut files);
    files.retain(|f| {
        f.file_name()
            .map(|n| n.to_string_lossy().ends_with(".jsonl"))
            .unwrap_or(false)
    });
    files.sort_by_key(|f| std::cmp::Reverse(mtime_ms(f)));
    for path in files {
        let mt = mtime_ms(&path);
        if ctx.since_ms > 0 && mt > 0 && mt < ctx.since_ms {
            st.skipped_old += 1;
            continue;
        }
        let mut sess = Sess {
            source: "pi".into(),
            agent: "Pi".into(),
            id: path
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default(),
            project: path
                .parent()
                .and_then(|p| p.file_name())
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default(),
            cwd: String::new(),
            path: path.to_string_lossy().to_string(),
            started_ms: 0,
            ended_ms: 0,
            rounds: 0,
            routed_rounds: 0,
            input: 0,
            output: 0,
            cache_read: 0,
            cache_write: 0,
            models: HashMap::new(),
            all_models: HashMap::new(),
            providers: HashSet::new(),
            external_top: String::new(),
        };
        st.files += 1;
        let started = std::time::Instant::now();
        let mut cur_provider = String::new();
        let n = tail_lines(&path, FILE_CAP, &mut |line| {
            if !line.contains("\"message\"") && !line.contains("model_change") {
                return;
            }
            let Ok(v) = serde_json::from_str::<Value>(line) else {
                return;
            };
            let t = v.get("type").and_then(|x| x.as_str()).unwrap_or("");
            if t == "session" {
                if let Some(c) = v.get("cwd").and_then(|x| x.as_str()) {
                    sess.cwd = c.to_string();
                }
                return;
            }
            if t == "model_change" {
                cur_provider = v
                    .get("provider")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string();
                return;
            }
            if t != "message" {
                return;
            }
            let msg = v.get("message").cloned().unwrap_or(Value::Null);
            if msg.get("role").and_then(|x| x.as_str()) != Some("assistant") {
                return;
            }
            let at = msg
                .get("timestamp")
                .and_then(|x| x.as_i64())
                .or_else(|| v.get("timestamp").and_then(|x| x.as_str()).map(iso_ms))
                .unwrap_or(0);
            if ctx.since_ms > 0 && at > 0 && at < ctx.since_ms {
                return;
            }
            let provider = msg
                .get("provider")
                .and_then(|x| x.as_str())
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string())
                .unwrap_or_else(|| cur_provider.clone());
            let model = msg.get("model").and_then(|x| x.as_str()).unwrap_or("");
            let u = msg.get("usage").cloned().unwrap_or(Value::Null);
            let usage = [
                pick_num(&u, &["input"]),
                pick_num(&u, &["output"]),
                pick_num(&u, &["cacheRead"]),
                pick_num(&u, &["cacheWrite"]),
                pick_num(&u, &["totalTokens", "total"]),
            ];
            ctx.round("pi", "Pi", &mut sess, &provider, model, at, usage, None, None, None);
        });
        st.bytes += n;
        if n >= FILE_CAP {
            st.truncated = true;
        }
        st.ms += started.elapsed().as_millis() as u64;
        if sess.rounds > 0 {
            st.rounds += sess.rounds;
            st.sessions += 1;
            ctx.sessions.push(sess);
        }
    }
}

/// Codex：~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl（单文件可能上百 MB，只读尾部）
fn scan_codex(ctx: &mut Ctx, home: &Path, st: &mut SrcStat) {
    let root = home.join(".codex").join("sessions");
    st.root = root.to_string_lossy().to_string();
    if !root.is_dir() {
        st.note = "没有找到 ~/.codex/sessions".into();
        return;
    }
    let mut files: Vec<PathBuf> = Vec::new();
    walk_files(&root, 0, &mut files);
    files.retain(|f| {
        f.file_name()
            .map(|n| n.to_string_lossy().ends_with(".jsonl"))
            .unwrap_or(false)
    });
    files.sort_by_key(|f| std::cmp::Reverse(mtime_ms(f)));
    for path in files {
        let mt = mtime_ms(&path);
        if ctx.since_ms > 0 && mt > 0 && mt < ctx.since_ms {
            st.skipped_old += 1;
            continue;
        }
        let mut sess = Sess {
            source: "codex".into(),
            agent: "Codex".into(),
            id: path
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default(),
            project: path
                .parent()
                .map(|p| p.to_string_lossy().replace('\\', "/"))
                .unwrap_or_default(),
            cwd: String::new(),
            path: path.to_string_lossy().to_string(),
            started_ms: 0,
            ended_ms: 0,
            rounds: 0,
            routed_rounds: 0,
            input: 0,
            output: 0,
            cache_read: 0,
            cache_write: 0,
            models: HashMap::new(),
            all_models: HashMap::new(),
            providers: HashSet::new(),
            external_top: String::new(),
        };
        st.files += 1;
        let started = std::time::Instant::now();
        let mut meta_provider = String::new();
        let mut cur_model = String::new();
        let n = tail_lines(&path, FILE_CAP, &mut |line| {
            if !line.contains("token_usage_record")
                && !line.contains("session_meta")
                && !line.contains("turn_context")
            {
                return;
            }
            let Ok(v) = serde_json::from_str::<Value>(line) else {
                return;
            };
            let t = v.get("type").and_then(|x| x.as_str()).unwrap_or("");
            let p = v.get("payload").cloned().unwrap_or(Value::Null);
            match t {
                "session_meta" => {
                    meta_provider = p
                        .get("model_provider")
                        .and_then(|x| x.as_str())
                        .unwrap_or("")
                        .to_string();
                    if let Some(c) = p.get("cwd").and_then(|x| x.as_str()) {
                        sess.cwd = c.to_string();
                    }
                    if let Some(id) = p.get("session_id").and_then(|x| x.as_str()) {
                        sess.id = id.to_string();
                    }
                }
                "turn_context" => {
                    if let Some(m) = p.get("model").and_then(|x| x.as_str()) {
                        cur_model = m.to_string();
                    }
                }
                "token_usage_record" => {
                    let at = v
                        .get("timestamp")
                        .and_then(|x| x.as_str())
                        .map(iso_ms)
                        .unwrap_or(0);
                    if ctx.since_ms > 0 && at > 0 && at < ctx.since_ms {
                        return;
                    }
                    let u = p.get("usage").cloned().unwrap_or(Value::Null);
                    let usage = [
                        pick_num(&u, &["input_tokens"]),
                        pick_num(&u, &["output_tokens"]),
                        pick_num(&u, &["cached_input_tokens"]),
                        pick_num(&u, &["cache_write_input_tokens"]),
                        pick_num(&u, &["total_tokens"]),
                    ];
                    let provider = if meta_provider.is_empty() {
                        "(未记录)"
                    } else {
                        meta_provider.as_str()
                    };
                    ctx.round("codex", "Codex", &mut sess, provider, &cur_model, at, usage, None, None, None);
                }
                _ => {}
            }
        });
        st.bytes += n;
        if n >= FILE_CAP {
            st.truncated = true;
        }
        st.ms += started.elapsed().as_millis() as u64;
        if sess.rounds > 0 {
            st.rounds += sess.rounds;
            st.sessions += 1;
            ctx.sessions.push(sess);
        }
    }
}

// ───────────────────────── 路由流水对照 ─────────────────────────

fn journal_window(state: &AppState, since_ms: i64, route: u8) -> Value {
    if route == 2 {
        return json!({"path": state.history_path().to_string_lossy().to_string(), "rounds": 0, "input": 0, "output": 0, "cache_read": 0, "cache_write": 0, "total": 0, "by_model": []});
    }
    let path = state.history_path();
    let since = if since_ms > 0 { since_ms as f64 / 1000.0 } else { 0.0 };
    let entries = super::journal_index::entries(state, since, None);
    let mut rounds = 0u64;
    let mut input = 0u64;
    let mut output = 0u64;
    let mut cache_read = 0u64;
    let mut cache_write = 0u64;
    let mut by_model: HashMap<String, (u64, u64, u64)> = HashMap::new();
    for v in &entries {
        let u = v.get("usage").cloned().unwrap_or(Value::Null);
        let i = pick_num(&u, &["input", "prompt_tokens"]);
        let o = pick_num(&u, &["output", "completion_tokens"]);
        let cr = pick_num(&u, &["cache_read"]);
        let cw = pick_num(&u, &["cache_write"]);
        rounds += 1;
        input += i;
        output += o;
        cache_read += cr;
        cache_write += cw;
        let model = v.get("model").and_then(|x| x.as_str()).unwrap_or("(未知)");
        let e = by_model.entry(model.to_string()).or_insert((0, 0, 0));
        e.0 += 1;
        e.1 += i;
        e.2 += o;
    }
    let mut models: Vec<Value> = by_model
        .into_iter()
        .map(|(m, (r, i, o))| json!({"model": m, "rounds": r, "input": i, "output": o}))
        .collect();
    models.sort_by_key(|v| std::cmp::Reverse(v.get("rounds").and_then(|x| x.as_u64()).unwrap_or(0)));
    json!({
        "path": path.to_string_lossy().to_string(),
        "rounds": rounds,
        "input": input,
        "output": output,
        "cache_read": cache_read,
        "cache_write": cache_write,
        "total": input + output,
        "by_model": models.into_iter().take(50).collect::<Vec<_>>(),
    })
}

// ───────────────────────── handler ─────────────────────────

pub async fn scan(State(state): State<Arc<AppState>>, Query(q): Query<ScanQuery>) -> Response {
    let port = {
        let p = state.port.load(std::sync::atomic::Ordering::Relaxed);
        if p == 0 {
            8010
        } else {
            p
        }
    };
    let has_hours = q.hours.is_some();
    let hours = q.hours.unwrap_or(168);
    let mut range = q
        .range
        .as_deref()
        .map(|s| s.trim().to_ascii_lowercase())
        .unwrap_or_default();
    // 默认看「今天」：既没传 range 也没传 hours 时按本机时区当天 00:00 起算。
    if range.is_empty() && !has_hours {
        range = "today".to_string();
    }
    let (since_ms, window_label) = match range.as_str() {
        // 「今天」按本机时区当天 00:00 起算，不是滚动 24 小时。
        "today" => {
            let now = chrono::Local::now();
            let secs = now.time().num_seconds_from_midnight() as i64;
            ((now.timestamp() - secs) * 1000, "today".to_string())
        }
        "all" => (0, "all".to_string()),
        _ => (
            if hours <= 0 {
                0
            } else {
                now_ms() - hours * 3600 * 1000
            },
            format!("{hours}h"),
        ),
    };
    let wanted: Vec<String> = match q.sources.as_deref().map(|s| s.trim()) {
        Some(s) if !s.is_empty() => s
            .split(',')
            .map(|x| x.trim().to_ascii_lowercase())
            .filter(|x| !x.is_empty())
            .collect(),
        _ => vec![
            "dsh".to_string(),
            "claude".to_string(),
            "pi".to_string(),
            "codex".to_string(),
        ],
    };
    let row_limit = q.limit.unwrap_or(ROW_LIMIT).clamp(10, 500);
    let route_filter = route_filter(q.route.as_deref());
    let metric = metric_mode(q.metric.as_deref());
    if wanted.is_empty() {
        return bad("sources 参数为空，请至少选一个数据源（dsh,claude,pi,codex）");
    }
    let cache_key = super::collector::scan_key(&q);
    if let Some(v) = super::collector::get(&cache_key, 15) {
        return Json(v).into_response();
    }

    let home = home();
    let started = std::time::Instant::now();
    let bases = collect_bases(&home);
    let mut aggs: AggMap = HashMap::new();
    let mut sessions: SessList = Vec::new();
    let mut stats: Vec<SrcStat> = Vec::new();

    let add_stat = |id: &str, name: &str| SrcStat {
        id: id.to_string(),
        name: name.to_string(),
        root: String::new(),
        files: 0,
        sessions: 0,
        bytes: 0,
        rounds: 0,
        skipped_old: 0,
        truncated: false,
        ms: 0,
        note: String::new(),
    };

    if wanted.iter().any(|s| s == "dsh") {
        let mut st = add_stat("dsh", "DeepSeek Harness");
        {
            let mut ctx = Ctx {
                bases: &bases,
                port,
                since_ms,
                route_filter,
                metric,
                magpie: HashMap::new(),
                aggs: &mut aggs,
                sessions: &mut sessions,
                records: None,
            };
            scan_dsh(&mut ctx, &home, &mut st);
        }
        stats.push(st);
    }
    if wanted.iter().any(|s| s == "claude") {
        let mut st = add_stat("claude", "Claude Code");
        {
            let mut ctx = Ctx {
                bases: &bases,
                port,
                since_ms,
                route_filter,
                metric,
                magpie: HashMap::new(),
                aggs: &mut aggs,
                sessions: &mut sessions,
                records: None,
            };
            scan_claude(&mut ctx, &home, &mut st);
        }
        stats.push(st);
    }
    if wanted.iter().any(|s| s == "pi") {
        let mut st = add_stat("pi", "Pi");
        {
            let mut ctx = Ctx {
                bases: &bases,
                port,
                since_ms,
                route_filter,
                metric,
                magpie: HashMap::new(),
                aggs: &mut aggs,
                sessions: &mut sessions,
                records: None,
            };
            scan_pi(&mut ctx, &home, &mut st);
        }
        stats.push(st);
    }
    if wanted.iter().any(|s| s == "codex") {
        let mut st = add_stat("codex", "Codex");
        {
            let mut ctx = Ctx {
                bases: &bases,
                port,
                since_ms,
                route_filter,
                metric,
                magpie: HashMap::new(),
                aggs: &mut aggs,
                sessions: &mut sessions,
                records: None,
            };
            scan_codex(&mut ctx, &home, &mut st);
        }
        stats.push(st);
    }

    let journal = journal_window(&state, since_ms, route_filter);

    // 汇总
    let mut rows: Vec<Agg> = aggs.into_values().collect();
    rows.sort_by(|a, b| {
        a.routed
            .cmp(&b.routed)
            .then(b.rounds.cmp(&a.rounds))
            .then(a.provider.cmp(&b.provider))
    });
    let mut t_rounds = 0u64;
    let mut e_rounds = 0u64;
    let mut t_in = 0u64;
    let mut t_out = 0u64;
    let mut t_cr = 0u64;
    let mut t_cw = 0u64;
    let mut e_in = 0u64;
    let mut e_out = 0u64;
    let mut e_cr = 0u64;
    let mut e_cw = 0u64;
    for r in &rows {
        t_rounds += r.rounds;
        t_in += r.input;
        t_out += r.output;
        t_cr += r.cache_read;
        t_cw += r.cache_write;
        if !r.routed {
            e_rounds += r.rounds;
            e_in += r.input;
            e_out += r.output;
            e_cr += r.cache_read;
            e_cw += r.cache_write;
        }
    }

    let row_json = |r: &Agg| {
        json!({
            "source": r.source,
            "agent": r.agent,
            "provider": r.provider,
            "model": r.model,
            "base_url": r.base_url,
            "routed": r.routed,
            "rounds": r.rounds,
            "input": r.input,
            "output": r.output,
            "cache_read": r.cache_read,
            "cache_write": r.cache_write,
            "total": r.total,
            "first_ms": r.first_ms,
            "last_ms": r.last_ms,
        })
    };

    sessions.sort_by_key(|s| std::cmp::Reverse(s.ended_ms));
    let sess_json: Vec<Value> = sessions
        .iter()
        .take(SESSION_LIMIT)
        .map(|s| {
            let mut models: Vec<(String, u64)> = s.models.iter().map(|(k, v)| (k.clone(), *v)).collect();
            models.sort_by_key(|(_, v)| std::cmp::Reverse(*v));
            json!({
                "source": s.source,
                "agent": s.agent,
                "id": s.id,
                "project": s.project,
                "cwd": s.cwd,
                "path": s.path,
                "started_ms": s.started_ms,
                "ended_ms": s.ended_ms,
                "rounds": s.rounds,
                "routed_rounds": s.routed_rounds,
                "external_rounds": s.rounds.saturating_sub(s.routed_rounds),
                "external_models": models.into_iter().take(5).map(|(m, c)| json!({"label": m, "rounds": c})).collect::<Vec<_>>(),
            })
        })
        .collect();

    let value = json!({
        "ok": true,
        "port": port,
        "base_url": format!("http://127.0.0.1:{port}/v1"),
        "generated_at": now_ms(),
        "window_hours": hours,
        "range": window_label,
        "since_ms": since_ms,
        "elapsed_ms": started.elapsed().as_millis() as u64,
        "sources": stats.iter().map(|s| json!({
            "id": s.id,
            "name": s.name,
            "root": s.root,
            "files": s.files,
            "sessions": s.sessions,
            "bytes": s.bytes,
            "rounds": s.rounds,
            "skipped_old": s.skipped_old,
            "truncated": s.truncated,
            "ms": s.ms,
            "note": s.note,
        })).collect::<Vec<_>>(),
        "rows": rows.iter().take(row_limit).map(row_json).collect::<Vec<_>>(),
        "rows_total": rows.len(),
        "totals": {
            "rounds": t_rounds, "input": t_in, "output": t_out,
            "cache_read": t_cr, "cache_write": t_cw, "total": t_in + t_out,
        },
        "external_totals": {
            "rounds": e_rounds, "input": e_in, "output": e_out,
            "cache_read": e_cr, "cache_write": e_cw, "total": e_in + e_out,
        },
        "routed_totals": {
            "rounds": t_rounds - e_rounds,
            "input": t_in - e_in,
            "output": t_out - e_out,
            "cache_read": t_cr - e_cr,
            "cache_write": t_cw - e_cw,
            "total": (t_in + t_out) - (e_in + e_out),
        },
        "provider_bases": bases,
        "journal": journal,
        "sessions": sess_json,
        "sessions_total": sessions.len(),
    });
    super::collector::put(cache_key, value.clone());
    Json(value).into_response()
}


// ───────────────────── MCP / 工具调用排行榜 ─────────────────────

#[derive(Deserialize, Default, Clone)]
pub struct ToolQuery {
    pub hours: Option<i64>,
    pub range: Option<String>,
    pub sources: Option<String>,
    pub limit: Option<usize>,
}

#[derive(Default)]
struct ToolAgg {
    name: String,
    calls: u64,
    failed: u64,
    durations: Vec<f64>,
    by_source: HashMap<String, u64>,
    sessions: std::collections::HashSet<String>,
    first_at: i64,
    last_at: i64,
    errors: HashMap<String, u64>,
}

fn touch_tool<'a>(
    aggs: &'a mut HashMap<String, ToolAgg>,
    name: &str,
    source: &str,
    session: &str,
    at: i64,
) -> &'a mut ToolAgg {
    let e = aggs.entry(name.to_string()).or_insert_with(|| ToolAgg {
        name: name.to_string(),
        ..Default::default()
    });
    e.calls += 1;
    *e.by_source.entry(source.to_string()).or_default() += 1;
    e.sessions.insert(session.to_string());
    if e.first_at == 0 || (at > 0 && at < e.first_at) {
        e.first_at = at;
    }
    if at > e.last_at {
        e.last_at = at;
    }
    e
}

fn tool_result(
    aggs: &mut HashMap<String, ToolAgg>,
    name: &str,
    start_ms: i64,
    end_ms: i64,
    is_error: bool,
    err: Option<String>,
) {
    let Some(e) = aggs.get_mut(name) else { return };
    if start_ms > 0 && end_ms >= start_ms {
        e.durations.push((end_ms - start_ms) as f64 / 1000.0);
    }
    if is_error {
        e.failed += 1;
        if let Some(msg) = err {
            let key: String = msg.chars().take(180).collect();
            if !key.is_empty() {
                *e.errors.entry(key).or_default() += 1;
            }
        }
    }
}

fn pct_ms(samples: &[f64], p: f64) -> f64 {
    if samples.is_empty() {
        return 0.0;
    }
    let mut v = samples.to_vec();
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let idx = ((v.len() as f64 - 1.0) * p).round() as usize;
    (v[idx.min(v.len() - 1)] * 1000.0 * 10.0).round() / 10.0
}

fn error_text(message: &Value) -> Option<String> {
    if let Some(s) = message.get("content").and_then(|c| c.as_str()) {
        return Some(s.chars().take(180).collect());
    }
    message
        .get("content")
        .and_then(|c| c.as_array())
        .and_then(|a| a.first())
        .and_then(|b| b.get("text"))
        .and_then(|s| s.as_str())
        .map(|s| s.chars().take(180).collect())
}

fn scan_dsh_tools(
    aggs: &mut HashMap<String, ToolAgg>,
    home: &Path,
    since_ms: i64,
    stat: &mut (u64, u64),
) {
    let root = home.join(".dsh").join("sessions");
    if !root.is_dir() {
        return;
    }
    let mut files: Vec<PathBuf> = Vec::new();
    if let Ok(projects) = std::fs::read_dir(&root) {
        for p in projects.flatten() {
            if p.path().is_dir() {
                walk_files(&p.path(), 0, &mut files);
            }
        }
    }
    files.retain(|f| {
        f.file_name()
            .map(|n| n.to_string_lossy().ends_with(".jsonl.zstd") || n.to_string_lossy().ends_with(".jsonl"))
            .unwrap_or(false)
    });
    files.sort_by_key(|f| std::cmp::Reverse(mtime_ms(f)));
    for path in files.into_iter().take(FILE_LIMIT) {
        if since_ms > 0 && mtime_ms(&path) > 0 && mtime_ms(&path) < since_ms {
            continue;
        }
        stat.0 += 1;
        let session = path
            .parent()
            .and_then(|p| p.file_name())
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        let is_zstd = path.to_string_lossy().ends_with(".zstd");
        let reader: Box<dyn Read> = if is_zstd {
            match std::fs::File::open(&path).map_err(|e| e.to_string()).and_then(|f| {
                zstd::stream::read::Decoder::new(BufReader::new(f)).map_err(|e| e.to_string())
            }) {
                Ok(d) => Box::new(d.take(FILE_CAP)),
                Err(_) => continue,
            }
        } else {
            match std::fs::File::open(&path) {
                Ok(f) => Box::new(f.take(FILE_CAP)),
                Err(_) => continue,
            }
        };
        let mut open: HashMap<String, (String, i64)> = HashMap::new();
        for line in BufReader::new(reader).lines() {
            let Ok(line) = line else { break };
            if !line.contains("tool/") {
                continue;
            }
            let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
            let ty = v.get("type").and_then(|x| x.as_str()).unwrap_or("");
            let at = v.get("time").and_then(|x| x.as_i64()).unwrap_or(0);
            if since_ms > 0 && at > 0 && at < since_ms {
                continue;
            }
            let d = v.get("data").cloned().unwrap_or(Value::Null);
            if ty == "tool/call" {
                let name = d.get("name").and_then(|x| x.as_str()).unwrap_or("").to_string();
                let id = d.get("callId").and_then(|x| x.as_str()).unwrap_or("").to_string();
                if !name.is_empty() {
                    touch_tool(aggs, &name, "dsh", &session, at);
                    stat.1 += 1;
                    if !id.is_empty() {
                        open.insert(id, (name, at));
                    }
                }
            } else if ty == "tool/result" {
                let msg = d.get("message").cloned().unwrap_or(Value::Null);
                let id = msg
                    .get("toolCallId")
                    .and_then(|x| x.as_str())
                    .or_else(|| msg.get("source").and_then(|s| s.get("callId")).and_then(|x| x.as_str()))
                    .unwrap_or("");
                if let Some((name, start)) = open.remove(id) {
                    let is_error = msg.get("isError").and_then(|x| x.as_bool()).unwrap_or(false);
                    tool_result(aggs, &name, start, at, is_error, error_text(&msg));
                }
            }
        }
    }
}

fn scan_claude_tools(
    aggs: &mut HashMap<String, ToolAgg>,
    home: &Path,
    since_ms: i64,
    stat: &mut (u64, u64),
) {
    let root = home.join(".claude").join("projects");
    if !root.is_dir() {
        return;
    }
    let mut files: Vec<PathBuf> = Vec::new();
    walk_files(&root, 0, &mut files);
    files.retain(|f| f.extension().map(|e| e == "jsonl").unwrap_or(false));
    files.sort_by_key(|f| std::cmp::Reverse(mtime_ms(f)));
    for path in files.into_iter().take(FILE_LIMIT) {
        if since_ms > 0 && mtime_ms(&path) > 0 && mtime_ms(&path) < since_ms {
            continue;
        }
        let Ok(f) = std::fs::File::open(&path) else { continue };
        stat.0 += 1;
        let session = path
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        let mut open: HashMap<String, (String, i64)> = HashMap::new();
        for line in BufReader::new(f.take(FILE_CAP)).lines() {
            let Ok(line) = line else { break };
            if !line.contains("tool_use") && !line.contains("tool_result") {
                continue;
            }
            let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
            let at = v
                .get("timestamp")
                .and_then(|x| x.as_str())
                .map(iso_ms)
                .unwrap_or(0);
            if since_ms > 0 && at > 0 && at < since_ms {
                continue;
            }
            let Some(content) = v
                .get("message")
                .and_then(|m| m.get("content"))
                .and_then(|c| c.as_array())
            else {
                continue;
            };
            for block in content {
                match block.get("type").and_then(|x| x.as_str()).unwrap_or("") {
                    "tool_use" => {
                        let name = block.get("name").and_then(|x| x.as_str()).unwrap_or("");
                        let id = block.get("id").and_then(|x| x.as_str()).unwrap_or("");
                        if !name.is_empty() {
                            touch_tool(aggs, name, "claude", &session, at);
                            stat.1 += 1;
                            if !id.is_empty() {
                                open.insert(id.to_string(), (name.to_string(), at));
                            }
                        }
                    }
                    "tool_result" => {
                        let id = block.get("tool_use_id").and_then(|x| x.as_str()).unwrap_or("");
                        if let Some((name, start)) = open.remove(id) {
                            let is_error = block.get("is_error").and_then(|x| x.as_bool()).unwrap_or(false);
                            tool_result(aggs, &name, start, at, is_error, error_text(block));
                        }
                    }
                    _ => {}
                }
            }
        }
    }
}


pub async fn records(State(state): State<Arc<AppState>>, Query(q): Query<ScanQuery>) -> Response {
    let port = {
        let p = state.port.load(std::sync::atomic::Ordering::Relaxed);
        if p == 0 { 8010 } else { p }
    };
    let has_hours = q.hours.is_some();
    let hours = q.hours.unwrap_or(168);
    let mut range = q.range.as_deref().map(|s| s.trim().to_ascii_lowercase()).unwrap_or_default();
    if range.is_empty() && !has_hours { range = "today".to_string(); }
    let since_ms = match range.as_str() {
        "today" => {
            let now = chrono::Local::now();
            let secs = now.time().num_seconds_from_midnight() as i64;
            (now.timestamp() - secs) * 1000
        }
        "all" => 0,
        _ => if hours <= 0 { 0 } else { now_ms() - hours * 3600 * 1000 },
    };
    let day = q.day.as_deref().unwrap_or("").trim().to_string();
    let month = q.month.as_deref().unwrap_or("").trim().to_string();
    let group = q.group.as_deref().unwrap_or("").trim().to_string();
    let upstream = q.upstream.as_deref().unwrap_or("").trim().to_string();
    let model = q.model.as_deref().unwrap_or("").trim().to_string();
    let route = route_filter(q.route.as_deref());
    let metric = metric_mode(q.metric.as_deref());
    let limit = q.limit.unwrap_or(50).clamp(5, 200);
    let offset = q.offset.unwrap_or(0);
    let value = session_records(port, since_ms, route, metric, &day, &month, &group, &upstream, &model, limit, offset);
    Json(value).into_response()
}

pub async fn tools(State(_state): State<Arc<AppState>>, Query(q): Query<ToolQuery>) -> Response {
    let has_hours = q.hours.is_some();
    let hours = q.hours.unwrap_or(168);
    let mut range = q
        .range
        .as_deref()
        .map(|s| s.trim().to_ascii_lowercase())
        .unwrap_or_default();
    if range.is_empty() && !has_hours {
        range = "today".to_string();
    }
    let (since_ms, window_label) = match range.as_str() {
        "today" => {
            let now = chrono::Local::now();
            let secs = now.time().num_seconds_from_midnight() as i64;
            ((now.timestamp() - secs) * 1000, "today".to_string())
        }
        "all" => (0, "all".to_string()),
        _ => (
            if hours <= 0 { 0 } else { now_ms() - hours * 3600 * 1000 },
            format!("{hours}h"),
        ),
    };
    let wanted: Vec<String> = q
        .sources
        .as_deref()
        .map(|s| s.split(',').map(|x| x.trim().to_ascii_lowercase()).filter(|x| !x.is_empty()).collect())
        .unwrap_or_else(|| vec!["dsh".to_string(), "claude".to_string()]);
    let limit = q.limit.unwrap_or(50).clamp(5, 200);
    let cache_key = super::collector::tools_key(&q);
    if let Some(v) = super::collector::get(&cache_key, 15) {
        return Json(v).into_response();
    }
    let home = home();
    let mut aggs: HashMap<String, ToolAgg> = HashMap::new();
    let mut dsh_stat = (0u64, 0u64);
    let mut claude_stat = (0u64, 0u64);
    if wanted.iter().any(|s| s == "dsh") {
        scan_dsh_tools(&mut aggs, &home, since_ms, &mut dsh_stat);
    }
    if wanted.iter().any(|s| s == "claude") {
        scan_claude_tools(&mut aggs, &home, since_ms, &mut claude_stat);
    }

    let mut rows: Vec<Value> = aggs
        .values()
        .map(|a| {
            let avg = if a.durations.is_empty() {
                0.0
            } else {
                a.durations.iter().sum::<f64>() / a.durations.len() as f64 * 1000.0
            };
            let mut errors: Vec<(String, u64)> = a.errors.clone().into_iter().collect();
            errors.sort_by(|x, y| y.1.cmp(&x.1));
            json!({
                "name": a.name,
                "calls": a.calls,
                "failed": a.failed,
                "success_rate": if a.calls > 0 { ((a.calls - a.failed) as f64 / a.calls as f64 * 1000.0).round() / 10.0 } else { 0.0 },
                "sessions": a.sessions.len(),
                "avg_ms": (avg * 10.0).round() / 10.0,
                "p50_ms": pct_ms(&a.durations, 0.50),
                "p95_ms": pct_ms(&a.durations, 0.95),
                "first_at": a.first_at,
                "last_at": a.last_at,
                "by_source": a.by_source,
                "errors": errors.into_iter().take(3).map(|(m, c)| json!({"message": m, "count": c})).collect::<Vec<_>>(),
            })
        })
        .collect();
    rows.sort_by_key(|r| -r["calls"].as_i64().unwrap_or(0));
    let total_calls: u64 = aggs.values().map(|a| a.calls).sum();
    let total_failed: u64 = aggs.values().map(|a| a.failed).sum();
    let mut all_durations: Vec<f64> = aggs.values().flat_map(|a| a.durations.clone()).collect();
    all_durations.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let avg_all = if all_durations.is_empty() {
        0.0
    } else {
        all_durations.iter().sum::<f64>() / all_durations.len() as f64 * 1000.0
    };
    let rows_total = rows.len();
    rows.truncate(limit);

    let value = json!({
        "ok": true,
        "range": window_label,
        "since_ms": since_ms,
        "generated_at": chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
        "sources": [
            {"source": "dsh", "files": dsh_stat.0, "calls": dsh_stat.1},
            {"source": "claude", "files": claude_stat.0, "calls": claude_stat.1},
        ],
        "totals": {
            "calls": total_calls,
            "failed": total_failed,
            "success_rate": if total_calls > 0 { ((total_calls - total_failed) as f64 / total_calls as f64 * 1000.0).round() / 10.0 } else { 0.0 },
            "tools": rows_total,
            "avg_ms": (avg_all * 10.0).round() / 10.0,
            "p50_ms": pct_ms(&all_durations, 0.50),
            "p95_ms": pct_ms(&all_durations, 0.95),
        },
        "rows": rows,
        "rows_total": rows_total,
    });
    super::collector::put(cache_key, value.clone());
    Json(value).into_response()
}

/// provider → baseURL 的映射快照（调试 / 排查用）。
pub async fn bases(State(_state): State<Arc<AppState>>) -> Response {
    let map = collect_bases(&home());
    ok(json!({"ok": true, "provider_bases": map}))
}
