# llm-router desktop（Tauri 2 · 原生 Rust 后端）

单进程桌面应用：Tauri 窗口 + 内嵌 Rust HTTP 服务，**不需要 Python，也没有单独的 backend exe**。

窗口是无边框的，顶部标题栏由 webui 自己绘制（只在使用桌面壳传 `?native=1` 时显示），
支持拖动、最小化、最大化/还原、关闭；关闭按钮默认隐藏到系统托盘，可在设置页切换为最小化或退出。

当前原生核心已实现：

- 读取 `.llmrouter/config.state.json`（上游 / 模型路由 / 分组）
- `GET /v1/models`
- `POST /v1/chat/completions`：按 priority / round_robin / random 路由，失败自动换下一家，
  非流式与 SSE 流式都支持，响应里的 `model` 会改回客户端请求的名字
- `POST /v1/messages`：Anthropic 协议适配，Claude Code 等 Agent 直接把
  `ANTHROPIC_BASE_URL` 指到本服务即可（非流式 + SSE 事件流）
- `GET /health`
- `GET /stats`：复用现有 webui.html 的统计结构（读取 `config.stats.json`）
- 管理接口：上游增删改、拉取模型、勾选模型、分组映射、模型路由保存/删除/排序/额度、
  默认参数、测速、请求流水查询
- 模型路由页：左侧模型分组列表，右侧直接显示所选分组的调用方式、上游顺序与额度，
  保存/删除/改名都在右栏完成，不再弹窗
- Agent 扫描与一键接入：`GET /agents`、`GET /agents/scan`、
  `POST /agents/configure`、`POST /agents/restore`、`POST /agents/repair`
  （内置规则 + npm 全局包动态发现，15 个 Agent 可写，配置漂移检测 + 一键修回；
  导入 DeepSeek Harness 时可同时把 `agent-default-model` 钉到本机路由）
- 上游健康看板：`GET /health/upstreams`、`POST /health/upstreams/clear`
  （成功率 / P50-P95-P99 / 首字延迟 / 缓存用量 / 冷却倒计时 / 展开模型明细 / 清熔断）
- 会话对账：`GET /sessions/scan`、`GET /sessions/bases`
  （读 DSH / Claude Code / Pi / Codex 的会话记录，对照本机流水标出没走本机路由的调用）
- 图表：`GET /stats/series`（请求页的调用趋势折线、延迟直方图、模型调用量条形；
  健康看板的上游成功率条形与能力雷达；全部内联 SVG，不引第三方图表库）
- MCP / 工具调用排行榜：`GET /sessions/tools`（配对 DSH `tool/call` + `tool/result`
  与 Claude Code `tool_use` + `tool_result`，统计调用次数、失败率、P50 / P95、常见错误）
- 勾选式导入 / 导出：`GET /data/summary`、`POST /data/export`、`POST /data/import`
  （请求记录 / 上游来源 / 模型路由 / 签到，四类任选，支持 merge / replace、预览、脱敏、自动备份）
- 用量与耗时：流式请求注入 `stream_options.include_usage` 并在流结束落完整 usage，
  归一化各家缓存字段；流水同时记录总耗时、首字延迟、有效内容块数，请求列表显示 TPS（输出 token/秒）
- 启动页 / 移动端「服务地址」连接

## 目录

```
desktop/
├─ ui/index.html                 # 启动页：桌面自动等本地服务，移动端填服务地址
└─ src-tauri/
   ├─ src/main.rs                # 入口
   ├─ src/lib.rs                 # Tauri 生命周期 + 内嵌 HTTP 服务启动
   ├─ src/native/                # Rust 原生后端（state / proxy / admin / agents / anthropic / transfer / health / stats / journal）
   ├─ tauri.conf.json
   └─ capabilities/default.json
```

## Windows 构建

```powershell
cd desktop
npm install
npm run build
```

产物：

- `src-tauri/target/release/llm-router-desktop.exe` —— 免安装版（单文件，不需要 backend）
- `src-tauri/target/release/bundle/nsis/*.exe` —— 安装包

开发调试：

```powershell
npm run dev
```

## Agent 接入

