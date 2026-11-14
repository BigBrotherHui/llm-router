//! 本机 Agent 识别与一键接入。
//!
//! 扫描 PATH / 常见安装目录 / 配置目录 / VS Code 扩展，列出已安装的编码 Agent；
//! 把当前路由里的模型写成各 Agent 的 OpenAI 兼容（或 Anthropic 兼容）配置。
//! 所有写入都先备份成 `<name>.llmrouter.bak`，可一键恢复。

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Serialize;
use serde_json::{json, Value};

use super::state::AppState;

#[derive(Debug, Clone, Serialize)]
pub struct AgentInfo {
    pub id: String,
    pub name: String,
    pub kind: String,
    pub detected: bool,
    pub detected_by: Vec<String>,
    /// 主配置路径（展示 / 写入的默认目标）
    pub config_path: String,
    /// 一键导入会触碰的全部文件（Pi 这类 Agent 需要写两个文件）
    pub config_paths: Vec<String>,
    pub configurable: bool,
    pub current_model: Option<String>,
    pub current_base: Option<String>,
    pub connected: bool,
    /// 曾经被 llm-router 写入过，但当前配置已被别的工具改走
    pub drifted: bool,
    /// 上次一键导入时写入的模型（漂移后「一键修回」用这个值）
    pub expected_model: Option<String>,
    pub expected_base: Option<String>,
    pub applied_at: Option<String>,
    /// DSH 这类 Agent 的「默认模型」是单独一处配置，单独回报
    pub default_provider: Option<String>,
    pub default_model: Option<String>,
    pub notes: String,
    /// 识别来源：内置规则 / npm 全局包 / 安装目录……
    pub source: String,
    /// npm 包名、版本、可执行命令（动态发现时带上，方便排查）
    pub pkg: Option<String>,
    pub version: Option<String>,
    pub bin: Option<String>,
}

impl AgentInfo {
    fn from_pkg(mut self, pkg: &NpmPkg) -> Self {
        self.source = if self.source == "内置规则" {
            "内置规则 + npm 全局包".to_string()
        } else if !self.source.is_empty() {
            format!("{} + npm 全局包", self.source)
        } else {
            "npm 全局包".to_string()
        };
        self.pkg = Some(pkg.name.clone());
        self.version = Some(pkg.version.clone());
        if let Some(b) = pkg.bins.first() {
            self.bin = Some(b.clone());
        }
        self
    }

    fn add_detected(&mut self, mut by: Vec<String>) {
        for item in by.drain(..) {
            if !self.detected_by.contains(&item) {
                self.detected_by.push(item);
            }
        }
        self.detected = !self.detected_by.is_empty();
    }
}

fn ok(v: Value) -> Response {
    Json(v).into_response()
}

fn bad(msg: &str) -> Response {
    (StatusCode::BAD_REQUEST, Json(json!({"ok": false, "error": msg}))).into_response()
}

fn bad_with(mut v: Value) -> Response {
    if let Some(o) = v.as_object_mut() {
        o.insert("ok".into(), json!(false));
    }
    (StatusCode::BAD_REQUEST, Json(v)).into_response()
}

fn home() -> PathBuf {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn appdata() -> PathBuf {
    std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join("AppData").join("Roaming"))
}

fn localappdata() -> PathBuf {
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join("AppData").join("Local"))
}

fn path_dirs() -> Vec<PathBuf> {
    std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default()
}

fn extra_bin_dirs() -> Vec<PathBuf> {
    let h = home();
    vec![
        appdata().join("npm"),
        appdata().join("Python").join("Scripts"),
        h.join(".cargo").join("bin"),
        h.join(".local").join("bin"),
        h.join("scoop").join("shims"),
        h.join("pipx").join("bin"),
        localappdata().join("pnpm"),
        localappdata().join("Programs"),
    ]
}

/// 在 PATH 与常见安装目录里找可执行文件；Windows 同时认 .exe/.cmd/.bat。
fn find_exe(names: &[&str]) -> Option<PathBuf> {
    let mut dirs = path_dirs();
    dirs.extend(extra_bin_dirs());
    for dir in dirs {
        for name in names {
            for suffix in ["", ".exe", ".cmd", ".bat"] {
                let p = dir.join(format!("{name}{suffix}"));
                if p.is_file() {
                    return Some(p);
                }
            }
        }
    }
    None
}

fn first_existing(paths: &[PathBuf]) -> Option<PathBuf> {
    paths.iter().find(|p| p.exists()).cloned()
}

fn vscode_settings_paths() -> Vec<PathBuf> {
    let mut out = Vec::new();
    for base in [appdata(), localappdata()] {
        for folder in ["Code", "Code - Insiders", "VSCodium", "Cursor", "Windsurf"] {
            out.push(base.join(folder).join("User").join("settings.json"));
        }
    }
    out
}

fn default_vscode_settings() -> PathBuf {
    first_existing(&vscode_settings_paths())
        .unwrap_or_else(|| appdata().join("Code").join("User").join("settings.json"))
}

fn vscode_ext(prefix: &str) -> Option<PathBuf> {
    let dir = home().join(".vscode").join("extensions");
    let entries = std::fs::read_dir(dir).ok()?;
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        if name.starts_with(prefix) {
            return Some(e.path());
        }
    }
    None
}

fn settings_has(key_prefix: &str) -> Option<PathBuf> {
    for p in vscode_settings_paths() {
        if let Ok(text) = std::fs::read_to_string(&p) {
            if text.contains(key_prefix) {
                return Some(p);
            }
        }
    }
    None
}

fn port_of(state: &AppState) -> u16 {
    let p = state.port.load(Ordering::Relaxed);
    if p == 0 { 8010 } else { p }
}

fn default_base_url(state: &AppState) -> String {
    format!("http://127.0.0.1:{}/v1", port_of(state))
}

fn openai_base(input: &str) -> String {
    let s = input.trim().trim_end_matches('/').to_string();
    if s.ends_with("/v1") {
        s
    } else {
        format!("{s}/v1")
    }
}

fn anthropic_base(input: &str) -> String {
    let s = input.trim().trim_end_matches('/').to_string();
    s.strip_suffix("/v1").unwrap_or(&s).to_string()
}

/// 与 /v1/models 同源：已启用上游里勾选过的模型分组。
fn model_ids(state: &AppState) -> Vec<String> {
    let file = state.file.read().unwrap();
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for up in &file.upstreams {
        if !up.enabled {
            continue;
        }
        for m in up.selected() {
            let group = up.group_of(&m);
            if seen.insert(group.clone()) {
                out.push(group);
            }
        }
    }
    out.sort();
    out
}

/// 从各种格式的配置文件里粗略提取当前 model / base_url，仅用于界面展示。
fn grab(text: &str, key: &str) -> Option<String> {
    for line in text.lines() {
        let t = line.trim().trim_start_matches('-').trim();
        for sep in ["=", ":"] {
            if let Some(pos) = t.find(sep) {
                let k = t[..pos].trim().trim_matches('"').trim();
                if k == key {
                    let v = t[pos + sep.len()..]
                        .trim()
                        .trim_end_matches(',')
                        .trim_matches('"')
                        .trim();
                    if !v.is_empty() {
                        return Some(v.to_string());
                    }
                }
            }
        }
    }
    None
}

fn read_current(path: &Path, port: u16) -> (Option<String>, Option<String>, bool) {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let model = grab(&text, "model")
        .or_else(|| grab(&text, "OPENAI_MODEL"))
        .or_else(|| grab(&text, "ANTHROPIC_MODEL"))
        .or_else(|| grab(&text, "openAiModelId"))
        .or_else(|| grab(&text, "modelId"))
        .or_else(|| grab(&text, "openai-api-model"));
    let base = grab(&text, "base_url")
        .or_else(|| grab(&text, "OPENAI_BASE_URL"))
        .or_else(|| grab(&text, "ANTHROPIC_BASE_URL"))
        .or_else(|| grab(&text, "openAiBaseUrl"))
        .or_else(|| grab(&text, "apiBase"))
        .or_else(|| grab(&text, "baseURL"))
        .or_else(|| grab(&text, "openai-api-base"))
        .or_else(|| grab(&text, "openAiApiBase"));
    let marker = format!("127.0.0.1:{port}");
    let connected = text.contains(&marker) || text.contains("llm-router");
    (model, base, connected)
}

fn make_agent(
    id: &str,
    name: &str,
    kind: &str,
    detected_by: Vec<String>,
    config_path: PathBuf,
    configurable: bool,
    notes: &str,
    port: u16,
) -> AgentInfo {
    let (current_model, current_base, connected) = read_current(&config_path, port);
    let path_str = config_path.to_string_lossy().to_string();
    AgentInfo {
        id: id.to_string(),
        name: name.to_string(),
        kind: kind.to_string(),
        detected: !detected_by.is_empty(),
        detected_by,
        config_path: path_str.clone(),
        config_paths: vec![path_str],
        configurable,
        current_model,
        current_base,
        connected,
        drifted: false,
        expected_model: None,
        expected_base: None,
        applied_at: None,
        default_provider: None,
        default_model: None,
        notes: notes.to_string(),
        source: "内置规则".to_string(),
        pkg: None,
        version: None,
        bin: None,
    }
}

/// 一个 npm 全局包的关键信息。
#[derive(Debug, Clone)]
struct NpmPkg {
    name: String,
    version: String,
    dir: PathBuf,
    bins: Vec<String>,
    keywords: Vec<String>,
    description: String,
}

fn npm_global_roots() -> Vec<PathBuf> {
    let h = home();
    let mut roots = vec![
        appdata().join("npm").join("node_modules"),
        localappdata().join("pnpm").join("global").join("node_modules"),
        h.join(".npm-global").join("lib").join("node_modules"),
        h.join(".local").join("lib").join("node_modules"),
        h.join(".bun").join("install").join("global").join("node_modules"),
        h.join(".yarn").join("global").join("node_modules"),
        PathBuf::from("/usr/local/lib/node_modules"),
        PathBuf::from("/usr/lib/node_modules"),
    ];
    if let Some(prefix) = std::env::var_os("npm_config_prefix") {
        roots.push(PathBuf::from(prefix).join("lib").join("node_modules"));
    }
    // PATH 里的 .../bin 通常和 .../lib/node_modules 同属一个 prefix。
    let mut extra = Vec::new();
    for dir in path_dirs() {
        if dir.file_name().map(|s| s == "bin").unwrap_or(false) {
            if let Some(parent) = dir.parent() {
                extra.push(parent.join("lib").join("node_modules"));
            }
        }
    }
    roots.extend(extra);
    roots.retain(|p| p.is_dir());
    roots.sort();
    roots.dedup();
    roots
}

/// 读取 npm 全局包目录里每个 package.json 的 name / version / bin / keywords。
fn npm_packages() -> Vec<NpmPkg> {
    let mut out: Vec<NpmPkg> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for root in npm_global_roots() {
        let Ok(entries) = std::fs::read_dir(&root) else { continue };
        let mut dirs = Vec::new();
        for e in entries.flatten() {
            let p = e.path();
            if !p.is_dir() {
                continue;
            }
            let n = e.file_name().to_string_lossy().to_string();
            if n.starts_with('@') {
                if let Ok(sub) = std::fs::read_dir(&p) {
                    for s in sub.flatten() {
                        if s.path().is_dir() {
                            dirs.push(s.path());
                        }
                    }
                }
            } else {
                dirs.push(p);
            }
        }
        for dir in dirs {
            let Ok(text) = std::fs::read_to_string(dir.join("package.json")) else { continue };
            let Ok(v) = serde_json::from_str::<Value>(&text) else { continue };
            let name = v.get("name").and_then(|x| x.as_str()).unwrap_or("").to_string();
            if name.is_empty() || !seen.insert(name.clone()) {
                continue;
            }
            let version = v
                .get("version")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string();
            let mut bins = Vec::new();
            match v.get("bin") {
                Some(Value::String(s)) => bins.push(s.clone()),
                Some(Value::Object(m)) => {
                    for k in m.keys() {
                        bins.push(k.clone());
                    }
                }
                _ => {}
            }
            let keywords = v
                .get("keywords")
                .and_then(|x| x.as_array())
                .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
                .unwrap_or_default();
            let description = v
                .get("description")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string();
            out.push(NpmPkg { name, version, dir, bins, keywords, description });
        }
    }
    out
}

