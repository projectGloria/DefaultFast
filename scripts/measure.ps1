param([string]$Exe=(Join-Path $PSScriptRoot '..\dist\DefaultPicker.exe'),[ValidateRange(1,20)][int]$Runs=5,[switch]$Software)
$ErrorActionPreference='Stop'
$Exe=(Resolve-Path -LiteralPath $Exe).Path
$dataDir=Join-Path $env:LOCALAPPDATA 'DefaultPicker'
$samples=@()
for($i=0;$i -lt $Runs;$i++){
    $flags=@('--startup-probe')
    if($Software){$flags+='--software'}
    $timer=[Diagnostics.Stopwatch]::StartNew()
    $process=Start-Process -FilePath $Exe -ArgumentList $flags -PassThru
    if(!$process.WaitForExit(15000)){throw "Startup probe did not finish within 15s (PID $($process.Id)); inspect its window."}
    $timer.Stop()
    if($process.ExitCode -ne 0){throw "Probe exited with $($process.ExitCode)."}
    $values=@{}
    Get-Content -LiteralPath (Join-Path $dataDir 'startup.txt') | ForEach-Object {
        if($_ -match '^([a-z_]+)=(\d+)$'){$values[$Matches[1]]=[long]$Matches[2]}
    }
    $samples += [pscustomobject]@{Run=$i+1;ProcessLifetimeMs=[math]::Round($timer.Elapsed.TotalMilliseconds,2);FirstNativeFrameUs=$values.entry_to_first_native_paint_us;FirstGpuFrameUs=$values.entry_to_first_gpu_paint_us;WorkingSetBytes=$values.working_set_at_exit_bytes;PeakWorkingSetBytes=$values.peak_working_set_bytes}
}
$samples | Format-Table -AutoSize
$samples | Export-Csv -LiteralPath (Join-Path $dataDir 'startup-samples.csv') -NoTypeInformation
$process=Start-Process -FilePath $Exe -ArgumentList '--benchmark' -Wait -PassThru
if($process.ExitCode -ne 0){throw 'Filter benchmark failed.'}
Get-Content -LiteralPath (Join-Path $dataDir 'benchmark.txt')
Write-Output 'These are repeated process launches with normal OS caching, not an OS cold-cache experiment. Lifetime includes shutdown and file flushes.'