控制台左侧「Agent」页（只在 `127.0.0.1` / `localhost` / 桌面壳 `?native=1` 下显示，
中继页面不显示）扫描 PATH、常见安装目录、配置目录、VS Code 扩展、开始菜单快捷方式，
**并且动态遍历 npm 全局包目录**（`%APPDATA%\npm\node_modules` 等，读每个 `package.json` 的
`name` / `version` / `bin` / `keywords`）——所以 Pi、Amp、Droid、Crush、Kilo Code、
CodeBuddy、Auggie、Copilot CLI、Command Code、OpenClaw 这类后来才装的 CLI 不用改代码也能被列出来。
已登记的 npm 包会与内置规则合并成一张卡片，未登记但像 coding agent 的包按「自动发现」列出：

| Agent | 写入位置 | 协议 |
| --- | --- | --- |
| Claude Code | `~/.claude/settings.json` 的 `env.ANTHROPIC_*` | Anthropic `/v1/messages` |
| Codex CLI | `~/.codex/config.toml` 的 `model_providers.llm-router` | OpenAI `/v1/chat/completions` |
| Aider | `~/.aider.conf.yml` | OpenAI |
| Qwen Code | `~/.qwen/settings.json` 的 `env.OPENAI_*` | OpenAI |
| OpenCode | `opencode.json` 的 `provider.llm-router` | OpenAI |
| Continue | `~/.continue/config.json` 的 `models[0]` | OpenAI |
| Cline / Roo Code | VS Code `settings.json` 的 `cline.*` / `roo-cline.*` | OpenAI |
| **Pi (Coding Agent)** | `~/.pi/agent/models.json` 的 `providers.llm-router` + `settings.json` 的 `defaultProvider` / `defaultModel` | OpenAI `openai-completions` |
| Droid / Factory | `~/.factory/settings.json` 的 `customModels[]` + 顶层 `model` | OpenAI `generic-chat-completion-api` |
| Amp | `~/.config/amp/settings.json` 的 `amp.url` + `~/.local/share/amp/secrets.json` 的 `apiKey@<URL>` | Amp 管理接口（+ `/v1/messages`） |
| Crush | `~/.config/crush/crushrc` 的 managed 命令块（`provider add` / `model add`） | OpenAI `completions` |
| CodeBuddy | `~/.codebuddy/models.json` 的 `models[]` + `settings.json` 的 `model` / `env.CODEBUDDY_*` | OpenAI（完整 `/chat/completions` 路径） |
| Kimi Code | `~/.kimi-code/config.toml` 的 `default_model` + `[providers.llm-router]` / `[models.*]` | OpenAI |
| ZCode | `~/.zcode/v2/config.json` 的 `provider.llm-router`（不覆盖 magpie） | Anthropic |
| WorkBuddy | `~/.workbuddy/models.json` 的自定义模型条目 | OpenAI（完整 `/chat/completions` 路径） |
| **DeepSeek Harness**（`dsh` / `dsh-tui`） | `~/.dsh/settings.yaml` 的 `llm-pi-ai.providers.llm-router` + `~/.dsh/profiles/*/cordis.patch.yml` 的 `providers.llmrouter` + `~/.dsh/.env` 的 `LLMROUTER_API_KEY` | OpenAI `openai-completions` |
| Gemini CLI / Cursor / Windsurf / Trae / Goose / Auggie / Copilot CLI / Command Code / OpenClaw / 其它 npm CLI | 只识别，不自动写入 | 用「复制通用配置」手动填 |

Pi 的一键导入会同时改两个文件（可用 `PI_CODING_AGENT_DIR` 改配置目录）：
`models.json` 里新增 `api: "openai-completions"` 的 `llm-router` provider，`baseUrl` 带 `/v1`；
`settings.json` 把默认模型切到当前路由里选中的那个。两个文件各自备份，恢复时一起还原。

Droid / Amp / Crush / CodeBuddy / Kimi Code / ZCode / WorkBuddy / DeepSeek Harness 的写入模板来自
各 CLI 自带文档（键名见 `native/agents.rs` 的 `write_template_plan`）：都只追加 / 替换 llm-router
自己的条目或 managed 块，**不整体覆盖**用户已有配置——Droid / CodeBuddy 保留 `hooks`，Kimi Code
不动 Orca 的 `[[hooks]]` 托管块，Crush 的 `crushrc` 是可执行 shell 所以写入值只保留安全字符，
DSH 只往 `providers` 尾部追加一个 `llm-router:` 节点（已有的 `magpie` / `github-copilot` 等
provider 原样保留），并把 `LLM_ROUTER_KEY` 写进 `~/.dsh/.env`。
Amp / CodeBuddy / DeepSeek Harness 同样是两个文件，全部各自备份、一起恢复。
Amp / ZCode / Crush / WorkBuddy 的默认模型需在各自 UI 里选，卡片上有说明；
DeepSeek Harness 重启 `dsh web` / `dsh-tui` 后即可在模型选择器里看到「LLM Router」这一组模型。

