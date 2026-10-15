param([Parameter(Mandatory=$true)][string]$FixtureRoot, [Parameter(Mandatory=$true)][string]$Nonce)
$ErrorActionPreference = 'Stop'
if ($env:MORPHEUS_WINDOWS_CAPTURE_ACCEPTANCE -ne '1' -or $env:GITHUB_ACTIONS -ne 'true' -or
    $env:RUNNER_ENVIRONMENT -ne 'github-hosted' -or $env:RUNNER_OS -ne 'Windows' -or
    $Nonce -notmatch '^[a-f0-9]{32}$') { exit 2 }
# Diagnostic phases contain literals only. Recording them is best effort and
# cannot replace an original fixture failure or change the acceptance guards.
function Write-FixtureStage([string]$Stage) {
    try { [System.IO.File]::WriteAllText((Join-Path $FixtureRoot 'fixture-stage'), $Stage) }
    catch { }
}
Write-FixtureStage 'load_forms'
Add-Type -AssemblyName System.Windows.Forms
Write-FixtureStage 'load_drawing'
Add-Type -AssemblyName System.Drawing
Write-FixtureStage 'interactive_check'
if (-not [Environment]::UserInteractive) { exit 3 }
Write-FixtureStage 'display_check'
if ([System.Windows.Forms.Screen]::AllScreens.Count -ne 1) { exit 3 }
Write-FixtureStage 'create_window'
[System.Windows.Forms.Application]::EnableVisualStyles()
$form = New-Object System.Windows.Forms.Form
$form.Text = "Morpheus capture QA $Nonce"
$form.FormBorderStyle = [System.Windows.Forms.FormBorderStyle]::None
$form.StartPosition = [System.Windows.Forms.FormStartPosition]::Manual
$form.Bounds = [System.Windows.Forms.Screen]::PrimaryScreen.Bounds
$form.TopMost = $true
$form.ShowInTaskbar = $false
$script:swapped = $false
$script:previous = ''
$red = New-Object System.Drawing.SolidBrush -ArgumentList ([System.Drawing.Color]::FromArgb(240,32,32))
$blue = New-Object System.Drawing.SolidBrush -ArgumentList ([System.Drawing.Color]::FromArgb(32,32,240))
$form.Add_Paint({ param($sender, $event)
    $half = [int][Math]::Floor($sender.ClientSize.Width / 2)
    $left = $red; $right = $blue
    if ($script:swapped) { $left = $blue; $right = $red }
    $event.Graphics.FillRectangle($left, 0, 0, $half, $sender.ClientSize.Height)
    $event.Graphics.FillRectangle($right, $half, 0, $sender.ClientSize.Width - $half, $sender.ClientSize.Height)
})
$timer = New-Object System.Windows.Forms.Timer
$timer.Interval = 50
$timer.Add_Tick({
    try { $command = [System.IO.File]::ReadAllText((Join-Path $FixtureRoot 'command')).Trim() }
    catch [System.IO.IOException] { return }
    if ($command -eq $script:previous) { return }
    switch ($command) {
        'swap' { $script:swapped = $true; $form.Refresh() }
        'retitle' { $form.Text = "Morpheus capture QA $Nonce changed"; $form.Refresh() }
        'close' { $form.Close(); return }
        default { return }
    }
    $script:previous = $command
    [System.IO.File]::WriteAllText((Join-Path $FixtureRoot 'ack'), $command)
})
$form.Add_Shown({
    Write-FixtureStage 'shown'
    $form.Activate()
    $form.Refresh()
    Write-FixtureStage 'painted'
    [System.IO.File]::WriteAllText((Join-Path $FixtureRoot 'ack'), 'ready')
    Write-FixtureStage 'ready'
    $timer.Start()
})
Write-FixtureStage 'show_window'
try { [System.Windows.Forms.Application]::Run($form) }
finally { $timer.Stop(); $timer.Dispose(); $form.Dispose(); $red.Dispose(); $blue.Dispose() }
