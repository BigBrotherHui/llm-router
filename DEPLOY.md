# 换电脑：导出 → 一条命令部署

上游（名字 / 地址 / Key / 勾选 / 分组 / 模型路由）都放在 `.llmrouter/` 里，而这个目录
**按设计不进仓库**（里面有明文 Key）。所以换机器不能只 clone 代码，还得把状态搬过去。
`migrate.py` + `deploy.ps1` 就是干这件事的。

---

## 一、老机器：导出全部上游

```powershell
cd <项目目录>
python migrate.py export
```

输出（默认在 `迁移包/`）：

| 文件 | 用途 |
| --- | --- |
| `bundle.json` | 全量包：`config.toml` 原文 + 全部上游 + 模型路由 + 签到记录。**导入只认它** |
| `upstreams.full.json` | 纯上游清单，Key 明文（核对 / 脚本处理用） |
| `upstreams.toml` | 可直接粘进 `config.toml` 的 `[[upstreams]]` 片段 |
| `upstreams.masked.md` | 脱敏清单，Key 只剩头尾，贴文档 / 发人看用这个 |
| `llm-router-migrate-<时间戳>.zip` | 上面四个打成一个包，**搬机器就用这个** |

顺手看一眼包里有什么：

```powershell
python migrate.py list .\llm-router-migrate-20261031-113234.zip
```

只想给个名单、不想给 Key：

```powershell
python migrate.py export --no-secrets     # Key 打码；这种包导入会被拦下（除非 --allow-masked）
```

> 包里是**明文 Key**。走 U 盘 / scp / 私有网盘，别进公共仓库。
> `.gitignore` 已经把 `迁移包/` 和 `llm-router-migrate-*.zip` 排除掉了。

---

## 二、新机器：一条命令

前提只有一样：**Python 3.11+**（程序用到了标准库 `tomllib`；没有第三方依赖）。

```powershell
winget install -e --id Python.Python.3.12     # 没装 Python 才需要
winget install -e --id Git.Git                # 没装 git 才需要
```

把 `deploy.ps1` 和迁移包拷过去，然后：

```powershell
# 方式 A：从 GitHub 拉代码 + 导入配置 + 启动
.\deploy.ps1 -Bundle .\llm-router-migrate-20261031-113234.zip

# 方式 B：代码已经拷好了（目录里有 router.py），只导入配置
.\deploy.ps1 -Dir D:\llm-router -Bundle .\llm-router-migrate-20261031-113234.zip

# 方式 C：装好但不启动，自己看完再起
.\deploy.ps1 -Bundle .\llm-router-migrate-xxx.zip -NoStart
```

脚本干的事，按顺序：

1. 找 3.11+ 的 Python（`python` / `python3` / `py` 都试），版本不对直接退出并给安装命令
2. 目录准备：有 `.git` → `git pull --ff-only`；有 `router.py` → 当现成项目用；都没有 → `git clone`
3. 解压迁移包 → `python migrate.py import <bundle.json> -y --root <目录>`
   （覆盖前会自动把旧的 `config.toml` / 状态文件备份成 `.bak-import-<时间戳>`）
4. 没有 `config.toml` 就从 `config.example.toml` 复制一份兜底
5. `python router.py -c config.toml --check` 自检，不过就停下并提示
6. 前台启动服务 → 打开 **http://127.0.0.1:8010/**

PowerShell 脚本执行策略拦下来的话，用这个起：

```powershell
powershell -ExecutionPolicy Bypass -File .\deploy.ps1 -Bundle .\llm-router-migrate-xxx.zip
```

---

## 三、不用脚本的手动版（三行）

```powershell
git clone git@github.com:BigBrotherHui/llm-router.git D:\llm-router
cd D:\llm-router
python migrate.py import D:\path\to\llm-router-migrate-20261031-113234.zip -y
python 启动.py
```

`migrate.py import` 支持三种输入：`.zip`、`bundle.json`、解压后的目录。

不想覆盖、想和这台机器现有的配置**合并**（比如新机器上已经配了几个上游）：

```powershell
python migrate.py import .\bundle.json --merge
```

合并规则：上游按 `id`、再按 `名字 + base_url` 去重；本机已有的模型路由优先保留；
`defaults` / `checkin_settings` / 屏蔽名单也用本机的。先看要改什么可以加 `--dry-run`。

---

## 四、迁移包里到底装了什么

一个上游在状态文件里长这样（简化）：

```json
{
  "id": "u00a60e",
  "name": "sensenova",
  "base_url": "https://token.sensenova.cn/v1",
  "base_urls": [],                     // 多线路镜像（可选）
  "mirror_strategy": "",               // fastest / priority / round_robin
  "api_keys": ["sk-..."],              // 同一家可以挂多把 Key
  "api_key_names": [],
  "key_strategy": "failover",          // 多 Key 怎么用：failover / round_robin
  "enabled": true,
  "selected_models": ["kimi-k3"],      // 勾了哪些模型
  "model_groups": {"kimi-k3": "kimi"}, // 勾选后起的统一分组名
  "available_models": ["..."],         // 上次拉取的结果
  "first_timeout_seconds": 30.0,
  "user_agent": "",
  "checkin": {}                        // 签到的访问令牌（跟推理 Key 分开存）
}
```

加上 `state.models`（每个客户端模型名 → 上游调用顺序 / 策略 / 额度 / 具名排序），
就是「所有上游」的完整定义。这些东西 `config.toml` 表达不全（一个上游只写一个
`api_key`、模型路由也不在里面），所以迁移用 `bundle.json` 而不是 `config.toml`。

---

## 五、换机器时容易踩的坑

**`base_url` 指本机的上游要一起搬。** 比如 `http://127.0.0.1:7863`（Claude Code 的
provider 代理）、`http://127.0.0.1:8790`（Qoder hub）。路由器配置搬过去了，但这些本地
服务在新机器上并不存在 —— 卡片会显示连不上，得在新机器上把对应代理也起起来，或者把
这类上游停用。

**`config.toml` 与 `.llmrouter/` 的关系：** 网页上的改动写进
`.llmrouter/<配置名>.state.json`（比如 `config.state.json`），不改 `config.toml`。
两边都有上游时，`config.toml` 是「初始来源」，状态文件是「当前真相」。所以迁移搬状态
文件；只搬 `config.toml` 会丢掉勾选、分组、模型路由、多 Key。

**端口 8010 被占：** 改 `config.toml` 里的 `[server] port`，或用
`python router.py -c config.toml --port 8011`（具体参数看 `--help`）。

**只想跑局域网给别人用：** `[server] host = "0.0.0.0"`，然后别人把 `base_url` 指向
`http://<这台机器IP>:8010/v1`。

**备份回滚：** 每次 import 都在原文件旁边留 `.bak-import-<时间戳>`；
运行时的状态轮转在 `.llmrouter/versions/`（保留最近若干份），reset 是移到
`.llmrouter/trash/` 而不是删掉 —— 导错了能捞回来。