### DeepSeek Harness 的 patch 层与默认模型

桌面版 DSH 真正读的路由不是 `settings.yaml`，而是 profile 的 patch 层
`~/.dsh/profiles/<profile>/cordis.patch.yml`（`llm-pi-ai` 条目下的 `providers`），
`agent-default-model` 条目则决定默认用哪个 provider / 模型——别的网关（如 Magpie）就是靠
周期性重写这个文件把 DSH 拉回自己那边的。所以导入 DSH 时：

1. `~/.dsh/settings.yaml` 追加 `llm-router:` provider（保留 magpie 等已有 provider）；
2. `~/.dsh/profiles/*/cordis.patch.yml` 里写入 / 替换 `llmrouter:` provider 节点
   （只动我们自己的 `# >>> llm-router managed >>>` 块，其余条目原样保留）；
3. 勾上「同时切默认模型」时，把 `agent-default-model` 的 `provider: llmrouter`、
   `model: <所选模型>` 改过去（条目不存在就追加一条），请求体里对应 `pin_default: true`；
4. `~/.dsh/.env` 写 `LLM_ROUTER_KEY` / `LLMROUTER_API_KEY`（占位 Key 时是 `llm-router`）。

钉过默认模型之后，如果 patch 层又被别的工具改回去（`agent-default-model` 不再是 `llmrouter`），
Agent 卡片会显示「默认模型 magpie/loomy/DeepSeek-V4.1-Flash（没走本机路由）」并标「配置已漂移」，
点「一键修回」即可按记录重新钉回；Magpie 这类网关会每隔几分钟重写一次
`profiles/tauri/cordis.patch.yml` 把默认模型拉回自己那边，所以「漂移 → 一键修回」可能要重复几次，
真正长期生效可以在 Magpie 里关掉它改写 DSH 路由的行为。改完需要重启 DSH 才生效。

### 配置漂移检测

每次真实导入会把「Agent id → 模型 / 地址 / 时间 / 文件清单」记到
`.llmrouter/agent-applied.json`；重新扫描时对比，出现「不再指向本机」「模型被改走」
就标红「配置已漂移」，并显示上次导入的模型与时间，点卡片上的「一键修回」
（`POST /agents/repair`）按记录重新写入。点「恢复原配置」会同时删掉该记录。

用法：

1. 打开「Agent」→「重新扫描」，列表里会标出识别依据（`npm 全局包 …@版本` / `命令 …` /
   `可执行文件 …` / `配置 …` / `开始菜单 …`）和当前配置；可用「只看已识别」与搜索框过滤。
2. 在「导入模型」里选一个当前 `/v1/models` 里的模型（和「模型路由」页同源）。
3. 点某个 Agent 的「一键导入此 Agent」，或先选中再点右上角「一键导入选中 Agent」。
   勾「只预览，不写文件」可先看生成内容；正式写入前每个文件都会在同目录生成
   `<原文件名>.llmrouter.bak`，点「恢复原配置」即可还原（Pi 会还原两个文件，
   恢复成功后 `.llmrouter.bak` 会被清掉，不留残留）。
4. 「复制通用配置」把 `OPENAI_*` / `ANTHROPIC_*` 环境变量复制到剪贴板，给未适配自动写入的 Agent 用。

路由 `/v1/*` 不校验客户端 API Key，配置里用占位 Key `llm-router` 即可；
真正的上游 Key 仍在状态文件里由服务端管理。Claude Code 走 Anthropic 协议，
所以 `ANTHROPIC_BASE_URL` 要填不带 `/v1` 的地址（`http://127.0.0.1:8010`），
其余 OpenAI 兼容 Agent 填带 `/v1` 的地址（`http://127.0.0.1:8010/v1`）。

