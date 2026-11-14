//! 会话对账：直接读各家 Agent 自己写的会话账本（DeepSeek Harness / Claude Code / Pi / Codex），
//! 把「经过本机 llm-router」和「压根没经过本机路由」的调用分开统计。
//!
//! 为什么需要它：路由流水（config.history.jsonl）只记录经过 8010 的请求；
//! Agent 们各自还写着自己的账本（模型 / token / 时间），把它们读出来就能回答
//! 「哪些调用没走我的路由、被谁接走了」。
//!
//! 全部只读：不改任何 Agent 的配置，也不写任何文件。

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use chrono::Timelike;
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

#[derive(Deserialize, Default)]
pub struct ScanQuery {
    /// 时间窗（小时），0 或缺省后用默认 168 小时；负数表示不限时间
    pub hours: Option<i64>,
    /// today = 本机时区当天 00:00 起；all = 不限时间；其余按 hours
    pub range: Option<String>,
    /// 逗号分隔：dsh,claude,pi,codex
    pub sources: Option<String>,
    /// 明细行上限
    pub limit: Option<usize>,
}

/// 按 (来源, provider, model) 聚合的一行账。
#[derive(Clone)]
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
    ) {
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
    models: HashMap<String, u64>,
    external_top: String,
}

type AggMap = HashMap<String, Agg>;
type SessList = Vec<Sess>;

fn bump(aggs: &mut AggMap, key: &str, make: impl FnOnce() -> Agg, at_ms: i64, u: [u64; 5]) {
    let e = aggs
        .entry(key.to_string())
        .or_insert_with(|| make());
    e.add(at_ms, u[0], u[1], u[2], u[3], u[4]);
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
    aggs: &'a mut AggMap,
    sessions: &'a mut SessList,
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
    ) {
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
        let key = format!("{source}\u{1}{provider}\u{1}{model}");
        let label = format!("{provider}/{model}");
        bump(
            self.aggs,
            &key,
            || Agg::new(source, agent, provider, model),
            at_ms,
            usage,
        );
        if let Some(e) = self.aggs.get_mut(&key) {
            e.routed = routed;
            if e.base_url.is_empty() {
                e.base_url = base.clone();
            }
        }
        sess.rounds += 1;
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
    }
}

/// DeepSeek Harness：~/.dsh/sessions/<project>/<session>/session.v4.jsonl.zstd
/// 记录形如 {"type":"assistant/message","time":<ms>,"data":{"message":{"source":{provider,model}},"usage":{inputTokens,...}}}
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
            models: HashMap::new(),
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
            ctx.round("dsh", "DeepSeek Harness", &mut sess, provider, model, at, usage);
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
            models: HashMap::new(),
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
                ctx.round("claude", "Claude Code", &mut sess, provider, model, at, usage);
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
            models: HashMap::new(),
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
            ctx.round("pi", "Pi", &mut sess, &provider, model, at, usage);
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
            models: HashMap::new(),
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
                    ctx.round("codex", "Codex", &mut sess, provider, &cur_model, at, usage);
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

fn journal_window(state: &AppState, since_ms: i64) -> Value {
    let path = state.history_path();
    let mut rounds = 0u64;
    let mut input = 0u64;
    let mut output = 0u64;
    let mut cache_read = 0u64;
    let mut cache_write = 0u64;
    let mut by_model: HashMap<String, (u64, u64, u64)> = HashMap::new();
    let mut seen_from = i64::MAX;
    let _ = tail_lines(&path, 32 * 1024 * 1024, &mut |line| {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            return;
        };
        let at = v
            .get("at")
            .and_then(|x| x.as_f64())
            .map(|f| (f * 1000.0) as i64)
            .unwrap_or(0);
        if since_ms > 0 && at > 0 && at < since_ms {
            return;
        }
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
        if at > 0 && at < seen_from {
            seen_from = at;
        }
        let model = v.get("model").and_then(|x| x.as_str()).unwrap_or("(未知)");
        let e = by_model.entry(model.to_string()).or_insert((0, 0, 0));
        e.0 += 1;
        e.1 += i;
        e.2 += o;
    });
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
    if wanted.is_empty() {
        return bad("sources 参数为空，请至少选一个数据源（dsh,claude,pi,codex）");
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
                aggs: &mut aggs,
                sessions: &mut sessions,
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
                aggs: &mut aggs,
                sessions: &mut sessions,
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
                aggs: &mut aggs,
                sessions: &mut sessions,
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
                aggs: &mut aggs,
                sessions: &mut sessions,
            };
            scan_codex(&mut ctx, &home, &mut st);
        }
        stats.push(st);
    }

    let journal = journal_window(&state, since_ms);

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

    ok(json!({
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
    }))
}


// ───────────────────── MCP / 工具调用排行榜 ─────────────────────

#[derive(Deserialize, Default)]
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

    ok(json!({
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
    }))
}

/// provider → baseURL 的映射快照（调试 / 排查用）。
pub async fn bases(State(_state): State<Arc<AppState>>) -> Response {
    let map = collect_bases(&home());
    ok(json!({"ok": true, "provider_bases": map}))
}
