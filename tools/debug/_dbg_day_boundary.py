"""校验「请求记录」的日期口径是**本机时区**而不是 UTC。

做法：起一个独立的、只服务一个假状态文件的实例（LLM_ROUTER_PORT 换端口，
不动正在跑的 8010），往它的流水里塞一条「本机时间今天凌晨 03:00」的记录 ——
这个时刻的 UTC 日期是**昨天**。于是：
  - 口径对（本地日）：/admin/journal?day=<今天> 命中 1 条，index.days 只有今天一个桶；
  - 口径错（UTC 日）：今天 0 条，那条会掉进昨天的桶（这正是修之前的行为）。

用法：python tools/debug/_dbg_day_boundary.py [--exe D:\\llm-router-target\\release\\llm-router-desktop.exe]
"""
import json
import os
import shutil
import subprocess
import sys
import time
import urllib.error
import urllib.request
from datetime import datetime, timedelta, timezone

STATE_DIR = os.path.join(os.path.dirname(os.path.abspath(__file__)), "_dayboundary")
PORT = 18011
EXE = r"D:\llm-router-target\release\llm-router-desktop.exe"
SEED = os.path.join(
    os.environ["APPDATA"], "com.llmrouter.desktop", ".llmrouter", "config.state.json"
)

for i, a in enumerate(sys.argv):
    if a == "--exe" and i + 1 < len(sys.argv):
        EXE = sys.argv[i + 1]


def get(path, timeout=20):
    with urllib.request.urlopen(f"http://127.0.0.1:{PORT}{path}", timeout=timeout) as r:
        return json.loads(r.read().decode("utf-8"))


def main():
    now = datetime.now()
    today = now.strftime("%Y-%m-%d")
    yesterday = (now - timedelta(days=1)).strftime("%Y-%m-%d")
    # 本机今天凌晨 03:00 —— 它在 UTC 下属于昨天
    local_3am = now.replace(hour=3, minute=0, second=0, microsecond=0)
    utc_day = datetime.fromtimestamp(local_3am.timestamp(), timezone.utc).strftime("%Y-%m-%d")
    assert utc_day != today, f"构造不出跨日用例：local 3am 的 UTC 日 = {utc_day}"

    if os.path.isdir(STATE_DIR):
        shutil.rmtree(STATE_DIR)
    os.makedirs(STATE_DIR)
    state = json.load(open(SEED, encoding="utf-8"))
    state["upstreams"] = []          # 别让这个临时实例去打真上游
    state["models"] = {}
    state_path = os.path.join(STATE_DIR, "config.state.json")
    json.dump(state, open(state_path, "w", encoding="utf-8"), ensure_ascii=False)

    entry = {
        "at": local_3am.timestamp(),
        "time": local_3am.strftime("%Y-%m-%d %H:%M:%S"),
        "model": "day-boundary-check",
        "ok": True,
        "status": 200,
        "usage": {"input": 1, "output": 1, "total": 2},
    }
    jsonl = os.path.join(STATE_DIR, "config.history.jsonl")
    with open(jsonl, "w", encoding="utf-8") as fh:
        fh.write(json.dumps(entry, ensure_ascii=False) + "\n")

    env = dict(os.environ, LLM_ROUTER_STATE=state_path, LLM_ROUTER_PORT=str(PORT))
    proc = subprocess.Popen([EXE], env=env, cwd=os.path.dirname(EXE))
    try:
        for _ in range(40):
            time.sleep(1)
            try:
                get("/health", timeout=2)
                break
            except Exception:
                pass
        else:
            print("FAIL: 临时实例没起来")
            return 1

        hit_today = get(f"/admin/journal?day={today}&limit=5")
        hit_utc = get(f"/admin/journal?day={utc_day}&limit=5")
        idx = get("/admin/journal/index")
        days = {d["key"]: d["count"] for d in (idx.get("days") or [])}

        print(f"本机今天 {today} 凌晨 03:00 = UTC {utc_day} 19:00 附近")
        print(f"  /admin/journal?day={today}  -> total={hit_today.get('total')}")
        print(f"  /admin/journal?day={utc_day} -> total={hit_utc.get('total')}")
        print(f"  /admin/journal/index days  -> {days}")

        ok = hit_today.get("total") == 1 and hit_utc.get("total") == 0 and days.get(today) == 1
        print("PASS: 日期按本机时区归桶" if ok else "FAIL: 日期仍是 UTC 归桶")
        return 0 if ok else 1
    finally:
        proc.terminate()
        try:
            proc.wait(timeout=10)
        except subprocess.TimeoutExpired:
            proc.kill()


if __name__ == "__main__":
    sys.exit(main())