## 上游健康看板

控制台「健康看板」页（同样只在本地 / 桌面壳显示）按 `config.history.jsonl` 聚合每个上游的：
尝试数 / 成功 / 失败 / 成功率、总耗时 P50 / P95 / P99 / 最大值、首字延迟 P50 / P95、
缓存读 / 缓存写 / 输入 / 输出 token、连续失败次数、冷却倒计时、最后一次错误与调用时间。
每张上游卡片点「展开模型明细」可把汇总按模型拆成一张小表（各模型的尝试 / 成功 / 失败 /
成功率 / P50 / P95 / 首字 P50 / 缓存读 / 缓存写），用来判断是整条上游都慢、还是只有某一个模型慢。
时间窗口默认「今天」（按本机时区当天 00:00 起算，不是滚动 24 小时），也可切 1 / 6 / 24 / 72 / 168 小时与「全部」，
可勾「只看异常」（有失败 / 冷却中 / 已禁用 / 配置已删的孤儿记录）。
「清除熔断」把内存里的连续失败计数与 60 秒冷却清零（阈值 3 次），不改状态文件。

聚合来源是流水里的 `trail[]`（每次上游尝试一条），所以重试、切换上游都算得清；
一条请求的 `usage`（缓存读 / 缓存写 / 输入 / 输出）与 `first_chunk_seconds` 只记在
**最后一次成功的尝试**上（全失败时记最后一次），避免按尝试平摊把缓存算丢。
配置里存在但近期没调用的上游也会列空行，流水里有但配置已删的标 `orphan`。

```
GET  /health/upstreams?range=today&limit=4000
GET  /health/upstreams?hours=24&limit=4000
POST /health/upstreams/clear  {"id":"..."} 或 {"all":true}
```

## 会话对账（找没走本机路由的调用）

控制台「会话对账」页直接读各家 Agent 自己的会话记录，把每一轮模型调用按
**provider / 模型**汇总，再和本机 `8010` 的落盘流水对照，标出哪些调用绕过了 llm-router：

| 数据源 | 会话记录位置 | 落点字段 |
| --- | --- | --- |
| DeepSeek Harness | `~/.dsh/sessions/<项目>/<会话>/session.v4.jsonl.zstd`（多 zstd 帧，逐个解码拼接） | `assistant/message` 的 `message.source.{provider,model}` + `usage.{inputTokens,outputTokens,totalTokens,cacheReadTokens}` |
| Claude Code | `~/.claude/projects/<项目>/<会话>.jsonl` | `message.model` + `message.usage.{input_tokens,output_tokens,cache_read_input_tokens,cache_creation_input_tokens}` |
| Pi | `~/.pi/agent/sessions/<项目>/<ts>_<id>.jsonl` | `message.{provider,model,usage,timestamp,durationMs}` |
| Codex | `~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl` | `session_meta.payload.model_provider` + `token_usage_record.payload.usage.*` |

「是否走路由」的判断分两步：先看 provider 名，再查该 provider 在**本机各 Agent 配置里登记的
baseURL**（`~/.dsh/settings.yaml` 与 profile patch 层、`~/.pi/agent/models.json`、
`~/.claude/settings.json`、`~/.codex/config.toml`、`opencode.json`、`kimi-code/config.toml`）——
指向 `127.0.0.1:8010` / 含 `llm-router` 才算走路由。所以别人的网关把 DSH 默认模型改回自己那边之后，
这一页会立刻显示「未走路由」的调用轮数与 token，并按会话列出外部模型。

大文件只读尾部（默认最后 48MiB / 每个数据源最多 400 个文件），窗口默认「今天」（本机时区当天 00:00 起），
也可切 24h / 3 天 / 7 天 / 30 天 / 全部，可勾「只看未走路由」。
扫描在打开本页、切换筛选条件或点「重新扫描」时触发；页面可见时跟随「设置 → 界面刷新 →
会话对账刷新间隔」自动重扫（默认 5 秒，范围 2–600 秒，独立于请求记录的间隔，只在本页可见且没有正在扫描时执行）。
页面只读，不改任何配置。

