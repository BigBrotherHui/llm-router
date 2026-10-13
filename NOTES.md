# NOTES

## 2026-10-13 上游「测速」改成按勾选的每个模型各发一个请求

需求：上游来源卡片的「测速」以前只测 `available_models[0]`，一个上游勾了
多个模型时，测一个不代表别的能用。

后端 `router.py`（2.3.4）

- `/admin/speedtest`（不带 `tests`）：对每个目标上游，**对它勾选的每个模型
  各发一个最小请求**（一个都没勾就退回第一个可用模型）。返回：
  - `results`：一条 = 一个 (上游, 模型)，带 `model` / `ok` / `latency_seconds` /
    `rate_limited` / `error`；
  - `upstreams`：每个上游一条汇总（任一模型成功即 ok，延迟取最快那个，
    带 `model_count`），给「按测速重排」和卡片汇总用。
- `last_checks`（model → 结果）随状态持久化；`last_check` 保留（最近一次）。
- `reorder_by_speed`：优先按 `(上游, 模型)` 精确匹配延迟，无 model 的退回按上游。

前端 `webui.html`

- 卡片「测速」：toast 汇总「N/M 个模型可用，最快 Xs」，卡片上把**勾选的每个
  模型**逐条列出（可用/不可用/限流 + 延迟；没测过显示「未测」）。
  数据来自会话内 `lastTestByModel["id|model"]` 或持久化的 `u.last_checks`。
- 模型路由弹窗每行的「测试」按钮不变（精确测那一行那个模型）。

验证：selftest 第 14 组改成「按模型逐条返回」（a1 两模型 + b1 一个 + c1 兜底
= 4 条），另加「结果带 model」「上游汇总也返回」；全量 **364 项全过**。
线上实测 `dshapi_zhouyh` 3 个模型分别 23.4s / 42.7s / 21.5s，汇总取最快 21.5s。

## 2026-10-13 模型路由：每行精确测某个模型（连通性）

需求（修正版）：要测的是**模型路由弹窗里每一行**那个上游 + **它那个具体模型**，
而不是上游来源卡片上的「随便挑一个模型测」—— 一个上游可能选了多个模型，
测第一个不代表这一行能用。

后端 `router.py`（2.3.4）

- `Upstream.last_check`：最近一次测试结果（`at/ok/status/latency_seconds/
  error/rate_limited/model`），随状态持久化。
- `ping（model=None）` 成功/失败两条路径都调 `_remember_check（）`。
- `POST /admin/speedtest` 新增 `tests: [{id, model}]` 参数：**精确测某上游上的
  某个模型**，结果里回带 `model`。旧的 `ids`（整家测，取 available_models[0]）
  保留给上游来源卡片的「测速」按钮。跑完 `_save_state（）`。

前端 `webui.html`

- **模型路由弹窗**每行新增「测试」按钮 `testPick（i）`：发
  `tests:[{id: p.id, model: p.model || 分组名}]`，只测这一行那个模型；
  结果 `pickTestBadge（）` 就地显示在该行（可用/不可用/限流 + 延迟），
  不重建整个弹窗（免得打断输入）。
- 上游来源卡片**保持原样**（按钮仍叫「测速」），不加连通性列 —— 那里没有
  「具体某个模型」的语义，加了会误导。

验证：selftest 第 14 组新增 `tests` 精确测一个模型 / 回带模型名 / 坏上游报失败，
以及 `last_check` 持久化（含重启后保留）；全量 **362 项全过**。
线上实测 `tests:[{id,model:"deepseek-v4-flash"}]` → `ok:true latency 1.663s`。

## 2026-10-13 v2.3.4：取消勾选「参与」不再删除该条路由

现象：按 checkbox 把某个上游的「参与」取消后保存，再打开弹窗这条就没了。
原因是 `saveModel()` 把 `enabled=false` 的条目都 filter 掉了，后端存进去
的 entries 里本来就没有它；下次 `loadPicks()` 自然读不到。

