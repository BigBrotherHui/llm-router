@echo off
chcp 65001 >nul
cd /d "%~dp0"
echo.
echo   启动后请打开网页控制台： http://127.0.0.1:8010/
echo.
if exist config.toml (
  python router.py -c config.toml
) else (
  echo 还没看到 config.toml
  echo 先复制一份： copy config.example.toml config.toml
  echo 下面先用示例配置做一次校验：
  echo.
  python router.py -c config.example.toml --check
)
pause
