# llm-router · 多上游 LLM API 路由代理

同一个模型（比如 `deepseek-chat`）你有好几个来源，希望**排好顺序依次调用**，或者**轮流调用来分摊额度**。

打开网页控制台就能全部搞定：填上游 → 一键拉取模型 → 按模型排顺序，不用手写路由配置。

零依赖，只用 Python 标准库（需要 Python 3.11+）。Windows / macOS / Linux 都能跑。

## 快速开始

```bash
# 1) 复制配置
copy config.example.toml config.toml     # Windows
cp config.example.toml config.toml       # macOS / Linux

# 2) 把上游的名字 / 地址 / Key 填进去
#    然后启动
python router.py -c config.toml
#   或者双击 启动.cmd / python 启动.py
```

启动后打开 **http://127.0.0.1:8010/**：

1. 填上游（名字 / 地址 / Key）→ 添加
2. 点「拉取全部模型」把每家支持的模型抓过来
3. 在每个上游卡片上**勾选你要用的模型**（拉取后默认一个都不选）
4. 到「模型路由」里点模型，调整多个上游之间的调用顺序

客户端把 `base_url` 指向 `http://127.0.0.1:8010/v1` 就行，其余代码完全不用改：

```python
from openai import OpenAI
client = OpenAI(base_url="http://127.0.0.1:8010/v1", api_key="随便填")
print(client.chat.completions.create(
    model="deepseek-chat",
    messages=[{"role": "user", "content": "你好"}],
).choices[0].message.content)
```

## 工作方式

分成三步：加上游 → 勾模型 → 给模型起统一的名字。

```
第一步：上游来源（只管填名字 / 地址 / Key，不关心模型）
  ├── 上游A     https://api-a.example.com
  ├── 上游B     https://api-b.example.com
  ├── 上游C     https://api-c.example.com
  └── 上游D     https://api-d.example.com

         ↓ 点「拉取全部模型」，问出每家有哪些模型（默认一个都不选）

第二步：在每个上游卡片上勾选要用的模型
  上游A  ☑ ds1   ☐ gpt4-a   ☐ other-a
  上游B  ☑ ds2   ☐ gpt4-b
  上游C  ☑ ds3   ☐ gpt4-c
  上游D  ☑ ds4   ☐ gpt4-d

         ↓ 勾选后每个模型右边出现「分组名」输入框（关键一步）

第三步：给各家的模型起一个统一的名字
  上游A  ☑ ds1  → 分组名 [deepseek]
  上游B  ☑ ds2  → 分组名 [deepseek]
  上游C  ☑ ds3  → 分组名 [deepseek]
  上游D  ☑ ds4  → 分组名 [deepseek]

         ↓ 填同一个名字 = 自动合并成一个可调用的模型

模型路由
  └── deepseek   4 个上游  →  上游A(ds1) → 上游B(ds2) → 上游C(ds3) → 上游D(ds4)
```

**客户端永远只用你起的那个统一名字**（这里是 `deepseek`）。请求会按你排的顺序发给各上游，
每家收到的则是它自己的真实名字（`ds1` / `ds2` / `ds3` / `ds4`），响应里的 `model` 字段会被改回 `deepseek`，
所以调用方完全感觉不到各家命名不同。

### 两个名字的区别

这是整个工具的核心概念，分清楚就不会绕：

| | 谁起的 | 例子 | 在哪填 |
|---|---|---|---|
| **真实模型名** | 上游定的，拉取得到 | `ds1`、`ds2`、`deepseek-ai/DeepSeek-V3` | 自动拉取，不用管 |
| **分组名** | **你起的**，客户端用这个名字调 | `deepseek` | 上游卡片里勾选后，右边那个框（可选已有 / 输入新建） |

不填分组名，默认就用真实名本身（一对一，各家用各自的名字）。
填了之后，**填法相同的就归到一个分组**——所以第二个上游要合并时，
直接在候选里点一下已有的 `deepseek` 就行，不用手打、也不会打错。

### 三种调用方式

| 方式 | 行为 | 适合 |
|---|---|---|
| **按顺序依次调用** | 永远先用排第一的，挂了/限流/额度用尽才用下一个 | 最常用，最省钱 |
| **轮流轮询** | 请求 1 走第一个，请求 2 走第二个……转一圈再来 | 多账号分摊额度 |
| **随机挑一个** | 每次随机 | 打散压力 |

