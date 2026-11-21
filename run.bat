@echo off
rem NIM Gateway (Rust). Default port 8100.
cd /d "%~dp0"
if not exist data mkdir data
set NGW_PORT=8100
targetelease
im-gateway.exe --port 8100 >> data\server.log 2>&1