/// 已知 npm 包 → (稳定 id, 展示名, 配置路径提示)。id 与内置规则一致时做合并而不是新增卡片。
fn known_npm(name: &str) -> Option<(&'static str, &'static str, &'static str)> {
    Some(match name {
        "@anthropic-ai/claude-code" => ("claude-code", "Claude Code", ""),
        "@openai/codex" => ("codex", "Codex CLI", ""),
        "@qwen-code/qwen-code" => ("qwen-code", "Qwen Code", ""),
        "@google/gemini-cli" => ("gemini-cli", "Gemini CLI", ""),
        "opencode-ai" => ("opencode", "OpenCode", ""),
        "@earendil-works/pi-coding-agent" => ("pi", "Pi (Coding Agent)", ".pi/agent/models.json"),
        "@sourcegraph/amp" => ("amp", "Amp (Sourcegraph)", ".config/amp/settings.json"),
        "@augmentcode/auggie" => ("auggie", "Auggie (Augment Code)", ".augment/settings.json"),
        "@charmland/crush" => ("crush", "Crush (Charm)", ".config/crush/crushrc"),
        "@factory/cli-win32-x64" | "@factory/cli" => ("droid", "Droid (Factory)", ".factory/settings.json"),
        "@github/copilot" => ("copilot-cli", "GitHub Copilot CLI", ".copilot/config.json"),
        "@kilocode/cli" => ("kilo-code", "Kilo Code", ".config/kilo/config.json"),
        "@continuedev/cli" => ("continue-cli", "Continue CLI", ".continue/config.json"),
        "@tencent-ai/codebuddy-code" => ("codebuddy", "CodeBuddy Code", ".codebuddy/models.json"),
        "cline" => ("cline-cli", "Cline CLI", ".cline/config.json"),
        "codebuff" => ("codebuff", "Codebuff", ".codebuff/config.json"),
        "command-code" => ("command-code", "Command Code", ".commandcode/settings.json"),
        "dsh-tui" => ("dsh-tui", "DeepSeek Harness", ".dsh/settings.yaml"),
        "freebuff" => ("freebuff", "Freebuff", ".freebuff/config.json"),
        "openclaw" => ("openclaw", "OpenClaw", ".openclaw/config.json"),
        _ => return None,
    })
}

fn is_likely_agent(pkg: &NpmPkg) -> bool {
    if pkg.name.starts_with("@modelcontextprotocol/") {
        return false;
    }
    const SKIP: &[&str] = &[
        "npm", "corepack", "typescript", "ts-node", "eslint", "prettier", "wrangler",
        "vercel", "netlify", "pnpm", "yarn", "bun", "gulp-cli", "grunt-cli", "http-server",
        "serve", "nodemon", "pm2", "node-gyp", "windows-build-tools", "@angular/cli",
        "@vue/cli", "@nestjs/cli", "create-react-app", "sass", "less",
    ];
    if SKIP.contains(&pkg.name.as_str()) {
        return false;
    }
    let hay = format!(
        "{} {} {}",
        pkg.name,
        pkg.keywords.join(" "),
        pkg.description
    )
    .to_lowercase();
    const HINTS: &[&str] = &[
        "coding agent", "coding-agent", "ai agent", "ai-agent", "cli agent",
        "coding assistant", "code assistant", "autonomous", "copilot", "llm",
        "chat", "tui", "terminal", "coding", "code cli", "agent",
    ];
    HINTS.iter().any(|k| hay.contains(k)) && !pkg.bins.is_empty()
}