修法（只有前端改）：
- `webui.html` `saveModel()` 里：
  - `entries = editing.picks.map(...)` 改成**全部 picks 都序列化**，保留
    `enabled: p.enabled`（不再 filter 掉 disabled）；
  - 校验改为「至少留一个 enabled=true 的」（禁止所有参与都关掉）；
  - 加了注释说明意图：禁用式路由——保留配置，下次重新勾上就能恢复。

后端本来就已经支持 enabled:false（`router.py` `resolve()` 跳过、
`RouteEntry.__init__` 解析 `cfg.get("enabled", True)`），不需要动。
selftest 新增 3 项验证：禁用条目调用被跳过、snap 里还在、enabled:false 保留。
总计 **355 项全过**。

## 2026-10-13 v2.3.3：修「筛选上游后左右两栏还是对不上」

用户截图：按 `chatanywhere_zhoushhh` 筛选，右侧 11 行全是 200（看着全成功），
左侧对比条却 10 败；上方「失败」和下面的数也对不上。查下来是**两件事**：

1. **右侧列表是旧页面**（假象，不是 bug）：浏览器跑的还是 v2.3.1 的 JS，
   后端已 v2.3.2。v2.3.1 的行只按客户端最终结果渲染（`h.ok`/`h.upstream`），
   所以换家失败的行显示成绿色 + 最终上游。确认：`/` 返回的 HTML 已含 v2.3.2 代码，
   接口也返回了带失败步的 `trail`。**重载一次即好**（HTML 响应带 `Cache-Control: no-store`，
   普通刷新即可）。
   为免以后再踩：`webui.html` 每次轮询 `/stats` 时对比版本号，服务端版本变了就
   `location.reload()`，旧 JS 配新后端这种假象不再出现。

2. **左栏数字确实是两个口径混着摆**（真 bug）：
   - 分组卡 / 上方「失败」= **客户端请求**失败数（请求在别家兜住就不算）；
   - 每个上游条上的「N 败」= **该上游每次尝试**的失败数（换家过程中失败的也计入）。
   两个数天生不同（实测那天 deepseek：客户端失败 5，各上游败相加 45），
   同屏并排就必然「对不上」。

   修法：选了某个上游时，**统计面板也缩到这个上游**（用 `stats_range` 里它的
   子桶：calls/ok/failed/usage），文案说明「按它每次被调用算」。这样面板、对比条、
   列表三者同口径。实测（2026-10-13 · deepseek · chatanywhere_zhoushhh）：
   面板 11 调用 / 1 成功 / 10 失败 = 列表 11 行（10 红 1 绿）= 对比条 11 · 10败。
   未筛选上游时，也在提示里点明「各条相加会大于客户端失败」。

验证
- 线上 8010 已重启到 2.3.3；接口实测三者数字一致。
- 后端未改逻辑（仅版本号），v2.3.2 的 351 项 selftest 结论仍有效。

## 2026-10-13 v2.3.2：修复「按上游筛选只看得到成功的，看不到失败的」

现象：某上游（chatanywhere_zhoushhh）卡片显示 11~12 次请求 / 10 次失败，
但请求记录里按它筛选只看到成功的记录。

根因（静态确认 + 实测）：**一件事两套粒度**。
- 流水里**一条 = 一个客户端请求**，其顶层 `upstream` 只记**最终成功的那一家**；
  请求中途换过家时，被跳过的几家失败只留在 `trail` 里。
- 上游卡片/聚合是**按每次尝试**计数（`StatsStore._apply` 遍历 `trail`）。
- 而我上一步加的「按上游筛选」和下拉计数只看了顶层 `upstream`（=最终上游），
  于是换家过程中失败的上游永远筛不出来。
- 实测 chatanywhere_zhoushhh：trail 里出现 12 次、失败 10 次，但只有 2 次是
  最终上游（都成功）—— 另 10 次失败后请求在 `modelscope_zhoush` 上成功。

修复 `router.py`
- 新增 `StatsStore._entry_trail(entry)`：取该记录依次试过的上游；老记录没 `trail`
  就按最终上游合成一步（和 `_apply` 的兜底一致）。
