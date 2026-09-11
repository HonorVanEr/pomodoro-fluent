@echo off
REM 番茄钟 · 一键启动脚本
chcp 65001 >nul
cd /d "%~dp0"

REM 设置代理镜像（如需要下载 Electron 二进制）
REM set ELECTRON_MIRROR=https://npmmirror.com/mirrors/electron/

if exist "node_modules\electron\dist\electron.exe" (
  start "" "node_modules\electron\dist\electron.exe" "%~dp0"
) else (
  echo [错误] 未找到 Electron，请先运行: npm install
  pause
)