fn pi_agent_dir() -> PathBuf {
    std::env::var_os("PI_CODING_AGENT_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".pi").join("agent"))
}

/// Pi 的当前默认模型与 provider 地址（两个文件合起来读）。
fn read_pi_current(dir: &Path, port: u16) -> (Option<String>, Option<String>, bool) {
    let load = |name: &str| -> Value {
        std::fs::read_to_string(dir.join(name))
            .ok()
            .map(|s| strip_jsonc(s.trim_start_matches('\u{feff}')))
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_else(|| json!({}))
    };
    let settings = load("settings.json");
    let models = load("models.json");
    let cur_model = settings
        .get("defaultModel")
        .and_then(|x| x.as_str())
        .map(String::from);
    let mut cur_base = None;
    let mut connected = false;
    let marker = format!("127.0.0.1:{port}");
    if let Some(providers) = models.get("providers").and_then(|x| x.as_object()) {
        for (pid, p) in providers {
            let b = p.get("baseUrl").and_then(|x| x.as_str()).unwrap_or("");
            if pid.contains("llm-router") || pid.contains("llmrouter") || b.contains(&marker) {
                cur_base = Some(b.to_string());
                connected = true;
                break;
            }
        }
        if cur_base.is_none() {
            if let Some(p) = providers.get("llm-router").or_else(|| providers.get("llmrouter")) {
                cur_base = p.get("baseUrl").and_then(|x| x.as_str()).map(String::from);
            }
        }
    }
    (cur_model, cur_base, connected)
}

fn shortcut_names() -> Vec<String> {
    let mut dirs = vec![
        appdata().join("Microsoft").join("Windows").join("Start Menu").join("Programs"),
        PathBuf::from("C:\\ProgramData\\Microsoft\\Windows\\Start Menu\\Programs"),
    ];
    if let Some(pd) = std::env::var_os("ProgramData") {
        dirs.push(PathBuf::from(pd).join("Microsoft").join("Windows").join("Start Menu").join("Programs"));
    }
    let mut out = Vec::new();
    for dir in dirs {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for e in entries.flatten() {
            let n = e.file_name().to_string_lossy().to_string();
            if let Some(stem) = n.strip_suffix(".lnk") {
                let s = stem.to_lowercase();
                if ["cursor", "windsurf", "trae", "magpie", "goose", "codebuddy", "workbuddy", "kimi"]
                    .iter()
                    .any(|k| s.contains(k))
                {
                    out.push(format!("开始菜单 {}", e.path().display()));
                }
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

/// 内置规则跑完后，用 npm 全局包 + 常见配置目录补齐识别结果。
fn enrich_agents(out: &mut Vec<AgentInfo>, port: u16) {
    let h = home();
    let pkgs = npm_packages();
    for pkg in &pkgs {
        let known = known_npm(&pkg.name);
        if known.is_none() && !is_likely_agent(pkg) {
            continue;
        }
        let (raw_id, display, hint) = known.unwrap_or(("", "", ""));
        let slug = pkg
            .name
            .trim_start_matches('@')
            .replace('/', "-")
            .replace('@', "-")
            .to_lowercase();
        let id = if raw_id.is_empty() {
            format!("npm-{slug}")
        } else {
            raw_id.to_string()
        };

        let mut by = vec![format!("npm 全局包 {}@{}", pkg.name, pkg.version)];
        if let Some(b) = pkg.bins.first() {
            by.push(format!("命令 {b}"));
        }
        let hint_path = if hint.is_empty() {
            pkg.dir.clone()
        } else {
            h.join(hint)
        };
        if hint_path.exists() {
            by.push(format!("配置 {}", hint_path.display()));
        }
        if let Some(existing) = out.iter_mut().find(|a| a.id == id) {
            existing.add_detected(by);
            let e = existing.clone().from_pkg(pkg);
            *existing = e;
            continue;
        }
        // `@factory/cli-win32-x64` 之类会附带一个同名平台包（如 `droid`），
        // 自动发现时按包名去重，避免同一 CLI 出现两张卡片。
        if known.is_none() {
            if let Some(idx) = out.iter().position(|a| a.id == slug || a.id == format!("npm-{slug}")) {
                out[idx].add_detected(by);
                let e = out[idx].clone().from_pkg(pkg);
                out[idx] = e;
                continue;
            }
        }

        let is_pi = id == "pi";
        let cfg = if is_pi { pi_agent_dir().join("models.json") } else { hint_path };
        let mut agent = make_agent(
            &id,
            if display.is_empty() { &pkg.name } else { display },
            if known.is_some() { "CLI · npm 全局包" } else { "CLI · 自动发现" },
            by,
            cfg,
            is_pi,
            if is_pi {
                "写入 models.json 的 providers.llm-router（openai-completions）并同步 settings.json 的 defaultProvider/defaultModel；两个文件都会备份。"
            } else {
                "已识别到 npm 全局 CLI；暂未适配自动写入，请点「复制通用配置」手动填自定义 OpenAI Base URL。"
            },
            port,
        )
        .from_pkg(pkg);
        agent.source = if known.is_some() {
            "npm 全局包（已知）".to_string()
        } else {
            "npm 全局包（自动发现）".to_string()
        };
        if is_pi {
            let dir = pi_agent_dir();
            agent.config_paths = vec![
                dir.join("models.json").to_string_lossy().to_string(),
                dir.join("settings.json").to_string_lossy().to_string(),
            ];
            let (m, b, c) = read_pi_current(&dir, port);
            agent.current_model = m;
            agent.current_base = b;
            agent.connected = c;
            if dir.exists() {
                agent.detected = true;
                agent
                    .detected_by
                    .push(format!("配置目录 {}", dir.display()));
            }
        }
        out.push(agent);
    }

    // 非 npm 安装：WingGet / 开始菜单 / 各家配置目录。
    let shortcuts = shortcut_names();
    for s in &shortcuts {
        let low = s.to_lowercase();
        let target = if low.contains("cursor") {
            Some("cursor")
        } else if low.contains("windsurf") {
            Some("windsurf")
        } else {
            None
        };
        if let Some(id) = target {
            if let Some(a) = out.iter_mut().find(|a| a.id == id) {
                a.add_detected(vec![s.clone()]);
            }
        }
    }
    if shortcuts.iter().any(|s| s.to_lowercase().contains("trae"))
        && !out.iter().any(|a| a.id == "trae")
    {
        let cfg = h.join(".trae").join("settings.json");
        out.push(make_agent(
            "trae",
            "Trae",
            "桌面 IDE",
            shortcuts.iter().filter(|s| s.to_lowercase().contains("trae")).cloned().collect(),
            cfg,
            false,
            "已识别；请用「复制通用配置」在自定义 OpenAI Base URL 里手动填写。",
            port,
        ));
    }
    let extra: &[(&str, &str, &[&str], &str)] = &[
        ("goose", "Goose (Block)", &["goose"], ".config/goose/config.yaml"),
        ("zcode", "ZCode", &["zcode"], ".zcode/v2/config.json"),
        ("kimi-code", "Kimi Code", &["kimi"], ".kimi-code/config.toml"),
        ("marscode", "MarsCode", &["marscode"], ".marscode/config.json"),
        ("codeverse", "CodeVerse", &["codeverse"], ".codeverse/settings.json"),
        ("sheetagent", "SheetAgent", &["sheetagent"], ".sheetagent/config.json"),
        ("workbuddy", "WorkBuddy", &["workbuddy"], ".workbuddy/models.json"),
    ];
    for (id, name, exes, hint) in extra {
        if out.iter().any(|a| a.id == *id) {
            continue;
        }
        let cfg = h.join(hint);
        let mut by = Vec::new();
        if let Some(p) = find_exe(exes) {
            by.push(format!("可执行文件 {}", p.display()));
        }
        if cfg.exists() {
            by.push(format!("配置 {}", cfg.display()));
        }
        if let Some(p) = cfg.parent() {
            if p.exists() {
                by.push(format!("配置目录 {}", p.display()));
            }
        }
        if by.is_empty() {
            continue;
        }
        out.push(make_agent(
            id,
            name,
            "CLI",
            by,
            cfg,
            false,
            "已识别；请用「复制通用配置」手动填自定义 OpenAI Base URL。",
            port,
        ));
    }

    // 可配置且已识别的排最前，其次已识别，最后才是未检测到的。
    out.sort_by(|a, b| {
        let rank = |x: &AgentInfo| -> u8 {
            match (x.detected, x.configurable) {
                (true, true) => 3,
                (true, false) => 2,
                (false, true) => 1,
                (false, false) => 0,
            }
        };
        rank(b).cmp(&rank(a)).then_with(|| a.name.cmp(&b.name))
    });
}

pub fn scan_agents(state: &AppState) -> Vec<AgentInfo> {
    let port = port_of(state);
    let h = home();
    let mut out = Vec::new();

    // ---------- Claude Code ----------
    {
        let mut by = Vec::new();
        if let Some(p) = find_exe(&["claude"]) {
            by.push(format!("可执行文件 {}", p.display()));
        }
        for p in [h.join(".claude").join("settings.json"), h.join(".claude.json")] {
            if p.exists() {
                by.push(format!("配置 {}", p.display()));
            }
        }
        out.push(make_agent(
            "claude-code",
            "Claude Code",
            "CLI",
            by,
            h.join(".claude").join("settings.json"),
            true,
            "写入 ANTHROPIC_BASE_URL / ANTHROPIC_AUTH_TOKEN / ANTHROPIC_MODEL；路由已提供 /v1/messages 适配。",
            port,
        ));
    }

    // ---------- Codex CLI ----------
    {
        let mut by = Vec::new();
        if let Some(p) = find_exe(&["codex"]) {
            by.push(format!("可执行文件 {}", p.display()));
        }
        let cfg = h.join(".codex").join("config.toml");
        if cfg.exists() {
            by.push(format!("配置 {}", cfg.display()));
        }
        out.push(make_agent(
            "codex",
            "Codex CLI",
            "CLI",
            by,
            cfg,
            true,
            "在 config.toml 增加 llm-router provider（wire_api = chat）；路由不校验客户端 Key，无需 env_key。",
            port,
        ));
    }

    // ---------- Aider ----------
    {
        let mut by = Vec::new();
        if let Some(p) = find_exe(&["aider"]) {
            by.push(format!("可执行文件 {}", p.display()));
        }
        let cfg = h.join(".aider.conf.yml");
        if cfg.exists() {
            by.push(format!("配置 {}", cfg.display()));
        }
        out.push(make_agent(
            "aider",
            "Aider",
            "CLI",
            by,
            cfg,
            true,
            "写入 openai-api-base / openai-api-key / model 三行，保留其余配置。",
            port,
        ));
    }

    // ---------- Qwen Code ----------
    {
        let mut by = Vec::new();
        if let Some(p) = find_exe(&["qwen"]) {
            by.push(format!("可执行文件 {}", p.display()));
        }
        let cfg = h.join(".qwen").join("settings.json");
        if cfg.exists() {
            by.push(format!("配置 {}", cfg.display()));
        }
        out.push(make_agent(
            "qwen-code",
            "Qwen Code",
            "CLI",
            by,
            cfg,
            true,
            "写入 OPENAI_BASE_URL / OPENAI_API_KEY / OPENAI_MODEL 到 settings.json 的 env。",
            port,
        ));
    }

    // ---------- OpenCode ----------
    {
        let mut by = Vec::new();
        if let Some(p) = find_exe(&["opencode"]) {
            by.push(format!("可执行文件 {}", p.display()));
        }
        let candidates = vec![
            h.join(".config").join("opencode").join("opencode.json"),
            appdata().join("opencode").join("opencode.json"),
            h.join(".opencode").join("opencode.json"),
        ];
        let cfg = first_existing(&candidates).unwrap_or_else(|| candidates[0].clone());
        if let Some(p) = first_existing(&candidates) {
            by.push(format!("配置 {}", p.display()));
        }
        out.push(make_agent(
            "opencode",
            "OpenCode",
            "CLI",
            by,
            cfg,
            true,
            "写入 provider.llm-router（@ai-sdk/openai-compatible）并把 model 指向选中的路由模型。",
            port,
        ));
    }

    // ---------- Continue ----------
    {
        let mut by = Vec::new();
        let dir = h.join(".continue");
        if dir.exists() {
            by.push(format!("配置目录 {}", dir.display()));
        }
        for f in ["config.json", "config.yaml", "config.yml"] {
            let p = dir.join(f);
            if p.exists() {
                by.push(format!("配置 {}", p.display()));
            }
        }
        out.push(make_agent(
            "continue",
            "Continue",
            "IDE 扩展",
            by,
            dir.join("config.json"),
            true,
            "在 config.json 的 models 数组里增加/更新 llm-router 条目（优先于同目录 config.yaml）。",
            port,
        ));
    }

    // ---------- Cline ----------
    {
        let mut by = Vec::new();
        if let Some(p) = vscode_ext("saoudrizwan.claude-dev-") {
            by.push(format!("VS Code 扩展 {}", p.display()));
        } else if let Some(p) = settings_has("cline.") {
            by.push(format!("设置 {}", p.display()));
        }
        let cfg = by
            .iter()
            .find_map(|s| {
                let p = PathBuf::from(s.trim_start_matches("设置 "));
                if p.is_file() { Some(p) } else { None }
            })
            .unwrap_or_else(default_vscode_settings);
        if cfg.exists() {
            by.push(format!("设置 {}", cfg.display()));
        }
        out.push(make_agent(
            "cline",
            "Cline",
            "IDE 扩展",
            by,
            cfg,
            true,
            "写入 VS Code settings.json 的 cline.* OpenAI 兼容键。",
            port,
        ));
    }

    // ---------- Roo Code ----------
    {
        let mut by = Vec::new();
        if let Some(p) = vscode_ext("rooveterinaryinc.roo-cline-") {
            by.push(format!("VS Code 扩展 {}", p.display()));
        } else if let Some(p) = settings_has("roo-cline.") {
            by.push(format!("设置 {}", p.display()));
        }
        let cfg = by
            .iter()
            .find_map(|s| {
                let p = PathBuf::from(s.trim_start_matches("设置 "));
                if p.is_file() { Some(p) } else { None }
            })
            .unwrap_or_else(default_vscode_settings);
        if cfg.exists() {
            by.push(format!("设置 {}", cfg.display()));
        }
        out.push(make_agent(
            "roo-code",
            "Roo Code",
            "IDE 扩展",
            by,
            cfg,
            true,
            "写入 VS Code settings.json 的 roo-cline.* OpenAI 兼容键。",
            port,
        ));
    }

    // ---------- 识别但不自动写入 ----------
    {
        let mut by = Vec::new();
        if let Some(p) = find_exe(&["gemini"]) {
            by.push(format!("可执行文件 {}", p.display()));
        }
        let cfg = h.join(".gemini").join("settings.json");
        if cfg.exists() {
            by.push(format!("配置 {}", cfg.display()));
        }
        out.push(make_agent(
            "gemini-cli",
            "Gemini CLI",
            "CLI",
            by,
            cfg,
            false,
            "已识别；原生 Gemini 协议未适配，请用下方「通用配置」手动填自定义 OpenAI Base URL。",
            port,
        ));
    }
    {
        let mut by = Vec::new();
        let p = localappdata().join("Programs").join("Cursor").join("Cursor.exe");
        if p.exists() {
            by.push(format!("安装 {}", p.display()));
        }
        let s = appdata().join("Cursor").join("User").join("settings.json");
        if s.exists() {
            by.push(format!("设置 {}", s.display()));
        }
        out.push(make_agent(
            "cursor",
            "Cursor",
            "桌面 IDE",
            by,
            s,
            false,
            "已识别；请用下方「通用配置」在 Cursor 的自定义 OpenAI Base URL 里手动填写。",
            port,
        ));
    }
    {
        let mut by = Vec::new();
        let p = localappdata().join("Programs").join("Windsurf").join("Windsurf.exe");
        if p.exists() {
            by.push(format!("安装 {}", p.display()));
        }
        let s = appdata().join("Windsurf").join("User").join("settings.json");
        if s.exists() {
            by.push(format!("设置 {}", s.display()));
        }
        out.push(make_agent(
            "windsurf",
            "Windsurf",
            "桌面 IDE",
            by,
            s,
            false,
            "已识别；请用下方「通用配置」手动填写自定义 OpenAI Base URL。",
            port,
        ));
    }

    enrich_agents(&mut out, port);
    apply_templates(&mut out, port);
    apply_drift(state, &mut out);
    out
}

/// 一键导入过的 Agent 会在这里留一份「期望状态」，扫描时用来判断配置漂移。
fn applied_path(state: &AppState) -> PathBuf {
    state
        .state_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("agent-applied.json")
}

fn load_applied(state: &AppState) -> Value {
    std::fs::read_to_string(applied_path(state))
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .unwrap_or_else(|| json!({}))
}

fn save_applied(state: &AppState, v: &Value) -> Result<(), String> {
    let path = applied_path(state);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("创建状态目录失败: {e}"))?;
    }
    let text = serde_json::to_string_pretty(v).map_err(|e| format!("序列化失败: {e}"))?;
    std::fs::write(&path, text).map_err(|e| format!("写入应用记录失败: {e}"))
}

/// 把「上次导入」与「当前配置」对比：只要不再指向本机路由、或模型被改掉，就标漂移。
fn apply_drift(state: &AppState, agents: &mut [AgentInfo]) {
    let applied = load_applied(state);
    let Some(map) = applied.as_object() else { return };
    let port = port_of(state);
    let marker = format!("127.0.0.1:{port}");
    for a in agents.iter_mut() {
        let Some(rec) = map.get(&a.id) else { continue };
        let expected_model = rec
            .get("model")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let expected_base = rec
            .get("base_url")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        a.expected_model = if expected_model.is_empty() { None } else { Some(expected_model.clone()) };
        a.expected_base = if expected_base.is_empty() { None } else { Some(expected_base.clone()) };
        a.applied_at = rec
            .get("applied_at")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        // 新增的 7 个模板会写多个文件 / 非 JSON 格式，粗略的 grab() 容易误判；
        // 直接把所有目标文件的原文拼起来，判「是否仍指向本机 + 是否仍含上次导入的模型」。
        if template_paths(&a.id).is_some() {
            let text = a
                .config_paths
                .iter()
                .map(|p| std::fs::read_to_string(p).unwrap_or_default())
                .collect::<Vec<_>>()
                .join("\n");
            let still_connected = text.contains(&marker) || text.contains("llm-router");
            // 有的模板会把模型名写进多处（customModels[].model + 顶层 model），
            // 有的（Amp）根本不写模型名：用导入时的出现次数做基线，比「是否包含」准确。
            let baseline = rec.get("model_hits").and_then(|v| v.as_u64());
            let hits = if expected_model.is_empty() {
                0
            } else {
                text.matches(expected_model.as_str()).count() as u64
            };
            let model_gone = match baseline {
                Some(n) => n > 0 && hits < n,
                // 旧记录没有基线字段时退回「完全不含模型名」的粗判。
                None => !expected_model.is_empty() && hits == 0,
            };
            a.connected = still_connected;
            a.drifted = !still_connected || model_gone;
            // DSH 的默认模型是单独的 agent-default-model 条目，import 时若钉过，
            // 现在被谁改回别的 provider（Magpie 就是这么干的）就算漂移。
            if a.id == "dsh-tui" {
                let pin = rec
                    .get("pin_default")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                if pin {
                    let provider_off = a.default_provider.as_deref() != Some("llmrouter");
                    let model_off = a.default_model.as_deref() != Some(expected_model.as_str());
                    if provider_off || model_off {
                        a.drifted = true;
                        let now = match (&a.default_provider, &a.default_model) {
                            (Some(p), Some(m)) => format!("{p}/{m}"),
                            (Some(p), None) => p.clone(),
                            (None, Some(m)) => m.clone(),
                            (None, None) => "（条目被删掉了）".to_string(),
                        };
                        a.notes = format!(
                            "{}；⚠ 默认模型已被改回 {}，可点「一键修回」",
                            a.notes, now
                        );
                    }
                }
            }
            if a.drifted && !a.notes.contains("⚠ 配置已漂移") && !a.notes.contains("⚠ 默认模型") {
                let why = if !still_connected {
                    "配置已不再指向本机 llm-router"
                } else {
                    "上次导入的模型已不在配置里"
                };
                a.notes = format!("{}；⚠ 配置已漂移：{}，可点「一键修回」", a.notes, why);
            }
            continue;
        }
        let model_changed = !expected_model.is_empty()
            && a.current_model
                .as_deref()
                .map(|m| m != expected_model)
                .unwrap_or(false);
        let base_changed = a
            .current_base
            .as_deref()
            .map(|b| !b.contains(&marker) && !b.contains("llm-router"))
            .unwrap_or(false);
        a.drifted = !a.connected || model_changed || base_changed;
        if a.drifted {
            let why = if !a.connected {
                "配置已不再指向本机 llm-router"
            } else if model_changed {
                "模型被改成了别的值"
            } else {
                "接口地址被改走了"
            };
            a.notes = format!("{}；⚠ 配置已漂移：{}，可点「一键修回」", a.notes, why);
        }
    }
}

fn backup_path_for(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "config".to_string());
    path.with_file_name(format!("{name}.llmrouter.bak"))
}

/// 写文件前保留一份原始副本；dry_run 只返回预览文本，不落盘。
fn persist(path: &Path, content: &str, dry_run: bool) -> Result<(Option<String>, Option<String>), String> {
    if dry_run {
        return Ok((None, Some(content.to_string())));
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("创建配置目录失败: {e}"))?;
    }
    let backup = backup_path_for(path);
    if !backup.exists() {
        if path.exists() {
            std::fs::copy(path, &backup).map_err(|e| format!("备份原配置失败: {e}"))?;
        } else {
            // 文件本来不存在：留一个标记，恢复时按「删除新建文件」处理。
            std::fs::write(&backup, NEW_FILE_MARKER)
                .map_err(|e| format!("写入新文件标记失败: {e}"))?;
        }
    }
    std::fs::write(path, content).map_err(|e| format!("写入配置失败: {e}"))?;
    Ok((Some(backup.to_string_lossy().to_string()), None))
}

/// 目标文件原本不存在时，备份文件里写这个标记，恢复时据此删除新建文件。
const NEW_FILE_MARKER: &str = "__LLMROUTER_CREATED_BY_IMPORT__";

/// VS Code 的 settings.json 常带 // 注释或尾逗号，先做一遍保留字符串字面量的清洗。
pub(super) fn strip_jsonc(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    let mut in_string = false;
    let mut escaped = false;
    let mut in_line_comment = false;
    let mut in_block_comment = false;
    while let Some(c) = chars.next() {
        if in_line_comment {
            if c == '\n' {
                in_line_comment = false;
                out.push(c);
            }
            continue;
        }
        if in_block_comment {
            if c == '*' && chars.peek() == Some(&'/') {
                chars.next();
                in_block_comment = false;
            }
            continue;
        }
        if in_string {
            out.push(c);
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_string = false;
            }
            continue;
        }
        if c == '"' {
            in_string = true;
            out.push(c);
            continue;
        }
        if c == '/' {
            match chars.peek() {
                Some('/') => {
                    chars.next();
                    in_line_comment = true;
                    continue;
                }
                Some('*') => {
                    chars.next();
                    in_block_comment = true;
                    continue;
                }
                _ => {}
            }
        }
        out.push(c);
    }
    // 去掉对象和数组最后一个元素后的尾逗号（同样不能碰字符串里的逗号）。
    let mut result = String::with_capacity(out.len());
    let mut in_string = false;
    let mut escaped = false;
    let mut it = out.chars().peekable();
    while let Some(c) = it.next() {
        if in_string {
            result.push(c);
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_string = false;
            }
            continue;
        }
        if c == '"' {
            in_string = true;
            result.push(c);
            continue;
        }
        if c == ',' {
            let mut look = it.clone();
            let mut next = None;
            while let Some(&n) = look.peek() {
                if n.is_whitespace() {
                    look.next();
                } else {
                    next = Some(n);
                    break;
                }
            }
            if matches!(next, Some('}') | Some(']')) {
                continue;
            }
        }
        result.push(c);
    }
    result
}

