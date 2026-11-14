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
        "@charmland/crush" => ("crush", "Crush (Charm)", ".config/crush/config.json"),
        "@factory/cli-win32-x64" | "@factory/cli" => ("droid", "Droid (Factory)", ".factory/config.json"),
        "@github/copilot" => ("copilot-cli", "GitHub Copilot CLI", ".copilot/config.json"),
        "@kilocode/cli" => ("kilo-code", "Kilo Code", ".config/kilo/config.json"),
        "@continuedev/cli" => ("continue-cli", "Continue CLI", ".continue/config.json"),
        "@tencent-ai/codebuddy-code" => ("codebuddy", "CodeBuddy Code", ".codebuddy/settings.json"),
        "cline" => ("cline-cli", "Cline CLI", ".cline/config.json"),
        "codebuff" => ("codebuff", "Codebuff", ".codebuff/config.json"),
        "command-code" => ("command-code", "Command Code", ".commandcode/settings.json"),
        "dsh-tui" => ("dsh-tui", "DSH TUI", ".config/dsh-tui/config.json"),
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
        ("workbuddy", "WorkBuddy", &["workbuddy"], ".workbuddy/config.json"),
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
    out
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
    if path.exists() && !backup.exists() {
        std::fs::copy(path, &backup).map_err(|e| format!("备份原配置失败: {e}"))?;
    }
    std::fs::write(path, content).map_err(|e| format!("写入配置失败: {e}"))?;
    Ok((Some(backup.to_string_lossy().to_string()), None))
}

/// VS Code 的 settings.json 常带 // 注释或尾逗号，先做一遍保留字符串字面量的清洗。
fn strip_jsonc(text: &str) -> String {
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

    let plan = match write_plan(&agent, model, &api_key, &openai, &anthropic) {
        Ok(v) => v,
        Err(e) => return bad(&e),
    };
    let mut backups: Vec<String> = Vec::new();
    let mut previews: Vec<String> = Vec::new();
    for (path, content) in &plan.files {
        let (backup, preview_raw) = match persist(path, content, dry_run) {
            Ok(v) => v,
            Err(e) => return bad(&e),
        };
        if let Some(b) = backup {
            backups.push(b);
        }
        if let Some(p) = preview_raw {
            previews.push(format!("===== {} =====\n{}", path.display(), p));
        }
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
        "preview": preview,
    }))
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
        if let Err(e) = std::fs::copy(&backup, path) {
            return bad(&format!("恢复失败: {e}"));
        }
        if first_backup.is_empty() {
            first_backup = backup.to_string_lossy().to_string();
        }
        restored.push(path.to_string_lossy().to_string());
    }
    if restored.is_empty() {
        return bad("没有找到 .llmrouter.bak 备份，无法恢复");
    }
    ok(json!({
        "ok": true,
        "agent_id": id,
        "config_path": agent.config_path,
        "restored_from": first_backup,
        "restored_files": restored,
    }))
}
