# tools/debug —— llm-router 链路诊断脚本

这一组脚本是排查「客户端（DSH harness / 桌面 App）→ billion-context 插件 → llm-router → 上游中转」这条链路时用过的工具，全部是 Node 单文件脚本，无依赖（`node xxx.mjs` 直接跑）。

## 链路与本次案例（2026-11-15 的 `max_completion_tokens` 400）

```
DSH harness ──(pi-ai adapter)──> billion-context 插件 ──> llm-router (127.0.0.1:8010) ──> 上游中转 ──> 模型
```

- 报错：`400 invalid_parameter_error: Range of max_completion_tokens should be [1, 393216]`（ModelScope 校验）。
- 根因：**billion-context（bili）的 #3438「dsh 车道 mid-band collapse rescue」把输出预算抬回它认到的 known output ceiling（1048576），再按 `窗口 − 输入` 夹一次** → 出站 `max_completion_tokens` 变成 992918 / 1018356，越过上游 393216 上限。改 DSH 侧 `maxTokens`（393216 / 131072）全部无效，因为「小于 ceiling」正是触发抬升的条件。
- 判定证据：`~/.local/state/billion-context/bili.log` 里的 `output budget restored X -> Y` / `clamped Y -> Z`；抓包（`_cap_proxy.mjs`）看到真实 wire 体。
- 修复：llm-router 出站侧夹取（`desktop/src-tauri/src/native/proxy.rs` 的 `output_token_cap()` + `clamp_output_tokens()`），模型组声明 `max_output_tokens` 后生效：`node tools/debug/_set_cap.mjs deepseek 393216`。

## 脚本索引

| 脚本 | 作用 | 用法 / 常用参数 |
| --- | --- | --- |
| `_cap_proxy.mjs` | 日志反向代理：把客户端请求原文落盘后原样转发给 router，记录 payload 关键字段 + 全部请求头 + 上游状态 | `CAP_LISTEN=8321 CAP_UPSTREAM=http://127.0.0.1:8010 node tools/debug/_cap_proxy.mjs`；配合把 DSH provider 的 `baseURL` 临时指向它 |
| `_set_cap.mjs` | 往 `.llmrouter/config.state.json` 的模型组写 `max_output_tokens`（router 出站夹取的上限来源），写入前自动备份 | `node tools/debug/_set_cap.mjs deepseek 393216` |
| `_prob2.mjs` | 直连上游中转发探针（a/b/c/d 变体） | `node tools/debug/_prob2.mjs` |
| `_probe3.mjs` | 经 router 发多形态探针（minimal / store:false / reasoning_effort / tools / all / +max_tokens） | `node tools/debug/_probe3.mjs` |
| `_prob4.mjs` | **prompt 体积阶梯**探针：固定输出预算，把输入放大到 20K/50K/100K tokens，看是否触发参数校验 400 | `MAXC=131072 node tools/debug/_prob4.mjs` |
| `_prob5.mjs` | **字段形状 fuzz**：越界值 / 0 / 负数 / null / 字符串 / 小数 / 双字段并存，判定某条报错文案到底由什么条件触发 | `node tools/debug/_prob5.mjs` |
| `_bili_log.mjs` | 打印 `bili.log` 里的输出预算决策行（`output budget restored/clamped`、`[headroom]`、`[window-change]`、`[degenerate-turn]`） | `node tools/debug/_bili_log.mjs` |
| `_bili_find.mjs` | 在 `billion-context` 包内按关键字定位并打印上下文（查常量、查函数体） | `node tools/debug/_bili_find.mjs <关键字>` |
| `_bili_cfg.mjs` / `_bili_ceiling.mjs` / `_bili_ceiling2.mjs` | 读 bili 自己的配置与 ceiling 解析结果 | `node tools/debug/_bili_cfg.mjs` |
| `_bili_sess.mjs` | 打印 bili 会话存储里 window / output / budget 相关键 | `node tools/debug/_bili_sess.mjs` |
| `_bili_writers.mjs` | 查看 bili 的 `writers/` 状态目录 | `node tools/debug/_bili_writers.mjs` |
| `_dbg_unz.js` / `_dbg_game_unz.js` / `_dbg_myunz.mjs` | **多帧 zstd 解压** DSH 会话 `session.v4.jsonl.zstd`（单次 `zstdDecompressSync` 只能解出首帧） | `node tools/debug/_dbg_myunz.mjs <会话目录或文件> <输出>` |
| `_dbg_types.mjs` | 会话记录类型清单 + 关键字面量计数（判断某字段是否真出现在会话里） | `node tools/debug/_dbg_types.mjs <解压出的 jsonl>` |
| `_dbg_hdr.mjs` | 打印 `request/header`（provider/model/reasoningEffort/maxTokens + adapterDefaults）与 `model/selection` | `node tools/debug/_dbg_hdr.mjs <jsonl>` |
| `_dbg_tail.js` | 打印会话尾部若干条记录 | `node tools/debug/_dbg_tail.js <jsonl> [n]` |
| `_dbg_sessions.mjs` | 把 router `config.history.jsonl` 按 session 分组（谁在用 router、哪些失败） | `node tools/debug/_dbg_sessions.mjs` |
| `_dbg_hooks.mjs` | 列出会话里的 hooks（是否有人改写请求） | `node tools/debug/_dbg_hooks.mjs <jsonl>` |
| `_find_window.mjs` | 在若干目录树里搜一个字面量（如 `1049000`），定位「这个数字到底从哪来」 | `node tools/debug/_find_window.mjs 1049000` |
| `_find_lr.mjs` / `_grep1049000.mjs` / `_cfg_scan.mjs` | 在 `.dsh` / `@deepseek-ai` 包树 / 配置里按关键字定位 | `node tools/debug/_find_lr.mjs llm-router` |
| `_presets.mjs` | 查 litellm 预设表（`~/.dsh/dsh-tauri-model/model-presets.json`）里某模型的 context/output | `node tools/debug/_presets.mjs deepseek` |
| `_dump_probe.mjs` | 解析 `dsh --profile <p> --dump-config` 的输出：新进程实际读到的模型卡片 | `dsh --profile tauri --dump-config > d.txt; node tools/debug/_dump_probe.mjs d.txt deepseek` |
| `_bak_probe.mjs` | 在 `cordis.patch.yml` 的历史备份里找某模型卡片（追溯配置漂移） | `node tools/debug/_bak_probe.mjs deepseek` |
| `_settings_probe.mjs` | 打印 `~/.dsh/settings.yaml` / profile 的 provider 段 | `node tools/debug/_settings_probe.mjs` |
| `_headless_overlay.yml` | `dsh --profile headless --patch` 用的 overlay 样例：把 provider 指向抓包代理 | `dsh --profile headless --patch tools/debug/_headless_overlay.yml "reply ok"` |