fn merge_json(path: &Path, f: impl FnOnce(&mut Value)) -> Result<String, String> {
    let raw = std::fs::read_to_string(path).unwrap_or_default();
    let text = strip_jsonc(raw.trim_start_matches('\u{feff}'));
    let mut v: Value = if text.trim().is_empty() {
        json!({})
    } else {
        serde_json::from_str(&text)
            .map_err(|e| format!("{} 不是合法 JSON（{}），已保留原文件", path.display(), e))?
    };
    if !v.is_object() {
        return Err(format!("{} 顶层不是对象，已保留原文件", path.display()));
    }
    f(&mut v);
    serde_json::to_string_pretty(&v).map_err(|e| e.to_string())
}

fn merge_settings(path: &Path, prefix: &str, model: &str, key: &str, base: &str) -> Result<String, String> {
    merge_json(path, |v| {
        let k1 = format!("{prefix}.apiProvider");
        let k2 = format!("{prefix}.openAiBaseUrl");
        let k3 = format!("{prefix}.openAiApiKey");
        let k4 = format!("{prefix}.openAiModelId");
        v[k1.as_str()] = json!("openai");
        v[k2.as_str()] = json!(base);
        v[k3.as_str()] = json!(key);
        v[k4.as_str()] = json!(model);
    })
}

fn merge_env(path: &Path, entries: &[(&str, String)]) -> Result<String, String> {
    merge_json(path, |v| {
        if !v.get("env").map(|x| x.is_object()).unwrap_or(false) {
            v["env"] = json!({});
        }
        let env = v["env"].as_object_mut().unwrap();
        for (k, val) in entries {
            env.insert((*k).to_string(), json!(val));
        }
    })
}

fn write_codex(path: &Path, model: &str, base: &str) -> Result<String, String> {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let mut doc: toml::Value = if text.trim().is_empty() {
        toml::Value::Table(toml::Table::new())
    } else {
        toml::from_str(&text).map_err(|e| format!("{} 不是合法 TOML（{}），已保留原文件", path.display(), e))?
    };
    let root = doc
        .as_table_mut()
        .ok_or_else(|| format!("{} 顶层不是表，已保留原文件", path.display()))?;
    root.insert("model".to_string(), toml::Value::String(model.to_string()));
    root.insert(
        "model_provider".to_string(),
        toml::Value::String("llm-router".to_string()),
    );
    let mut providers = root
        .get("model_providers")
        .and_then(|v| v.as_table())
        .cloned()
        .unwrap_or_default();
    let mut provider = toml::Table::new();
    provider.insert("name".to_string(), toml::Value::String("llm-router".to_string()));
    provider.insert("base_url".to_string(), toml::Value::String(base.to_string()));
    provider.insert("wire_api".to_string(), toml::Value::String("chat".to_string()));
    providers.insert("llm-router".to_string(), toml::Value::Table(provider));
    root.insert("model_providers".to_string(), toml::Value::Table(providers));
    toml::to_string_pretty(&doc).map_err(|e| e.to_string())
}

fn write_aider(path: &Path, model: &str, key: &str, base: &str) -> Result<String, String> {
    let wanted = [
        ("openai-api-base", base.to_string()),
        ("openai-api-key", key.to_string()),
        ("model", model.to_string()),
    ];
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let mut lines: Vec<String> = Vec::new();
    let mut replaced = HashSet::new();
    for line in text.lines() {
        let t = line.trim_start();
        let mut hit = false;
        for (k, v) in &wanted {
            if t.starts_with(&format!("{k}:"))
                || t.starts_with(&format!("{k} ="))
                || t.starts_with(&format!("\"{k}\":"))
            {
                lines.push(format!("{k}: {v}"));
                replaced.insert(*k);
                hit = true;
                break;
            }
        }
        if !hit {
            lines.push(line.to_string());
        }
    }
    for (k, v) in &wanted {
        if !replaced.contains(k) {
            lines.push(format!("{k}: {v}"));
        }
    }
    Ok(lines.join("\n") + "\n")
}

/// 返回 (待写入内容, 改动的配置键)。
fn write_agent(
    id: &str,
    path: &Path,
    model: &str,
    api_key: &str,
    openai: &str,
    anthropic: &str,
) -> Result<(String, Vec<String>), String> {
    let key = api_key.to_string();
    match id {
        "claude-code" => Ok((
            merge_env(
                path,
                &[
                    ("ANTHROPIC_BASE_URL", anthropic.to_string()),
                    ("ANTHROPIC_AUTH_TOKEN", key.clone()),
                    ("ANTHROPIC_MODEL", model.to_string()),
                    ("ANTHROPIC_SMALL_FAST_MODEL", model.to_string()),
                ],
            )?,
            vec![
                "env.ANTHROPIC_BASE_URL".into(),
                "env.ANTHROPIC_AUTH_TOKEN".into(),
                "env.ANTHROPIC_MODEL".into(),
                "env.ANTHROPIC_SMALL_FAST_MODEL".into(),
            ],
        )),
        "codex" => Ok((
            write_codex(path, model, openai)?,
            vec!["model".into(), "model_provider".into(), "model_providers.llm-router".into()],
        )),
        "aider" => Ok((
            write_aider(path, model, &key, openai)?,
            vec!["openai-api-base".into(), "openai-api-key".into(), "model".into()],
        )),
        "qwen-code" => Ok((
            merge_env(
                path,
                &[
                    ("OPENAI_BASE_URL", openai.to_string()),
                    ("OPENAI_API_KEY", key.clone()),
                    ("OPENAI_MODEL", model.to_string()),
                ],
            )?,
            vec!["env.OPENAI_BASE_URL".into(), "env.OPENAI_API_KEY".into(), "env.OPENAI_MODEL".into()],
        )),
        "opencode" => Ok((
            merge_json(path, |v| {
                v["model"] = json!(format!("llm-router/{model}"));
                if !v.get("provider").map(|x| x.is_object()).unwrap_or(false) {
                    v["provider"] = json!({});
                }
                let mut models = serde_json::Map::new();
                models.insert(model.to_string(), json!({"name": model}));
                v["provider"]["llm-router"] = json!({
                    "npm": "@ai-sdk/openai-compatible",
                    "name": "llm-router",
                    "options": {"baseURL": openai, "apiKey": key},
                    "models": models,
                });
            })?,
            vec!["model".into(), "provider.llm-router".into()],
        )),
        "continue" => Ok((
            merge_json(path, |v| {
                if !v.get("models").map(|x| x.is_array()).unwrap_or(false) {
                    v["models"] = json!([]);
                }
                let arr = v["models"].as_array_mut().unwrap();
                arr.retain(|x| {
                    !(x.get("provider").and_then(|p| p.as_str()) == Some("openai")
                        && x.get("apiBase").and_then(|p| p.as_str()) == Some(openai))
                });
                arr.insert(
                    0,
                    json!({
                        "title": format!("llm-router / {model}"),
                        "provider": "openai",
                        "model": model,
                        "apiBase": openai,
                        "apiKey": key,
                    }),
                );
            })?,
            vec!["models[0]".into()],
        )),
        "cline" => Ok((
            merge_settings(path, "cline", model, &key, openai)?,
            vec![
                "cline.apiProvider".into(),
                "cline.openAiBaseUrl".into(),
                "cline.openAiApiKey".into(),
                "cline.openAiModelId".into(),
            ],
        )),
        "roo-code" => Ok((
            merge_settings(path, "roo-cline", model, &key, openai)?,
            vec![
                "roo-cline.apiProvider".into(),
                "roo-cline.openAiBaseUrl".into(),
                "roo-cline.openAiApiKey".into(),
                "roo-cline.openAiModelId".into(),
            ],
        )),
        _ => Err("该 Agent 暂未适配自动写入".to_string()),
    }
}

