# Telegram Downloader RS - install the bilingual login GUI.
# Idempotent: safe to run multiple times. Current user only, no Administrator.
$ErrorActionPreference = 'Stop'

$dst = Join-Path $env:LOCALAPPDATA 'tgdlrs'
$exeDir = Join-Path $env:LOCALAPPDATA 'tgdlrs\bin'
New-Item -ItemType Directory -Force $dst | Out-Null

Copy-Item (Join-Path $PSScriptRoot 'tgdlrs-login.ps1') (Join-Path $dst 'tgdlrs-login.ps1') -Force
Copy-Item (Join-Path $PSScriptRoot 'tgdlrs-login.cmd') (Join-Path $dst 'tgdlrs-login.cmd') -Force
Copy-Item (Join-Path $PSScriptRoot 'tgdlrs-notify.ps1') (Join-Path $dst 'tgdlrs-notify.ps1') -Force

$ws = New-Object -ComObject WScript.Shell
$desktop = [Environment]::GetFolderPath('Desktop')
$lnk = Join-Path $desktop 'Telegram Downloader - Login.lnk'
$s = $ws.CreateShortcut($lnk)
$s.TargetPath = Join-Path $dst 'tgdlrs-login.cmd'
$s.IconLocation = (Join-Path $exeDir 'tgdlrs.exe') + ',0'
$s.Description = 'Telegram Downloader RS - Login (English / Tiếng Việt)'
$s.Save()

Write-Output "Installed GUI to: $dst"
Write-Output "Desktop shortcut:  $lnk"
Write-Output 'Double-click "Telegram Downloader - Login" to sign in.'
