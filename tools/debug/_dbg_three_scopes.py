"""三个口径的对账：所有请求 / 只过 LLM Router / 不过 LLM Router。

跑之前确保新版在 8010 上跑（重启一次 App）。

- 所有请求 = 不过 LLM Router（Agent 会话绕过 8010 的轮）+ 只过 LLM Router（8010 流水）
- 会话对账页的「今日所有请求」必须是同一个并集
"""
import json
import time
import urllib.request
from datetime import datetime

B = "http://127.0.0.1:8010"


def g(p):
    with urllib.request.urlopen(B + p, timeout=180) as r:
        return json.loads(r.read().decode())


day = datetime.now().strftime("%Y-%m-%d")
scan = g(f"/sessions/scan?sources=dsh,claude,pi,codex&limit=200&range=today&metric=newapi")
union = g(f"/sessions/records?include=router&day={day}&limit=1&metric=newapi")
only = g(f"/sessions/records?journal_only=1&day={day}&limit=1&metric=newapi")
ext = g(f"/sessions/records?route=external&day={day}&limit=1&metric=newapi")
jrn = g(f"/admin/journal?day={day}&limit=1")
ser = g(f"/stats/series?range=today&scope=all&include=router&metric=newapi")
# 分组筛选也要能筛出东西（分组键是 上游/模型，选一个 8010 那边的分组验证合并后的过滤）
grp = ""
for r in (union.get("groups") or []):
    if r["group"].startswith("modelscope"):
        grp = r["group"]
        break
gfilt = g(f"/sessions/records?include=router&day={day}&group={grp}&limit=1&metric=newapi") if grp else {}

e = (scan.get("external_totals") or {}).get("rounds", 0)
j = (scan.get("journal") or {}).get("rounds", 0)
union_calls = sum(b.get("calls", 0) for b in ser.get("buckets", []))

print(f"now = {datetime.now():%Y-%m-%d %H:%M:%S}   day = {day}\n")
print(f"{'口径':<40}{'条数':>7}")
print("-" * 48)
print(f"{'请求记录 · 所有请求 (include=router)':<40}{union.get('total', 0):>7}")
print(f"{'请求记录 · 不过 LLM Router (route=external)':<40}{ext.get('total', 0):>7}")
print(f"{'请求记录 · 只过 LLM Router (journal_only=1)':<40}{only.get('total', 0):>7}")
print(f"{'  （同一份 8010 流水：/admin/journal 只读 8MB 尾部）':<40}{jrn.get('total', 0):>7}")
print("-" * 48)
print(f"{'会话对账 · 今日所有请求 = 不过 + 过':<40}{e + j:>7}   ({e} + {j})")
print(f"{'会话对账 · 不过 LLM Router':<40}{e:>7}")
print(f"{'会话对账 · 过 LLM Router':<40}{j:>7}")
print("-" * 48)
print(f"{'图表 scope=all&include=router 的调用合计':<40}{union_calls:>7}")
print(f"{'图表（带 day 筛选）':<40}{sum(b.get('calls', 0) for b in g(f'/stats/series?range=all&day={day}&scope=all&include=router&metric=newapi').get('buckets', [])):>7}")
print(f"所有请求 totals = {json.dumps(union.get('totals'), ensure_ascii=False)}")
print(f"所有请求 scope  = {union.get('scope')}   只过 scope = {only.get('scope')}")
print(f"只过 totals     = {json.dumps(only.get('totals'), ensure_ascii=False)}")
print(f"分组 Top3       = {json.dumps([(r['group'], r['calls'], r['failed']) for r in (union.get('groups') or [])[:3]], ensure_ascii=False)}")
print(f"按分组 {grp} 筛  = {gfilt.get('total')} 条")
ok = (union.get("total") == ext.get("total", 0) + only.get("total", 0) == e + j
      and union.get("total") == union_calls
      and (not grp or 0 < gfilt.get("total", 0) < union.get("total", 0) + 1))
print("\nPASS: 所有请求 = 不过 + 只过（= 会话对账同一并集），图表与分组筛选一致"
      if ok else "\nFAIL: 口径或筛选对不上")
