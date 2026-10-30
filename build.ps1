param([ValidateSet('release','small')][string]$Profile='release', [switch]$Test)
$ErrorActionPreference='Stop'
$cargoCommand=Get-Command cargo -ErrorAction SilentlyContinue
$cargoPath=if($cargoCommand){$cargoCommand.Source}else{Join-Path $env:USERPROFILE '.cargo\bin\cargo.exe'}
if(!(Test-Path -LiteralPath $cargoPath)){throw 'Install stable Rust (MSVC target) and Visual Studio Build Tools with Desktop development with C++.'}
Push-Location $PSScriptRoot
try {
    if($Test){
        & $cargoPath test --locked
        if($LASTEXITCODE -ne 0){throw 'Tests failed.'}
    }
    & $cargoPath build --profile $Profile --locked
    if($LASTEXITCODE -ne 0){throw 'Build failed.'}
    $outputDir=Join-Path $PSScriptRoot 'dist'
    New-Item -ItemType Directory -Force -Path $outputDir | Out-Null
    $outputPath=Join-Path $outputDir 'DefaultPicker.exe'
    Copy-Item -LiteralPath (Join-Path $PSScriptRoot "target\$Profile\defaultpicker.exe") -Destination $outputPath -Force
    Write-Output $outputPath
    Write-Output ("Size: {0:N0} bytes" -f (Get-Item -LiteralPath $outputPath).Length)
} finally {Pop-Location}
