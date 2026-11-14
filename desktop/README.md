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
- 本机 Agent 扫描与一键接入：`GET /agents`、`GET /agents/scan`、
  `POST /agents/configure`、`POST /agents/restore`、`POST /agents/repair`
  （内置规则 + npm 全局包动态发现，15 个 Agent 可写，配置漂移检测 + 一键修回）
- 上游健康看板：`GET /health/upstreams`、`POST /health/upstreams/clear`
  （成功率 / P50-P95-P99 / 首字延迟 / 缓存用量 / 冷却倒计时 / 展开模型明细 / 清熔断）
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

## 本机 Agent 接入

控制台左侧「本机 Agent」页（只在 `127.0.0.1` / `localhost` / 桌面壳 `?native=1` 下显示，
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
| **DeepSeek Harness**（`dsh` / `dsh-tui`） | `~/.dsh/settings.yaml` 的 `llm-pi-ai.providers.llm-router` + `~/.dsh/.env` 的 `LLM_ROUTER_KEY` | OpenAI `openai-completions` |
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

### 配置漂移检测

每次真实导入会把「Agent id → 模型 / 地址 / 时间 / 文件清单」记到
`.llmrouter/agent-applied.json`；重新扫描时对比，出现「不再指向本机」「模型被改走」
就标红「配置已漂移」，并显示上次导入的模型与时间，点卡片上的「一键修回」
（`POST /agents/repair`）按记录重新写入。点「恢复原配置」会同时删掉该记录。

用法：

1. 打开「本机 Agent」→「重新扫描」，列表里会标出识别依据（`npm 全局包 …@版本` / `命令 …` /
   `可执行文件 …` / `配置 …` / `开始菜单 …`）和当前配置；可用「只看已识别」与搜索框过滤。
2. 在「导入模型」里选一个当前 `/v1/models` 里的模型（和「模型路由」页同源）。
3. 点某个 Agent 的「一键导入此 Agent」，或先选中再点右上角「一键导入选中 Agent」。
   勾「只预览，不写文件」可先看生成内容；正式写入前每个文件都会在同目录生成
   `<原文件名>.llmrouter.bak`，点「恢复原配置」即可还原（Pi 会还原两个文件）。
4. 「复制通用配置」把 `OPENAI_*` / `ANTHROPIC_*` 环境变量复制到剪贴板，给未适配自动写入的 Agent 用。

路由 `/v1/*` 不校验客户端 API Key，配置里用占位 Key `llm-router` 即可；
真正的上游 Key 仍在状态文件里由服务端管理。Claude Code 走 Anthropic 协议，
所以 `ANTHROPIC_BASE_URL` 要填不带 `/v1` 的地址（`http://127.0.0.1:8010`），
其余 OpenAI 兼容 Agent 填带 `/v1` 的地址（`http://127.0.0.1:8010/v1`）。

## 上游健康看板

控制台「健康看板」页（同样只在本地 / 桌面壳显示）按 `config.history.jsonl` 聚合每个上游的：
尝试数 / 成功 / 失败 / 成功率、总耗时 P50 / P95 / P99 / 最大值、首字延迟 P50 / P95、
缓存读 / 缓存写 token、连续失败次数、冷却倒计时、最后一次错误与调用时间。
每张上游卡片点「展开模型明细」可把汇总按模型拆成一张小表（各模型的尝试 / 成功 / 失败 /
成功率 / P50 / P95 / 首字 P50），用来判断是整条上游都慢、还是只有某一个模型慢。
时间窗口选 1 / 6 / 24 / 72 / 168 小时或「全部」，
可勾「只看异常」（有失败 / 冷却中 / 已禁用 / 配置已删的孤儿记录）。
「清除熔断」把内存里的连续失败计数与 60 秒冷却清零（阈值 3 次），不改状态文件。

聚合来源是流水里的 `trail[]`（每次上游尝试一条），所以重试、切换上游都算得清；
配置里存在但近期没调用的上游也会列空行，流水里有但配置已删的标 `orphan`。

```
GET  /health/upstreams?hours=24&limit=4000
POST /health/upstreams/clear  {"id":"..."} 或 {"all":true}
```

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

默认点右上角关闭按钮是**隐藏到系统托盘**，后台 HTTP 服务继续工作（客户端调用不中断）。托盘图标左键恢复窗口，右键菜单可显示主界面或退出。
在「设置」页最上方的「窗口行为」里可以改成「最小化到任务栏」或「退出程序」；设置存在状态文件的
`window_behavior` 字段（`tray` / `minimize` / `close`），保存后立即生效。

## 界面刷新间隔

设置页「界面刷新」可改控制台的轮询间隔，默认 **5 秒**，范围 1–120 秒。
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
`/v1/messages`（Anthropic 适配 + SSE）/ 本机 Agent 扫描、15 个 Agent 一键导入、
配置漂移检测与一键修回、备份恢复 / 上游健康看板（成功率、P50-P99、首字延迟、清熔断）/
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
