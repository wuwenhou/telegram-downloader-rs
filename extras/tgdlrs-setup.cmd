@echo off
chcp 65001 >nul
start "" powershell -NoProfile -STA -ExecutionPolicy Bypass -File "%~dp0tgdlrs-setup.ps1" %*
