//! 更新通道：检查 GitHub Releases，区分 stable / beta，并提供安装包下载地址。
//! 这里不静默安装，只把“有新版本 + 下载按钮”送到设置页/托盘，避免后台替换正在运行的 exe。

use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Query, State};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};

use super::state::{AppState, VERSION};

const REPO: &str = "BigBrotherHui/llm-router";

#[derive(Deserialize, Default)]
pub struct UpdateQuery {
    pub channel: Option<String>,
}

fn ver_key(s: &str) -> Vec<u64> {
    s.trim()
        .trim_start_matches('v')
        .split(['-', '+'])
        .next()
        .unwrap_or("")
        .split('.')
        .map(|x| x.parse::<u64>().unwrap_or(0))
        .collect()
}

fn is_newer(latest: &str, current: &str) -> bool {
    let a = ver_key(latest);
    let b = ver_key(current);
    for i in 0..a.len().max(b.len()) {
        let x = a.get(i).copied().unwrap_or(0);
        let y = b.get(i).copied().unwrap_or(0);
        if x != y {
            return x > y;
        }
    }
    false
}

fn pick_release(list: &[Value], channel: &str) -> Option<Value> {
    let mut candidates: Vec<&Value> = list
        .iter()
        .filter(|r| !r.get("draft").and_then(|v| v.as_bool()).unwrap_or(false))
        .filter(|r| {
            let pre = r.get("prerelease").and_then(|v| v.as_bool()).unwrap_or(false);
            channel == "beta" || !pre
        })
        .collect();
    candidates.sort_by(|a, b| {
        let av = ver_key(a.get("tag_name").and_then(|v| v.as_str()).unwrap_or(""));
        let bv = ver_key(b.get("tag_name").and_then(|v| v.as_str()).unwrap_or(""));
        av.cmp(&bv)
    });
    candidates.last().map(|r| (*r).clone())
}

fn atom_release(text: &str, channel: &str) -> Option<Value> {
    let mut candidates: Vec<Value> = Vec::new();
    for chunk in text.split("<entry>").skip(1) {
        let body = chunk.split("</entry>").next().unwrap_or("");
        let title = body
            .split("<title>")
            .nth(1)
            .and_then(|x| x.split("</title>").next())
            .unwrap_or("")
            .trim()
            .to_string();
        if title.is_empty() {
            continue;
        }
        let pre = title.contains("-beta") || title.contains("-rc") || title.contains("-alpha");
        if channel != "beta" && pre {
            continue;
        }
        let link = body
            .split("rel=\"alternate\"")
            .nth(1)
            .and_then(|x| x.split("href=\"").nth(1))
            .and_then(|x| x.split('"').next())
            .unwrap_or("")
            .to_string();
        let updated = body
            .split("<updated>")
            .nth(1)
            .and_then(|x| x.split("</updated>").next())
            .unwrap_or("")
            .to_string();
        candidates.push(json!({
            "tag_name": title,
            "name": title,
            "html_url": link,
            "published_at": updated,
            "prerelease": pre,
            "assets": [],
        }));
    }
    candidates.sort_by(|a, b| {
        let av = ver_key(a.get("tag_name").and_then(|v| v.as_str()).unwrap_or(""));
        let bv = ver_key(b.get("tag_name").and_then(|v| v.as_str()).unwrap_or(""));
        av.cmp(&bv)
    });
    candidates.last().cloned()
}

