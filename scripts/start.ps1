param(
    [string] $Bind = '127.0.0.1:8741',
    [string] $DataDirectory = '',
    [string] $Binary = '',
    [switch] $SkipBuild
)
$ErrorActionPreference = 'Stop'
$projectRoot = Split-Path $PSScriptRoot
. (Join-Path $PSScriptRoot 'compiler-environment.ps1')
if (!$DataDirectory) { $DataDirectory = Join-Path $projectRoot 'data' }
New-Item -ItemType Directory -Force -Path $DataDirectory | Out-Null
$DataDirectory = (Resolve-Path -LiteralPath $DataDirectory).Path
$env:Path = "$(Join-Path $env:USERPROFILE '.cargo\bin');$env:Path"
if (!$SkipBuild) {
    & (Join-Path $PSScriptRoot 'cargo-windows.ps1') build --release --locked
    if ($LASTEXITCODE) { throw 'Build failed.' }
}
if (!$Binary) {
    $buildRoot = if ($env:CARGO_TARGET_DIR) { $env:CARGO_TARGET_DIR } else { Join-Path $projectRoot 'target' }
    $Binary = Join-Path $buildRoot 'release\harness-bench.exe'
}
if (!(Test-Path -LiteralPath $binary)) { throw "Missing executable: $binary" }
$service = Start-Process -FilePath $binary -WorkingDirectory $projectRoot -WindowStyle Hidden `
    -ArgumentList @('serve', '--bind', $Bind, '--data', ('"' + $DataDirectory + '"')) `
    -RedirectStandardOutput (Join-Path $DataDirectory 'stdout.log') `
    -RedirectStandardError (Join-Path $DataDirectory 'stderr.log') -PassThru
Start-Sleep -Milliseconds 500
if ($service.HasExited) { throw (Get-Content -Raw (Join-Path $DataDirectory 'stderr.log')) }
$service.Id | Set-Content (Join-Path $DataDirectory 'service.pid')
$Binary | Set-Content (Join-Path $DataDirectory 'service-binary')
Write-Output "Evaluator running at http://$Bind (PID $($service.Id))"
Write-Output "Admin token: $(Join-Path $DataDirectory 'admin-token')"
Write-Output "Judge token: $(Join-Path $DataDirectory 'judge-token')"
