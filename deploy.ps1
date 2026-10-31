# === llm-router 一键部署（Windows PowerShell 5.1+ / PowerShell 7）===
#
#   新机器上做三件事：装 Python 3.11+、把代码弄下来、把迁移包塞进去。
#   这个脚本把后两件做完，然后直接起服务。
#
# 用法：
#   .\deploy.ps1 -Bundle .\llm-router-migrate-20261031-113234.zip
#   .\deploy.ps1 -Bundle ...\llm-router-migrate-xxx.zip -Dir D:\llm-router
#   .\deploy.ps1                      # 只准备代码、不导配置、不起服务之外的动作
#   .\deploy.ps1 -Bundle ... -NoStart # 装好就停，不启动服务
#
# 已有目录也能用：有 .git 就 git pull，没 .git 但有 router.py 就当成现成的用。

[CmdletBinding()]
param(
    [string]$Dir = $PSScriptRoot,
    [string]$Repo = "git@github.com:BigBrotherHui/llm-router.git",
    [string]$Bundle = "",
    [switch]$NoStart,
    [switch]$Merge
)

$ErrorActionPreference = "Stop"
function Info($msg) { Write-Host "  $msg" }
function Step($msg) { Write-Host ""; Write-Host "== $msg" -ForegroundColor Cyan }

# ---------------------------------------------------------------- 1. Python
Step "检查 Python（需要 3.11+，程序用到了标准库 tomllib）"

$python = $null
foreach ($cand in @("python", "python3", "py")) {
    $exe = Get-Command $cand -ErrorAction SilentlyContinue
    if (-not $exe) { continue }
    try {
        $v = & $exe.Source -c "import sys;print('%d.%d'%sys.version_info[:2])" 2>$null
    } catch { continue }
    if (-not $v) { continue }
    $parts = $v.Trim().Split(".")
    $ok = ([int]$parts[0] -gt 3) -or ([int]$parts[0] -eq 3 -and [int]$parts[1] -ge 11)
    if ($ok) { $python = $exe.Source; break }
    Info "$cand 是 $v，太旧了"
}
if (-not $python) {
    Write-Host "  没找到 3.11+ 的 Python。" -ForegroundColor Yellow
    Write-Host "  装一个：winget install -e --id Python.Python.3.12"
    Write-Host "        或 https://www.python.org/downloads/ （安装时勾 Add to PATH）"
    exit 1
}
Info "Python: $python ($(& $python -c 'import sys;print(sys.version.split()[0])'))"

# ---------------------------------------------------------------- 2. 目录 / 代码
Step "准备代码目录 $Dir"

if (-not (Test-Path -LiteralPath $Dir)) {
    New-Item -ItemType Directory -Path $Dir -Force | Out-Null
}
$Dir = (Resolve-Path -LiteralPath $Dir).Path
$routerPy = Join-Path $Dir "router.py"
$gitDir = Join-Path $Dir ".git"

if (Test-Path -LiteralPath $gitDir) {
    if (Get-Command git -ErrorAction SilentlyContinue) {
        Info "已有 git 仓库，拉最新代码"
        Push-Location $Dir
        try { & git pull --ff-only } finally { Pop-Location }
    } else {
        Info "已有 git 仓库但没有 git 命令，跳过更新"
    }
} elseif (Test-Path -LiteralPath $routerPy) {
    Info "目录里已经有 router.py，当成现成项目用"
} else {
    if (Get-Command git -ErrorAction SilentlyContinue) {
        Info "克隆 $Repo"
        & git clone $Repo $Dir
        if ($LASTEXITCODE -ne 0) { Write-Host "  clone 失败：检查仓库地址/SSH Key/网络" -ForegroundColor Yellow; exit 1 }
    } else {
        Info "这台机器没装 git，也没有现成项目。"
        Write-Host "  两条路：" -ForegroundColor Yellow
        Write-Host "    1) winget install -e --id Git.Git  然后重跑本脚本"
        Write-Host "    2) 把老机器的项目目录（不含 迁移包/）整个拷到 $Dir 再来"
        exit 1
    }
}

