@echo off
rem 打包本地脚本为单文件 exe（自带 Python 与依赖）：pip install pyinstaller 后双击本文件
cd /d %~dp0
pyinstaller --noconfirm --onefile --noconsole --name aocker_local aocker_local.py
echo.
echo 输出：dist\aocker_local.exe
pause