async fn fetch(state: &AppState, channel: &str) -> Result<Value, String> {
    let url = format!("https://api.github.com/repos/{REPO}/releases?per_page=20");
    let resp = state
        .client
        .get(url)
        .header("Accept", "application/vnd.github+json")
        .header("User-Agent", "llm-router-updater")
        .timeout(Duration::from_secs(20))
        .send()
        .await;
    if let Ok(resp) = resp {
        if resp.status().is_success() {
            if let Ok(list) = resp.json::<Vec<Value>>().await {
                let Some(rel) = pick_release(&list, channel) else {
                    return Ok(json!({"ok": true, "current": VERSION, "channel": channel, "latest": null, "has_update": false, "releases": list.len()}));
                };
                let latest = rel.get("tag_name").and_then(|v| v.as_str()).unwrap_or("").to_string();
                let assets: Vec<Value> = rel
                    .get("assets")
                    .and_then(|v| v.as_array())
                    .map(|a| {
                        a.iter()
                            .map(|x| {
                                json!({
                                    "name": x.get("name").and_then(|v| v.as_str()).unwrap_or(""),
                                    "size": x.get("size").and_then(|v| v.as_u64()).unwrap_or(0),
                                    "url": x.get("browser_download_url").and_then(|v| v.as_str()).unwrap_or(""),
                                })
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                return Ok(json!({
                    "ok": true,
                    "current": VERSION,
                    "channel": channel,
                    "latest": latest,
                    "has_update": is_newer(&latest, VERSION),
                    "name": rel.get("name").and_then(|v| v.as_str()).unwrap_or(""),
                    "notes": rel.get("body").and_then(|v| v.as_str()).unwrap_or(""),
                    "published_at": rel.get("published_at").and_then(|v| v.as_str()).unwrap_or(""),
                    "html_url": rel.get("html_url").and_then(|v| v.as_str()).unwrap_or(""),
                    "prerelease": rel.get("prerelease").and_then(|v| v.as_bool()).unwrap_or(false),
                    "assets": assets,
                    "checked_at": chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
                }));
            }
        }
    }
    // GitHub API 对共享出口经常 403/限流；退回公开 releases.atom（不需要 token）。
    let atom_url = format!("https://github.com/{REPO}/releases.atom");
    let atom = state
        .client
        .get(atom_url)
        .header("Accept", "application/atom+xml")
        .header("User-Agent", "llm-router-updater")
        .timeout(Duration::from_secs(20))
        .send()
        .await
        .map_err(|e| format!("请求 GitHub 失败: {e}"))?;
    let text = atom.text().await.map_err(|e| format!("读取 releases.atom 失败: {e}"))?;
    let Some(rel) = atom_release(&text, channel) else {
        return Ok(json!({"ok": true, "current": VERSION, "channel": channel, "latest": null, "has_update": false, "releases": 0, "source": "atom"}));
    };
    let latest = rel.get("tag_name").and_then(|v| v.as_str()).unwrap_or("").to_string();
    Ok(json!({
        "ok": true,
        "current": VERSION,
        "channel": channel,
        "latest": latest,
        "has_update": is_newer(&latest, VERSION),
        "name": rel.get("name").and_then(|v| v.as_str()).unwrap_or(""),
        "notes": "",
        "published_at": rel.get("published_at").and_then(|v| v.as_str()).unwrap_or(""),
        "html_url": rel.get("html_url").and_then(|v| v.as_str()).unwrap_or(""),
        "prerelease": rel.get("prerelease").and_then(|v| v.as_bool()).unwrap_or(false),
        "assets": [],
        "source": "atom",
        "checked_at": chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
    }))
}

pub async fn check(State(state): State<Arc<AppState>>, Query(q): Query<UpdateQuery>) -> Response {
    let channel = q.channel.as_deref().unwrap_or("stable").to_ascii_lowercase();
    match fetch(&state, &channel).await {
        Ok(v) => Json(v).into_response(),
        Err(e) => Json(json!({"ok": false, "error": e, "current": VERSION, "channel": channel})).into_response(),
    }
}

/// 后台每 6 小时检查一次；事件只做提醒，不阻塞主线程。
pub fn start(state: Arc<AppState>) {
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(20)).await;
        loop {
            if let Ok(v) = fetch(&state, "stable").await {
                if v.get("has_update").and_then(|x| x.as_bool()).unwrap_or(false) {
                    let _ = state.events_tx.send(json!({"type": "update", "payload": v}));
                }
            }
            tokio::time::sleep(Duration::from_secs(6 * 3600)).await;
        }
    });
}