# 依赖：整套只有标准库，没有 requirements.txt，这里只是确认一句
Info "依赖：无（纯标准库）"

# ---------------------------------------------------------------- 3. 导入迁移包
Step "导入迁移包"

$imported = $false
if ($Bundle) {
    if (-not (Test-Path -LiteralPath $Bundle)) {
        Write-Host "  找不到迁移包：$Bundle" -ForegroundColor Yellow
        exit 1
    }
    $bundlePath = (Resolve-Path -LiteralPath $Bundle).Path
    $work = Join-Path ([System.IO.Path]::GetTempPath()) ("llmr-import-" + (Get-Date -Format "yyyyMMddHHmmss"))
    New-Item -ItemType Directory -Path $work -Force | Out-Null
    try {
        if ([System.IO.Path]::GetExtension($bundlePath) -ieq ".zip") {
            Info "解压 $(Split-Path -Leaf $bundlePath)"
            Expand-Archive -LiteralPath $bundlePath -DestinationPath $work -Force
            $target = Join-Path $work "bundle.json"
        } else {
            $target = $bundlePath
        }
        $migrate = Join-Path $Dir "migrate.py"
        if (-not (Test-Path -LiteralPath $migrate)) {
            Write-Host "  项目里没有 migrate.py，没法导入" -ForegroundColor Yellow
            exit 1
        }
        $migrateArgs = @($migrate, "import", $target, "-y", "--root", $Dir)
        if ($Merge) { $migrateArgs += "--merge" }
        Push-Location $Dir
        try { & $python @migrateArgs } finally { Pop-Location }
        if ($LASTEXITCODE -ne 0) { Write-Host "  导入失败，看上面的输出" -ForegroundColor Yellow; exit 1 }
        $imported = $true
    } finally {
        Remove-Item -LiteralPath $work -Recurse -Force -ErrorAction SilentlyContinue
    }
} else {
    Info "没给 -Bundle：沿用目录里现有的 config.toml / .llmrouter 状态"
}

# ---------------------------------------------------------------- 4. 配置兜底
$config = Join-Path $Dir "config.toml"
if (-not (Test-Path -LiteralPath $config)) {
    $example = Join-Path $Dir "config.example.toml"
    if (Test-Path -LiteralPath $example) {
        Copy-Item -LiteralPath $example -Destination $config
        Info "没有 config.toml，已从 config.example.toml 复制一份（上游还得自己填，或者用 -Bundle 导）"
    } else {
        Write-Host "  既没有 config.toml 也没有 config.example.toml" -ForegroundColor Yellow
        exit 1
    }
}

# ---------------------------------------------------------------- 5. 自检
Step "自检配置"
Push-Location $Dir
try { & $python (Join-Path $Dir "router.py") -c $config --check } finally { Pop-Location }
if ($LASTEXITCODE -ne 0) {
    Write-Host "  自检没过，先修上面的报错再启动" -ForegroundColor Yellow
    exit 1
}

if (-not $imported -and -not (Test-Path -LiteralPath (Join-Path $Dir ".llmrouter"))) {
    Write-Host "  提示：目录里还没有 .llmrouter（= 没有上游状态）。" -ForegroundColor Yellow
    Write-Host "        带 -Bundle 跑一次，或在网页控制台里重新加上游。"
}

# ---------------------------------------------------------------- 6. 启动
Step "完成"
Info "项目目录：$Dir"
Info "网页控制台：http://127.0.0.1:8010/"
if ($NoStart) {
    Info "按 -NoStart 停下。要启动：双击 启动.cmd，或 python 启动.py"
    exit 0
}
Info "启动服务（Ctrl+C 停止）"
Write-Host ""
Push-Location $Dir
try { & $python (Join-Path $Dir "router.py") -c $config } finally { Pop-Location }