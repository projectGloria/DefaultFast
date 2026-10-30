# Measures DefaultFast against its budgets on this machine.
# Usage: .\scripts\measure.ps1 [-Runs 5] [-Exe .\target\release\DefaultFast.exe]
# Each run launches with --startup-probe, which records first-frame time, idle working set
# (index built, window untouched) and working set after a real search, then exits.
param([string]$Exe = (Join-Path $PSScriptRoot '..\target\release\DefaultFast.exe'), [ValidateRange(1, 20)][int]$Runs = 5)
$ErrorActionPreference = 'Stop'
$Exe = (Resolve-Path -LiteralPath $Exe).Path
$report = Join-Path $env:LOCALAPPDATA 'DefaultFast\startup.txt'
$rows = @()
for ($i = 1; $i -le $Runs; $i++) {
    Remove-Item -LiteralPath $report -ErrorAction SilentlyContinue
    $p = Start-Process -FilePath $Exe -ArgumentList '--startup-probe' -PassThru
    if (!$p.WaitForExit(30000)) { $p.Kill(); throw "Probe run $i did not finish within 30 s." }
    $v = @{}
    Get-Content -LiteralPath $report | ForEach-Object { if ($_ -match '^([a-z_]+)=([\d.]+)$') { $v[$Matches[1]] = [double]$Matches[2] } }
    $rows += [pscustomobject]@{
        Run              = $i
        FirstFrameMs     = $v.first_frame_ms
        IdleWsMB         = [math]::Round($v.idle_working_set_bytes / 1MB, 1)
        AfterSearchWsMB  = [math]::Round($v.after_search_working_set_bytes / 1MB, 1)
        AfterSearchPrivMB= [math]::Round($v.after_search_private_bytes / 1MB, 1)
        PeakWsMB         = [math]::Round($v.peak_working_set_bytes / 1MB, 1)
        SearchRenderMs   = [math]::Round($v.search_and_render_us / 1000, 2)
    }
}
$rows | Format-Table -AutoSize
"Binary size: {0:N2} MB" -f ((Get-Item -LiteralPath $Exe).Length / 1MB)
'Warm launches (OS file cache populated); not a cold-boot measurement.'
