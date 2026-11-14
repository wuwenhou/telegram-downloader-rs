# Telegram Downloader RS - bilingual (EN/VI) one-time login window.
# Shown when not logged in: enter API ID/Hash + phone, click ①.
# OTP / 2FA are typed into the terminal window (hidden), never into this form.

$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Windows.Forms
Add-Type -AssemblyName System.Drawing

$TgdlrsHome = if ($env:TGDLRS_HOME) { $env:TGDLRS_HOME } else { Join-Path $env:APPDATA 'tgdlrs' }
$ConfigPath = Join-Path $TgdlrsHome 'config.json'
$SessionPath = Join-Path $TgdlrsHome 'telegram.session.json'
$TgdlrsExe = Join-Path $env:LOCALAPPDATA 'tgdlrs\bin\tgdlrs.exe'
$script:loginProc = $null

$script:LANG = @{
    vi = @{
        form_title   = 'Telegram Downloader RS — Đăng nhập'
        title        = 'ĐĂNG NHẬP TELEGRAM (chạy một lần)'
        subtitle     = 'Điền API lấy từ my.telegram.org rồi bấm ①. Bí mật chỉ lưu trên máy bạn (config.json).'
        grp_status   = 'Trạng thái hiện tại'
        worker_check = 'Worker: đang kiểm tra…'
        session_check= 'Session: đang kiểm tra…'
        worker_running   = 'Worker: đang chạy (pid {0}, v{1})'
        worker_stopped   = 'Worker: chưa chạy (tgdlrs daemon start)'
        worker_unreachable = 'Worker: không phản hồi'
        session_yes  = 'ĐÃ ĐĂNG NHẬP'
        session_no   = 'chưa đăng nhập'
        session_file = 'Session: file có sẵn (chưa xác thực qua worker)'
        session_none = 'Session: chưa có file session'
        grp_api      = 'Thông tin từ my.telegram.org'
        l_apiid      = 'API ID (số, ví dụ 12345678)'
        l_apihash    = 'API Hash (32 ký tự hex)'
        chk_show     = 'Hiện hash'
        l_phone      = 'Số điện thoại (định dạng quốc tế, ví dụ +84912345678)'
        chk_force    = 'Đăng nhập lại từ đầu (--force) — dùng khi đổi tài khoản'
        grp_guide    = 'Hướng dẫn nhanh'
        guide        = @"
1. Trình duyệt vào https://my.telegram.org/apps → đăng nhập → Create application.
2. Copy App api_id (số) và App api_hash (32 hex) vào 2 ô trên → bấm ①.
3. Cửa sổ đen hiện ra → gõ mã OTP Telegram gửi vào app (không hiện chữ)
   rồi gõ mật khẩu 2FA nếu có. Xong quay lại cửa sổ này, tự kiểm tra.
"@
        btn_send     = '①  Lưu và gửi mã OTP'
        btn_refresh  = '②  Kiểm tra lại trạng thái'
        btn_config   = 'Mở config.json'
        lbl_ready    = 'Sẵn sàng — điền API ID/Hash rồi bấm ①.'
        st_saved     = 'Đã lưu config. Đang mở cửa sổ đen — gõ mã OTP (và 2FA nếu có)…'
        st_saved_force = 'Đã lưu config. Đang mở cửa sổ đăng nhập lại từ đầu (--force) — gõ OTP (và 2FA)…'
        st_success   = 'ĐÃ ĐĂNG NHẬP ✓ — có thể đóng cửa sổ này.'
        st_refresh_no = 'Chưa đăng nhập — bấm ① nếu chưa làm.'
        st_failed_fmt= 'Đăng nhập chưa thành công (cửa sổ đen thoát mã {0}). Kiểm tra lại API ID/Hash hoặc OTP, rồi bấm ① thử lại.'
        err_apiid    = 'API ID phải là một số nguyên dương.'
        err_apihash  = 'API Hash phải gồm đúng 32 ký tự hex (0-9 a-f).'
        err_phone    = 'Số điện thoại phải bắt đầu bằng + và mã quốc gia (ví dụ +849…).'
        err_write    = 'Không ghi được config.json: {0}'
        err_noexe    = 'Không thấy tgdlrs.exe tại {0}'
        grp_download = 'Thư mục tải'
        l_dir        = 'Vị trí tải file về (dùng cho mọi job mới)'
        btn_browse   = '…'
        btn_savedir  = 'Lưu thư mục tải'
        dir_hint     = 'Chỉ áp dụng cho job MỚI; worker đang chạy giữ thư mục cũ.'
        dir_saved    = 'Đã lưu thư mục tải. Job mới sẽ tải về đó; job đang chạy giữ thư mục cũ.'
        dir_invalid  = 'Đường dẫn không hợp lệ hoặc không tạo/không ghi được: {0}'
        browse_desc  = 'Chọn thư mục tải về'
        lang_label   = 'Ngôn ngữ:'
    }
    en = @{
        form_title   = 'Telegram Downloader RS — Login'
        title        = 'TELEGRAM LOGIN (one-time)'
        subtitle     = 'Fill in the API from my.telegram.org, then click ①. Secrets are stored only on this machine (config.json).'
        grp_status   = 'Current status'
        worker_check = 'Worker: checking…'
        session_check= 'Session: checking…'
        worker_running   = 'Worker: running (pid {0}, v{1})'
        worker_stopped   = 'Worker: not running (tgdlrs daemon start)'
        worker_unreachable = 'Worker: not responding'
        session_yes  = 'LOGGED IN'
        session_no   = 'not logged in'
        session_file = 'Session: file present (not verified via worker)'
        session_none = 'Session: no session file'
        grp_api      = 'Credentials from my.telegram.org'
        l_apiid      = 'API ID (number, e.g. 12345678)'
        l_apihash    = 'API Hash (32 hex characters)'
        chk_show     = 'Show hash'
        l_phone      = 'Phone number (international, e.g. +84912345678)'
        chk_force    = 'Log in again from scratch (--force) — use to switch accounts'
        grp_guide    = 'Quick help'
        guide        = @"
1. Browser → https://my.telegram.org/apps → log in → Create application.
2. Copy App api_id (number) and App api_hash (32 hex) into the fields → click ①.
3. A black window opens → type the OTP Telegram sent to the app (hidden),
   then your 2FA password if you have one. This window checks the result.
"@
        btn_send     = '①  Save and send OTP'
        btn_refresh  = '②  Check status again'
        btn_config   = 'Open config.json'
        lbl_ready    = 'Ready — fill API ID/Hash and click ①.'
        st_saved     = 'Config saved. Opening console — type the OTP (and 2FA if any)…'
        st_saved_force = 'Config saved. Opening re-login window (--force) — type OTP (and 2FA)…'
        st_success   = 'LOGGED IN ✓ — you may close this window.'
        st_refresh_no = 'Not logged in yet — click ① if you have not.'
        st_failed_fmt= 'Login failed (console exited with {0}). Check API ID/Hash or OTP, then click ① to retry.'
        err_apiid    = 'API ID must be a positive integer.'
        err_apihash  = 'API Hash must be exactly 32 hex characters (0-9 a-f).'
        err_phone    = 'Phone must start with + and a country code (e.g. +849…).'
        err_write    = 'Cannot write config.json: {0}'
        err_noexe    = 'tgdlrs.exe not found at {0}'
        grp_download = 'Download folder'
        l_dir        = 'Where files get saved (applies to new jobs)'
        btn_browse   = '…'
        btn_savedir  = 'Save folder'
        dir_hint     = 'Applies to NEW jobs only; a running worker keeps the old folder.'
        dir_saved    = 'Download folder saved. New jobs will use it; running jobs keep the old folder.'
        dir_invalid  = 'Path is invalid or cannot be created/written: {0}'
        browse_desc  = 'Choose the download folder'
        lang_label   = 'Language:'
    }
}

