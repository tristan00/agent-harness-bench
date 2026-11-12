param(
    [string] $Bind = '127.0.0.1:8741',
    [string] $DataDirectory = '',
    [switch] $SkipBuild
)
$ErrorActionPreference = 'Stop'
$projectRoot = Split-Path $PSScriptRoot
if (!$DataDirectory) { $DataDirectory = Join-Path $projectRoot 'data' }
New-Item -ItemType Directory -Force -Path $DataDirectory | Out-Null
$DataDirectory = (Resolve-Path -LiteralPath $DataDirectory).Path
$env:Path = "$(Join-Path $env:USERPROFILE '.cargo\bin');$env:Path"
if (!$SkipBuild) {
    & (Join-Path $PSScriptRoot 'cargo-windows.ps1') build --release --locked
    if ($LASTEXITCODE) { throw 'Build failed.' }
}
$binary = Join-Path $projectRoot 'target\release\harness-bench.exe'
if (!(Test-Path -LiteralPath $binary)) { throw "Missing executable: $binary" }
$service = Start-Process -FilePath $binary -WorkingDirectory $projectRoot -WindowStyle Hidden `
    -ArgumentList @('serve', '--bind', $Bind, '--data', ('"' + $DataDirectory + '"')) `
    -RedirectStandardOutput (Join-Path $DataDirectory 'stdout.log') `
    -RedirectStandardError (Join-Path $DataDirectory 'stderr.log') -PassThru
Start-Sleep -Milliseconds 500
if ($service.HasExited) { throw (Get-Content -Raw (Join-Path $DataDirectory 'stderr.log')) }
$service.Id | Set-Content (Join-Path $DataDirectory 'service.pid')
Write-Output "Evaluator running at http://$Bind (PID $($service.Id))"
Write-Output "Admin token: $(Join-Path $DataDirectory 'admin-token')"
Write-Output "Judge token: $(Join-Path $DataDirectory 'judge-token')"