页面顶部是总览卡：调用轮数、未走路由、路由覆盖率、输入 / 输出 / 缓存读写和本机流水轮数；下面是路由覆盖比例条、四个数据源的扫描卡片，再往下是 provider / 模型明细表与会话卡片。筛选条支持时间窗口、数据源多选、只看未走路由，以及按 provider / 模型 / 接口地址搜索。

```
GET /sessions/scan?range=today&sources=dsh,claude,pi,codex&limit=200
GET /sessions/scan?hours=168&sources=dsh,claude,pi,codex&limit=200
GET /sessions/bases
```

## 图表与 MCP 工具排行榜

- **请求记录页 → 趋势与分布**：调用 / 失败折线（后端按窗口自动分桶）、延迟直方图
  （≤0.5s、0.5–1s、1–2s、2–5s、5–10s、10–30s、30–60s、>60s）、模型调用量 Top 8。
  图表窗口独立于下方列表筛选，默认「今天」。
- **健康看板**：上游成功率 Top 10 横向条形；单个上游对比「全部平均」的能力雷达
  （成功率 / 响应速度 / 首字速度 / 稳定性 / 缓存复用五个维度）。
- **独立「MCP 工具」页**：调用次数 Top 10、平均耗时 Top 10，
  以及逐工具的失败率、P50 / P95、涉及会话数和常见错误。数据来自 DSH 的
  `tool/call` / `tool/result` 与 Claude Code 的 `tool_use` / `tool_result` 配对；
  与会话对账拆成左侧两个 tab，切换查看，避免单页过长。该页与会话对账共用同一个
  自动刷新间隔（同样只在页面可见时刷新）。
- **请求瀑布**：展开任一请求，在「请求瀑布」区按时间轴画出每次上游尝试（成功/失败）
  的耗时占比；没有 trail 的旧记录按「首字等待 / 生成」两段拆分。
- 直方图复用同一组 SVG 柱子做增量过渡，自动刷新时不再整块重建闪一下。
- **托盘迷你 HUD**：托盘图标悬浮显示今日请求量 / 成功率 / P50；左键打开无边框、
  置顶、可拖动的迷你 HUD（同样四格指标，3 秒刷新），右键菜单也能打开或退出。
- 图表全部用内联 SVG 绘制，不引第三方图表库；切页、柱状、折线、雷达、状态灯都有
  轻量 CSS 动画，并遵守系统 `prefers-reduced-motion` 设置。

```
GET /stats/series?range=today
GET /stats/series?hours=24
GET /sessions/tools?range=today&sources=dsh,claude&limit=50
```

## 路由策略与会话粘性

模型路由目前有 `priority`（按配置顺序，冷却中的排后）、`random`、`round_robin` 三种策略，
**每次请求都独立选路**：同一个会话的上一轮可能落到上游 A，下一轮轮询到 B 或随机到 C。
失败时会按候选列表继续尝试下一家，但不会记住「这个会话上次在哪家成功」。

「会话级粘性路由」就是给一段会话（例如带 `X-Session-Id` 的请求，或按首条消息哈希）
记住上次成功的上游，在 TTL 内优先回到同一家，只有它失败或冷却时才切换。好处是能复用
上游的 KV / prompt cache，长对话的首字延迟和缓存读会明显更稳；代价是负载不再均匀，
热门会话会集中在一家，而且某家变慢时要等失败或 TTL 到期才会迁走。适合长上下文对话，
不适合纯短请求的高吞吐场景。

## 耗时与缓存用量口径

请求流水列表里标注的「总耗时」是 `elapsed_seconds`：从收到请求到整个响应结束，
**包含所有上游重试与流式输出全过程**；「首字」是 `first_chunk_seconds`：首个内容块到达，
非流式请求里两者相等。`trail[].elapsed_seconds` 是单次尝试耗时。旧流水没有首字字段，
所以升级前的记录只显示总耗时，升级后新写的流水才有首字与「有效内容块」数。

请求列表的 `TPS` badge 是输出 token / 秒，按口径分两种：新流水（有首字延迟）显示
**生成速度** = `output ÷ (elapsed_seconds - first_chunk_seconds)`，即扣掉首字等待后的纯生成阶段，
最贴近体感；旧流水没有首字字段时退回 **全链路速度** = `output ÷ elapsed_seconds`（含首字等待与重试）。
悬浮在 badge 上可看到计算公式，展开请求的「耗时」区同时列出这两个值。
生成窗口小于 50ms 或算出超过 1000 tok/s（上游把整段一次性吐回、首字≈总耗时）时视为不可测，
自动退回全链路口径，避免出现几万 tok/s 的失真数字。没有输出 token 或耗时的记录不显示 TPS。

