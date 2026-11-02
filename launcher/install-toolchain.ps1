param(
    [Parameter(Mandatory=$true)][string]$Destination,
    [Parameter(Mandatory=$true)][string]$Version,
    [Parameter(Mandatory=$true)][string]$LlvmMingw,
    [Parameter(Mandatory=$true)][string]$LlvmMingwSha256
)
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
$target = 'x86_64-pc-windows-gnu'
$downloads = Join-Path $Destination 'downloads'
New-Item -ItemType Directory -Force $downloads | Out-Null
$marker = Join-Path $Destination 'complete'
$rustMarker = Join-Path $Destination 'rust.complete'
# An older install (marker "<Version>" or "<Version> llvm-mingw-<release>") already has this Rust; only llvm-mingw
# is (re)extracted then, from the cached archive when present.
if ((Test-Path -LiteralPath $marker) -and ((Get-Content -LiteralPath $marker -TotalCount 1) -split ' ')[0] -eq $Version) {
    Set-Content -LiteralPath $rustMarker -Value $Version
}
Remove-Item -LiteralPath $marker -ErrorAction SilentlyContinue
$rustReady = (Test-Path -LiteralPath $rustMarker) -and (Get-Content -LiteralPath $rustMarker -TotalCount 1) -eq $Version -and
    (Test-Path -LiteralPath (Join-Path $Destination 'bin/rustc.exe')) -and (Test-Path -LiteralPath (Join-Path $Destination 'bin/cargo.exe'))

# Downloads $url to $archive (resuming a partial file) unless it already has SHA-256 $expected.
function Get-Verified([string]$label, [string]$url, [string]$archive, [string]$expected) {
    if ($expected -notmatch '^[0-9a-fA-F]{64}$') { throw "Invalid SHA-256 for $label" }
    $valid = (Test-Path -LiteralPath $archive) -and ((Get-FileHash -LiteralPath $archive -Algorithm SHA256).Hash -eq $expected)
    if (!$valid) {
        Write-Output "Downloading $label. Progress below: transferred bytes / total bytes. Retry resumes partial files."
        & "$env:SystemRoot\System32\curl.exe" --fail --location --retry 3 --connect-timeout 30 --continue-at - --output $archive $url
        if ($LASTEXITCODE -ne 0) {
            Write-Output 'Resume failed; retrying a fresh download (the server may not support this partial range).'
            & "$env:SystemRoot\System32\curl.exe" --fail --location --retry 3 --connect-timeout 30 --output $archive $url
            if ($LASTEXITCODE -ne 0) { throw "Download failed. Check your internet connection and retry to resume $label." }
        }
        if ((Get-FileHash -LiteralPath $archive -Algorithm SHA256).Hash -ne $expected) {
            Remove-Item -LiteralPath $archive
            throw "SHA-256 mismatch for $label. Corrupt archive removed; retry to download again."
        }
    }
    Write-Output "Verified SHA-256: $label ($((Get-Item -LiteralPath $archive).Length) bytes). Extracting..."
}

if ($rustReady) { Write-Output "Rust $Version already installed; updating llvm-mingw only." }
foreach ($component in @('rustc','cargo','rust-std','rust-mingw') | Where-Object { !$rustReady }) {
    $name = "$component-$Version-$target"
    $archive = Join-Path $downloads "$name.tar.xz"
    $url = "https://static.rust-lang.org/dist/$name.tar.xz"
    $checksum = (Invoke-WebRequest -UseBasicParsing -Uri "$url.sha256").Content
    if ($checksum -is [byte[]]) { $checksum = [Text.Encoding]::UTF8.GetString($checksum) }
    Get-Verified "$component $Version" $url $archive ($checksum.Trim() -split '\s+')[0]
    & "$env:SystemRoot\System32\tar.exe" -xf $archive -C $downloads
    if ($LASTEXITCODE -ne 0) { throw "Could not extract $name" }
    $package = Join-Path $downloads $name
    foreach ($part in Get-Content -LiteralPath (Join-Path $package 'components')) {
        $payload = Join-Path $package $part
        foreach ($folder in @('bin','lib','libexec','share')) {
            $item = Join-Path $payload $folder
            if (Test-Path -LiteralPath $item) { Copy-Item -LiteralPath $item -Destination $Destination -Recurse -Force }
        }
    }
    Remove-Item -LiteralPath $package -Recurse -Force
}
Set-Content -LiteralPath $rustMarker -Value $Version

# llvm-mingw: its llvm-dlltool makes the import libraries for raw-dylib crates (Rust's own dlltool needs as.exe,
# which isn't shipped), and its clang is the C/C++ compiler for crates with build scripts using cc (see
# Toolchain::portable_gnu). Pinned release, SHA-256 checked against the pinned value. Extracted: the host tools,
# the shared headers, the x86_64 sysroot and clang's own headers/builtins; other architectures, docs, python and
# lldb support are skipped.
$llvmName = "llvm-mingw-$LlvmMingw-ucrt-x86_64"
$llvmArchive = Join-Path $downloads "$llvmName.zip"
Get-Verified "llvm-mingw $LlvmMingw" "https://github.com/mstorsjo/llvm-mingw/releases/download/$LlvmMingw/$llvmName.zip" $llvmArchive $LlvmMingwSha256
$llvmDir = Join-Path $Destination 'llvm-mingw'
if (Test-Path -LiteralPath $llvmDir) { Remove-Item -LiteralPath $llvmDir -Recurse -Force }
if (Test-Path -LiteralPath (Join-Path $downloads $llvmName)) { Remove-Item -LiteralPath (Join-Path $downloads $llvmName) -Recurse -Force }
& "$env:SystemRoot\System32\tar.exe" -xf $llvmArchive -C $downloads "$llvmName/bin" "$llvmName/include" "$llvmName/x86_64-w64-mingw32" "$llvmName/lib/clang/*/include" "$llvmName/lib/clang/*/lib/windows/libclang_rt.builtins-x86_64.a"
if ($LASTEXITCODE -ne 0) { throw "Could not extract $llvmName" }
Move-Item -LiteralPath (Join-Path $downloads $llvmName) -Destination $llvmDir
$dlltool = Join-Path $llvmDir 'bin/x86_64-w64-mingw32-dlltool.exe'

