# tgdlrs-notify.ps1 — best-effort toast window (bottom-right, auto-closes).
# Spawned detached by the worker; never blocks; never fails loudly.
param(
    [Parameter(Mandatory = $true)][string]$Title,
    [Parameter(Mandatory = $true)][string]$Body
)

try {
    Add-Type -AssemblyName System.Windows.Forms
    Add-Type -AssemblyName System.Drawing

    $form = New-Object System.Windows.Forms.Form
    $form.FormBorderStyle = 'FixedToolWindow'
    $form.ShowInTaskbar = $false
    $form.TopMost = $true
    $form.BackColor = [System.Drawing.Color]::FromArgb(243, 244, 246)
    $form.ClientSize = New-Object System.Drawing.Size(380, 76)

    $wa = [System.Windows.Forms.Screen]::PrimaryScreen.WorkingArea
    $form.StartPosition = 'Manual'
    $form.Location = New-Object System.Drawing.Point(($wa.Right - $form.Width - 16), ($wa.Bottom - $form.Height - 16))
    $form.Text = 'tgdlrs'

    $accent = New-Object System.Windows.Forms.Panel
    $accent.Dock = 'Left'
    $accent.Width = 6
    $accent.BackColor = [System.Drawing.Color]::FromArgb(26, 115, 232)
    $form.Controls.Add($accent)

    $lblTitle = New-Object System.Windows.Forms.Label
    $lblTitle.Text = $Title
    $lblTitle.Font = New-Object System.Drawing.Font('Segoe UI Semibold', 9.5)
    $lblTitle.ForeColor = [System.Drawing.Color]::FromArgb(17, 24, 39)
    $lblTitle.Location = New-Object System.Drawing.Point(18, 8)
    $lblTitle.Size = New-Object System.Drawing.Size(346, 22)
    $lblTitle.AutoEllipsis = $true
    $form.Controls.Add($lblTitle)

    $lblBody = New-Object System.Windows.Forms.Label
    $lblBody.Text = $Body
    $lblBody.Font = New-Object System.Drawing.Font('Segoe UI', 8.5)
    $lblBody.ForeColor = [System.Drawing.Color]::FromArgb(75, 85, 99)
    $lblBody.Location = New-Object System.Drawing.Point(18, 32)
    $lblBody.Size = New-Object System.Drawing.Size(346, 34)
    $form.Controls.Add($lblBody)

    $timer = New-Object System.Windows.Forms.Timer
    $timer.Interval = 8000
    $timer.Add_Tick({ $timer.Stop(); $form.Close() })
    $timer.Start()

    $form.Add_Shown({ $form.Activate() })
    [void]$form.ShowDialog()
} catch {
    exit 0
}