每个分组可以单独设，互不影响。调用名还能整体改（弹窗右上角「改调用名」）：
**只改你在客户端填的那个名字**，各上游那边的真实模型名一个都不动，它们的映射会自动指向新名字。

## 网页控制台

### 上游来源

只填三个框：**名字 / 地址 / Key**。地址带不带 `/v1` 都可以，会自动试出正确的拼法。

添加后会自动拉取该上游的模型列表。点上游卡片上的「选模型」展开面板：

- **左边勾选**要用的模型（拉取后默认一个都不选）
- **勾上之后，右边出现分组名输入框** —— 点一下会列出**已有分组**直接选，也能输入新名字新建
  - 候选里会标出每个分组「几个上游在用 / 是否已建路由」
  - 支持搜索（打几个字就筛）、上下键选择、回车确认
  - 最下面有「用真实模型名（不分组）」用来清除映射
- 多个上游里选同一个分组名，它们就会合并成一个可调用的模型
- 面板里有筛选框、全选、全不选；下面会显示「本上游的分组：deepseek ← ds1」

其他操作：拉取模型、测速、编辑、停用、删除，以及请求数、成功率、平均延迟和延迟对比条。

### 模型路由

这里列出**你勾选并分好组的模型**，点任意一个打开配置弹窗：

- 自动列出参与这个分组的所有上游，**每个都标着它那边的真实名字**（如 `上游A · ds1`）
- **拖动 `⠿` 就能调整调用顺序**
- 每条可以单独勾掉「参与」，临时把某个上游排除
- 选「轮流轮询」时，下面会实时显示轮询顺序预览
- 右上角「改调用名」可以给整个分组改名叫别的，**上游那边的真实名字不受影响**
- 列表卡片上「各家名字不同」的紫色标记，提醒你这个分组的成员命名不一致

还有个「按上游顺序自动配好」：把所有分组按上游当前顺序一次性建好路由，已手工调过的不覆盖。

### 左侧「请求记录」面板

左上角有个可收起的面板（点「收起」隐藏，左边会留一个竖直把手点回来）。里面分两块：

**统计区**（顶部概览 + 按分组）

- 总请求、成功、失败、输入/输出 token、缓存读、缓存写
- 每个分组一张卡，显示该分组的调用次数、成功失败、token 用量、平均耗时
- 卡里用横条画各上游的调用次数对比，哪家接得多一眼看出

**请求区**

- 每次请求一行：时间、状态码、分组名、实际上游、耗时、是否流式
- **点任意一行展开详情**：
  - Token 用量：输入 / 输出 / 合计 / 缓存读 / 缓存写（各家字段名不同都做了兼容）
  - 实际发往上游的模型名、路由来源、试了几个上游
  - **输入内容**（messages 拼成可读文字，多模态只记文字并标 [图片]）
  - **输出内容**
  - 推理模型的**思考过程**（DeepSeek-R1/V4 这类会单独吐 `reasoning_content`）
- 右上角下拉可以**只看某个分组**的请求，或切成「只看统计 / 只看请求」

数据来源是最近 200 条请求（内存里，不落盘）。想清空点顶部「清空统计」。

### 其他

- 顶部的「拉取全部模型」和「测速」对全部上游批量执行
- 没有登录、没有 Key 校验，本机打开就能用

## 配置项速查

平时用网页配就够了，这里只说明 `config.toml` 里的基础设置：

### `[server]`

| 字段 | 默认 | 说明 |
|---|---|---|
| `host` | `127.0.0.1` | 只给自己用就保持默认；局域网共用填 `0.0.0.0` |
| `port` | `8010` | 监听端口 |
| `log_requests` | `true` | 是否打印请求日志 |
| `max_body_mb` | `32` | 请求体上限 |
| `enable_cors` | `false` | 浏览器前端直连时打开 |

### `[router]`

| 字段 | 默认 | 说明 |
|---|---|---|
| `default_strategy` | `priority` | 新建模型路由时的默认方式 |
| `failure_threshold` | `3` | 连续失败几次后暂时踢出队列 |
| `cooldown_seconds` | `60` | 踢出后多久自动恢复 |

### `[[upstreams]]`