function Get-ConfigSafe {
    if (Test-Path $ConfigPath) {
        try { return (Get-Content $ConfigPath -Raw | ConvertFrom-Json) } catch { return $null }
    }
    return $null
}

function Set-GuiLang([string]$lang) {
    $cfg = [pscustomobject]@{}
    if (Test-Path $ConfigPath) {
        try { $cfg = Get-Content $ConfigPath -Raw | ConvertFrom-Json } catch { $cfg = [pscustomobject]@{} }
    }
    $cfg | Add-Member -NotePropertyName gui_lang -NotePropertyValue $lang -Force
    try {
        $json = $cfg | ConvertTo-Json -Depth 10
        [System.IO.File]::WriteAllText($ConfigPath, $json, (New-Object System.Text.UTF8Encoding($false)))
    } catch {}
}

function Get-GuiLang {
    $cfg = Get-ConfigSafe
    if ($cfg -and $cfg.gui_lang -and $cfg.gui_lang -in @('vi','en')) { return [string]$cfg.gui_lang }
    $culture = [System.Globalization.CultureInfo]::CurrentUICulture.Name
    if ($culture -like 'vi*') { return 'vi' }
    return 'en'
}

$script:currentLang = Get-GuiLang
$script:T = $script:LANG[$script:currentLang]

