use std::sync::Arc;
use std::time::Duration;

use chrono::{Datelike, Local, TimeZone};
use rand::Rng;

use super::checkin;
use super::state::AppState;

fn parse_hhmm(text: &str) -> (u32, u32) {
    let parts: Vec<&str> = text.split(|c| c == ':' || c == '：').collect();
    if parts.len() != 2 {
        return (8, 0);
    }
    let hh = parts[0].trim().parse::<u32>().unwrap_or(8);
    let mm = parts[1].trim().parse::<u32>().unwrap_or(0);
    if hh <= 23 && mm <= 59 { (hh, mm) } else { (8, 0) }
}

pub fn spawn(state: Arc<AppState>) {
    tauri::async_runtime::spawn(async move {
        // 启动先等一拍，别和拉模型/窗口初始化抢资源；之后每 30 秒检查一次。
        tokio::time::sleep(Duration::from_secs(20)).await;
        loop {
            tick(&state).await;
            tokio::time::sleep(Duration::from_secs(30)).await;
        }
    });
}

async fn tick(state: &Arc<AppState>) {
    let cfg = checkin::settings(state);
    if !cfg.get("enabled").and_then(|v| v.as_bool()).unwrap_or(false) {
        return;
    }
    let (hh, mm) = parse_hhmm(cfg.get("at").and_then(|v| v.as_str()).unwrap_or("08:00"));
    let window = cfg
        .get("random_minutes")
        .and_then(|v| v.as_i64())
        .unwrap_or(30)
        .clamp(0, 240);

    let now = Local::now();
    let day = now.format("%Y-%m-%d").to_string();
    if *state.checkin_last_day.lock().unwrap() == day {
        return;
    }

    let target = {
        let mut plan = state.checkin_plan.lock().unwrap();
        if let Some((plan_day, ts)) = plan.as_ref() {
            if plan_day == &day {
                *ts
            } else {
                let ts = make_target(&now, hh, mm, window);
                *plan = Some((day.clone(), ts));
                ts
            }
        } else {
            let ts = make_target(&now, hh, mm, window);
            *plan = Some((day.clone(), ts));
            ts
        }
    };

    if now.timestamp() < target {
        return;
    }
    if !checkin::has_targets(state) {
        return;
    }
    let ids = checkin::pending_ids(state);
    if ids.is_empty() {
        *state.checkin_last_day.lock().unwrap() = day.clone();
        return;
    }
    let results = checkin::run(state.clone(), ids, true).await;
    *state.checkin_last_day.lock().unwrap() = day.clone();
    *state.checkin_plan.lock().unwrap() = None;
    eprintln!("[checkin] auto run {day}: {} targets", results.len());
}

fn make_target(now: &chrono::DateTime<Local>, hh: u32, mm: u32, window_minutes: i64) -> i64 {
    let base = Local
        .with_ymd_and_hms(now.year(), now.month(), now.day(), hh, mm, 0)
        .single()
        .unwrap_or_else(Local::now);
    let offset = if window_minutes > 0 {
        rand::thread_rng().gen_range(0..=window_minutes * 60) as i64
    } else {
        0
    };
    base.timestamp() + offset
}
