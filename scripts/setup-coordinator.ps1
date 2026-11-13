param(
    [string] $DataDirectory = '',
    [string] $CoordinatorSource = '',
    [string] $Bind = '127.0.0.1:8741',
    [switch] $SkipBuild
)
$ErrorActionPreference = 'Stop'
$projectRoot = Split-Path $PSScriptRoot
$workspace = Split-Path $projectRoot
if (!$DataDirectory) { $DataDirectory = Join-Path $workspace 'runtime\benchmark-service' }
if (!$CoordinatorSource) { $CoordinatorSource = Join-Path $workspace 'agent_coordinator' }
$DataDirectory = [IO.Path]::GetFullPath($DataDirectory)
$CoordinatorSource = (Resolve-Path -LiteralPath $CoordinatorSource).Path
if ($DataDirectory.Equals($CoordinatorSource, [StringComparison]::OrdinalIgnoreCase) -or $DataDirectory.StartsWith($CoordinatorSource.TrimEnd('\')+'\', [StringComparison]::OrdinalIgnoreCase)) {
    throw 'Benchmark data must be outside coordinator source'
}
New-Item -ItemType Directory -Force -Path $DataDirectory | Out-Null
$buildRoot = Join-Path (Split-Path $DataDirectory) 'build'
$python = (Get-Command python.exe).Source
$installed = & (Join-Path $env:USERPROFILE '.cargo/bin/rustup.exe') toolchain list
$toolchain = if ($installed -match 'stable-x86_64-pc-windows-msvc') { 'stable-x86_64-pc-windows-msvc' } else { 'stable-x86_64-pc-windows-gnu' }
$coordinatorBuild = Join-Path $buildRoot 'coordinator'
$benchmarkBuild = Join-Path $buildRoot 'benchmark'
if (!$SkipBuild) {
    $env:CARGO_TARGET_DIR = $coordinatorBuild
    if ($toolchain -like '*gnu') { $env:RUSTFLAGS = '-C linker=rust-lld -C linker-flavor=ld.lld' }
    & (Join-Path $env:USERPROFILE '.cargo/bin/cargo.exe') "+$toolchain" build --release --locked --manifest-path (Join-Path $CoordinatorSource 'Cargo.toml')
    if ($LASTEXITCODE) { throw 'Coordinator build failed' }
    $env:CARGO_TARGET_DIR = $benchmarkBuild
    Push-Location $projectRoot
    try { & (Join-Path $PSScriptRoot 'cargo-windows.ps1') build --release --locked } finally { Pop-Location }
    if ($LASTEXITCODE) { throw 'Benchmark build failed' }
}
$coordinator = Join-Path $coordinatorBuild 'release\agent-coordinator.exe'
$profileFile = Join-Path $DataDirectory 'coordinator-profiles.json'
if (!(Test-Path -LiteralPath $profileFile)) {
    $profiles = @{default=@{
        coordinator_url='http://127.0.0.1:4242'
        workspace=@{}
        session_command=@($python, (Join-Path $PSScriptRoot 'template_session.py'))
        session_planner_command=@($python, (Join-Path $PSScriptRoot 'template_planner.py'))
    }} | ConvertTo-Json -Depth 8
    [IO.File]::WriteAllText($profileFile, $profiles, [Text.UTF8Encoding]::new($false))
}
$profiles = Get-Content -Raw -LiteralPath $profileFile | ConvertFrom-Json -AsHashtable
if ($profiles.ContainsKey('default')) {
    if (!$profiles.default.ContainsKey('workspace')) { $profiles.default.workspace = @{} }
    if (!$profiles.default.ContainsKey('coordinator_url')) { $profiles.default.coordinator_url = 'http://127.0.0.1:4242' }
    if (!$profiles.default.ContainsKey('session_command')) { $profiles.default.session_command = @($python, (Join-Path $PSScriptRoot 'template_session.py')) }
    if (!$profiles.default.ContainsKey('session_planner_command')) { $profiles.default.session_planner_command = @($python, (Join-Path $PSScriptRoot 'template_planner.py')) }
    [IO.File]::WriteAllText($profileFile, ($profiles | ConvertTo-Json -Depth 20), [Text.UTF8Encoding]::new($false))
}
& (Join-Path $PSScriptRoot 'start.ps1') -SkipBuild -Bind $Bind -DataDirectory $DataDirectory -Binary (Join-Path $benchmarkBuild 'release\harness-bench.exe')