function Save-Credentials([int]$apiId, [string]$apiHash, [string]$phone) {
    if (Test-Path $ConfigPath) {
        Copy-Item $ConfigPath "$ConfigPath.bak-$(Get-Date -Format yyyyMMdd-HHmmss)" -Force
    }
    $cfg = Get-ConfigSafe
    if ($null -eq $cfg) { $cfg = [pscustomobject]@{} }
    $cfg | Add-Member -NotePropertyName api_id  -NotePropertyValue $apiId  -Force
    $cfg | Add-Member -NotePropertyName api_hash -NotePropertyValue $apiHash -Force
    if ($phone) { $cfg | Add-Member -NotePropertyName phone -NotePropertyValue $phone -Force }
    $cfg | Add-Member -NotePropertyName gui_lang -NotePropertyValue $script:currentLang -Force
    $json = $cfg | ConvertTo-Json -Depth 10
    [System.IO.File]::WriteAllText($ConfigPath, $json, (New-Object System.Text.UTF8Encoding($false)))
}

function Set-StatusText([string]$text, [System.Drawing.Color]$color) {
    $lbl = $form.Controls.Find('lblStatus', $true)[0]
    $lbl.Text = $text
    $lbl.ForeColor = $color
}

function Get-StatusSafe {
    try {
        $json = & $TgdlrsExe --json status 2>$null | Out-String
        return ($json | ConvertFrom-Json)
    } catch { return $null }
}

function Update-StatusLabel {
    $st = Get-StatusSafe
    $worker = $form.Controls.Find('lblWorker', $true)[0]
    $session = $form.Controls.Find('lblSession', $true)[0]
    if ($null -ne $st) {
        $worker.Text = if ($st.worker -and $st.worker.running) {
            [string]::Format($script:T.worker_running, $st.worker.pid, $st.worker.version)
        } else {
            $script:T.worker_stopped
        }
        $ok = ($null -ne $st.worker -and $st.worker.authorized)
        $user = if ($ok -and $st.worker.user) { " ($($st.worker.user))" } else { "" }
        $session.Text = "Session: " + $(if ($ok) { "$($script:T.session_yes)$user" } else { $script:T.session_no })
        $session.ForeColor = if ($ok) { [System.Drawing.Color]::FromArgb(0, 128, 0) } else { [System.Drawing.Color]::FromArgb(190, 0, 0) }
        return $ok
    }
    $worker.Text = $script:T.worker_unreachable
    $sessionPresent = (Test-Path $SessionPath) -and ((Get-Item $SessionPath).Length -gt 2)
    $session.Text = if ($sessionPresent) { $script:T.session_file } else { $script:T.session_none }
    $session.ForeColor = [System.Drawing.Color]::FromArgb(190, 0, 0)
    return $false
}

