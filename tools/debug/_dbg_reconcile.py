"""一次取齐所有「今日」口径，检查它们是否互相一致。

会话对账「今日所有请求/过 LLM Router/未走路由」、请求记录两个口径、
/stats 的当天聚合、SQLite 索引、JSONL 行数 —— 全部按同一个时刻取。
"""
import json
import os
import sqlite3
import sys
import time
import urllib.request
from datetime import datetime, timezone

BASE_URL = "http://127.0.0.1:8010"
HOME = os.path.expanduser("~")
STATE = os.path.join(HOME, ".zcode", "workspace", "default", "llm-router", ".llmrouter")
JSONL = os.path.join(STATE, "config.history.jsonl")
SQLITE = os.path.join(STATE, "config.history.sqlite3")


def get(path):
    with urllib.request.urlopen(BASE_URL + path, timeout=180) as r:
        return json.loads(r.read().decode("utf-8"))


day = datetime.now().strftime("%Y-%m-%d")
local_mid = datetime.now().replace(hour=0, minute=0, second=0, microsecond=0)
now = datetime.now()

scan = get(f"/sessions/scan?sources=dsh,claude,pi,codex&limit=200&range=today&metric=newapi")
recs = get(f"/sessions/records?day={day}&limit=1&metric=newapi")
jour = get(f"/admin/journal?day={day}&limit=1")
idx = get("/admin/journal/index")
stats = get(f"/stats?range=day:{day}&full=0")
hud = None

rows = []
with open(JSONL, "rb") as fh:
    for raw in fh:
        raw = raw.strip()
        if not raw:
            continue
        try:
            rows.append(json.loads(raw))
        except Exception:
            pass


def local_date(at):
    return datetime.fromtimestamp(at).astimezone().date().isoformat()


def utc_date(at):
    return datetime.fromtimestamp(at, timezone.utc).date().isoformat()


jsonl_day = sum(1 for e in rows if local_date(e.get("at", 0)) == day)
jsonl_day_utc = sum(1 for e in rows if utc_date(e.get("at", 0)) == day)

conn = sqlite3.connect(f"file:{SQLITE}?mode=ro", uri=True)
sql_day = conn.execute("SELECT COUNT(*) FROM journal WHERE at >= ?", (local_mid.timestamp(),)).fetchone()[0]
sql_all = conn.execute("SELECT COUNT(*) FROM journal").fetchone()[0]
conn.close()

t = scan.get("totals", {}) or {}
e = scan.get("external_totals", {}) or {}
j = scan.get("journal", {}) or {}

print(f"now(local) = {now:%Y-%m-%d %H:%M:%S}   今天窗口 = 本机 {local_mid:%Y-%m-%d %H:%M} 起\n")
print(f"{'口径':<46}{'值':>8}")
print("-" * 56)
print(f"{'会话对账 · 今日所有请求 (= 未走路由 + 8010 流水)':<46}{e.get('rounds', 0) + j.get('rounds', 0):>8}")
print(f"{'会话对账 · 过 LLM Router (= 8010 流水)':<46}{j.get('rounds', 0):>8}")
print(f"{'会话对账 · 未走路由 (Agent 会话)':<46}{e.get('rounds', 0):>8}")
print(f"{'会话对账 · Agent 会话总轮数':<46}{t.get('rounds', 0):>8}")
print("-" * 56)
print(f"{'请求记录 · 全部 Agent 会话 · 今天':<46}{recs.get('total', 0):>8}")
print(f"{'请求记录 · 本机路由流水 · 今天':<46}{jour.get('total', 0):>8}")
print(f"{'请求记录 · tab 角标来源 journalIndex.total':<46}{idx.get('total', 0):>8}")
print("-" * 56)
print(f"{'面板统计 /stats?range=day:今天 · calls':<46}{(stats.get('stats_range') or {}).get('key', '?'):>8}")
print(f"{'   -> totals.calls':<46}{((stats.get('stats_range') or {}).get('totals') or {}).get('calls', 0):>8}")
print(f"{'   -> totals.attempts':<46}{((stats.get('stats_range') or {}).get('totals') or {}).get('attempts', 0):>8}")
print("-" * 56)
print(f"{'SQLite 索引 · at >= 本地零点':<46}{sql_day:>8}")
print(f"{'SQLite 索引 · 全表':<46}{sql_all:>8}")
print(f"{'JSONL · 本地日 = 今天':<46}{jsonl_day:>8}")
print(f"{'JSONL · UTC 日 = 今天':<46}{jsonl_day_utc:>8}")
print(f"{'JSONL · 总行数':<46}{len(rows):>8}")
print(f"\n/index.days = {[(d['key'], d['count']) for d in (idx.get('days') or [])]}")
