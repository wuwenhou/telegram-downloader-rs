<#
 tgdlrs-setup.ps1 — Windows installer wizard for Telegram Downloader RS.
 No external dependencies (PowerShell 5.1+ / WinForms, already on Windows).
 Run via tgdlrs-setup.cmd or:
   powershell -NoProfile -ExecutionPolicy Bypass -File tgdlrs-setup.ps1
#>
param(
    [string]$SourceBin = ""   # folder containing tgdlrs.exe / tgdlrs-worker.exe / tgdlrs-mcp.exe
)

Add-Type -AssemblyName System.Windows.Forms
Add-Type -AssemblyName System.Drawing

$ErrorActionPreference = 'Stop'
$AppName = 'Telegram Downloader RS'
$Version = '0.1.0'

# --- locate source binaries -----------------------------------------------
if ([string]::IsNullOrWhiteSpace($SourceBin)) {
    $candidates = @(
        (Join-Path $PSScriptRoot '..\target\release'),
        (Join-Path $env:LOCALAPPDATA 'tgdlrs\bin'),
        $PSScriptRoot
    )
    foreach ($c in $candidates) {
        if (Test-Path (Join-Path $c 'tgdlrs.exe')) { $SourceBin = $c; break }
    }
}

# --- shared state ----------------------------------------------------------
$script:InstallDir = Join-Path $env:LOCALAPPDATA 'tgdlrs'
$script:Autostart = $true
$script:DesktopShortcut = $true
$script:LoginShortcut = $true
$script:ConfigDone = $false

# --- form ------------------------------------------------------------------
$form = New-Object System.Windows.Forms.Form
$form.Text = "$AppName $Version - Setup"
$form.Size = New-Object System.Drawing.Size(620, 470)
$form.StartPosition = 'CenterScreen'
$form.FormBorderStyle = 'FixedDialog'
$form.MaximizeBox = $false
$form.MinimizeBox = $false
$form.BackColor = [System.Drawing.Color]::FromArgb(32, 33, 36)

$header = New-Object System.Windows.Forms.Label
$header.Dock = 'Top'
$header.Height = 70
$header.Font = New-Object System.Drawing.Font('Segoe UI', 16, [System.Drawing.FontStyle]::Bold)
$header.ForeColor = [System.Drawing.Color]::White
$header.BackColor = [System.Drawing.Color]::FromArgb(45, 47, 52)
$header.Padding = New-Object System.Windows.Forms.Padding(20, 18, 0, 0)
$form.Controls.Add($header)

$content = New-Object System.Windows.Forms.Panel
$content.Dock = 'Fill'
$content.Padding = New-Object System.Windows.Forms.Padding(24, 18, 24, 10)
$form.Controls.Add($content)

$bottom = New-Object System.Windows.Forms.Panel
$bottom.Dock = 'Bottom'
$bottom.Height = 60
$bottom.BackColor = [System.Drawing.Color]::FromArgb(45, 47, 52)
$form.Controls.Add($bottom)

$btnBack = New-Object System.Windows.Forms.Button
$btnBack.Text = 'Back'
$btnBack.Size = New-Object System.Drawing.Size(100, 34)
$btnBack.Location = New-Object System.Drawing.Point(410, 13)
$btnBack.FlatStyle = 'Flat'
$btnBack.BackColor = [System.Drawing.Color]::FromArgb(70, 72, 78)
$btnBack.ForeColor = [System.Drawing.Color]::White
$bottom.Controls.Add($btnBack)

$btnNext = New-Object System.Windows.Forms.Button
$btnNext.Text = 'Next'
$btnNext.Size = New-Object System.Drawing.Size(120, 34)
$btnNext.Location = New-Object System.Drawing.Point(520, 13)
$btnNext.FlatStyle = 'Flat'
$btnNext.BackColor = [System.Drawing.Color]::FromArgb(0, 120, 215)
$btnNext.ForeColor = [System.Drawing.Color]::White
$bottom.Controls.Add($btnNext)

function Add-Label($text, $x, $y, $w, $h, $bold = $false) {
    $l = New-Object System.Windows.Forms.Label
    $l.Text = $text
    $l.Location = New-Object System.Drawing.Point($x, $y)
    $l.Size = New-Object System.Drawing.Size($w, $h)
    $l.ForeColor = [System.Drawing.Color]::FromArgb(230, 230, 232)
    $l.Font = New-Object System.Drawing.Font('Segoe UI', 10, $(if ($bold) { [System.Drawing.FontStyle]::Bold } else { [System.Drawing.FontStyle]::Regular }))
    $content.Controls.Add($l)
    return $l
}

function Clear-Content { $content.Controls.Clear() }