// ---------- 七个新增 Agent 的写入模板 ----------
//
// 键名来自本机各 CLI 自带文档 / 官方配置说明的只读调研，见 desktop/README.md。
// 原则：只追加/替换 llm-router 自己的条目或 managed 块，绝不整体覆盖用户原有配置。

/// 这几个 Agent 一键导入会触碰的全部文件（绝对路径）。None = 未适配。
fn template_paths(id: &str) -> Option<Vec<PathBuf>> {
    let h = home();
    Some(match id {
        "droid" => vec![h.join(".factory").join("settings.json")],
        "amp" => vec![
            h.join(".config").join("amp").join("settings.json"),
            h.join(".local").join("share").join("amp").join("secrets.json"),
        ],
        "crush" => vec![h.join(".config").join("crush").join("crushrc")],
        "codebuddy" => vec![
            h.join(".codebuddy").join("models.json"),
            h.join(".codebuddy").join("settings.json"),
        ],
        "kimi-code" => vec![h.join(".kimi-code").join("config.toml")],
        "zcode" => vec![h.join(".zcode").join("v2").join("config.json")],
        "workbuddy" => vec![h.join(".workbuddy").join("models.json")],
        // DeepSeek Harness（dsh / dsh-tui 共用一套 host 配置）
        "dsh-tui" => {
            let mut v = vec![
                h.join(".dsh").join("settings.yaml"),
                h.join(".dsh").join(".env"),
            ];
            // profile patch 层才是桌面版真正读的路由（Magpie 也是改它）
            v.extend(dsh_patch_paths());
            v
        }
        _ => return None,
    })
}

fn template_notes(id: &str) -> &'static str {
    match id {
        "droid" => "写入 customModels[]（provider=generic-chat-completion-api）与顶层 model；保留原有 hooks。",
        "amp" => "写入用户级 amp.url 与 secrets.json 的 apiKey@<URL>（两个文件）。Amp 固定 model ID，若报模型不存在请在 Amp 里选路由中已有的模型。",
        "crush" => "在 crushrc 追加 provider add / model add 命令块（completions）；只写我们生成的命令，默认模型请在 Crush 里手动选。",
        "codebuddy" => "写入 models.json 的 models[]（完整 /chat/completions 路径）与 settings.json 的 model / env.CODEBUDDY_*；保留 hooks。",
        "kimi-code" => "在 Orca 托管块之外追加 default_model 与 [providers.llm-router] / [models.*]；不动 [[hooks]]。",
        "zcode" => "追加 provider.llm-router（kind=anthropic）而不覆盖 magpie；默认模型需在 ZCode UI 里选。",
        "workbuddy" => "在 models.json 数组追加一条自定义模型；默认模型需在 WorkBuddy UI 里选。",
        "dsh-tui" => {
            "把 llm-router 作为 openai-completions provider 追加到 ~/.dsh/settings.yaml 与 ~/.dsh/profiles/*/cordis.patch.yml（桌面版真正读的 patch 层），并在 ~/.dsh/.env 写 LLM_ROUTER_API_KEY；勾选「同时切默认模型」会把 agent-default-model 钉到 llmrouter，重启 dsh web 后生效。"
        }
        _ => "",
    }
}

/// npm 动态发现出来的卡片默认 configurable=false，这里按稳定 id 补齐写入能力。
fn apply_templates(out: &mut [AgentInfo], port: u16) {
    for a in out.iter_mut() {
        let Some(paths) = template_paths(&a.id) else {
            continue;
        };
        let p: Vec<String> = paths.iter().map(|x| x.to_string_lossy().to_string()).collect();
        a.configurable = true;
        a.config_path = p[0].clone();
        a.config_paths = p;
        a.notes = template_notes(&a.id).to_string();
        if a.id == "dsh-tui" {
            // settings.yaml 里同时有多种 provider，不能靠通用 grab() 猜；
            // 只读我们自己的 llm-router 块。
            let text = std::fs::read_to_string(&paths[0]).unwrap_or_default();
            let (m, b) = read_dsh_provider(&text);
            a.connected = b
                .as_deref()
                .map(|x| x.contains(&format!("127.0.0.1:{port}")) || x.contains("llm-router"))
                .unwrap_or(false);
            a.current_model = m;
            a.current_base = b;
            // patch 层里 agent-default-model 现在钉的是谁 —— 这才是「DSH 实际在用哪个模型」。
            // 桌面版真正读的是 profiles/tauri（Magpie 也只重写这一份），优先看它；
            // 没有带上默认模型的 tauri 时，退回第一个带默认模型的其它 profile。
            let mut chosen: Option<(Option<String>, Option<String>)> = None;
            let mut tauri: Option<(Option<String>, Option<String>)> = None;
            for pp in dsh_patch_paths() {
                let t = std::fs::read_to_string(&pp).unwrap_or_default();
                let (dp, dm) = read_dsh_patch_default(&t);
                if dp.is_none() && dm.is_none() {
                    continue;
                }
                if pp
                    .parent()
                    .and_then(|x| x.file_name())
                    .map(|x| x == "tauri")
                    .unwrap_or(false)
                {
                    tauri = Some((dp, dm));
                    break;
                }
                if chosen.is_none() {
                    chosen = Some((dp, dm));
                }
            }
            if let Some((dp, dm)) = tauri.or(chosen) {
                if dp.as_deref() == Some("llmrouter") {
                    a.connected = true;
                    // 默认模型已经钉到本机路由时，卡片上的「模型」就用钉的那个，
                    // 而不是 settings.yaml 里 provider 块的第一条（那只是列表顺序）。
                    if dm.is_some() {
                        a.current_model = dm.clone();
                    }
                }
                a.default_provider = dp;
                a.default_model = dm;
            }
            continue;
        }
        let (m, b, c) = read_current(&paths[0], port);
        a.current_model = m;
        a.current_base = b;
        a.connected = c;
    }
}

fn toml_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => out.push_str(&format!("\\u{:04X}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

/// crushrc 是可执行 shell：值里只保留安全字符，避免引入 $()/反引号/引号。
fn shell_safe(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | ':' | '/') {
                c
            } else {
                '-'
            }
        })
        .collect()
}

