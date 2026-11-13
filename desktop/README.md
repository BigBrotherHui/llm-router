# llm-router desktop（Tauri 2 · 原生 Rust 后端）

单进程桌面应用：Tauri 窗口 + 内嵌 Rust HTTP 服务，**不需要 Python，也没有单独的 backend exe**。
`router.py` 的 Python 版仍保留在仓库根目录，作为过渡/对照。

当前原生核心已实现：

- 读取 `.llmrouter/config.state.json`（上游 / 模型路由 / 分组）
- `GET /v1/models`
- `POST /v1/chat/completions`：按 priority / round_robin / random 路由，失败自动换下一家，
  非流式与 SSE 流式都支持，响应里的 `model` 会改回客户端请求的名字
- `GET /health`
- `GET /stats`：复用现有 webui.html 的统计结构（读取 `config.stats.json`）
- 管理接口：上游增删改、拉取模型、勾选模型、分组映射、模型路由保存/删除/排序/额度、
  默认参数、测速、请求流水查询
- 启动页 / 移动端「服务地址」连接

## 目录

```
desktop/
├─ ui/index.html                 # 启动页：桌面自动等本地服务，移动端填服务地址
└─ src-tauri/
   ├─ src/main.rs                # 入口
   ├─ src/lib.rs                 # Tauri 生命周期 + 内嵌 HTTP 服务启动
   ├─ src/native/                # Rust 原生后端（state / proxy / admin / stats / journal）
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

## 窗口行为

默认点右上角关闭按钮是**最小化到任务栏**，后台 HTTP 服务继续工作（客户端调用不中断）。
在「设置」页最上方的「窗口行为」里可以改成「退出程序」；设置存在状态文件的
`window_behavior` 字段（`minimize` / `close`），保存后立即生效。

## 数据和端口

默认状态文件：exe 所在目录向上 4 层内找到 `.llmrouter/config.state.json` 就直接用
（把免安装版放在仓库 `desktop/dist/` 时会自动接上仓库现有数据）；找不到才用
`%APPDATA%\com.llmrouter.desktop\.llmrouter\config.state.json`（首次启动为空，
网页控制台添加上游即可）。

想加载已有的 Python 版状态做迁移/对照，启动前设：

```powershell
$env:LLM_ROUTER_STATE = "C:\path\to\llm-router\.llmrouter\config.state.json"
$env:LLM_ROUTER_PORT = "18010"       # 避开已经在跑的 8010
npm run dev
```

## 原生迁移进度

已实现：上游增删改 / 拉取模型 / 勾选与分组 / 模型路由保存、排序、额度 / 默认参数 /
测速（短请求）/ `/stats` 统计读取 / 请求流水查询 / `/v1/models` /
`/v1/chat/completions`（priority、round_robin、random，流式 + 失败切换 + 模型名回写）/
签到页配置读取与保存 + New API / One API 常见签到路径 + 每日自动签到调度
（基准时刻 + 随机窗口，服务重启后当天未签会补签）。

仍按 Python 版行为继续补齐：签到奖励汇率换算、代理出口探测/网络测速、
历史版本恢复、余额查询。这些页面在原生版里目前返回空结果或占位提示，不影响核心转发。

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
