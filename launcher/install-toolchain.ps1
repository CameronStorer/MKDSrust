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
Remove-Item -LiteralPath (Join-Path $Destination 'complete') -ErrorAction SilentlyContinue

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

foreach ($component in @('rustc','cargo','rust-std','rust-mingw')) {
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
}

# llvm-mingw: its llvm-dlltool makes the import libraries for raw-dylib crates (Rust's own dlltool needs as.exe,
# which isn't shipped). Pinned release, SHA-256 checked against the pinned value; only bin/ is extracted.
$llvmName = "llvm-mingw-$LlvmMingw-ucrt-x86_64"
$llvmArchive = Join-Path $downloads "$llvmName.zip"
Get-Verified "llvm-mingw $LlvmMingw" "https://github.com/mstorsjo/llvm-mingw/releases/download/$LlvmMingw/$llvmName.zip" $llvmArchive $LlvmMingwSha256
$llvmDir = Join-Path $Destination 'llvm-mingw'
if (Test-Path -LiteralPath $llvmDir) { Remove-Item -LiteralPath $llvmDir -Recurse -Force }
if (Test-Path -LiteralPath (Join-Path $downloads $llvmName)) { Remove-Item -LiteralPath (Join-Path $downloads $llvmName) -Recurse -Force }
& "$env:SystemRoot\System32\tar.exe" -xf $llvmArchive -C $downloads "$llvmName/bin"
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
Set-Content -LiteralPath (Join-Path $Destination 'complete') -Value "$Version llvm-mingw-$LlvmMingw"
Write-Output 'Portable toolchain ready. No system settings were changed.'
