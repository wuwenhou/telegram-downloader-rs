@echo off
rem Install launcher for projects that ship this file.
rem The GUI itself lives in %LOCALAPPDATA%\tgdlrs (see install-login-gui.ps1).
set "TGDLRS_GUI=%~dp0tgdlrs-login.ps1"
start "" /min powershell -NoProfile -ExecutionPolicy Bypass -File "%TGDLRS_GUI%"