/// 删掉旧的 managed 块（含首尾标记行），再把新块追加到末尾。
fn replace_managed(text: &str, start: &str, end: &str, block: &str) -> String {
    let mut out = String::with_capacity(text.len() + block.len());
    let mut skipping = false;
    for line in text.lines() {
        if line.trim() == start {
            skipping = true;
            continue;
        }
        if skipping {
            if line.trim() == end {
                skipping = false;
            }
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    let mut base = out.trim_end_matches('\n').to_string();
    if !block.is_empty() {
        if !base.is_empty() {
            base.push('\n');
        }
        base.push_str(block.trim_end_matches('\n'));
    }
    base.push('\n');
    base
}

/// 读 DSH settings.yaml 里我们自己写的 llm-router provider：返回 (第一个模型 id, baseURL)。
fn read_dsh_provider(text: &str) -> (Option<String>, Option<String>) {
    let mut inside = false;
    let mut base: Option<String> = None;
    let mut model: Option<String> = None;
    for line in text.lines() {
        let t = line.trim();
        if t == "llm-router:" {
            inside = true;
            continue;
        }
        if !inside {
            continue;
        }
        // 缩进回到 4 空格说明已经到了下一个 provider，块结束。
        if !t.is_empty() && !t.starts_with('#') && line.starts_with("    ") && !line.starts_with("      ") {
            break;
        }
        if let Some(v) = t.strip_prefix("baseURL:") {
            if base.is_none() {
                base = Some(v.trim().trim_matches('"').to_string());
            }
        }
        if model.is_none() {
            if let Some(v) = t.strip_prefix("- id:") {
                model = Some(v.trim().trim_matches('"').to_string());
            }
        }
    }
    (model, base)
}

/// ~/.dsh/.env 这类 KEY=VALUE 文件：替换同名键，其余行原样保留，缺的追加到末尾。
fn merge_dotenv(path: &Path, entries: &[(&str, String)]) -> Result<String, String> {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let mut lines: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for line in text.lines() {
        let t = line.trim();
        let mut hit = false;
        for (k, v) in entries {
            if t.starts_with(&format!("{k}=")) || t.starts_with(&format!("export {k}=")) {
                lines.push(format!("{k}={v}"));
                seen.insert((*k).to_string());
                hit = true;
                break;
            }
        }
        if !hit {
            lines.push(line.to_string());
        }
    }
    for (k, v) in entries {
        if !seen.contains(*k) {
            lines.push(format!("{k}={v}"));
        }
    }
    Ok(lines.join("\n") + "\n")
}

/// YAML 双引号标量里需要转义的只有反斜杠和双引号。
fn yaml_str(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// llm-router 在 DSH settings.yaml 里的 provider 块（缩进 4 空格，挂在 providers: 下）。
fn dsh_provider_block(model: &str, all: &[String], openai: &str) -> String {
    let mut list: Vec<String> = all.to_vec();
    if !list.iter().any(|m| m == model) {
        list.insert(0, model.to_string());
    }
    let mut b = String::new();
    b.push_str("    # >>> llm-router managed >>>\n");
    b.push_str("    llm-router:\n");
    b.push_str("      displayName: LLM Router\n");
    b.push_str("      apiKeyEnv: LLM_ROUTER_KEY\n");
    b.push_str("      api: openai-completions\n");
    b.push_str(&format!("      baseURL: \"{}\"\n", yaml_str(openai)));
    b.push_str("      models:\n");
    for m in &list {
        let id = yaml_str(m);
        b.push_str(&format!("        - id: \"{id}\"\n"));
        b.push_str(&format!("          name: \"{id} · LLM Router\"\n"));
        b.push_str("          contextWindow: 128000\n");
        b.push_str("          maxTokens: 8192\n");
        b.push_str("          input: [text, image]\n");
    }
    b.push_str("    # <<< llm-router managed <<<\n");
    b
}

/// 把 provider 块并进 settings.yaml：已有 providers 映射就替换旧块并追加到末尾。
fn dsh_settings_with_provider(raw: &str, block: &str) -> Result<String, String> {
    const START: &str = "# >>> llm-router managed >>>";
    const END: &str = "# <<< llm-router managed <<<";
    if raw.trim().is_empty() {
        return Ok(format!("llm-pi-ai:\n  providers:\n{block}"));
    }
    if !raw.contains("providers:") {
        return Ok(format!("{}\nllm-pi-ai:\n  providers:\n{}", raw.trim_end(), block));
    }
    // 追加到文件末尾只有在末尾仍处于 providers 映射（缩进 >= 4）时才安全。
    let tail = raw.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("");
    let indent = tail.len() - tail.trim_start().len();
    if indent < 4 {
        return Err(format!(
            "{} 末尾缩进只有 {indent} 空格，不在 providers 块内；为避免写坏原文件已中止",
            "settings.yaml"
        ));
    }
    Ok(replace_managed(raw, START, END, block))
}

/// DSH 每个 profile 的 patch 层：~/.dsh/profiles/<profile>/cordis.patch.yml
/// 真正决定桌面版模型来源的就是这一层（Magpie 也是改它）。
fn dsh_patch_paths() -> Vec<PathBuf> {
    let mut out = Vec::new();
    let dir = home().join(".dsh").join("profiles");
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for e in entries.flatten() {
            let p = e.path().join("cordis.patch.yml");
            if p.is_file() {
                out.push(p);
            }
        }
    }
    out.sort();
    out
}

/// 缩进 n 个空格的行的前导空格数；空行/注释返回 usize::MAX（调用方按需要跳过）。
fn line_indent(line: &str) -> usize {
    line.len() - line.trim_start().len()
}

fn is_blank_or_comment(line: &str) -> bool {
    let t = line.trim();
    t.is_empty() || t.starts_with('#')
}

/// 生成 patch 层里 llm-router provider 节点（indent 6，挂在 llm-pi-ai.config.providers 下）。
fn dsh_patch_provider_block(all: &[String], openai: &str) -> String {
    let mut b = String::new();
    b.push_str("      # >>> llm-router managed >>>\n");
    b.push_str("      llmrouter:\n");
    b.push_str("        displayName: LLM Router\n");
    b.push_str("        apiKeyEnv: LLMROUTER_API_KEY\n");
    b.push_str("        api: openai-completions\n");
    b.push_str(&format!("        baseURL: \"{}\"\n", yaml_str(openai)));
    b.push_str("        models:\n");
    for m in all {
        let id = yaml_str(m);
        b.push_str(&format!("          - id: \"{id}\"\n"));
        b.push_str(&format!("            name: \"{id} · LLM Router\"\n"));
        b.push_str("            contextWindow: 128000\n");
        b.push_str("            maxTokens: 8192\n");
        b.push_str("            input:\n");
        b.push_str("              - text\n");
        b.push_str("              - image\n");
    }
    b.push_str("      # <<< llm-router managed <<<\n");
    b
}

/// 用我们的块替换 patch 层里已有的 `llmrouter:` provider；没有就插到 `providers:` 之后。
fn patch_llmrouter_provider(text: &str, block: &str) -> Result<String, String> {
    let mut lines: Vec<String> = text.lines().map(|s| s.to_string()).collect();
    let key_at = lines.iter().position(|l| {
        l.trim_end().starts_with("llmrouter:")
            && line_indent(l) == 6
            && l.trim_start().starts_with("llmrouter:")
    });
    let block_lines: Vec<String> = block.lines().map(|s| s.to_string()).collect();
    if let Some(i) = key_at {
        // 找到 provider 节点结束：下一个缩进 <= 6 的实义行
        let mut j = i + 1;
        while j < lines.len() {
            let l = &lines[j];
            if is_blank_or_comment(l) {
                // managed 块的注释行属于旧块，一并吃掉
                j += 1;
                continue;
            }
            if line_indent(l) <= 6 {
                break;
            }
            j += 1;
        }
        lines.splice(i..j, block_lines);
        return Ok(lines.join("\n") + "\n");
    }
    // 没有就插在 providers: 之后
    let providers = lines.iter().position(|l| {
        let t = l.trim_start();
        line_indent(l) == 4 && t.starts_with("providers:")
    });
    let Some(p) = providers else {
        return Err("patch 层里没有找到 llm-pi-ai 的 providers: 节点，已跳过".into());
    };
    let mut at = p + 1;
    while at < lines.len() && is_blank_or_comment(&lines[at]) {
        at += 1;
    }
    lines.splice(at..at, block_lines);
    Ok(lines.join("\n") + "\n")
}

/// 读 patch 层里 agent-default-model 现在钉的是谁。
fn read_dsh_patch_default(text: &str) -> (Option<String>, Option<String>) {
    let mut provider = None;
    let mut model = None;
    let mut inside = false;
    for line in text.lines() {
        let t = line.trim_start();
        if t.starts_with("- id:") {
            inside = t.contains("agent-default-model");
            if !inside {
                continue;
            }
        }
        if !inside {
            continue;
        }
        // patch 层里可能是 `provider: magpie` 也可能是流式 `config: { provider: x, model: y }`
        if provider.is_none() {
            if let Some(v) = line_value_of(line, "provider") {
                provider = Some(v);
            }
        }
        if model.is_none() {
            if let Some(v) = line_value_of(line, "model") {
                model = Some(v);
            }
        }
        if provider.is_some() && model.is_some() {
            break;
        }
    }
    (provider, model)
}

/// 从一行里取 `key: value`（含流式 `{ key: value, ... }`）的 value，取不到返回 None。
fn line_value_of(line: &str, key: &str) -> Option<String> {
    let hay = line;
    let mut search = 0usize;
    while search < hay.len() {
        let Some(rel) = hay[search..].find(key) else {
            break;
        };
        let at = search + rel;
        let before_ok = at == 0
            || matches!(
                hay.as_bytes()[at - 1],
                b' ' | b'{' | b',' | b'\t' | b'-' | b'\n'
            );
        if before_ok {
            if let Some(rest) = hay[at + key.len()..].strip_prefix(':') {
                let v: String = rest
                    .trim()
                    .chars()
                    .take_while(|c| !matches!(c, ',' | '}' | '\n' | '#'))
                    .collect();
                let v = v.trim().trim_matches('"').trim_matches('\'').to_string();
                if !v.is_empty() {
                    return Some(v);
                }
            }
        }
        search = at + key.len();
    }
    None
}

/// 把 agent-default-model 钉到 llm-router：改 provider/model，没有这个条目就追加一条。
fn patch_default_model(text: &str, provider: &str, model: &str) -> Result<String, String> {
    let mut lines: Vec<String> = text.lines().map(|s| s.to_string()).collect();
    let at = lines
        .iter()
        .position(|l| l.trim_start().starts_with("- id:") && l.contains("agent-default-model"));
    let Some(i) = at else {
        let mut out = text.trim_end().to_string();
        out.push_str(&format!(
            "\n- id: agent-default-model\n  config:\n    provider: {provider}\n    model: \"{}\"\n",
            yaml_str(model)
        ));
        return Ok(out);
    };
    // 条目范围：到下一个顶层 `- ` 行为止
    let mut end = i + 1;
    while end < lines.len() && !lines[end].trim_start().starts_with("- ") {
        end += 1;
    }
    let mut done_provider = false;
    let mut done_model = false;
    for l in lines.iter_mut().take(end).skip(i + 1) {
        if is_blank_or_comment(l) {
            continue;
        }
        if let Some(v) = line_value_of(l, "provider") {
            if !v.starts_with("http") {
                *l = format!("{}provider: {provider}", " ".repeat(line_indent(l)));
                done_provider = true;
                continue;
            }
        }
        if let Some(v) = line_value_of(l, "model") {
            if !v.starts_with("http") {
                *l = format!(
                    "{}model: \"{}\"",
                    " ".repeat(line_indent(l)),
                    yaml_str(model)
                );
                done_model = true;
                continue;
            }
        }
    }
    if !done_provider || !done_model {
        // 补在 config: 行之后（没有 config: 就补在条目末尾）
        let anchor = lines
            .iter()
            .enumerate()
            .take(end)
            .skip(i + 1)
            .find(|(_, l)| l.trim_start().starts_with("config:"))
            .map(|(n, _)| n);
        let insert_at = anchor.map(|n| n + 1).unwrap_or(i + 1);
        let mut extra: Vec<String> = Vec::new();
        if !done_provider {
            extra.push(format!("    provider: {provider}"));
        }
        if !done_model {
            extra.push(format!("    model: \"{}\"", yaml_str(model)));
        }
        lines.splice(insert_at..insert_at, extra);
    }
    Ok(lines.join("\n") + "\n")
}

#[allow(clippy::too_many_arguments)]
fn write_template_plan(
    id: &str,
    paths: &[PathBuf],
    model: &str,
    api_key: &str,
    openai: &str,
    anthropic: &str,
    all_models: &[String],
    pin_default: bool,
) -> Result<WritePlan, String> {
    let chat = format!("{}/chat/completions", openai.trim_end_matches('/'));
    let display = format!("llm-router / {model}");
    match id {
        "droid" => {
            let content = merge_json(&paths[0], |v| {
                if !v.get("customModels").map(|x| x.is_array()).unwrap_or(false) {
                    v["customModels"] = json!([]);
                }
                let arr = v["customModels"].as_array_mut().unwrap();
                arr.retain(|x| {
                    !(x.get("displayName")
                        .and_then(|s| s.as_str())
                        .map(|s| s.starts_with("llm-router /"))
                        .unwrap_or(false)
                        || x.get("model").and_then(|s| s.as_str()) == Some(model))
                });
                arr.push(json!({
                    "model": model,
                    "displayName": display,
                    "baseUrl": openai,
                    "apiKey": api_key,
                    "provider": "generic-chat-completion-api",
                    "maxOutputTokens": 8192
                }));
                v["model"] = json!(model);
            })?;
            Ok(WritePlan {
                files: vec![(paths[0].clone(), content)],
                changed_keys: vec!["customModels[]".into(), "model".into()],
            })
        }
        "amp" => {
            let settings = merge_json(&paths[0], |v| {
                v["amp.url"] = json!(openai);
            })?;
            let secret_key = format!("apiKey@{openai}");
            let key_copy = secret_key.clone();
            let secrets = merge_json(&paths[1], |v| {
                if let Some(obj) = v.as_object_mut() {
                    let stale: Vec<String> = obj
                        .keys()
                        .filter(|k| k.starts_with("apiKey@http://127.0.0.1:"))
                        .cloned()
                        .collect();
                    for k in stale {
                        obj.remove(&k);
                    }
                }
                v[key_copy.as_str()] = json!(api_key);
            })?;
            Ok(WritePlan {
                files: vec![(paths[0].clone(), settings), (paths[1].clone(), secrets)],
                changed_keys: vec!["amp.url".into(), secret_key],
            })
        }
        "crush" => {
            let url = shell_safe(openai);
            let key = shell_safe(api_key);
            let mid = shell_safe(model);
            let block = format!(
                "# >>> llm-router managed >>>\nprovider add llm-router --type completions --base-url \"{url}\" --api-key \"{key}\"\nmodel add llm-router/{mid} --name \"llm-router / {mid}\" --context-window 128000 --default-max-tokens 8192\n# <<< llm-router managed <<<\n"
            );
            let raw = std::fs::read_to_string(&paths[0]).unwrap_or_default();
            let content = replace_managed(
                &raw,
                "# >>> llm-router managed >>>",
                "# <<< llm-router managed <<<",
                &block,
            );
            Ok(WritePlan {
                files: vec![(paths[0].clone(), content)],
                changed_keys: vec![
                    "provider add llm-router".into(),
                    format!("model add llm-router/{mid}"),
                ],
            })
        }
        "codebuddy" => {
            let models_content = merge_json(&paths[0], |v| {
                if !v.get("models").map(|x| x.is_array()).unwrap_or(false) {
                    v["models"] = json!([]);
                }
                let arr = v["models"].as_array_mut().unwrap();
                arr.retain(|x| {
                    !(x.get("id").and_then(|s| s.as_str()) == Some(model)
                        || x.get("name")
                            .and_then(|s| s.as_str())
                            .map(|s| s.starts_with("llm-router /"))
                            .unwrap_or(false))
                });
                arr.push(json!({
                    "id": model,
                    "name": display,
                    "vendor": "llm-router",
                    "api": "openai-completions",
                    "url": chat,
                    "apiKey": api_key,
                    "maxInputTokens": 128000,
                    "maxOutputTokens": 8192,
                    "supportsToolCall": true
                }));
                if !v.get("availableModels").map(|x| x.is_array()).unwrap_or(false) {
                    v["availableModels"] = json!([]);
                }
                let av = v["availableModels"].as_array_mut().unwrap();
                av.retain(|x| x.as_str() != Some(model));
                av.push(json!(model));
            })?;
            let settings_content = merge_json(&paths[1], |v| {
                v["model"] = json!(model);
                if !v.get("env").map(|x| x.is_object()).unwrap_or(false) {
                    v["env"] = json!({});
                }
                v["env"]["CODEBUDDY_MODEL"] = json!(model);
                v["env"]["CODEBUDDY_BASE_URL"] = json!(openai);
                v["env"]["CODEBUDDY_API_KEY"] = json!(api_key);
                v["env"]["CODEBUDDY_SMALL_FAST_MODEL"] = json!(model);
            })?;
            Ok(WritePlan {
                files: vec![
                    (paths[0].clone(), models_content),
                    (paths[1].clone(), settings_content),
                ],
                changed_keys: vec![
                    "models[]".into(),
                    "availableModels".into(),
                    "settings.model".into(),
                    "env.CODEBUDDY_*".into(),
                ],
            })
        }
        "kimi-code" => {
            let alias = format!("llm-router/{model}");
            let top = format!(
                "# >>> llm-router managed top >>>\ndefault_model = \"{}\"\n# <<< llm-router managed top <<<\n",
                toml_str(&alias)
            );
            let bottom = format!(
                "# >>> llm-router managed >>>\n[providers.llm-router]\ntype = \"openai\"\nbase_url = \"{}\"\napi_key = \"{}\"\n\n[models.\"{}\"]\nprovider = \"llm-router\"\nmodel = \"{}\"\nmax_context_size = 128000\nmax_output_size = 8192\ndisplay_name = \"{}\"\n# <<< llm-router managed <<<\n",
                toml_str(openai),
                toml_str(api_key),
                toml_str(&alias),
                toml_str(model),
                toml_str(&display)
            );
            let raw = std::fs::read_to_string(&paths[0]).unwrap_or_default();
            let stripped = replace_managed(
                &raw,
                "# >>> llm-router managed top >>>",
                "# <<< llm-router managed top <<<",
                "",
            );
            let body = replace_managed(
                &stripped,
                "# >>> llm-router managed >>>",
                "# <<< llm-router managed <<<",
                "",
            );
            let content = format!("{top}{}{bottom}", body.trim_start_matches('\n'));
            Ok(WritePlan {
                files: vec![(paths[0].clone(), content)],
                changed_keys: vec![
                    "default_model".into(),
                    "providers.llm-router".into(),
                    format!("models.\"{alias}\""),
                ],
            })
        }
        "zcode" => {
            let mut models = serde_json::Map::new();
            models.insert(
                format!("llm-router/{model}"),
                json!({
                    "limit": {"context": 128000, "output": 8192},
                    "modalities": {"input": ["text"], "output": ["text"]},
                    "name": display,
                    "reasoning": {
                        "defaultLevel": "high",
                        "enabled": false,
                        "levels": ["low", "high"],
                        "providerOptionsByLevel": {}
                    }
                }),
            );
            let content = merge_json(&paths[0], |v| {
                if !v.get("provider").map(|x| x.is_object()).unwrap_or(false) {
                    v["provider"] = json!({});
                }
                v["provider"]["llm-router"] = json!({
                    "enabled": true,
                    "kind": "anthropic",
                    "source": "custom",
                    "name": "llm-router",
                    "options": {"baseURL": anthropic, "apiKey": api_key},
                    "models": models
                });
            })?;
            Ok(WritePlan {
                files: vec![(paths[0].clone(), content)],
                changed_keys: vec!["provider.llm-router".into()],
            })
        }
        "workbuddy" => {
            let raw = std::fs::read_to_string(&paths[0]).unwrap_or_default();
            let text = strip_jsonc(raw.trim_start_matches('\u{feff}'));
            let mut doc: Value = if text.trim().is_empty() {
                json!([])
            } else {
                serde_json::from_str(&text).map_err(|e| {
                    format!("{} 不是合法 JSON（{}），已保留原文件", paths[0].display(), e)
                })?
            };
            let entry = json!({
                "id": model,
                "name": display,
                "url": chat,
                "apiKey": api_key,
                "maxInputTokens": 128000,
                "maxOutputTokens": 8192,
                "supportsToolCall": true,
                "supportsImages": false,
                "supportsReasoning": false
            });
            let ours = |x: &Value| -> bool {
                x.get("id").and_then(|s| s.as_str()) == Some(model)
                    || x.get("name")
                        .and_then(|s| s.as_str())
                        .map(|s| s.starts_with("llm-router /"))
                        .unwrap_or(false)
            };
            if doc.is_array() {
                let arr = doc.as_array_mut().unwrap();
                arr.retain(|x| !ours(x));
                arr.push(entry);
            } else if doc.get("models").map(|x| x.is_array()).unwrap_or(false) {
                let arr = doc["models"].as_array_mut().unwrap();
                arr.retain(|x| !ours(x));
                arr.push(entry);
            } else {
                return Err(format!(
                    "{} 顶层不是数组，也不是含 models 数组的对象，已保留原文件",
                    paths[0].display()
                ));
            }
            Ok(WritePlan {
                files: vec![(paths[0].clone(), serde_json::to_string_pretty(&doc).map_err(|e| e.to_string())?)],
                changed_keys: vec!["models[]".into()],
            })
        }
        "dsh-tui" => {
            let block = dsh_provider_block(model, all_models, openai);
            let raw = std::fs::read_to_string(&paths[0]).unwrap_or_default();
            let content = dsh_settings_with_provider(&raw, &block)?;
            let env = merge_dotenv(
                &paths[1],
                &[
                    ("LLM_ROUTER_KEY", api_key.to_string()),
                    ("LLMROUTER_API_KEY", api_key.to_string()),
                ],
            )?;
            let mut files = vec![(paths[0].clone(), content), (paths[1].clone(), env)];
            let mut changed_keys = vec![
                "llm-pi-ai.providers.llm-router".to_string(),
                "models[]".to_string(),
                "LLM_ROUTER_KEY".to_string(),
                "LLMROUTER_API_KEY".to_string(),
            ];
            // profile patch 层：真正决定 DSH 桌面版模型来源的就是它（Magpie 也是改这里）
            let patches = dsh_patch_paths();
            let patch_block = dsh_patch_provider_block(all_models, openai);
            for p in &patches {
                let raw = std::fs::read_to_string(p).unwrap_or_default();
                if raw.trim().is_empty() {
                    continue;
                }
                let mut next = patch_llmrouter_provider(&raw, &patch_block)?;
                if pin_default {
                    next = patch_default_model(&next, "llmrouter", model)?;
                }
                files.push((p.clone(), next));
            }
            if !patches.is_empty() {
                changed_keys.push("profiles/*/cordis.patch.yml:providers.llmrouter".to_string());
                if pin_default {
                    changed_keys.push("agent-default-model".to_string());
                }
            }
            Ok(WritePlan { files, changed_keys })
        }
        _ => Err("该 Agent 暂未适配自动写入".to_string()),
    }
}

/// 一键导入的写入计划：普通 Agent 一个文件，Pi 需要 models.json + settings.json 两个。
struct WritePlan {
    files: Vec<(PathBuf, String)>,
    changed_keys: Vec<String>,
}

fn write_plan(
    agent: &AgentInfo,
    model: &str,
    api_key: &str,
    openai: &str,
    anthropic: &str,
    all_models: &[String],
    pin_default: bool,
) -> Result<WritePlan, String> {
    if agent.id == "pi" {
        let models_path = PathBuf::from(&agent.config_path);
        let settings_path = models_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join("settings.json");
        let models_content = merge_json(&models_path, |v| {
            if !v.get("providers").map(|x| x.is_object()).unwrap_or(false) {
                v["providers"] = json!({});
            }
            v["providers"]["llm-router"] = json!({
                "name": "llm-router",
                "baseUrl": openai,
                "api": "openai-completions",
                "apiKey": api_key,
                "models": [{
                    "id": model,
                    "name": model,
                    "reasoning": false,
                    "input": ["text"],
                    "contextWindow": 128000,
                    "maxTokens": 8192
                }]
            });
        })?;
        let settings_content = merge_json(&settings_path, |v| {
            v["defaultProvider"] = json!("llm-router");
            v["defaultModel"] = json!(model);
        })?;
        return Ok(WritePlan {
            files: vec![(models_path, models_content), (settings_path, settings_content)],
            changed_keys: vec![
                "providers.llm-router".into(),
                "defaultProvider".into(),
                "defaultModel".into(),
            ],
        });
    }
    if let Some(paths) = template_paths(&agent.id) {
        return write_template_plan(
            &agent.id,
            &paths,
            model,
            api_key,
            openai,
            anthropic,
            all_models,
            pin_default,
        );
    }
    let (content, changed_keys) = write_agent(
        &agent.id,
        Path::new(&agent.config_path),
        model,
        api_key,
        openai,
        anthropic,
    )?;
    Ok(WritePlan {
        files: vec![(PathBuf::from(&agent.config_path), content)],
        changed_keys,
    })
}

/// 简化行级 diff：只列出新增/删除的整行，足够给用户确认“会改什么”。
fn diff_text(before: &str, after: &str) -> (usize, usize, String) {
    let b: Vec<&str> = before.lines().collect();
    let a: Vec<&str> = after.lines().collect();
    let bset: HashSet<&str> = b.iter().copied().collect();
    let aset: HashSet<&str> = a.iter().copied().collect();
    let mut lines: Vec<String> = Vec::new();
    let mut removed = 0usize;
    let mut added = 0usize;
    for line in &b {
        if !aset.contains(line) {
            removed += 1;
            if lines.len() < 400 {
                lines.push(format!("- {line}"));
            }
        }
    }
    for line in &a {
        if !bset.contains(line) {
            added += 1;
            if lines.len() < 400 {
                lines.push(format!("+ {line}"));
            }
        }
    }
    if lines.len() >= 400 {
        lines.push("…（diff 过长，只显示前 400 行）".into());
    }
    (added, removed, lines.join("\n"))
}

fn plan_diff(plan: &WritePlan) -> Vec<Value> {
    plan.files
        .iter()
        .map(|(path, after)| {
            let before = std::fs::read_to_string(path).unwrap_or_default();
            let existed = path.exists();
            let (added, removed, text) = diff_text(&before, after);
            json!({
                "path": path.to_string_lossy(),
                "existed": existed,
                "before_bytes": before.len(),
                "after_bytes": after.len(),
                "added_lines": added,
                "removed_lines": removed,
                "changed": before != *after,
                "text": text,
            })
        })
        .collect()
}

/// 应用后逐个文件回读，确认磁盘内容与计划完全一致；这是事务的“校验”阶段。
fn verify_plan(plan: &WritePlan) -> (bool, Vec<Value>) {
    let mut all_ok = true;
    let checks: Vec<Value> = plan
        .files
        .iter()
        .map(|(path, expected)| {
            let actual = std::fs::read_to_string(path).unwrap_or_default();
            let ok = actual == *expected;
            if !ok {
                all_ok = false;
            }
            json!({
                "path": path.to_string_lossy(),
                "ok": ok,
                "bytes": actual.len(),
            })
        })
        .collect();
    (all_ok, checks)
}

/// 失败时按写入顺序回滚已落盘的文件；备份是 persist 在写前建的。
fn rollback_applied(applied: &[(PathBuf, PathBuf)]) -> usize {
    let mut n = 0usize;
    for (path, backup) in applied.iter().rev() {
        let created = std::fs::read_to_string(backup)
            .map(|s| s.trim() == NEW_FILE_MARKER)
            .unwrap_or(false);
        let res = if created {
            if path.exists() {
                std::fs::remove_file(path).map(|_| ())
            } else {
                Ok(())
            }
        } else {
            std::fs::copy(backup, path).map(|_| ())
        };
        if res.is_ok() {
            let _ = std::fs::remove_file(backup);
            n += 1;
        }
    }
    n
}

pub async fn list(State(state): State<Arc<AppState>>) -> Response {
    ok(json!({
        "ok": true,
        "port": port_of(&state),
        "base_url": default_base_url(&state),
        "models": model_ids(&state),
        "agents": scan_agents(&state),
        "scan_at": chrono::Utc::now().to_rfc3339(),
    }))
}

pub async fn configure(State(state): State<Arc<AppState>>, Json(p): Json<Value>) -> Response {
    let id = p.get("agent_id").and_then(|v| v.as_str()).unwrap_or("");
    let model = p.get("model").and_then(|v| v.as_str()).unwrap_or("");
    if id.is_empty() || model.is_empty() {
        return bad("缺少 agent_id 或 model");
    }
    let models = model_ids(&state);
    if !models.iter().any(|m| m == model) {
        return bad("所选模型不在当前路由的 /v1/models 里");
    }
    let agents = scan_agents(&state);
    let Some(agent) = agents.iter().find(|a| a.id == id && a.configurable) else {
        return bad("找不到这个可配置的 Agent");
    };
    let api_key = p
        .get("api_key")
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty())
        .unwrap_or("llm-router")
        .to_string();
    let base_input = p
        .get("base_url")
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty())
        .unwrap_or("")
        .to_string();
    let base_input = if base_input.is_empty() {
        default_base_url(&state)
    } else {
        base_input
    };
    let openai = openai_base(&base_input);
    let anthropic = anthropic_base(&base_input);
    let dry_run = p.get("dry_run").and_then(|v| v.as_bool()).unwrap_or(false);
    let pin_default = p
        .get("pin_default")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let plan = match write_plan(
        &agent,
        model,
        &api_key,
        &openai,
        &anthropic,
        &models,
        pin_default,
    ) {
        Ok(v) => v,
        Err(e) => return bad(&e),
    };
    let diffs = plan_diff(&plan);
    let mut backups: Vec<String> = Vec::new();
    let mut applied: Vec<(PathBuf, PathBuf)> = Vec::new();
    let mut previews: Vec<String> = Vec::new();
    for (path, content) in &plan.files {
        match persist(path, content, dry_run) {
            Ok((Some(b), _)) => {
                backups.push(b.clone());
                applied.push((path.clone(), PathBuf::from(b)));
            }
            Ok((None, Some(p))) => {
                previews.push(format!("===== {} =====\n{}", path.display(), p));
            }
            Ok((None, None)) => {}
            Err(e) => {
                let rolled = rollback_applied(&applied);
                return bad_with(json!({
                    "error": e,
                    "transaction": {
                        "id": format!("txn-{}", chrono::Utc::now().timestamp_millis()),
                        "steps": [
                            {"name": "plan", "ok": true, "files": plan.files.len()},
                            {"name": "diff", "ok": true, "files": diffs},
                            {"name": "apply", "ok": false, "error": e},
                            {"name": "rollback", "ok": true, "performed": true, "restored": rolled}
                        ]
                    }
                }));
            }
        }
    }
    let (verified, checks) = if dry_run { (true, Vec::new()) } else { verify_plan(&plan) };
    if !dry_run && !verified {
        let rolled = rollback_applied(&applied);
        return bad_with(json!({
            "error": "写入后校验失败，已自动回滚",
            "transaction": {
                "id": format!("txn-{}", chrono::Utc::now().timestamp_millis()),
                "steps": [
                    {"name": "plan", "ok": true, "files": plan.files.len()},
                    {"name": "diff", "ok": true, "files": diffs},
                    {"name": "apply", "ok": true, "files": applied.len()},
                    {"name": "verify", "ok": false, "checks": checks},
                    {"name": "rollback", "ok": true, "performed": true, "restored": rolled}
                ]
            }
        }));
    }
    // 占位 Key 不是秘密，只有用户自己填了 Key 才做脱敏，避免把 provider 名也打码。
    let preview = if previews.is_empty() {
        None
    } else {
        let joined = previews.join("\n\n");
        Some(if api_key == "llm-router" {
            joined
        } else {
            joined.replace(&api_key, "***")
        })
    };
    if !dry_run {
        let hits: usize = plan
            .files
            .iter()
            .map(|(_, content)| content.matches(model).count())
            .sum();
        let rec = json!({
            "model": model,
            "base_url": openai,
            "anthropic_base_url": anthropic,
            "api_key": api_key,
            "config_paths": agent.config_paths,
            "model_hits": hits,
            "pin_default": pin_default,
            "applied_at": chrono::Utc::now().to_rfc3339(),
        });
        let mut all = load_applied(&state);
        if let Some(m) = all.as_object_mut() {
            m.insert(agent.id.clone(), rec);
        } else {
            all = json!({ agent.id.clone(): rec });
        }
        if let Err(e) = save_applied(&state, &all) {
            let rolled = rollback_applied(&applied);
            return bad_with(json!({"error": e, "transaction": {"steps": [
                {"name": "plan", "ok": true},
                {"name": "diff", "ok": true, "files": diffs},
                {"name": "apply", "ok": true, "files": applied.len()},
                {"name": "verify", "ok": true, "checks": checks},
                {"name": "record", "ok": false, "error": e},
                {"name": "rollback", "ok": true, "performed": true, "restored": rolled}
            ]}}));
        }
    }
    let transaction = json!({
        "id": format!("txn-{}", chrono::Utc::now().timestamp_millis()),
        "mode": if dry_run { "plan" } else { "apply" },
        "steps": [
            {"name": "plan", "ok": true, "files": plan.files.len(), "keys": plan.changed_keys},
            {"name": "diff", "ok": true, "files": diffs},
            {"name": "apply", "ok": !dry_run, "skipped": dry_run, "files": applied.len()},
            {"name": "verify", "ok": verified, "skipped": dry_run, "checks": checks},
            {"name": "rollback", "ok": true, "performed": false}
        ],
        "diff": diffs,
        "verified": verified
    });
    ok(json!({
        "ok": true,
        "agent_id": agent.id,
        "name": agent.name,
        "model": model,
        "base_url": openai,
        "anthropic_base_url": anthropic,
        "config_path": agent.config_path,
        "config_paths": agent.config_paths,
        "backup_path": backups.first().cloned(),
        "backup_paths": backups,
        "changed_keys": plan.changed_keys,
        "dry_run": dry_run,
        "pin_default": pin_default,
        "pinned_default": pin_default && id == "dsh-tui" && !dsh_patch_paths().is_empty(),
        "preview": preview,
        "transaction": transaction,
    }))
}