function Show-Step1 {
    $script:step = 1
    $header.Text = "Welcome to $AppName setup"
    $btnBack.Enabled = $false
    $btnNext.Text = 'Next'
    Clear-Content
    Add-Label 'Telegram Downloader RS installs:' 10 10 560 25 $true | Out-Null
    Add-Label '- tgdlrs.exe  (CLI + MCP server)' 20 45 560 22 | Out-Null
    Add-Label '- tgdlrs-worker.exe  (background download worker)' 20 70 560 22 | Out-Null
    Add-Label '- tgdlrs-mcp.exe  (MCP bridge)' 20 95 560 22 | Out-Null
    Add-Label '- Login GUI + completion notifications' 20 120 560 22 | Out-Null
    Add-Label '' 10 150 560 10 | Out-Null
    Add-Label "Source binaries: $SourceBin" 10 175 560 22 | Out-Null
    Add-Label "Install folder:  $($script:InstallDir)" 10 205 560 22 | Out-Null
    Add-Label 'No Administrator rights required. The worker starts at Windows login.' 10 245 560 40 | Out-Null
}

function Show-Step2 {
    $script:step = 2
    $header.Text = 'Choose install options'
    $btnBack.Enabled = $true
    $btnNext.Text = 'Install'
    Clear-Content

    Add-Label 'Install folder:' 10 10 560 22 $true | Out-Null
    $tbDir = New-Object System.Windows.Forms.TextBox
    $tbDir.Text = $script:InstallDir
    $tbDir.Location = New-Object System.Drawing.Point(10, 38)
    $tbDir.Size = New-Object System.Drawing.Size(440, 28)
    $tbDir.Font = New-Object System.Drawing.Font('Segoe UI', 10)
    $content.Controls.Add($tbDir)
    $script:tbDir = $tbDir

    $btnBrowse = New-Object System.Windows.Forms.Button
    $btnBrowse.Text = 'Browse…'
    $btnBrowse.Location = New-Object System.Drawing.Point(460, 37)
    $btnBrowse.Size = New-Object System.Drawing.Size(90, 28)
    $btnBrowse.FlatStyle = 'Flat'
    $btnBrowse.BackColor = [System.Drawing.Color]::FromArgb(70, 72, 78)
    $btnBrowse.ForeColor = [System.Drawing.Color]::White
    $content.Controls.Add($btnBrowse)
    $btnBrowse.Add_Click({
        $dlg = New-Object System.Windows.Forms.FolderBrowserDialog
        $dlg.SelectedPath = $script:tbDir.Text
        if ($dlg.ShowDialog() -eq 'OK') { $script:tbDir.Text = $dlg.SelectedPath }
    })

    $cbAuto = New-Object System.Windows.Forms.CheckBox
    $cbAuto.Text = 'Start worker automatically at Windows login'
    $cbAuto.Checked = $script:Autostart
    $cbAuto.Location = New-Object System.Drawing.Point(10, 95)
    $cbAuto.Size = New-Object System.Drawing.Size(560, 24)
    $cbAuto.ForeColor = [System.Drawing.Color]::White
    $content.Controls.Add($cbAuto)
    $script:cbAuto = $cbAuto

    $cbDesk = New-Object System.Windows.Forms.CheckBox
    $cbDesk.Text = 'Create desktop shortcut (login GUI)'
    $cbDesk.Checked = $script:DesktopShortcut
    $cbDesk.Location = New-Object System.Drawing.Point(10, 125)
    $cbDesk.Size = New-Object System.Drawing.Size(560, 24)
    $cbDesk.ForeColor = [System.Drawing.Color]::White
    $content.Controls.Add($cbDesk)
    $script:cbDesk = $cbDesk

    $cbLogin = New-Object System.Windows.Forms.CheckBox
    $cbLogin.Text = 'Install login GUI launcher into the app folder'
    $cbLogin.Checked = $script:LoginShortcut
    $cbLogin.Location = New-Object System.Drawing.Point(10, 155)
    $cbLogin.Size = New-Object System.Drawing.Size(560, 24)
    $cbLogin.ForeColor = [System.Drawing.Color]::White
    $content.Controls.Add($cbLogin)
    $script:cbLogin = $cbLogin

    Add-Label 'You can change these later by re-running this setup.' 10 225 560 22 | Out-Null
}