# ---------- Build form ----------
$form = New-Object System.Windows.Forms.Form
$form.Text = $script:T.form_title
$form.Size = New-Object System.Drawing.Size(660, 744)
$form.StartPosition = 'CenterScreen'
$form.FormBorderStyle = 'FixedDialog'
$form.MaximizeBox = $false
$form.MinimizeBox = $false
$form.BackColor = [System.Drawing.Color]::FromArgb(245, 246, 248)
$form.Font = New-Object System.Drawing.Font('Segoe UI', 9.5)

$lblTitle = New-Object System.Windows.Forms.Label
$lblTitle.Name = 'lblTitle'
$lblTitle.Font = New-Object System.Drawing.Font('Segoe UI Semibold', 14)
$lblTitle.ForeColor = [System.Drawing.Color]::FromArgb(32, 33, 36)
$lblTitle.Location = New-Object System.Drawing.Point(18, 12)
$lblTitle.AutoSize = $true
$form.Controls.Add($lblTitle)

$cmbLang = New-Object System.Windows.Forms.ComboBox
$cmbLang.Name = 'cmbLang'
$cmbLang.Location = New-Object System.Drawing.Point(515, 16)
$cmbLang.Size = New-Object System.Drawing.Size(120, 24)
$cmbLang.DropDownStyle = 'DropDownList'
[void]$cmbLang.Items.Add('Tiếng Việt')
[void]$cmbLang.Items.Add('English')
$cmbLang.SelectedIndex = if ($script:currentLang -eq 'vi') { 0 } else { 1 }
$form.Controls.Add($cmbLang)

$lblSub = New-Object System.Windows.Forms.Label
$lblSub.Name = 'lblSub'
$lblSub.ForeColor = [System.Drawing.Color]::FromArgb(95, 99, 104)
$lblSub.Location = New-Object System.Drawing.Point(18, 42)
$lblSub.Size = New-Object System.Drawing.Size(620, 20)
$form.Controls.Add($lblSub)

$grpStatus = New-Object System.Windows.Forms.GroupBox
$grpStatus.Name = 'grpStatus'
$grpStatus.Location = New-Object System.Drawing.Point(15, 70)
$grpStatus.Size = New-Object System.Drawing.Size(620, 74)
$form.Controls.Add($grpStatus)

$lblWorker = New-Object System.Windows.Forms.Label
$lblWorker.Name = 'lblWorker'
$lblWorker.Location = New-Object System.Drawing.Point(15, 22)
$lblWorker.AutoSize = $true
$grpStatus.Controls.Add($lblWorker)

$lblSession = New-Object System.Windows.Forms.Label
$lblSession.Name = 'lblSession'
$lblSession.Location = New-Object System.Drawing.Point(15, 44)
$lblSession.AutoSize = $true
$grpStatus.Controls.Add($lblSession)

$grp = New-Object System.Windows.Forms.GroupBox
$grp.Name = 'grpApi'
$grp.Location = New-Object System.Drawing.Point(15, 156)
$grp.Size = New-Object System.Drawing.Size(620, 214)
$form.Controls.Add($grp)

$l1 = New-Object System.Windows.Forms.Label
$l1.Name = 'l1'
$l1.Location = New-Object System.Drawing.Point(15, 26)
$l1.AutoSize = $true
$grp.Controls.Add($l1)

$tbApiId = New-Object System.Windows.Forms.TextBox
$tbApiId.Location = New-Object System.Drawing.Point(15, 46)
$tbApiId.Size = New-Object System.Drawing.Size(260, 26)
$grp.Controls.Add($tbApiId)