/// 一键修回：用上次成功导入的参数重新写入（被别的工具改走时点这个）。
pub async fn repair(State(state): State<Arc<AppState>>, Json(p): Json<Value>) -> Response {
    let id = p.get("agent_id").and_then(|v| v.as_str()).unwrap_or("");
    if id.is_empty() {
        return bad("缺少 agent_id");
    }
    let applied = load_applied(&state);
    let Some(rec) = applied.get(id) else {
        return bad("没有找到这个 Agent 的导入记录，请先一键导入");
    };
    let model = rec.get("model").and_then(|v| v.as_str()).unwrap_or("");
    if model.is_empty() || !model_ids(&state).iter().any(|m| m == model) {
        return bad("记录里的模型已不在当前路由的 /v1/models 里，请重新选模型导入");
    }
    let req = json!({
        "agent_id": id,
        "model": model,
        "api_key": rec.get("api_key").and_then(|v| v.as_str()).unwrap_or("llm-router"),
        "base_url": rec.get("base_url").and_then(|v| v.as_str()).unwrap_or(""),
        "pin_default": rec.get("pin_default").and_then(|v| v.as_bool()).unwrap_or(false),
        "dry_run": p.get("dry_run").and_then(|v| v.as_bool()).unwrap_or(false),
    });
    configure(State(state), Json(req)).await
}

