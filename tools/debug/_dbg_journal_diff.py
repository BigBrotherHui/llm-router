"""诊断：请求记录（JSONL 尾部口径）vs 会话对账（SQLite 索引口径）今日条数差异。

只读，不改任何数据。
"""
import hashlib
import json
import os
import sqlite3
import sys
from datetime import datetime, timedelta, timezone

HOME = os.path.expanduser("~")
BASE = os.path.join(HOME, ".zcode", "workspace", "default", "llm-router", ".llmrouter")
JSONL = os.path.join(BASE, "config.history.jsonl")
SQLITE = os.path.join(BASE, "config.history.sqlite3")
TAIL_BYTES = 8 * 1024 * 1024

local_now = datetime.now().astimezone()
local_mid = local_now.replace(hour=0, minute=0, second=0, microsecond=0)
utc_mid = datetime.now(timezone.utc).replace(hour=0, minute=0, second=0, microsecond=0)
print(f"now(local)   = {local_now.isoformat()}")
print(f"today window = 会话对账(本地 00:00 起) {local_mid.isoformat()}")
print(f"               请求记录(UTC 日)      {utc_mid.astimezone().isoformat()}"
      f"  (UTC {utc_mid.isoformat()})")

lines = []
with open(JSONL, "rb") as fh:
    data = fh.read()
for raw in data.split(b"\n"):
    raw = raw.strip()
    if not raw:
        continue
    try:
        lines.append(json.loads(raw))
    except Exception:
        pass
print(f"\nJSONL 总行数(全文件) = {len(lines)}   文件 {len(data)} bytes")

tail_start = max(0, len(data) - TAIL_BYTES)
tail = data[tail_start:]
tail_lines = [json.loads(r.strip()) for r in tail.split(b"\n") if r.strip().startswith(b"{")]
print(f"8MB 尾部行数        = {len(tail_lines)}")


def local_date(at):
    return datetime.fromtimestamp(at).astimezone().date().isoformat()


def utc_date(at):
    return datetime.fromtimestamp(at, timezone.utc).date().isoformat()


def count(rows, pred):
    return sum(1 for e in rows if pred(e.get("at", 0)))


today_local = local_mid.date().isoformat()
today_utc = utc_mid.date().isoformat()

print("\n--- JSONL 口径 ---")
print(f"全部行中 本地日={today_local}        : {count(lines, lambda a: local_date(a) == today_local)}")
print(f"全部行中 UTC 日={today_utc}          : {count(lines, lambda a: utc_date(a) == today_utc)}")
print(f"尾部行中 本地日={today_local}        : {count(tail_lines, lambda a: local_date(a) == today_local)}")
print(f"尾部行中 UTC 日={today_utc}  ← 请求记录 tab 今天 : {count(tail_lines, lambda a: utc_date(a) == today_utc)}")

uniq_fp = {hashlib.md5(json.dumps(e, sort_keys=True).encode()).hexdigest() for e in lines}
uniq_raw = {json.dumps(e, sort_keys=True) for e in lines}
print(f"\n全文件行数={len(lines)}  去重(json文本)={len(uniq_raw)}  重复行={len(lines) - len(uniq_raw)}")

print("\n--- SQLite 索引口径 ---")
conn = sqlite3.connect(SQLITE)
rows = conn.execute("SELECT COUNT(*) FROM journal").fetchone()[0]
print(f"索引总行数 = {rows}")
local_mid_ts = local_mid.timestamp()
utc_mid_ts = utc_mid.timestamp()
print(f"at >= 本地今日00:00  ← 会话对账 journal.rounds : "
      f"{conn.execute('SELECT COUNT(*) FROM journal WHERE at >= ?', (local_mid_ts,)).fetchone()[0]}")
print(f"at >= UTC 今日00:00                           : "
      f"{conn.execute('SELECT COUNT(*) FROM journal WHERE at >= ?', (utc_mid_ts,)).fetchone()[0]}")
fp_rows = conn.execute("SELECT COUNT(DISTINCT fp) FROM journal").fetchone()[0]
print(f"索引 distinct fp = {fp_rows}")

# 索引缺了哪些行？raw 是 serde_json::to_string 的结果，用排序后的 JSON 做宽松比较
idx_raw = {r[0] for r in conn.execute("SELECT raw FROM journal")}
idx_norm = {json.dumps(json.loads(r), sort_keys=True) for r in idx_raw}
file_norm = [json.dumps(e, sort_keys=True) for e in lines]
missing_norm = [e for e, n in zip(lines, file_norm) if n not in idx_norm]
print(f"\n索引缺失行数 = {len(missing_norm)}")
if missing_norm:
    for e in missing_norm[:10]:
        print("  缺失:", datetime.fromtimestamp(e.get("at", 0)).isoformat(),
              e.get("model"), e.get("upstream"), e.get("status"), e.get("ok"))
conn.close()