$l2 = New-Object System.Windows.Forms.Label
$l2.Name = 'l2'
$l2.Location = New-Object System.Drawing.Point(15, 80)
$l2.AutoSize = $true
$grp.Controls.Add($l2)

$tbApiHash = New-Object System.Windows.Forms.TextBox
$tbApiHash.Location = New-Object System.Drawing.Point(15, 100)
$tbApiHash.Size = New-Object System.Drawing.Size(420, 26)
$tbApiHash.PasswordChar = '•'
$grp.Controls.Add($tbApiHash)

$chkShow = New-Object System.Windows.Forms.CheckBox
$chkShow.Name = 'chkShow'
$chkShow.Location = New-Object System.Drawing.Point(445, 102)
$chkShow.AutoSize = $true
$chkShow.Add_CheckedChanged({ if ($chkShow.Checked) { $tbApiHash.PasswordChar = [char]0 } else { $tbApiHash.PasswordChar = '•' } })
$grp.Controls.Add($chkShow)

$l3 = New-Object System.Windows.Forms.Label
$l3.Name = 'l3'
$l3.Location = New-Object System.Drawing.Point(15, 134)
$l3.AutoSize = $true
$grp.Controls.Add($l3)

$tbPhone = New-Object System.Windows.Forms.TextBox
$tbPhone.Location = New-Object System.Drawing.Point(15, 154)
$tbPhone.Size = New-Object System.Drawing.Size(260, 26)
$cfgNow = Get-ConfigSafe
if ($cfgNow -and $cfgNow.phone) { $tbPhone.Text = [string]$cfgNow.phone }
if ($cfgNow -and $cfgNow.api_id) { $tbApiId.Text = [string]$cfgNow.api_id }
$grp.Controls.Add($tbPhone)

$chkForce = New-Object System.Windows.Forms.CheckBox
$chkForce.Name = 'chkForce'
$chkForce.Location = New-Object System.Drawing.Point(15, 184)
$chkForce.AutoSize = $true
$chkForce.Font = New-Object System.Drawing.Font('Segoe UI', 8.5)
$grp.Controls.Add($chkForce)

$grpDownload = New-Object System.Windows.Forms.GroupBox
$grpDownload.Name = 'grpDownload'
$grpDownload.Location = New-Object System.Drawing.Point(15, 384)
$grpDownload.Size = New-Object System.Drawing.Size(620, 100)
$form.Controls.Add($grpDownload)

$lDir = New-Object System.Windows.Forms.Label
$lDir.Name = 'lDir'
$lDir.Location = New-Object System.Drawing.Point(15, 22)
$lDir.AutoSize = $true
$grpDownload.Controls.Add($lDir)

$tbDir = New-Object System.Windows.Forms.TextBox
$tbDir.Name = 'tbDir'
$tbDir.Location = New-Object System.Drawing.Point(15, 44)
$tbDir.Size = New-Object System.Drawing.Size(385, 26)
$cfgDir = Get-ConfigSafe
if ($cfgDir -and $cfgDir.download_dir) { $tbDir.Text = [string]$cfgDir.download_dir } else { $tbDir.Text = 'D:\Downloads\Brave' }
$grpDownload.Controls.Add($tbDir)

$btnBrowse = New-Object System.Windows.Forms.Button
$btnBrowse.Name = 'btnBrowse'
$btnBrowse.Location = New-Object System.Drawing.Point(408, 44)
$btnBrowse.Size = New-Object System.Drawing.Size(34, 26)
$grpDownload.Controls.Add($btnBrowse)

$btnSaveDir = New-Object System.Windows.Forms.Button
$btnSaveDir.Name = 'btnSaveDir'
$btnSaveDir.Location = New-Object System.Drawing.Point(450, 44)
$btnSaveDir.Size = New-Object System.Drawing.Size(150, 26)
$grpDownload.Controls.Add($btnSaveDir)

