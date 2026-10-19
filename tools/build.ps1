param([switch]$CheckOnly, [switch]$Offline, [switch]$SkipCopy)
$ErrorActionPreference = 'Stop'
$projectRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
$buildRoot = Join-Path $projectRoot '.build'
@('cargo-home', 'target', 'tmp', 'uv') | ForEach-Object {
    [IO.Directory]::CreateDirectory((Join-Path $buildRoot $_)) | Out-Null
}
$env:CARGO_HOME = Join-Path $buildRoot 'cargo-home'
$env:CARGO_TARGET_DIR = Join-Path $buildRoot 'target'
$env:TEMP = Join-Path $buildRoot 'tmp'
$env:TMP = $env:TEMP
$env:UV_CACHE_DIR = Join-Path $buildRoot 'uv'
$qbRustSysroot = (& rustc --print sysroot).Trim()
if ($LASTEXITCODE -ne 0) { throw 'Could not resolve the Rust sysroot.' }
# Encoded flags preserve spaces in workspace/toolchain paths. Remapping keeps
# source-location strings in published binaries free of local account paths.
$env:RUSTFLAGS = $null
$env:CARGO_ENCODED_RUSTFLAGS = @(
    '-C', 'target-feature=+crt-static',
    "--remap-path-prefix=$projectRoot=.",
    "--remap-path-prefix=$qbRustSysroot=rust-toolchain"
) -join [char]31
if ($Offline) { $env:CARGO_NET_OFFLINE = 'true' }
# Native resource assertions observe per-process GDI counts. Concurrent tests
# would count one another's temporary objects, so default to serial execution.
if (-not $env:RUST_TEST_THREADS) { $env:RUST_TEST_THREADS = '1' }
$manifestPath = Join-Path $projectRoot 'Cargo.toml'
function Invoke-Cargo([string[]]$CargoArgs) {
    & cargo @CargoArgs
    if ($LASTEXITCODE -ne 0) { throw "cargo $($CargoArgs -join ' ') failed: $LASTEXITCODE" }
}
Invoke-Cargo -CargoArgs @('fmt', '--manifest-path', $manifestPath, '--', '--check')
Invoke-Cargo -CargoArgs @('test', '--manifest-path', $manifestPath, '--locked')
Invoke-Cargo -CargoArgs @('clippy', '--manifest-path', $manifestPath, '--locked', '--all-targets', '--', '-D', 'warnings')
if (-not $CheckOnly) {
    Invoke-Cargo -CargoArgs @('build', '--manifest-path', $manifestPath, '--locked', '--release')
    if ($SkipCopy) {
        Write-Output "Staged release executable: $(Join-Path $buildRoot 'target/release/quiet-battery.exe')"
        return
    }
    $distPath = Join-Path $projectRoot 'dist'
    [IO.Directory]::CreateDirectory($distPath) | Out-Null
    Copy-Item -LiteralPath (Join-Path $buildRoot 'target/release/quiet-battery.exe') -Destination $distPath
    Write-Output "Release executable: $(Join-Path $distPath 'quiet-battery.exe')"
}
