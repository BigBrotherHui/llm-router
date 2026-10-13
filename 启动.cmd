@echo off
chcp 65001 >nul
cd /d "%~dp0"
echo.
echo   启动后请打开网页控制台: http://127.0.0.1:8010/
echo.
if exist config.toml (
  python router.py -c config.toml
) else (
  echo config.toml not found
  echo Please copy config.example.toml to config.toml
  python router.py -c config.example.toml --check
)
pause