- `query_journal`：
  - `upstream` 筛选改成「**这次请求试过的任意一家** == 目标」（匹配 trail），
    而不是只看最终上游。
  - `upstreams` 计数改成**按每次尝试**统计（和卡片同口径），下拉失败数不再偏小。

修复 `webui.html`
- 选中某上游筛选时，行内状态徽标显示**该上游这一次**的结果（成功/失败+状态码），
  而不是客户端最终结果（原来失败的尝试会因为请求后来在别家成功而整行绿色）。
- 行内上游徽标显示被筛的上游，换过家时补一个 `→ 最终上游`。
- 展开详情新增「尝试链」：依次列出每个上游的 ✓/✗ · 状态码 · 模型 · 失败原因。

验证
- `selftest.py` 第 29 组新增 3 项：故障转移后请求仍成功、失败换家的上游能被筛出、
  组内计数按尝试含失败 —— **351 项全过**。
- 线上 8010 已重启到 2.3.2；实测筛 chatanywhere_zhoushhh 返回 12 条请求、
  其中 10 条该上游那一步为失败（403/520），和卡片 12/10 对上。

## 2026-10-13 v2.3.1：请求记录支持「分组 → 组内上游」二级筛选

需求：选完分组后，要能再选该分组里的某个上游看它的调用情况
（典型场景：deepseek 分组里有个上游一直报错，要单独翻它的记录）。

后端 `router.py`
- `query_journal()` 新增 `upstream` 参数（按上游名过滤，和 day/month/group 同一套）。
- 同时回传 `upstreams`：在「天/月/分组」筛选**之后**、上游筛选**之前**统计，
  每项 `{key, calls, failed}`，按失败数、调用数倒序。
  所以下拉里只列当前分组真正用过的上游，并带失败数。
- `/admin/journal` 接口透传 `upstream` 查询参数。

前端 `webui.html`
- 请求记录筛选行新增 `#sideUpstream` 下拉：**只有选中某个分组时才启用**并填充
  （全量时不列，免得一长串）。选项文案带失败数，如 `dshapi_zhoush（5 败）`。
- 换分组时先清空上游选择再查（否则会带着旧分组的上游把新分组筛成空）。
- `curJournalFilter/curFilterLabel/loadJournal/refresh hash` 都带上上游维度。
- 统计区的每个上游对比条也按选中的上游过滤，和列表口径一致。

验证
- `selftest.py` 第 29 组新增 3 项（按上游筛命中数、组内上游计数含失败数、
  不存在上游得 0）——**348 项全过**。
- 线上 8010 已重启到 2.3.1（`_restart.py` 现在从 router.py 读版本号，不再硬编码；
  回滚文件指向已知可用的 2.3.0）。
- 实测：deepseek 分组 1934 条，列出 12 个上游；`dshapi_zhoush` 6 次调用 5 次失败，
  按它筛选返回 542 条 `dshapi_zhouyh`、6 条 `dshapi_zhoush` 等，条目上游名全部匹配。

## 2026-10-13 修复：HTTP/1.1 长连接下「点了没反应 / 添加后退化成拉全部」

用户报两个现象：①添加新上游后默认拉取了**所有**上游模型（应只拉新的）；
②点「停用」一下就好，点「启用」要点很多次才生效。

根因是**同一个 bug**：`Handler._body_cache` / `_audit_body`（请求体缓存）
只在 `setup（）` 里清空，而 `setup` 每条 TCP 连接只跑一次；
但 `protocol_version = "HTTP/1.1"` 开了长连接，同一个 handler 实例会连续
处理多条请求。于是**同一连接上的第二条请求会读到上一条缓存的 body**，
真正的 body 还留在 socket 里，顺带把下一条请求也搞坏
（实测见过 `Unsupported method ('{...}POST')`）。

- 表现①：`add` 的 body 没有 `ids` → 紧跟的 `fetch-models` 复用后拿不到 ids
  → 后端按「没传 ids = 全部」执行 → 拉全量。
- 表现②：第一次 toggle 的 body 被缓存，后续 toggle 一直复用旧 body
  → 状态看起来不变，要多点几次。

修复：在 `handle_one_request（）` 开头（每条请求都经过）重置两个缓存，
`router.py:2959-2960`。`setup（）` 里的重置保留。

