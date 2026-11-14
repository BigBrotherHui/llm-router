//! 勾选式导入 / 导出：请求记录、上游来源、模型路由、签到。
//!
//! 四类数据的落点：
//!   journal   -> .llmrouter/config.history.jsonl（+ config.stats.json）
//!   upstreams -> config.state.json 的 upstreams
//!   models    -> config.state.json 的 models / suppressed_models
//!   checkin   -> config.state.json 的 checkin_settings + 每个 upstream.extra 的
//!                checkin / checkin_state，以及 config.checkin.json 的签到流水
//!
//! 导出包里含明文 API Key / 签到 token，`mask_secrets` 可选打码。
//! 导入前对每个要改的文件留一份 `<原名>.bak-import-<时间戳>` 备份。

use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Map, Value};

use super::state::{AppState, ModelRoute, Upstream, VERSION};

const FORMAT: &str = "llm-router-backup";
const FORMAT_VERSION: i64 = 1;
const LEGACY_SCHEMA: &str = "llm-router-migrate/1";
const ALL_PARTS: [&str; 4] = ["journal", "upstreams", "models", "checkin"];

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

fn wanted_parts(p: Option<&Value>, data: Option<&Value>) -> Vec<String> {
    if let Some(arr) = p.and_then(|v| v.as_array()) {
        let picked: Vec<String> = arr
            .iter()
            .filter_map(|v| v.as_str())
            .filter(|s| ALL_PARTS.contains(s))
            .map(|s| s.to_string())
            .collect();
        return picked;
    }
    // 没指定就按包里实际有的部分来（导出默认全勾）。
    if let Some(d) = data.and_then(|v| v.as_object()) {
        let present: Vec<String> = ALL_PARTS
            .iter()
            .filter(|k| d.contains_key(**k))
            .map(|k| k.to_string())
            .collect();
        if !present.is_empty() {
            return present;
        }
    }
    ALL_PARTS.iter().map(|s| s.to_string()).collect()
}

fn as_strings(v: Option<&Value>) -> Vec<String> {
    v.and_then(|x| x.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default()
}

fn read_json(path: &Path) -> Value {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .unwrap_or_else(|| json!({}))
}

fn read_journal(path: &Path, limit: usize) -> Vec<Value> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return vec![];
    };
    let mut out: Vec<Value> = text
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l.trim()).ok())
        .collect();
    if limit > 0 && out.len() > limit {
        out = out[out.len() - limit..].to_vec();
    }
    out
}

fn mask_upstream(v: &mut Value) {
    if let Some(obj) = v.as_object_mut() {
        if let Some(keys) = obj.get_mut("api_keys").and_then(|x| x.as_array_mut()) {
            for k in keys.iter_mut() {
                if let Some(s) = k.as_str() {
                    *k = json!(super::state::mask_key(s));
                }
            }
        }
        if let Some(token) = obj
            .get_mut("checkin")
            .and_then(|x| x.as_object_mut())
            .and_then(|m| m.get_mut("token"))
        {
            if let Some(s) = token.as_str() {
                *token = json!(super::state::mask_key(s));
            }
        }
    }
}

fn backup_file(path: &Path, stamp: &str) -> Result<Option<String>, String> {
    if !path.exists() {
        return Ok(None);
    }
    let base = path.to_string_lossy();
    let mut target = PathBuf::from(format!("{base}.bak-import-{stamp}"));
    let mut seq = 1;
    while target.exists() {
        target = PathBuf::from(format!("{base}.bak-import-{stamp}-{seq}"));
        seq += 1;
    }
    std::fs::copy(path, &target)
        .map_err(|e| format!("备份 {} 失败: {e}", path.display()))?;
    Ok(Some(target.to_string_lossy().to_string()))
}

fn write_json(path: &Path, v: &Value) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let text = serde_json::to_string_pretty(v).map_err(|e| e.to_string())?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, text).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, path).map_err(|e| e.to_string())?;
    Ok(())
}