function Show-Step3 {
    $script:step = 3
    $header.Text = 'Installing…'
    $btnBack.Enabled = $false
    $btnNext.Enabled = $false
    Clear-Content
    $script:lblStatus = Add-Label 'Preparing…' 10 20 560 25
    [System.Windows.Forms.Application]::DoEvents()

    try {
        $script:InstallDir = $script:tbDir.Text.Trim()
        $script:Autostart = $script:cbAuto.Checked
        $script:DesktopShortcut = $script:cbDesk.Checked
        $script:LoginShortcut = $script:cbLogin.Checked

        if ([string]::IsNullOrWhiteSpace($SourceBin) -or !(Test-Path (Join-Path $SourceBin 'tgdlrs.exe'))) {
            throw "Source binaries not found. Run setup from the release folder or pass -SourceBin."
        }

        $script:lblStatus.Text = 'Creating folders…'; [System.Windows.Forms.Application]::DoEvents()
        $bin = Join-Path $script:InstallDir 'bin'
        New-Item -ItemType Directory -Force -Path $bin | Out-Null
        $extra = Join-Path $script:InstallDir 'extras'
        New-Item -ItemType Directory -Force -Path $extra | Out-Null

        $script:lblStatus.Text = 'Copying files…'; [System.Windows.Forms.Application]::DoEvents()
        foreach ($f in @('tgdlrs.exe','tgdlrs-worker.exe','tgdlrs-mcp.exe')) {
            $src = Join-Path $SourceBin $f
            if (!(Test-Path $src)) { throw "Missing binary: $src" }
            $dst = Join-Path $bin $f
            try {
                Copy-Item $src $dst -Force
            } catch {
                # File may be in use by a running worker/MCP: stage + rename.
                $tmp = "$dst.new"
                Copy-Item $src $tmp -Force
                Move-Item $tmp $dst -Force
            }
        }
        if ($script:LoginShortcut -and (Test-Path (Join-Path $PSScriptRoot 'tgdlrs-login.ps1'))) {
            Copy-Item (Join-Path $PSScriptRoot 'tgdlrs-login.ps1') (Join-Path $extra 'tgdlrs-login.ps1') -Force
            Copy-Item (Join-Path $PSScriptRoot 'tgdlrs-login.cmd') (Join-Path $extra 'tgdlrs-login.cmd') -Force
            Copy-Item (Join-Path $PSScriptRoot 'tgdlrs-notify.ps1') (Join-Path $extra 'tgdlrs-notify.ps1') -Force
        }

        $script:lblStatus.Text = 'Registering autostart…'; [System.Windows.Forms.Application]::DoEvents()
        $worker = Join-Path $bin 'tgdlrs-worker.exe'
        if ($script:Autostart) {
            $runKey = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run'
            $cmd = "`"$worker`""
            New-ItemProperty -Path $runKey -Name 'tgdlrs-worker' -Value $cmd -PropertyType String -Force | Out-Null
        } else {
            Remove-ItemProperty -Path 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run' -Name 'tgdlrs-worker' -ErrorAction SilentlyContinue
        }

        $script:lblStatus.Text = 'Creating shortcuts…'; [System.Windows.Forms.Application]::DoEvents()
        if ($script:DesktopShortcut) {
            $sh = New-Object -ComObject WScript.Shell
            $sc = $sh.CreateShortcut([System.IO.Path]::Combine([Environment]::GetFolderPath('Desktop'), 'Telegram Downloader RS.lnk'))
            $sc.TargetPath = (Join-Path $extra 'tgdlrs-login.cmd')
            $sc.WorkingDirectory = $extra
            $sc.IconLocation = (Join-Path $bin 'tgdlrs.exe') + ',0'
            $sc.Description = 'Telegram Downloader RS - sign in'
            $sc.Save()
        }

        $script:lblStatus.Text = 'Starting worker…'; [System.Windows.Forms.Application]::DoEvents()
        & $worker --version 2>&1 | Out-Null
        Start-Process -FilePath $worker -WindowStyle Hidden -ErrorAction SilentlyContinue

        $script:lblStatus.Text = 'Install finished.'; [System.Windows.Forms.Application]::DoEvents()
        $script:InstallOk = $true
        $script:InstallError = ''
    } catch {
        $script:InstallOk = $false
        $script:InstallError = $_.Exception.Message
        $script:lblStatus.Text = "ERROR: $($script:InstallError)"
    }
    $btnNext.Enabled = $true
    $btnNext.Text = 'Finish'
}

function Show-Step4 {
    $script:step = 4
    $header.Text = if ($script:InstallOk) { 'Installation complete' } else { 'Installation failed' }
    $btnBack.Enabled = $false
    $btnNext.Text = 'Close'
    Clear-Content
    if ($script:InstallOk) {
        Add-Label "$AppName $Version installed to:" 10 20 560 22 $true | Out-Null
        Add-Label $script:InstallDir 10 50 560 22 | Out-Null
        Add-Label '' 10 85 560 10 | Out-Null
        Add-Label '- Binaries:  bin\' 20 110 560 22 | Out-Null
        Add-Label '- Login GUI: extras\tgdlrs-login.cmd' 20 135 560 22 | Out-Null
        Add-Label '- Autostart: worker runs at login' 20 160 560 22 | Out-Null
        Add-Label '' 10 200 560 10 | Out-Null
        Add-Label 'Next: run the login GUI (desktop shortcut) to sign in to Telegram.' 10 230 560 40 | Out-Null
    } else {
        Add-Label $script:InstallError 10 30 560 80 | Out-Null
        Add-Label 'Fix the issue and run setup again.' 10 130 560 22 | Out-Null
    }
}

$script:step = 1
Show-Step1

$btnBack.Add_Click({ if ($script:step -eq 2) { Show-Step1 } })
$btnNext.Add_Click({
    switch ($script:step) {
        1 { Show-Step2 }
        2 { Show-Step3; Show-Step4 }
        3 { Show-Step4 }
        4 { $form.Close() }
    }
})

[System.Windows.Forms.Application]::Run($form)
