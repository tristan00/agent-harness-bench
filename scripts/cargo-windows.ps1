param([Parameter(ValueFromRemainingArguments)] [string[]] $CargoArguments)
$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot 'compiler-environment.ps1')
$installed = & (Join-Path $env:USERPROFILE '.cargo/bin/rustup.exe') toolchain list
if ($installed -match 'stable-x86_64-pc-windows-msvc') {
    $env:RUSTUP_TOOLCHAIN = 'stable-x86_64-pc-windows-msvc'
    if (!$CargoArguments) { $CargoArguments = @('test', '--locked') }
    if ($CargoArguments[0] -in @('build', 'test', 'check', 'clippy', 'run')) {
        $CargoArguments = @($CargoArguments[0], '--features', 'rusqlite/bundled') + @($CargoArguments | Select-Object -Skip 1)
    }
    & (Join-Path $env:USERPROFILE '.cargo/bin/cargo.exe') @CargoArguments
    exit $LASTEXITCODE
}
$toolchain = 'stable-x86_64-pc-windows-gnu'
$rustup = Join-Path $env:USERPROFILE '.cargo/bin/rustup.exe'
$cargo = Join-Path $env:USERPROFILE '.cargo/bin/cargo.exe'
$env:Path = "$(Split-Path $cargo);$env:Path"
$root = Split-Path $PSScriptRoot
$target = if ($env:CARGO_TARGET_DIR) { $env:CARGO_TARGET_DIR } else { Join-Path $root 'target' }
$native = Join-Path $target 'windows-sqlite'
New-Item -ItemType Directory -Force -Path $native | Out-Null
$native = (Resolve-Path -LiteralPath $native).Path

$registry = if ($env:CARGO_HOME) { $env:CARGO_HOME } else { Join-Path $env:USERPROFILE '.cargo' }
$lock = Get-Content -Raw (Join-Path $root 'Cargo.lock')
$version = [regex]::Match($lock, '(?m)^name = "libsqlite3-sys"\r?\nversion = "([^"]+)"').Groups[1].Value
$binding = Get-ChildItem "$registry/registry/src/*/libsqlite3-sys-$version/sqlite3/bindgen_bundled_version.rs" | Select-Object -First 1
if (!$binding) { throw 'Fetch the locked Cargo dependencies before using this helper.' }

$definition = Join-Path $native 'winsqlite3.def'
$import = Join-Path $native 'libsqlite3.dll.a'
$exportSource = @"
import ctypes, pathlib, re, sys
library = ctypes.WinDLL(str(pathlib.Path(sys.argv[3]) / "System32" / "winsqlite3.dll"))
bindings = pathlib.Path(sys.argv[1]).read_text(encoding="utf-8")
exports = sorted(set(re.findall(r"pub fn (sqlite3_\w+)\(", bindings)))
pathlib.Path(sys.argv[2]).write_text("LIBRARY winsqlite3.dll\nEXPORTS\n" + "\n".join(name for name in exports if hasattr(library, name)) + "\n", encoding="ascii")
"@
$exportSource | py -3 - $binding.FullName $definition $env:WINDIR
if ($LASTEXITCODE) { exit $LASTEXITCODE }
$sysroot = & $rustup run $toolchain rustc --print sysroot
if ($LASTEXITCODE) { exit $LASTEXITCODE }
$linker = Join-Path $sysroot 'lib/rustlib/x86_64-pc-windows-gnu/bin/rust-lld.exe'
& $linker -flavor link /lib "/def:$definition" /machine:x64 "/out:$import"
if ($LASTEXITCODE) { exit $LASTEXITCODE }

$env:LIBSQLITE3_SYS_USE_PKG_CONFIG = '1'
$env:SQLITE3_LIB_DIR = $native
$env:RUSTFLAGS = ("$env:RUSTFLAGS -C linker=rust-lld -C linker-flavor=ld.lld " + '--cfg getrandom_backend="windows_legacy"').Trim()
$env:RUSTUP_TOOLCHAIN = $toolchain
$env:CARGO_TARGET_DIR = $target
if (!$CargoArguments) { $CargoArguments = @('test', '--locked', '--offline') }
Push-Location $root
try {
    & $cargo @CargoArguments
    $code = $LASTEXITCODE
} finally {
    Pop-Location
}
exit $code