验证：

- 复现（修复前，线上 8010）：同一连接 `disable` 后 `enable` 返回 `False`；
  `add` 后 `fetch ids=[new]` 返回了全部 20 个上游。
- 修复后同连接：`enable -> True`、`fetch -> ['新id']`。
- `selftest.py` 新增第 30 组 3 项（**长连接**里 add→fetch 只拉指定上游、
  连续 disable→enable 第二次生效）。特意把修复撤掉跑了一遍，
  第 30 组确实失败（`ids=['a1','b1','c1','uffa690']`），说明用例有效。
- 全量：**345 项全过**。

教训：`selftest` 里原有用例每条请求都新开连接（`urllib`），所以永远碰不到
长连接复用这条路径 —— 这类「连接级状态」的 bug 必须专门用同连接连发来测。

## 2026-10-13 v2.3.0：调用记录全量持久化 + 按天/月/分组筛选

起因：用户指出「调用记录没持久化，白天的记录没了」，而且「左边显示 375、
右边显示 188」。查下来两件事都是真的，根因是**同一份数据存在两套口径**：

- 左侧角标 / 列表 —— 读内存里最近 500 条（`state.history`）；
- 右侧上游卡片 —— 读 `Upstream.requests`，进程内存计数，重启归零；
- 「今天/本月/全部」统计 —— 读落盘聚合 `config.stats.json`。

三套数字各自演化，必然对不上。而**真正的流水其实一直在**
（`config.history.jsonl`），只是界面上没有任何入口去读它。

后端 `router.py`（2.2.0 → 2.3.0）

- **流水成为唯一权威口径**：
  - `StatsStore.query_journal(day, month, group, offset, limit, with_text)`
    —— 从流水文件按条件翻页取记录，最新的在前；列表默认**不带正文**
    （`request_text` 等），展开某条时再用 `/admin/journal/entry` 取回，
    否则一页几十 MB 根本发不动。
  - `StatsStore.journal_index()` —— 流水里出现过的天/月/分组及各自条数，
    带文件指纹缓存，前端用它生成筛选下拉（只列真有数据的那几天）。
  - `StatsStore.find_journal_entry(at, upstream, status)` —— 按时间戳取单条完整记录。
    **先精确匹配时间戳**，再退到 0.1 秒窗内 + 上游/状态打分：高并发时
    几条记录会挤在同一秒里，只按四舍五入会取回「同一格里的另一条」，
    正文就对不上（这个 bug 在 selftest 里复现过）。
  - `StatsStore.journal_retention()` / `rebuild_stats_from_journal()`。
- **启动时按流水对平聚合**：`reconcile_from_journal()` 改成「不一致就以流水为准重算」。
  早先版本是「只在差一点点时才补」，于是 375/188 这种长期不一致就一直留着。
  实测重启时输出：`启动校正：聚合 661 条 -> 按流水 1403 条重算`。
- **上游卡片改走落盘聚合**：`snapshot()` 用 `stats.totals.upstreams` 的数字
  覆盖 `Upstream.requests` 这类内存计数，并加 `counters_persistent` 标记。
  重启后卡片数字不再归零。
- 新接口：`GET /admin/journal`（筛选+翻页）、`/admin/journal/index`、
  `/admin/journal/entry`、`/admin/journal/retention`、
  `POST /admin/stats/rebuild`。

前端 `webui.html`

- **去掉顶部三个按钮**：拉取全部模型、测速、记录与快照。
  前两个在每个上游卡片里本来就有（`data-act="fetch"` / `"test"`），
  要哪个点哪个比全量更可控；「记录与快照」的内容并进请求记录页，
  只留一个「配置快照」小按钮。现在顶栏只剩「接入方式」。
- **请求记录页改成按天 / 按月 / 按分组筛选 + 翻页**：三个下拉由流水索引填充，
  「加载更多」是真的去流水里多取一页（不是把手上已有的往前翻），
  所以 1400 条也能一路翻到底。统计范围跟着筛选走
  （`statsRangeKey()`），面板数字和列表从此同一个口径。
