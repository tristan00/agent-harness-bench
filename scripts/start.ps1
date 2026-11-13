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
$pidFile = Join-Path $DataDirectory 'service.pid'
$binaryFile = Join-Path $DataDirectory 'service-binary'
if ((Test-Path -LiteralPath $pidFile) -and (Test-Path -LiteralPath $binaryFile)) {
    $existingProcess = Get-Process -Id ([int](Get-Content -Raw -LiteralPath $pidFile)) -ErrorAction SilentlyContinue
    if ($existingProcess -and $existingProcess.Path -eq (Get-Content -Raw -LiteralPath $binaryFile).Trim()) {
        throw 'Benchmark service is already running. Leave it running until the active batch finishes.'
    }
}
$nextBinaryFile = Join-Path $DataDirectory 'next-service-binary'
$nextBinary = if (Test-Path -LiteralPath $nextBinaryFile) { (Get-Content -Raw -LiteralPath $nextBinaryFile).Trim() } else { '' }
if (!$Binary -and $nextBinary) { $Binary = $nextBinary; $SkipBuild = $true }
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
if ($nextBinary -and [IO.Path]::GetFullPath($Binary) -eq [IO.Path]::GetFullPath($nextBinary)) {
    Remove-Item -LiteralPath $nextBinaryFile
}
Write-Output "Evaluator running at http://$Bind (PID $($service.Id))"
Write-Output "Admin token: $(Join-Path $DataDirectory 'admin-token')"
Write-Output "Judge token: $(Join-Path $DataDirectory 'judge-token')"
