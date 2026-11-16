"""盯一会儿：面板统计（stats.json 滚动聚合）和「请求记录」列表（JSONL / 索引）会不会越差越多。

固定差值 = 上次「硬杀进程」丢掉的滚动聚合（写盘有节流，JSONL 是逐条落盘）；
持续变大 = 有别的系统性口径差。
"""
import json
import time
import urllib.request
from datetime import datetime

B = "http://127.0.0.1:8010"


def g(p):
    with urllib.request.urlopen(B + p, timeout=120) as r:
        return json.loads(r.read().decode())


day = datetime.now().strftime("%Y-%m-%d")
prev = None
for i in range(4):
    s = g(f"/stats?range=day:{day}&full=0")
    j = g(f"/admin/journal?day={day}&limit=1")
    t = ((s.get("stats_range") or {}).get("totals") or {})
    gap = (j.get("total") or 0) - (t.get("calls") or 0)
    print(f"{datetime.now():%H:%M:%S}  面板 calls={t.get('calls'):>4} attempts={t.get('attempts'):>4}"
          f"  |  列表 total={j.get('total'):>4}   差={gap}")
    prev = gap
    if i < 3:
        time.sleep(20)