## 取证素材位置（排障先看这里）

| 位置 | 内容 |
| --- | --- |
| `<repo>/.llmrouter/config.history.jsonl` | router 逐请求流水：`ok/status/upstream/trail[]（每次 attempt）/usage`，**不落请求体** |
| `<repo>/.llmrouter/config.state.json` | 上游、模型组、`defaults`，自定义字段（如 `max_output_tokens`）原样保留 |
| `~/.local/state/billion-context/bili.log` | bili 逐请求决策：`output budget restored/clamped`、`[headroom]`、`[window-change]` |
| `~/.config/billion-context/billion-context.json` | bili 自身配置（`models.<name>.{context,output}`、`providers.<url>.models.<name>.{...}`） |
| `~/.dsh/sessions/<cwd-slug>/session-*/session.v4.jsonl.zstd` | DSH 会话（多帧 zstd；含 `request/header` 的 provider/model/maxTokens） |
| `~/.dsh/profiles/<profile>/cordis.patch.yml` | provider 与模型卡片（`contextWindow` / `maxTokens`）真正生效的 patch 层 |

## 已知坑

- **PowerShell 内联 JS**：带转义引号的 `node -e "..."` 容易被解析器打断 → 一律写成 `.mjs` 再 `node file.mjs`。
- **UTF-16 dump**：`dsh --dump-config` 等输出可能带 BOM（`ff fe`），按 `utf16le` 读；`Select-String`/`ConvertFrom-Json` 读 UTF-8 中文易报 `ArgumentException`。
- **zstd**：DSH 会话文件是**多帧**拼接，必须逐帧找 magic `28 B5 2F FD` 分别解压再拼。
- **cargo 不在 PATH**：实际在 `C:\Users\<user>\.cargo\bin\cargo.exe`。
- **重编译前先停进程**：否则链接阶段报 `error: failed to remove file ...llm-router-desktop.exe  Caused by: 拒绝访问 (os error 5)`。
- **bili 配置优先级**：known output ceiling 的解析顺序是「客户端上报 `x-bili-plugin-max-output` > launcher 通道 > bili 配置声明 > models.dev 注册表」—— 光改 bili 配置压不住客户端上报值。