/// 把旧 `migrate.py` 的 bundle.json 归一成新包形状。
fn normalize(payload: &Value) -> Result<Value, String> {
    let fmt = payload.get("format").and_then(|v| v.as_str());
    if fmt == Some(FORMAT) {
        return Ok(payload.clone());
    }
    if payload.get("schema").and_then(|v| v.as_str()) == Some(LEGACY_SCHEMA) {
        let st = payload.get("state").cloned().unwrap_or_else(|| json!({}));
        let mut data = Map::new();
        let ups = st
            .get("upstreams")
            .cloned()
            .filter(|v| v.is_array())
            .or_else(|| payload.get("upstreams").cloned())
            .unwrap_or_else(|| json!([]));
        data.insert("upstreams".into(), ups);
        if let Some(v) = st.get("models") {
            data.insert("models".into(), v.clone());
        }
        if let Some(v) = st.get("suppressed_models") {
            data.insert("suppressed_models".into(), v.clone());
        }
        let log = payload
            .get("checkin_log")
            .cloned()
            .filter(|v| v.is_object())
            .unwrap_or_else(|| json!({}));
        let mut ck = Map::new();
        ck.insert("settings".into(), st.get("checkin_settings").cloned().unwrap_or_else(|| json!({})));
        ck.insert("log".into(), log);
        data.insert("checkin".into(), Value::Object(ck));
        return Ok(json!({
            "format": FORMAT,
            "format_version": 1,
            "legacy": true,
            "data": Value::Object(data),
        }));
    }
    Err(format!(
        "不是 llm-router 备份文件（format={:?}），请选导出的 JSON 或旧版 bundle.json",
        fmt
    ))
}

/// 当前四类数据各有多少条，供设置页显示。
pub async fn summary(State(state): State<Arc<AppState>>) -> Response {
    let file = state.file.read().unwrap();
    let checkin = read_json(&state.checkin_path());
    let entries = checkin
        .get("entries")
        .and_then(|v| v.as_array())
        .map(|a| a.len())
        .unwrap_or(0);
    // tail_cached 只看 8MB 尾巴，统计条数必须按整个文件数行，不能用 index().total。
    let journal = std::fs::read(state.history_path())
        .map(|b| {
            if b.is_empty() {
                0u64
            } else {
                let n = b.iter().filter(|&&c| c == b'\n').count() as u64;
                n + u64::from(b.last() != Some(&b'\n'))
            }
        })
        .unwrap_or(0);
    ok(json!({
        "ok": true,
        "counts": {
            "journal": journal,
            "upstreams": file.upstreams.len(),
            "models": file.models.len(),
            "checkin_entries": entries,
        },
        "paths": {
            "state": state.state_path.to_string_lossy(),
            "history": state.history_path().to_string_lossy(),
            "stats": state.stats_path().to_string_lossy(),
            "checkin": state.checkin_path().to_string_lossy(),
        },
    }))
}