流式请求现在会自动注入 `stream_options.include_usage`，并在流结束后把上游最后一帧里的
`usage` 写进流水（旧实现首包就落 `usage: {}`，所以缓存读 / 写一直是空的）。
各家上游的原始字段名不一样，`native/stats.rs` 的 `normalize_usage` 统一归一成
`input` / `output` / `total` / `cache_read` / `cache_write` 五个标准键，
识别 `prompt_tokens_details.cached_tokens`、`cache_read_input_tokens`、
`cache_creation_input_tokens`、`claude_cache_creation_1_h_tokens` / `_5_m_tokens`、
`billing_usage.claude_usage.*`、`opencode.tokens.*` 等别名。
流水里既保留上游原始 usage，也补上这五个键；统计页「缓存写」与分组卡的缓存列读的是同一套键。

## 窗口行为

默认点右上角关闭按钮是**隐藏到系统托盘**，后台 HTTP 服务继续工作（客户端调用不中断）。托盘图标左键打开/收起迷你 HUD，右键菜单可打开迷你 HUD、显示主界面或退出。
在「设置」页最上方的「窗口行为」里可以改成「最小化到任务栏」或「退出程序」；设置存在状态文件的
`window_behavior` 字段（`tray` / `minimize` / `close`），保存后立即生效。

## 界面刷新间隔

设置页「界面刷新」有两个独立间隔，互不影响，都存在当前浏览器 localStorage：

| 设置项 | 默认 | 范围 | 作用 |
| --- | --- | --- | --- |
| 请求记录刷新间隔 | 5 秒 | 1–120 秒 | 控制台状态与请求记录的兜底轮询（长连接正常时增量由 WS 推送） |
| 会话对账刷新间隔 | 5 秒 | 2–600 秒 | 「会话对账」页重新扫描各家 Agent 会话文件的节奏（页面可见时才扫描） |

会话对账每次都要读 DSH / Claude Code / Pi / Codex 的会话文件，比轮询重，建议不低于 5 秒。
设置存在浏览器 `localStorage`，按设备保存（桌面和手机可以分别设）；手机走 Tailscale
中继时建议 5–10 秒。

## 数据和端口

默认状态文件：exe 所在目录向上 4 层内找到 `.llmrouter/config.state.json` 就直接用
（把免安装版放在仓库 `desktop/dist/` 时会自动接上仓库现有数据）；找不到才用
`%APPDATA%\com.llmrouter.desktop\.llmrouter\config.state.json`（首次启动为空，
网页控制台添加上游即可）。

想显式指定状态文件位置（比如迁移过来的旧数据），启动前设：

```powershell
$env:LLM_ROUTER_STATE = "C:\path\to\llm-router\.llmrouter\config.state.json"
$env:LLM_ROUTER_PORT = "18010"       # 避开已经在跑的 8010
npm run dev
```

## 导入 / 导出（设置页最下方卡片）

四个 checkbox 分别对应：

| 类别 | 导出内容 | 落盘位置 |
| --- | --- | --- |
| 请求记录 | `config.history.jsonl` 全部流水 + `config.stats.json` 统计快照 | 同上 |
| 上游来源 | `config.state.json` 的 `upstreams`（含 Key、分组、勾选模型） | 同上 |
| 模型路由 | `models` 路由表 + `suppressed_models` | 同上 |
| 签到 | `checkin_settings`、每个上游的 `checkin` / `checkin_state`、`config.checkin.json` 签到流水 | 同上 |

- 导出成单个 `llm-router-backup-时间戳.json`，字段 `format: "llm-router-backup"` / `format_version: 1`。
  「请求记录最近 N 条」填 0 即全量（本机流水约 10MB），只想留个底就填 2000。
- **导出包含明文 API Key 与签到 token**，别传公共仓库；勾「Key 打码」会把 `api_keys` 和
  `checkin.token` 换成掩码串（打码后的包再导回去，Key 也变成掩码，需要重新填）。