$lblDirHint = New-Object System.Windows.Forms.Label
$lblDirHint.Name = 'lblDirHint'
$lblDirHint.Location = New-Object System.Drawing.Point(15, 76)
$lblDirHint.AutoSize = $true
$lblDirHint.ForeColor = [System.Drawing.Color]::FromArgb(120, 120, 120)
$lblDirHint.Font = New-Object System.Drawing.Font('Segoe UI', 8)
$grpDownload.Controls.Add($lblDirHint)

$grpGuide = New-Object System.Windows.Forms.GroupBox
$grpGuide.Name = 'grpGuide'
$grpGuide.Location = New-Object System.Drawing.Point(15, 496)
$grpGuide.Size = New-Object System.Drawing.Size(620, 100)
$form.Controls.Add($grpGuide)

$lblGuide = New-Object System.Windows.Forms.Label
$lblGuide.Name = 'lblGuide'
$lblGuide.Location = New-Object System.Drawing.Point(12, 20)
$lblGuide.AutoSize = $true
$grpGuide.Controls.Add($lblGuide)

$btnSend = New-Object System.Windows.Forms.Button
$btnSend.Name = 'btnSend'
$btnSend.Font = New-Object System.Drawing.Font('Segoe UI Semibold', 10)
$btnSend.Location = New-Object System.Drawing.Point(30, 608)
$btnSend.Size = New-Object System.Drawing.Size(190, 42)
$btnSend.BackColor = [System.Drawing.Color]::FromArgb(26, 115, 232)
$btnSend.ForeColor = [System.Drawing.Color]::White
$btnSend.FlatStyle = 'Flat'
$form.Controls.Add($btnSend)

$btnRefresh = New-Object System.Windows.Forms.Button
$btnRefresh.Name = 'btnRefresh'
$btnRefresh.Location = New-Object System.Drawing.Point(240, 608)
$btnRefresh.Size = New-Object System.Drawing.Size(180, 42)
$form.Controls.Add($btnRefresh)

$btnConfig = New-Object System.Windows.Forms.Button
$btnConfig.Name = 'btnConfig'
$btnConfig.Location = New-Object System.Drawing.Point(440, 608)
$btnConfig.Size = New-Object System.Drawing.Size(165, 42)
$form.Controls.Add($btnConfig)

$lblStatus = New-Object System.Windows.Forms.Label
$lblStatus.Name = 'lblStatus'
$lblStatus.Location = New-Object System.Drawing.Point(18, 664)
$lblStatus.Size = New-Object System.Drawing.Size(620, 40)
$lblStatus.ForeColor = [System.Drawing.Color]::FromArgb(60, 64, 67)
$form.Controls.Add($lblStatus)

$timer = New-Object System.Windows.Forms.Timer
$timer.Interval = 2000