| 字段 | 必填 | 说明 |
|---|---|---|
| `name` | 建议 | 显示名，可以用中文 |
| `id` | 否 | 稳定标识，网页上的改动靠它认人；不写自动生成 |
| `base_url` | 是 | 上游地址，带不带 `/v1` 都行 |
| `api_key` | 否 | 该上游的 Key（本地 Ollama 随便填） |
| `enabled` | 否 | `false` 可临时禁用 |
| `models` | 否 | 想预先声明可用模型时才写；平时让它拉取就行。写了会被视为「已勾选」 |

> 平时不用手写。网页上的勾选、分组、排序都会存进 `.llmrouter/` 子目录（见下）。

## 遇到问题

**Q：请求时先说某个上游失败，然后才成功？**
正常。这就是故障转移——第一个上游挂了会自动换下一个，响应头 `X-Upstream` 告诉你最后是谁处理的。

**Q：我自己参数写错了，会白白消耗其他上游吗？**
不会。`400` 这类客户端错误原样返回给你，只有 `401/403/429/5xx` 这种「上游自己的问题」才切换。切换的判定列表在 `router.py` 的 `DEFAULT_RETRY_STATUS`。

**Q：有「最多试几个上游」的限制吗？**
没有。配了几个就一路试到底，直到成功或者全部失败。

**Q：某个模型我没勾选，请求它会怎样？**
依然能用，走「自动兜底」：先看你有没有给某个上游的这个模型起过分组名，再挑确实拉取到这个模型的上游。历史记录里会标明。想精确控制就用勾选 + 分组 + 排序。

**Q：A 家叫 ds1、B 家叫 ds2，怎么让它们变成一个模型？**
勾选之后在各自右边的「分组名」框里都填同一个名字（比如 `deepseek`），就会自动合并成一条路由。客户端只请求 `deepseek`，路由器按你的顺序发给各家，实际发送的是各家的真实名（`ds1`/`ds2`），响应里的 model 会改回 `deepseek`。详见上面「工作方式」。

**Q：我只填了分组名，没建路由，能调用吗？**
能。会自动按分组兜底，顺序按上游顺序。想要确定的顺序就点进模型路由里排一下。

**Q：上游的 /v1/models 报不全，我能不能手填模型名？**
能。你在配置弹窗或分组里手填的名字，路由器会认，并记进这个上游的模型表，不会被过滤掉。这是刻意的——不少厂商的模型列表是不完整的。

**Q：为什么拉取完模型一个都没选中？**
故意的。一个上游动辄几十上百个模型，全选进来会让列表没法看，也容易误调。你只勾要用的那几个。

**Q：上游下架了某个模型怎么办？**
下次拉取时会自动把它从你的勾选里清掉，并在拉取结果里告诉你清掉了哪些。

**Q：网页上的改动存在哪？会弄乱我的 config.toml 吗？**
存进 `.llmrouter/config.state.json`，**你的 config.toml 完全不被改动**，注释和排版都安全。

状态文件特意放在 `.llmrouter/` 子目录里，不跟项目文件混在一起——这样清理项目时（比如 `rm *.json`）
不会误伤你的配置。另外它还有三层保护：每次保存都留一份 `.bak`、写入用原子替换、重置时是移进
`.llmrouter/trash/` 而不是删除。想恢复就往回拷。

**Q：网页上能看到我的 Key 吗？**
不能，只显示掩码，比如 `sk-off…3456`。

**Q：上游列表接口格式不一样（`data` / `models` / 嵌套的 `result.models`）？**
都兼容，会自动识别。完全不认识的格式会明确报错，不会静默当成 0 个模型。

**Q：客户端要填什么 API Key？**
随便填。这个代理是本机自用，不校验 Key。

**Q：支持流式吗？**
支持，SSE 逐块透传，不会被缓冲。上游返回 200 但流是空的，也会判定失败并切换到下一个。

**Q：怎么自测？**
`python selftest.py` 会起几个假上游，验证 182 项行为（模型拉取、勾选、分组合并、按顺序/轮询/故障转移、流式、增删改、路径拼接、持久化、中文名等）。

其中专门有一组测试模拟「ABCD 四个上游，同一个模型分别叫 ds1/ds2/ds3/ds4」：
验证统一成分组名之后能正确合并、按顺序调用、并且各家收到的是自己的真实名字。

假上游里有一个是「严格模式」的：它只接受 `/v1/xxx` 形式的路径，拼成 `/v1/v1/xxx` 就返回 404。
现实中的厂商（硅基流动、火山方舟）就是这样，所以这个测试专门用来防路径拼错的回归。