pub async fn restore(State(state): State<Arc<AppState>>, Json(p): Json<Value>) -> Response {
    let id = p.get("agent_id").and_then(|v| v.as_str()).unwrap_or("");
    let agents = scan_agents(&state);
    let Some(agent) = agents.iter().find(|a| a.id == id) else {
        return bad("找不到这个 Agent");
    };
    let paths: Vec<PathBuf> = if agent.config_paths.is_empty() {
        vec![PathBuf::from(&agent.config_path)]
    } else {
        agent.config_paths.iter().map(PathBuf::from).collect()
    };
    let mut restored: Vec<String> = Vec::new();
    let mut first_backup = String::new();
    for path in &paths {
        let backup = backup_path_for(path);
        if !backup.exists() {
            continue;
        }
        let created = std::fs::read_to_string(&backup)
            .map(|s| s.trim() == NEW_FILE_MARKER)
            .unwrap_or(false);
        let res = if created {
            if path.exists() {
                std::fs::remove_file(path)
            } else {
                Ok(())
            }
        } else {
            std::fs::copy(&backup, path).map(|_| ())
        };
        if let Err(e) = res {
            return bad(&format!("恢复失败: {e}"));
        }
        // 恢复成功后备份已完成使命（文件已回到原样），顺手清掉，避免残留一堆 .bak。
        let _ = std::fs::remove_file(&backup);
        if first_backup.is_empty() {
            first_backup = backup.to_string_lossy().to_string();
        }
        restored.push(path.to_string_lossy().to_string());
    }
    if restored.is_empty() {
        return bad("没有找到 .llmrouter.bak 备份，无法恢复");
    }
    let mut all = load_applied(&state);
    if let Some(m) = all.as_object_mut() {
        m.remove(id);
        let _ = save_applied(&state, &all);
    }
    ok(json!({
        "ok": true,
        "agent_id": id,
        "config_path": agent.config_path,
        "restored_from": first_backup,
        "restored_files": restored,
    }))
}