/// POST /data/export  { parts?, journal_limit?, mask_secrets? }
pub async fn export_data(State(state): State<Arc<AppState>>, Json(p): Json<Value>) -> Response {
    let parts = wanted_parts(p.get("parts"), None);
    if parts.is_empty() {
        return bad("至少勾选一项再导出");
    }
    let limit = p
        .get("journal_limit")
        .and_then(|v| v.as_u64())
        .unwrap_or(0) as usize;
    let mask = p
        .get("mask_secrets")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let mut data = Map::new();
    let mut counts = Map::new();

    if parts.iter().any(|x| x == "journal") {
        let entries = read_journal(&state.history_path(), limit);
        counts.insert("journal".into(), json!(entries.len()));
        data.insert("journal".into(), json!(entries));
        data.insert("stats".into(), super::stats::load_runtime(&state));
    }
    if parts.iter().any(|x| x == "upstreams") {
        let file = state.file.read().unwrap();
        let mut ups: Vec<Value> = file
            .upstreams
            .iter()
            .map(|u| serde_json::to_value(u).unwrap_or_else(|_| json!({})))
            .collect();
        if mask {
            for u in ups.iter_mut() {
                mask_upstream(u);
            }
        }
        counts.insert("upstreams".into(), json!(ups.len()));
        data.insert("upstreams".into(), json!(ups));
    }
    if parts.iter().any(|x| x == "models") {
        let file = state.file.read().unwrap();
        let models = serde_json::to_value(&file.models).unwrap_or_else(|_| json!({}));
        counts.insert(
            "models".into(),
            json!(models.as_object().map(|m| m.len()).unwrap_or(0)),
        );
        data.insert("models".into(), models);
        data.insert(
            "suppressed_models".into(),
            json!(file.suppressed_models),
        );
    }
    if parts.iter().any(|x| x == "checkin") {
        let file = state.file.read().unwrap();
        let mut per = Map::new();
        for up in &file.upstreams {
            let mut cfg = Map::new();
            if let Some(v) = up.extra.get("checkin") {
                cfg.insert("checkin".into(), v.clone());
            }
            if let Some(v) = up.extra.get("checkin_state") {
                cfg.insert("checkin_state".into(), v.clone());
            }
            if !cfg.is_empty() {
                let mut item = Value::Object(cfg);
                if mask {
                    mask_upstream(&mut item);
                }
                per.insert(up.id.clone(), item);
            }
        }
        let log = read_json(&state.checkin_path());
        let entries = log
            .get("entries")
            .and_then(|v| v.as_array())
            .map(|a| a.len())
            .unwrap_or(0);
        counts.insert("checkin_entries".into(), json!(entries));
        data.insert(
            "checkin".into(),
            json!({
                "settings": file.checkin_settings,
                "per_upstream": Value::Object(per),
                "log": log,
            }),
        );
    }

    ok(json!({
        "format": FORMAT,
        "format_version": FORMAT_VERSION,
        "app_version": VERSION,
        "exported_at": chrono::Local::now().to_rfc3339(),
        "parts": parts,
        "mask_secrets": mask,
        "counts": Value::Object(counts),
        "data": Value::Object(data),
    }))
}

