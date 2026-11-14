//! 请求流水的 SQLite 索引层。
//!
//! JSONL 仍是原始事实（带完整请求/响应文本，便于 grep、导入导出、灾备）；
//! 这里只保存可聚合的标量字段和 `raw` 副本，所有图表/健康/会话对账查询都
//! 走索引，避免每次扫描 8MB 尾部 JSON。索引落后时按文件 offset 增量补齐。

use std::collections::hash_map::DefaultHasher;
use std::fs::File;
use std::hash::{Hash, Hasher};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};

use super::state::AppState;

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS journal (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    at REAL NOT NULL,
    time TEXT,
    model TEXT,
    effective_model TEXT,
    upstream TEXT,
    status INTEGER,
    ok INTEGER,
    stream INTEGER,
    elapsed REAL,
    first REAL,
    input INTEGER,
    output INTEGER,
    cache_read INTEGER,
    cache_write INTEGER,
    total INTEGER,
    raw TEXT NOT NULL,
    fp TEXT NOT NULL UNIQUE
);
CREATE INDEX IF NOT EXISTS idx_journal_at ON journal(at);
CREATE INDEX IF NOT EXISTS idx_journal_model ON journal(model);
CREATE INDEX IF NOT EXISTS idx_journal_upstream ON journal(upstream);
CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
"#;

pub fn db_path_for(history: &Path) -> PathBuf {
    let name = history
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "config.history.jsonl".to_string());
    history.with_file_name(name.replace(".history.jsonl", ".history.sqlite3"))
}

pub fn db_path(state: &AppState) -> PathBuf {
    db_path_for(&state.history_path())
}

fn open(path: &Path) -> Result<Connection, String> {
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let conn = Connection::open(path).map_err(|e| format!("打开 SQLite 索引失败: {e}"))?;
    conn.busy_timeout(Duration::from_secs(3)).map_err(|e| e.to_string())?;
    conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;")
        .map_err(|e| e.to_string())?;
    conn.execute_batch(SCHEMA).map_err(|e| format!("初始化 SQLite 索引失败: {e}"))?;
    Ok(conn)
}

fn s(v: &Value, key: &str) -> String {
    v.get(key).and_then(|x| x.as_str()).unwrap_or("").to_string()
}

fn f(v: &Value, key: &str) -> f64 {
    v.get(key).and_then(|x| x.as_f64()).unwrap_or(0.0)
}

fn u(v: &Value, key: &str) -> i64 {
    v.get(key).and_then(|x| x.as_i64()).unwrap_or(0)
}

fn usage_u(entry: &Value, key: &str) -> i64 {
    entry
        .get("usage")
        .and_then(|x| x.get(key))
        .and_then(|x| x.as_i64())
        .unwrap_or(0)
}

fn fp(entry: &Value) -> String {
    let text = serde_json::to_string(entry).unwrap_or_default();
    let mut h = DefaultHasher::new();
    text.hash(&mut h);
    format!("{:016x}", h.finish())
}

fn insert(conn: &Connection, entry: &Value) -> Result<bool, String> {
    let raw = serde_json::to_string(entry).map_err(|e| e.to_string())?;
    let changed = conn
        .execute(
            r#"INSERT OR IGNORE INTO journal
               (at,time,model,effective_model,upstream,status,ok,stream,elapsed,first,
                input,output,cache_read,cache_write,total,raw,fp)
               VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17)"#,
            params![
                f(entry, "at"),
                s(entry, "time"),
                s(entry, "model"),
                s(entry, "effective_model"),
                s(entry, "upstream"),
                u(entry, "status"),
                if entry.get("ok").and_then(|x| x.as_bool()).unwrap_or(false) { 1 } else { 0 },
                if entry.get("stream").and_then(|x| x.as_bool()).unwrap_or(false) { 1 } else { 0 },
                f(entry, "elapsed_seconds"),
                f(entry, "first_chunk_seconds"),
                usage_u(entry, "input"),
                usage_u(entry, "output"),
                usage_u(entry, "cache_read"),
                usage_u(entry, "cache_write"),
                usage_u(entry, "total"),
                raw,
                fp(entry),
            ],
        )
        .map_err(|e| format!("写入 SQLite 索引失败: {e}"))?;
    Ok(changed > 0)
}