& (Join-Path $Destination 'bin/rustc.exe') --version
if ($LASTEXITCODE -ne 0) { throw 'Portable rustc validation failed' }
& (Join-Path $Destination 'bin/cargo.exe') --version
if ($LASTEXITCODE -ne 0) { throw 'Portable Cargo validation failed' }
$rustlib = Join-Path $Destination "lib/rustlib/$target/bin"
$env:PATH = "$(Join-Path $Destination 'bin');$rustlib;$(Join-Path $rustlib 'self-contained');$(Join-Path $llvmDir 'bin');$env:PATH"
# Probe the exact link setup the builder uses (see Toolchain::portable_gnu): a raw-dylib import made by llvm-dlltool,
# linked by rust-lld, then run. (With GNU ld the full game links its many raw-dylib imports wrongly and crashes
# before main.)
$probe = Join-Path $Destination 'toolchain-check.rs'
$probeExe = Join-Path $Destination 'toolchain-check.exe'
Set-Content -LiteralPath $probe -Encoding ASCII -Value @'
#[link(name = "kernel32", kind = "raw-dylib")]
unsafe extern "system" { fn GetCurrentThreadId() -> u32; }
fn main() { assert!(unsafe { GetCurrentThreadId() } != 0); }
'@
& (Join-Path $Destination 'bin/rustc.exe') --edition 2024 --crate-name toolchain_check $probe -o $probeExe "-Cdlltool=$dlltool" "-Clinker=$(Join-Path $rustlib 'rust-lld.exe')" '-Clinker-flavor=ld.lld' '-Clink-self-contained=yes'
if ($LASTEXITCODE -ne 0) { throw 'Portable GNU linker validation failed. See the compiler error above; retry installation.' }
& $probeExe
if ($LASTEXITCODE -ne 0) { throw 'Portable GNU toolchain check program failed to run; retry installation.' }
# C probe: a crate whose build script compiles C with the cc crate, using the same compiler settings as the builder
# (CC/CXX/AR from llvm-mingw, see Toolchain::portable_gnu), linked and run. Needs crates.io access like any build.
$clang = Join-Path $llvmDir 'bin/x86_64-w64-mingw32-clang.exe'
foreach ($name in @('CC', 'CC_x86_64_pc_windows_gnu')) { Set-Item "env:$name" $clang }
foreach ($name in @('CXX', 'CXX_x86_64_pc_windows_gnu')) { Set-Item "env:$name" (Join-Path $llvmDir 'bin/x86_64-w64-mingw32-clang++.exe') }
foreach ($name in @('AR', 'AR_x86_64_pc_windows_gnu')) { Set-Item "env:$name" (Join-Path $llvmDir 'bin/llvm-ar.exe') }
Remove-Item env:RUSTFLAGS, env:CARGO_BUILD_TARGET, env:CFLAGS, env:CXXFLAGS -ErrorAction SilentlyContinue
$env:RUSTC = Join-Path $Destination 'bin/rustc.exe'
$env:CARGO_ENCODED_RUSTFLAGS = @("-Cdlltool=$dlltool", "-Clinker=$(Join-Path $rustlib 'rust-lld.exe')", '-Clinker-flavor=ld.lld', '-Clink-self-contained=yes') -join [char]0x1f
$cprobe = Join-Path $Destination 'toolchain-check-c'
if (Test-Path -LiteralPath $cprobe) { Remove-Item -LiteralPath $cprobe -Recurse -Force }
New-Item -ItemType Directory -Force (Join-Path $cprobe 'src') | Out-Null
Set-Content -LiteralPath (Join-Path $cprobe 'Cargo.toml') -Encoding ASCII -Value @'
[package]
name = "toolchain_check_c"
version = "0.1.0"
edition = "2024"

[build-dependencies]
cc = "=1.8.0"
'@
Set-Content -LiteralPath (Join-Path $cprobe 'build.rs') -Encoding ASCII -Value 'fn main() { cc::Build::new().file("src/probe.c").compile("probe"); }'
Set-Content -LiteralPath (Join-Path $cprobe 'src/probe.c') -Encoding ASCII -Value @'
#include <assert.h>
#include <stdlib.h>
int probe(int x) { assert(x > 0); return abs(-x) + (int)strtol("1", NULL, 10); }
'@
Set-Content -LiteralPath (Join-Path $cprobe 'src/main.rs') -Encoding ASCII -Value @'
unsafe extern "C" { fn probe(x: i32) -> i32; }
fn main() { assert_eq!(unsafe { probe(41) }, 42); }
'@
& (Join-Path $Destination 'bin/cargo.exe') run --release --quiet --manifest-path (Join-Path $cprobe 'Cargo.toml') --target-dir (Join-Path $cprobe 'target')
if ($LASTEXITCODE -ne 0) { throw 'Portable C compiler validation failed. See the error above; check internet access and retry installation.' }
Remove-Item -LiteralPath $cprobe -Recurse -Force
Set-Content -LiteralPath (Join-Path $Destination 'complete') -Value "$Version llvm-mingw-$LlvmMingw cc"
Write-Output 'Portable toolchain ready. No system settings were changed.'
