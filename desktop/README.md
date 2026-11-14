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
  `POST /agents/configure`、`POST /agents/restore`（内置规则 + npm 全局包动态发现，Pi 双文件写入）
- 勾选式导入 / 导出：`GET /data/summary`、`POST /data/export`、`POST /data/import`
  （请求记录 / 上游来源 / 模型路由 / 签到，四类任选，支持 merge / replace、预览、脱敏、自动备份）
- 启动页 / 移动端「服务地址」连接

## 目录

```
desktop/
├─ ui/index.html                 # 启动页：桌面自动等本地服务，移动端填服务地址
└─ src-tauri/
   ├─ src/main.rs                # 入口
   ├─ src/lib.rs                 # Tauri 生命周期 + 内嵌 HTTP 服务启动
   ├─ src/native/                # Rust 原生后端（state / proxy / admin / agents / anthropic / transfer / stats / journal）
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
| Gemini CLI / Cursor / Windsurf / Trae / Goose / ZCode / Kimi Code / 其它 npm CLI | 只识别，不自动写入 | 用「复制通用配置」手动填 |

Pi 的一键导入会同时改两个文件（可用 `PI_CODING_AGENT_DIR` 改配置目录）：
`models.json` 里新增 `api: "openai-completions"` 的 `llm-router` provider，`baseUrl` 带 `/v1`；
`settings.json` 把默认模型切到当前路由里选中的那个。两个文件各自备份，恢复时一起还原。

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
`/v1/messages`（Anthropic 适配 + SSE）/ 本机 Agent 扫描、一键导入、备份恢复 /
请求记录 / 上游 / 模型路由 / 签到 四类数据的勾选式导入导出（预览、合并/覆盖、脱敏、自动备份）/
签到页配置读取与保存 + New API / One API 常见签到路径 + 每日自动签到调度
（基准时刻 + 随机窗口，服务重启后当天未签会补签）。

仍继续补齐：签到奖励汇率换算、代理出口探测/网络测速、
历史版本恢复、余额查询。这些页面目前返回空结果或占位提示，不影响核心转发。

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