fn meta_get(conn: &Connection, key: &str) -> Option<String> {
    conn.query_row("SELECT value FROM meta WHERE key=?1", [key], |r| r.get(0))
        .optional()
        .ok()
        .flatten()
}

fn meta_set(conn: &Connection, key: &str, value: &str) -> Result<(), String> {
    conn.execute(
        "INSERT INTO meta(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
        params![key, value],
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}

/// 把 JSONL 中尚未入索引的行追加进 SQLite。返回新增行数。
pub fn sync(state: &AppState) -> Result<usize, String> {
    sync_path(&state.history_path())
}

pub fn sync_path(path: &Path) -> Result<usize, String> {
    let db = db_path_for(path);
    let mut conn = open(&db)?;
    let Ok(meta) = std::fs::metadata(path) else {
        return Ok(0);
    };
    let mut offset: u64 = meta_get(&conn, "last_offset")
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    if offset > meta.len() {
        // 日志轮转过：重建索引。
        conn.execute("DELETE FROM journal", []).map_err(|e| e.to_string())?;
        offset = 0;
    }
    if offset == meta.len() {
        return Ok(0);
    }
    let mut file = File::open(path).map_err(|e| e.to_string())?;
    file.seek(SeekFrom::Start(offset)).map_err(|e| e.to_string())?;
    let mut buf = String::new();
    file.read_to_string(&mut buf).map_err(|e| e.to_string())?;
    let end = meta.len();
    let tx = conn.transaction().map_err(|e| e.to_string())?;
    let mut added = 0usize;
    for line in buf.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Ok(v) = serde_json::from_str::<Value>(line) {
            if insert(&tx, &v)? {
                added += 1;
            }
        }
    }
    meta_set(&tx, "last_offset", &end.to_string())?;
    tx.commit().map_err(|e| e.to_string())?;
    Ok(added)
}

/// 读取 since 起的索引行（默认按时间升序）。
pub fn entries(state: &AppState, since: f64, limit: Option<usize>) -> Vec<Value> {
    let _ = sync(state);
    let db = db_path(state);
    let Ok(conn) = open(&db) else {
        return Vec::new();
    };
    let cap = limit.unwrap_or(100_000).min(200_000) as i64;
    let mut stmt = match conn.prepare("SELECT raw FROM journal WHERE at >= ?1 ORDER BY at ASC LIMIT ?2") {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    let rows = stmt.query_map(params![since, cap], |r| r.get::<_, String>(0));
    let Ok(rows) = rows else {
        return Vec::new();
    };
    rows.flatten()
        .filter_map(|raw| serde_json::from_str::<Value>(&raw).ok())
        .collect()
}

/// 索引自身状态，供 /journal/db/status 和设置页展示。
pub fn status(state: &AppState) -> Value {
    let path = db_path(state);
    let history = state.history_path();
    let jsonl_bytes = std::fs::metadata(&history).map(|m| m.len()).unwrap_or(0);
    let mut rows = 0i64;
    let mut bytes = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0) as i64;
    let mut last_offset = 0i64;
    if let Ok(conn) = open(&path) {
        rows = conn.query_row("SELECT COUNT(*) FROM journal", [], |r| r.get(0)).unwrap_or(0);
        last_offset = meta_get(&conn, "last_offset").and_then(|s| s.parse().ok()).unwrap_or(0);
        bytes = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0) as i64;
    }
    json!({
        "ok": true,
        "db_path": path.to_string_lossy(),
        "jsonl_path": history.to_string_lossy(),
        "jsonl_bytes": jsonl_bytes,
        "db_bytes": bytes,
        "rows": rows,
        "last_offset": last_offset,
        "lag_bytes": (jsonl_bytes as i64 - last_offset).max(0),
    })
}