- 展开某条时才按需取回正文（`ensureEntryText`），列表 payload 保持轻量。

数据修复

- 10-12 14:11–15:32 那批 407 条调用发生在「落盘流水」功能上线之前，
  只存在于当时的 `_live.json` / `_fresh.json` 等快照里。用 `_merge_history.py`
  按时间戳去重后并进流水（**只补不删**，合并前备份到 `_backups/`）。
- 结果：流水 812 → 1219 条；覆盖 10-12 全天 929 条 + 10-13。
- 注意：合并进去的记录里混进过 1 条 `up-a/up-b/probe` 的测试记录
  （15:08:22，来自我早期诊断脚本导出）。它留在流水里没删 ——
  流水是只增不减的账本，为了删一条测试数据而改写历史不划算。
  按分组筛选时能看到一个 `probe` 分组，那 1 条就是它。

验证

- `selftest.py`：**342 项全过**（连跑两次稳定）。新增第 29 组 20 项：
  索引条数、按天/月/分组筛选、不存在的条件返回 0、翻页不重叠、
  列表不带正文而单条带正文、**统计口径与流水条数一致**、
  卡片数字来自落盘聚合且未命中的上游显示 0。
- 线上重启后用 `_restart.py`（带健康检查和自动回滚）切到 2.3.0；
  启动日志确认聚合按流水校正到 1403 条。
- 浏览器实测（1280×900 与 390×844）：顶栏只剩「接入方式」；
  三个筛选下拉分别列出 2 个日期 / 1 个月份 / 2 个分组；
  按天筛 2026-10-12 得到 929 条且时间都是那天；
  展开某条能取回 4000 字正文；「加载更多」100 → 200 条；
  手机宽度下 tab 仍在左侧 84px 竖栏。

## 2026-10-13 v2.2.0：统计不可清除 + 状态多版本备份 + 移动端左侧栏

起因：用户发现「请求数不对、GPT 模型不见了、手机页面 tab 位置不对」。
排查后确认两件事是真的：配置里 3 个 GPT 上游和 5 个分组被删过，且流水
（`config.history.jsonl`）之前会被「重写成最近 N 条」——等于真删历史。

**流水改成只增不减**
- `StatsStore._rewrite_journal()`（重写成最近 N 条）删除，换成
  `_rotate_journal()`：攒到 `JOURNAL_ROTATE_AT`（1 万行）就改名成
  `<stem>.history.<时间戳>.jsonl` 归档，另起一份接着写。归档文件永久保留。
- `history_limit` 现在只影响内存里保留多少条明细（`_recent`）供界面展示，
  不再影响落盘文件。
- `StatsStore.clear()` 保留但改成直接抛错，防止误调用；删掉了唯一会
  删掉流水的代码路径。
- `record()` 的记账和落盘顺序不变，但新增 `reconcile_from_journal()`：
  启动时若「流水比聚合多一点」（差值 ≤ 50，属于崩溃窗口漏记），按流水补记；
  差值更大则判定为历史遗留（老版本裁剪过流水），保持聚合值不变，
  只在 stderr 说明——避免一启动就改写界面上的数字。

**去掉「清空统计」**
- 前端删掉 `#resetBtn`，换成 `#historyBtn`「记录与快照」弹窗：
  看流水文件清单、看/回退状态快照、看最近管理操作。
- `/admin/reset` 从「统计+流水全清」改成「仅复位冷却与失败计数」。
- 新增 `GET /admin/journal/files`、`GET /admin/state/versions`、
  `POST /admin/journal/rotate`、`POST /admin/state/restore`、
  `GET /admin/admin-log`。

**状态文件多版本轮转**
- 每次 `_save_state()` 前把当前版本存进 `.llmrouter/versions/`，
  文件名 `<配置名>.state.<时间戳>.json`，保留最近 `STATE_BACKUP_KEEP`(20) 份；
  同名冲突补序号，不覆盖。旧的 `.bak` 仍保留（兼容 README 和习惯）。