/// POST /data/import { payload | <备份对象本身>, parts?, mode?, dry_run? }
pub async fn import_data(State(state): State<Arc<AppState>>, Json(p): Json<Value>) -> Response {
    let payload = p.get("payload").cloned().unwrap_or_else(|| p.clone());
    let norm = match normalize(&payload) {
        Ok(v) => v,
        Err(e) => return bad(&e),
    };
    let data = match norm.get("data") {
        Some(v) if v.is_object() => v.clone(),
        _ => return bad("备份文件缺少 data 段"),
    };
    // 只导入备份里实际存在的部分：包里只有上游时，勾选了「请求记录」也不能
    // 让 replace 模式把 10MB 流水清空。
    let parts: Vec<String> = wanted_parts(p.get("parts"), Some(&data))
        .into_iter()
        .filter(|k| data.get(k.as_str()).is_some())
        .collect();
    if parts.is_empty() {
        return bad("备份文件里没有勾选的这些部分");
    }
    let mode = p.get("mode").and_then(|v| v.as_str()).unwrap_or("merge");
    if mode != "merge" && mode != "replace" {
        return bad("mode 只能是 merge 或 replace");
    }
    let dry_run = p
        .get("dry_run")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let journal_entries = data
        .get("journal")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let incoming_ups: Vec<Upstream> = if parts.iter().any(|x| x == "upstreams") {
        match serde_json::from_value(
            data.get("upstreams").cloned().unwrap_or_else(|| json!([])),
        ) {
            Ok(v) => v,
            Err(e) => return bad(&format!("上游来源解析失败: {e}")),
        }
    } else {
        vec![]
    };
    let incoming_models: HashMap<String, ModelRoute> =
        if parts.iter().any(|x| x == "models") {
            match serde_json::from_value(
                data.get("models").cloned().unwrap_or_else(|| json!({})),
            ) {
                Ok(v) => v,
                Err(e) => return bad(&format!("模型路由解析失败: {e}")),
            }
        } else {
            HashMap::new()
        };
    let ck = data.get("checkin").cloned().unwrap_or_else(|| json!({}));

    let mut applied = Map::new();
    if parts.iter().any(|x| x == "journal") {
        applied.insert("journal".into(), json!(journal_entries.len()));
        applied.insert(
            "stats".into(),
            json!(data.get("stats").map(|v| v.is_object()).unwrap_or(false)),
        );
    }
    if parts.iter().any(|x| x == "upstreams") {
        applied.insert("upstreams".into(), json!(incoming_ups.len()));
    }
    if parts.iter().any(|x| x == "models") {
        applied.insert("models".into(), json!(incoming_models.len()));
    }
    if parts.iter().any(|x| x == "checkin") {
        applied.insert(
            "checkin_settings".into(),
            json!(ck.get("settings").is_some()),
        );
        applied.insert(
            "checkin_upstreams".into(),
            json!(ck
                .get("per_upstream")
                .and_then(|v| v.as_object())
                .map(|m| m.len())
                .unwrap_or(0)),
        );
        let n = ck
            .get("log")
            .and_then(|v| v.get("entries"))
            .and_then(|v| v.as_array())
            .map(|a| a.len())
            .unwrap_or(0);
        applied.insert("checkin_entries".into(), json!(n));
    }

    // dry_run：只回报将要做的事，不碰任何文件。
    if dry_run {
        return ok(json!({
            "ok": true,
            "dry_run": true,
            "mode": mode,
            "parts": parts,
            "applied": Value::Object(applied),
            "backups": [],
            "format": norm.get("format").cloned().unwrap_or_else(|| json!(FORMAT)),
            "legacy": norm.get("legacy").and_then(|v| v.as_bool()).unwrap_or(false),
        }));
    }

    let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S").to_string();
    let mut backups: Vec<String> = vec![];
    let backup_one = |path: &Path, out: &mut Vec<String>| -> Result<(), String> {
        if let Some(s) = backup_file(path, &stamp)? {
            out.push(s);
        }
        Ok(())
    };
    let mut do_backup = || -> Result<(), String> {
        if parts.iter().any(|x| x == "upstreams" || x == "models" || x == "checkin") {
            backup_one(&state.state_path, &mut backups)?;
        }
        if parts.iter().any(|x| x == "journal") {
            backup_one(&state.history_path(), &mut backups)?;
            if data.get("stats").map(|v| v.is_object()).unwrap_or(false) {
                backup_one(&state.stats_path(), &mut backups)?;
            }
        }
        if parts.iter().any(|x| x == "checkin")
            && ck.get("log").and_then(|v| v.as_object()).is_some()
        {
            backup_one(&state.checkin_path(), &mut backups)?;
        }
        Ok(())
    };
    if let Err(e) = do_backup() {
        return bad(&e);
    }

    // 1) 请求记录：流水 + 统计快照。
    if parts.iter().any(|x| x == "journal") {
        let hist = state.history_path();
        let lines: Result<Vec<String>, String> = journal_entries
            .iter()
            .map(|e| serde_json::to_string(e).map_err(|x| x.to_string()))
            .collect();
        let lines = match lines {
            Ok(v) => v,
            Err(e) => return bad(&format!("请求记录序列化失败: {e}")),
        };
        let written = if mode == "replace" {
            let mut text = lines.join("\n");
            if !text.is_empty() {
                text.push('\n');
            }
            std::fs::write(&hist, text).map(|_| ()).map_err(|e| e.to_string())
        } else {
            match std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&hist)
            {
                Ok(mut f) => lines
                    .iter()
                    .try_for_each(|l| writeln!(f, "{l}"))
                    .map_err(|e| e.to_string()),
                Err(e) => Err(e.to_string()),
            }
        };
        if let Err(e) = written {
            return bad(&format!("写入请求流水失败: {e}"));
        }
        if let Ok(mut cache) = state.journal_cache.lock() {
            *cache = None;
        }
        if let Some(s) = data.get("stats").filter(|v| v.is_object()) {
            if let Err(e) = write_json(&state.stats_path(), s) {
                return bad(&format!("写入统计快照失败: {e}"));
            }
            if let Ok(mut mem) = state.stats_mem.lock() {
                *mem = Some(s.clone());
            }
            state.stats_dirty.store(false, Ordering::Relaxed);
        }
    }

    // 2) 配置类改动统一在写锁里做，锁外再 save（save 内部要拿读锁）。
    let mut touched_state = false;
    {
        let mut file = state.file.write().unwrap();
        if parts.iter().any(|x| x == "upstreams") {
            if mode == "replace" {
                file.upstreams = incoming_ups.clone();
            } else {
                for up in incoming_ups.iter() {
                    if let Some(existing) = file.find_upstream_mut(&up.id) {
                        *existing = up.clone();
                    } else {
                        file.upstreams.push(up.clone());
                    }
                }
            }
            touched_state = true;
        }
        if parts.iter().any(|x| x == "models") {
            if mode == "replace" {
                file.models = incoming_models.clone();
            } else {
                for (k, v) in incoming_models.iter() {
                    file.models.insert(k.clone(), v.clone());
                }
            }
            let suppressed = as_strings(data.get("suppressed_models"));
            if mode == "replace" {
                file.suppressed_models = suppressed;
            } else {
                let mut set: HashSet<String> = file.suppressed_models.iter().cloned().collect();
                set.extend(suppressed);
                file.suppressed_models = set.into_iter().collect();
            }
            touched_state = true;
        }
        if parts.iter().any(|x| x == "checkin") {
            if let Some(s) = ck.get("settings") {
                file.checkin_settings = s.clone();
                touched_state = true;
            }
            if let Some(map) = ck.get("per_upstream").and_then(|v| v.as_object()) {
                for (id, cfg) in map {
                    if let Some(up) = file.find_upstream_mut(id) {
                        if let Some(v) = cfg.get("checkin") {
                            up.extra.insert("checkin".into(), v.clone());
                        }
                        if let Some(v) = cfg.get("checkin_state") {
                            up.extra.insert("checkin_state".into(), v.clone());
                        }
                        touched_state = true;
                    }
                }
            }
        }
    }
    if touched_state {
        if let Err(e) = state.save() {
            return bad(&format!("保存配置失败: {e}"));
        }
        state.bump_catalog();
    }

    // 3) 签到流水：merge 时与现有合并去重，replace 时整体替换。
    if parts.iter().any(|x| x == "checkin") {
        if let Some(log) = ck.get("log").filter(|v| v.is_object()) {
            let mut entries: Vec<Value> = if mode == "merge" {
                read_json(&state.checkin_path())
                    .get("entries")
                    .and_then(|v| v.as_array())
                    .cloned()
                    .unwrap_or_default()
            } else {
                vec![]
            };
            if let Some(add) = log.get("entries").and_then(|v| v.as_array()) {
                entries.extend(add.iter().cloned());
            }
            let mut seen: HashSet<String> = HashSet::new();
            entries.retain(|e| seen.insert(serde_json::to_string(e).unwrap_or_default()));
            entries.sort_by(|a, b| {
                a.get("at")
                    .and_then(|v| v.as_f64())
                    .unwrap_or(0.0)
                    .partial_cmp(&b.get("at").and_then(|v| v.as_f64()).unwrap_or(0.0))
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            if entries.len() > 2000 {
                entries = entries[entries.len() - 2000..].to_vec();
            }
            let mut out = log.clone();
            out["entries"] = json!(entries);
            applied.insert("checkin_entries".into(), json!(entries.len()));
            if let Err(e) = write_json(&state.checkin_path(), &out) {
                return bad(&format!("写入签到记录失败: {e}"));
            }
        }
    }

    let _ = state.events_tx.send(json!({"type": "state", "reason": "import"}));
    ok(json!({
        "ok": true,
        "dry_run": false,
        "mode": mode,
        "parts": parts,
        "applied": Value::Object(applied),
        "backups": backups,
        "state_path": state.state_path.to_string_lossy(),
    }))
}
