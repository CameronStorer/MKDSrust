param([Parameter(Mandatory=$true)][string]$Destination, [Parameter(Mandatory=$true)][string]$Version)
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
$target = 'x86_64-pc-windows-gnu'
$downloads = Join-Path $Destination 'downloads'
New-Item -ItemType Directory -Force $downloads | Out-Null
foreach ($component in @('rustc','cargo','rust-std','rust-mingw')) {
    $name = "$component-$Version-$target"
    $archive = Join-Path $downloads "$name.tar.xz"
    $url = "https://static.rust-lang.org/dist/$name.tar.xz"
    Write-Output "Downloading $component $Version. Progress below: transferred bytes / total bytes. Retry resumes partial files."
    $checksum = (Invoke-WebRequest -UseBasicParsing -Uri "$url.sha256").Content
    if ($checksum -is [byte[]]) { $checksum = [Text.Encoding]::UTF8.GetString($checksum) }
    $expected = ($checksum.Trim() -split '\s+')[0]
    if ($expected -notmatch '^[0-9a-fA-F]{64}$') { throw "Invalid published SHA-256 for $name" }
    $valid = (Test-Path -LiteralPath $archive) -and ((Get-FileHash -LiteralPath $archive -Algorithm SHA256).Hash -eq $expected)
    if (!$valid) {
        & "$env:SystemRoot\System32\curl.exe" --fail --location --retry 3 --connect-timeout 30 --continue-at - --output $archive $url
        if ($LASTEXITCODE -ne 0) {
            Write-Output 'Resume failed; retrying a fresh download (the server may not support this partial range).'
            & "$env:SystemRoot\System32\curl.exe" --fail --location --retry 3 --connect-timeout 30 --output $archive $url
            if ($LASTEXITCODE -ne 0) { throw "Download failed. Check your internet connection and retry to resume $component." }
        }
        if ((Get-FileHash -LiteralPath $archive -Algorithm SHA256).Hash -ne $expected) {
            Remove-Item -LiteralPath $archive
            throw "SHA-256 mismatch for $component. Corrupt archive removed; retry to download again."
        }
    }
    Write-Output "Verified SHA-256: $name ($((Get-Item -LiteralPath $archive).Length) bytes). Extracting..."
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
& (Join-Path $Destination 'bin/rustc.exe') --version
if ($LASTEXITCODE -ne 0) { throw 'Portable rustc validation failed' }
& (Join-Path $Destination 'bin/cargo.exe') --version
if ($LASTEXITCODE -ne 0) { throw 'Portable Cargo validation failed' }
$env:PATH = "$(Join-Path $Destination 'bin');$(Join-Path $Destination "lib/rustlib/$target/bin");$(Join-Path $Destination "lib/rustlib/$target/bin/self-contained");$env:PATH"
$probe = Join-Path $Destination 'toolchain-check.rs'
Set-Content -LiteralPath $probe -Value 'fn main() {}' -Encoding ASCII
& (Join-Path $Destination 'bin/rustc.exe') --crate-name toolchain_check --target $target $probe -o (Join-Path $Destination 'toolchain-check.exe')
if ($LASTEXITCODE -ne 0) { throw 'Portable GNU linker validation failed. See the compiler error above; retry installation.' }
Set-Content -LiteralPath (Join-Path $Destination 'complete') -Value $Version
Write-Output 'Portable toolchain ready. No system settings were changed.'