function Apply-Language([string]$lang) {
    $script:currentLang = $lang
    try {
        $script:T = $script:LANG[$lang]
        $form.Text = $script:T.form_title
        $form.Controls.Find('lblTitle', $false)[0].Text = $script:T.title
        $form.Controls.Find('lblSub', $false)[0].Text = $script:T.subtitle
        $form.Controls.Find('grpStatus', $false)[0].Text = $script:T.grp_status
        $form.Controls.Find('grpApi', $false)[0].Text = $script:T.grp_api
        $form.Controls.Find('l1', $true)[0].Text = $script:T.l_apiid
        $form.Controls.Find('l2', $true)[0].Text = $script:T.l_apihash
        $form.Controls.Find('chkShow', $true)[0].Text = $script:T.chk_show
        $form.Controls.Find('l3', $true)[0].Text = $script:T.l_phone
        $form.Controls.Find('chkForce', $true)[0].Text = $script:T.chk_force
        $form.Controls.Find('grpGuide', $false)[0].Text = $script:T.grp_guide
        $form.Controls.Find('lblGuide', $true)[0].Text = $script:T.guide
        $form.Controls.Find('btnSend', $false)[0].Text = $script:T.btn_send
        $form.Controls.Find('btnRefresh', $false)[0].Text = $script:T.btn_refresh
        $form.Controls.Find('btnConfig', $false)[0].Text = $script:T.btn_config
        $form.Controls.Find('grpDownload', $false)[0].Text = $script:T.grp_download
        $form.Controls.Find('lDir', $true)[0].Text = $script:T.l_dir
        $form.Controls.Find('btnSaveDir', $true)[0].Text = $script:T.btn_savedir
        $form.Controls.Find('lblDirHint', $true)[0].Text = $script:T.dir_hint
        $lbl = $form.Controls.Find('lblStatus', $false)[0]
        if ($lbl.Text -notmatch '✓|✗|mã|exit|success|fail') { $lbl.Text = $script:T.lbl_ready }
        $cmb = $form.Controls.Find('cmbLang', $false)[0]
        $cmb.SelectedIndex = if ($lang -eq 'vi') { 0 } else { 1 }
    } catch {
        $log = Join-Path $TgdlrsHome 'gui-error.log'
        try { [System.IO.File]::AppendAllText($log, "$(Get-Date -Format o) Apply-Language: $_`n") } catch {}
        return
    }
    try { Set-GuiLang $lang } catch {}
    try { Update-StatusLabel | Out-Null } catch {
        $log = Join-Path $TgdlrsHome 'gui-error.log'
        try { [System.IO.File]::AppendAllText($log, "$(Get-Date -Format o) Update-StatusLabel: $_`n") } catch {}
    }
}

$cmbLang.Add_SelectedIndexChanged({
    try {
        if ($cmbLang.SelectedIndex -eq 0) { Apply-Language 'vi' } else { Apply-Language 'en' }
    } catch {
        $log = Join-Path $TgdlrsHome 'gui-error.log'
        try { [System.IO.File]::AppendAllText($log, "$(Get-Date -Format o) combo: $_`n") } catch {}
    }
})

$timer.Add_Tick({
    if ($null -ne $script:loginProc -and -not $script:loginProc.HasExited) { return }
    if ($null -ne $script:loginProc -and $script:loginProc.HasExited) {
        $timer.Stop()
        $code = $script:loginProc.ExitCode
        $sessionOk = (Test-Path $SessionPath) -and ((Get-Item $SessionPath).Length -gt 2)
        if ($code -eq 0 -and $sessionOk) {
            $UpdateStatus = Update-StatusLabel
            Set-StatusText $script:T.st_success ([System.Drawing.Color]::FromArgb(0, 128, 0))
            [System.Media.SystemSounds]::Asterisk.Play()
        } else {
            Set-StatusText ([string]::Format($script:T.st_failed_fmt, $code)) ([System.Drawing.Color]::FromArgb(190, 0, 0))
            [System.Media.SystemSounds]::Hand.Play()
        }
        return
    }
    Update-StatusLabel | Out-Null
})

$btnBrowse.Add_Click({
    $dialog = New-Object System.Windows.Forms.FolderBrowserDialog
    $dialog.Description = $script:T.browse_desc
    $dialog.ShowNewFolderButton = $true
    $current = $tbDir.Text.Trim()
    if ($current -and (Test-Path $current)) { $dialog.SelectedPath = $current }
    if ($dialog.ShowDialog() -eq [System.Windows.Forms.DialogResult]::OK) { $tbDir.Text = $dialog.SelectedPath }
})