**Q：只校验配置不启动？**
`python router.py -c config.toml --check`

## 管理接口

网页用的就是这些接口，想写脚本调也可以：

```bash
# 完整状态（上游、模型路由、最近请求）
curl http://127.0.0.1:8010/stats

# 拉取模型列表
curl -X POST -H "Content-Type: application/json" -d '{}' \
  http://127.0.0.1:8010/admin/fetch-models

# 勾选某个上游要用的模型（勾选即参与路由）
curl -X POST -H "Content-Type: application/json" \
  -d '{"id":"u1","models":["ds1","ds2"]}' \
  http://127.0.0.1:8010/admin/select-models
curl -X POST -H "Content-Type: application/json" -d '{"id":"u1","all":true}' \
  http://127.0.0.1:8010/admin/select-models      # 全选
curl -X POST -H "Content-Type: application/json" -d '{"id":"u1","none":true}' \
  http://127.0.0.1:8010/admin/select-models      # 全不选

# 给各家的模型起统一的名字（分组）—— 填同一个值就合并
curl -X POST -H "Content-Type: application/json" \
  -d '{"id":"u1","mapping":{"ds1":"deepseek"}}' \
  http://127.0.0.1:8010/admin/set-group
curl -X POST -H "Content-Type: application/json" \
  -d '{"id":"u2","mapping":{"ds2":"deepseek"}}' \
  http://127.0.0.1:8010/admin/set-group
# 传空字符串取消映射，回到用真实名当分组名
curl -X POST -H "Content-Type: application/json" \
  -d '{"id":"u2","mapping":{"ds2":""}}' \
  http://127.0.0.1:8010/admin/set-group

# 给整个分组改名（所有上游的映射一起改）
curl -X POST -H "Content-Type: application/json" \
  -d '{"from":"deepseek","to":"llm"}' \
  http://127.0.0.1:8010/admin/rename-group

# 添加 / 修改 / 删除上游
curl -X POST -H "Content-Type: application/json" \
  -d '{"name":"新上游","base_url":"https://api.example.com","api_key":"sk-x"}' \
  http://127.0.0.1:8010/admin/upstream/add
curl -X POST -H "Content-Type: application/json" \
  -d '{"id":"u1","name":"改个名","enabled":false}' \
  http://127.0.0.1:8010/admin/upstream/update
curl -X POST -H "Content-Type: application/json" -d '{"id":"u1"}' \
  http://127.0.0.1:8010/admin/upstream/delete

# 保存某个模型的路由（数组顺序 = 调用顺序）
curl -X POST -H "Content-Type: application/json" -d '{
  "name":"deepseek-chat","strategy":"priority",
  "entries":[{"upstream":"u1","model":"deepseek-chat"},
             {"upstream":"u2","model":"deepseek-ai/DeepSeek-V3"}]}' \
  http://127.0.0.1:8010/admin/model/save
curl -X POST -H "Content-Type: application/json" -d '{"name":"deepseek-chat"}' \
  http://127.0.0.1:8010/admin/model/delete

# 一键给所有模型建路由
curl -X POST -H "Content-Type: application/json" -d '{}' \
  http://127.0.0.1:8010/admin/auto-configure

# 测速 / 清空统计 / 恢复成配置文件的样子
curl -X POST -H "Content-Type: application/json" -d '{}' http://127.0.0.1:8010/admin/speedtest
curl -X POST -H "Content-Type: application/json" -d '{}' http://127.0.0.1:8010/admin/reset
curl -X POST -H "Content-Type: application/json" -d '{}' http://127.0.0.1:8010/admin/reset-to-config
```

## 文件说明

| 文件 | 用途 |
|---|---|
| `router.py` | 主程序，单文件 |
| `webui.html` | 网页控制台（和 router.py 放同一目录） |
| `config.example.toml` | 配置模板 |
| `config.toml` | 你自己的配置（从模板复制） |
| `.llmrouter/config.state.json` | 你的上游/勾选/分组/路由存这里，自动生成 |
| `.llmrouter/config.state.json.bak` | 上一版状态的自动备份 |
| `.llmrouter/trash/` | 重置时把旧状态移到这里，可恢复 |
| `启动.cmd` / `启动.py` | 一键启动 |
| `selftest.py` | 自测脚本 |