**管理操作审计**
- 所有改配置的 `/admin/*` 调用走 `_audit()` 记进
  `.llmrouter/<配置名>.admin.log`（JSON Lines）：时间、接口、来源 IP、
  关键字段。Key 只记 `mask_key()` 后的提示，不落明文。
- 读取 body 前先缓存（`_body_cache`），否则审计读一次会把 HTTP 流消费掉。

**移动端 tab 不再横排**
- `@media(max-width:760px)` 原来把 `#tabs` 翻成顶部横排（`flex-direction:row`），
  手机上三个 tab 跑到顶部、角标还被隐藏。改成仍是左侧竖栏，只是收窄到 84px、
  标签两字一行、数字挪到标签下面。
- 已用 390×844 视口截图确认：`#tabs` 在 x=8、宽 84、`flex-direction: column`，
  `main` 从 x=101 开始。

**恢复被删的 GPT 配置**
- 从 `D:\Application\EasyCLIProxyAPI-...\cpa-core\config.yaml` 取回 3 个上游的
  Key（前缀/长度与丢失前 `config.state.json.bak` 记录完全一致）：
  `dli`(api.dlizz.com)、`aigcdesk`、`Synora`(synoralink.com)。
- 恢复 5 个分组：`gpt-5.6-luna`、`gpt-5.6-terra`、`gpt-6-astra`、
  `gpt-6-sol`、`gpt-6.1-sol`，成员顺序同丢失前。
- 三个上游一律置为**停用**，要不要启用由用户决定（付费线路，额度可能已耗尽）。

**验证**
- `python selftest.py`：320 项全过（改写了第 15/24/27 组等依赖
  `/admin/reset` 归零的断言，改成差量断言；第 27 组从「明细文件收缩」
  改成「落盘流水一条不少 + 归档保留」；新增第 28 组：reset 不清统计、
  审计日志可读、快照可列出/可回退）。
- 线上 8010 已重启到 2.2.0（`_restart.py` 带健康检查和自动回滚——
  本会话的推理请求就走这个代理，起不来就没机会再修，所以回滚做进脚本本身）。
- 重启日志确认：`流水 668 条、聚合 333 条，相差 335 条（超出崩溃补记范围），
  保持聚合值不变` —— 用户原有统计数字没有被我的改动动过。

## 2026-10-12 上游来源页：卡片式网格 + 全部启动/停用

- 需求：一屏看更多上游，减少滚动；加"全部启动"按钮；用户也接受替代思路。
- 改动只在前端 `webui.html`（后端接口不动）：
  - `#upstreams` 改为 CSS grid（`repeat(auto-fill,minmax(340px,1fr))`），窗口越宽列数越多。
  - 展开"选模型"面板的卡片加 `.open` 类，`grid-column:1/-1` 占满整行，面板宽度不受卡片列宽限制。
  - 卡片内边距 13/14 → 12/13，`margin-bottom` 移除（改由 grid gap 控制）。
  - 新增 `.up-tools` 行：上游计数文案 `#upCount` + `#enableAllBtn` / `#disableAllBtn`。
  - `renderTabNums()` 里顺带填充 `#upCount`（N 个上游，其中 M 个已启用）。
  - `setAllUpstreams(enabled)`：逐个调 `/admin/upstream/update`，只更新状态有差异的。
- 校验：`node` 解析 `<script>` 通过；div/section 标签配平；`python selftest.py` 303 项全过；
  运行中的服务（127.0.0.1:8010）已返回新页面，无需重启。
- 未做视觉验证（没有浏览器截图）；下一步如需要可看实际列数/密度再调 `minmax` 宽度。

## 2026-10-12 文案与配色修正

- 「全部启动」→「全部启用」（与卡片上的「启用/停用」用词一致）。
- 请求记录 tab 角标不再因有失败而变红：410 是请求总数，染红会被误读成 410 个错误；
  失败数只在 tooltip 里说明。`setTabNum("req", nCalls, false, ...)`。
- 上游调用对比条：原来只要有失败就整条变红（414 次里错 16 次看起来像全挂了）。
  现在条长仍按调用量，条内按成功/失败比例分两段：`.track i.ok`（蓝）+ `.track i.fail`（红）。