$btnSaveDir.Add_Click({
    $path = $tbDir.Text.Trim()
    try {
        if ([string]::IsNullOrWhiteSpace($path)) { throw 'empty path' }
        $path = [System.IO.Path]::GetFullPath($path)
        if (-not (Test-Path $path)) { New-Item -ItemType Directory -Force $path | Out-Null }
        $probe = Join-Path $path '.tgdlrs-write-probe'
        [System.IO.File]::WriteAllText($probe, 'ok')
        Remove-Item $probe -Force
    } catch {
        Set-StatusText ([string]::Format($script:T.dir_invalid, $_)) ([System.Drawing.Color]::FromArgb(190, 0, 0))
        [System.Media.SystemSounds]::Hand.Play()
        return
    }
    $cfg = Get-ConfigSafe
    if ($null -eq $cfg) { $cfg = [pscustomobject]@{} }
    if (Test-Path $ConfigPath) { Copy-Item $ConfigPath "$ConfigPath.bak-$(Get-Date -Format yyyyMMdd-HHmmss)" -Force }
    $cfg | Add-Member -NotePropertyName download_dir -NotePropertyValue $path -Force
    $cfg | Add-Member -NotePropertyName gui_lang -NotePropertyValue $script:currentLang -Force
    $json = $cfg | ConvertTo-Json -Depth 10
    [System.IO.File]::WriteAllText($ConfigPath, $json, (New-Object System.Text.UTF8Encoding($false)))
    Set-StatusText $script:T.dir_saved ([System.Drawing.Color]::FromArgb(0, 128, 0))
    [System.Media.SystemSounds]::Asterisk.Play()
})

$btnSend.Add_Click({
    $apiIdText = $tbApiId.Text.Trim()
    $apiHash = $tbApiHash.Text.Trim().ToLower()
    $phone = $tbPhone.Text.Trim()

    $apiId = 0
    if (-not [int]::TryParse($apiIdText, [ref]$apiId) -or $apiId -le 0) {
        Set-StatusText $script:T.err_apiid ([System.Drawing.Color]::FromArgb(190, 0, 0))
        [System.Media.SystemSounds]::Hand.Play()
        return
    }
    if ($apiHash -notmatch '^[0-9a-f]{32}$') {
        Set-StatusText $script:T.err_apihash ([System.Drawing.Color]::FromArgb(190, 0, 0))
        [System.Media.SystemSounds]::Hand.Play()
        return
    }
    if ($phone -ne '' -and $phone -notmatch '^\+\d[\d ]{7,17}$') {
        Set-StatusText $script:T.err_phone ([System.Drawing.Color]::FromArgb(190, 0, 0))
        [System.Media.SystemSounds]::Hand.Play()
        return
    }

    try {
        Save-Credentials $apiId $apiHash $phone
    } catch {
        Set-StatusText ([string]::Format($script:T.err_write, $_)) ([System.Drawing.Color]::FromArgb(190, 0, 0))
        return
    }

    if (-not (Test-Path $TgdlrsExe)) {
        Set-StatusText ([string]::Format($script:T.err_noexe, $TgdlrsExe)) ([System.Drawing.Color]::FromArgb(190, 0, 0))
        return
    }

    $force = $chkForce.Checked
    if ($force) {
        Set-StatusText $script:T.st_saved_force ([System.Drawing.Color]::FromArgb(26, 115, 232))
        $script:loginProc = Start-Process -FilePath $TgdlrsExe -ArgumentList 'login', '--force' -PassThru
    } else {
        Set-StatusText $script:T.st_saved ([System.Drawing.Color]::FromArgb(26, 115, 232))
        $script:loginProc = Start-Process -FilePath $TgdlrsExe -ArgumentList 'login' -PassThru
    }
    $timer.Start()
})

$btnRefresh.Add_Click({
    $ok = Update-StatusLabel
    if ($ok) {
        Set-StatusText $script:T.st_success ([System.Drawing.Color]::FromArgb(0, 128, 0))
        [System.Media.SystemSounds]::Asterisk.Play()
    } else {
        Set-StatusText $script:T.st_refresh_no ([System.Drawing.Color]::FromArgb(95, 99, 104))
        [System.Media.SystemSounds]::Beep.Play()
    }
})

$btnConfig.Add_Click({
    try { Start-Process notepad.exe $ConfigPath } catch { [System.Windows.Forms.MessageBox]::Show($_, 'Error') }
})

Apply-Language $script:currentLang
$form.Add_Shown({
    $form.Activate()
})

[void]$form.ShowDialog()
