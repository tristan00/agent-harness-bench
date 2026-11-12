param([string] $DataDirectory = '')
$ErrorActionPreference = 'Stop'
$projectRoot = Split-Path $PSScriptRoot
if (!$DataDirectory) { $DataDirectory = Join-Path $projectRoot 'data' }
$pidFile = Join-Path $DataDirectory 'service.pid'
if (!(Test-Path -LiteralPath $pidFile)) { throw "No service PID file at $pidFile" }
$serviceProcessId = [int](Get-Content -Raw -LiteralPath $pidFile)
$service = Get-Process -Id $serviceProcessId -ErrorAction SilentlyContinue
if ($service) {
    $binaryFile = Join-Path $DataDirectory 'service-binary'
    $binary = if (Test-Path -LiteralPath $binaryFile) { (Get-Content -Raw -LiteralPath $binaryFile).Trim() } else { Join-Path $projectRoot 'target\release\harness-bench.exe' }
    $expected = (Resolve-Path -LiteralPath $binary).Path
    if ($service.Path -ne $expected) { throw 'PID belongs to another executable; refusing to stop it.' }
    Stop-Process -Id $serviceProcessId
    $service.WaitForExit()
}
Remove-Item -LiteralPath $pidFile
Write-Output 'Evaluator stopped. Database and submissions are retained.'