- 导入支持「合并」（按上游 id / 路由名更新，流水追加、签到流水去重）和「覆盖」（用包里的替换）。
  「预览变更」只回报会改多少条、不碰文件；正式导入前给每个要改的文件留
  `<原名>.bak-import-时间戳` 备份，需要回滚直接把备份改名回去。
- 旧版 `migrate.py export` 生成的 `bundle.json`（`schema: "llm-router-migrate/1"`）也能直接导，
  会自动把 `state.upstreams` / `models` / `checkin_settings` + `checkin_log` 映射过来；
  旧包没有请求流水，导入时即使勾了「请求记录」也不会清空现有流水。
- 只在 `127.0.0.1` / `localhost` / 桌面壳 `?native=1` 显示；中继 Worker 不代理 `/data/*`。

对应接口：

```
GET  /data/summary                      # 四类数据当前条数 + 文件路径
POST /data/export  {parts, journal_limit, mask_secrets}
POST /data/import  {payload, parts, mode: "merge"|"replace", dry_run}
```

## 原生迁移进度

已实现：上游增删改 / 拉取模型 / 勾选与分组 / 模型路由保存、排序、额度 / 默认参数 /
测速（短请求）/ `/stats` 统计读取 / 请求流水查询 / `/v1/models` /
`/v1/chat/completions`（priority、round_robin、random，流式 + 失败切换 + 模型名回写）/
`/v1/messages`（Anthropic 适配 + SSE）/ Agent 扫描、15 个 Agent 一键导入、
配置漂移检测与一键修回、备份恢复（含 DSH profile patch 层与默认模型钉定）/
上游健康看板（成功率、P50-P99、首字延迟、清熔断）/
会话对账（读 DSH / Claude Code / Pi / Codex 会话记录，标出没走本机路由的调用）/
请求趋势 / 延迟直方图 / 模型调用量 / 上游成功率条形 / 能力雷达（内联 SVG）/
MCP 工具调用排行榜（调用次数、失败率、P50-P95、常见错误）/ 轻量界面动画/
流式 usage 采集与缓存读/写归一化 / 请求记录 / 上游 / 模型路由 / 签到
四类数据的勾选式导入导出（预览、合并/覆盖、脱敏、自动备份）/
签到页配置读取与保存 + New API / One API 常见签到路径 + 每日自动签到调度
（基准时刻 + 随机窗口，服务重启后当天未签会补签）。

仍继续补齐：签到奖励汇率换算、代理出口探测/网络测速、
历史版本恢复、余额查询、Amp / ZCode 默认模型的 UI 直写。
这些页面目前返回空结果或占位提示，不影响核心转发。

## 远程访问（Tailscale）

设置页 →「远程访问（Tailscale）」勾选保存 → 重启应用，服务会从只监听 `127.0.0.1`
改为监听 `0.0.0.0`，手机 / 平板即可访问。

步骤：

1. 这台电脑和手机都安装 [Tailscale](https://tailscale.com/download)，登录**同一账号**
2. 电脑上确认 Tailscale 托盘图标是已连接状态
3. 回到控制台「设置 → 远程访问」，点「刷新状态」，会显示可用地址：
   - `http://<机器名>.<tailnet>.ts.net:8010`
   - `http://100.x.x.x:8010`
4. 手机浏览器打开该地址即可看到上游和模型路由；Tauri 手机壳则在启动页填这个地址

注意：

- 控制台**没有登录鉴权**，只建议走 Tailscale 私网，不要做公网端口映射
- 首次开启时 Windows 防火墙可能弹窗，需要允许专用网络访问
- 关闭开关并重启后恢复为只监听本机

## 移动端

移动端只是壳，连远端服务（原生 Rust 服务只在桌面运行）：

```bash
cd desktop
npm run tauri android init     # 需要 Android SDK
npm run tauri ios init         # 需要 macOS + Xcode
```

首次打开填服务地址（例如 Tailscale 的 `http://my-pc.tailxxxx.ts.net:8010`），
保存后下次自动连。控制台没有登录鉴权，只走 Tailscale/局域网，别直接映射公网。

iOS/Android 访问纯 HTTP 需要在平台工程里允许明文流量：
Android 在 `AndroidManifest.xml` 的 `<application>` 上加
`android:usesCleartextTraffic="true"`；iOS 在 `Info.plist` 配 `NSAppTransportSecurity`。
