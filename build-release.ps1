# Build a shippable PadMix: tests -> release exe -> installer -> dist\
#   powershell -ExecutionPolicy Bypass -File build-release.ps1
# Uses its own build folder (target-release) so a running PadMix never blocks it.
$ErrorActionPreference = 'Stop'
Set-Location $PSScriptRoot

$version = (Select-String -Path Cargo.toml -Pattern '^version = "(.+)"').Matches[0].Groups[1].Value
$tdir = Join-Path $PSScriptRoot 'target-release'
Write-Host "== PadMix $version =="

Write-Host "-- tests"
cargo test --quiet
if ($LASTEXITCODE) { throw "tests failed" }

Write-Host "-- release build"
cargo build --release --target-dir $tdir
if ($LASTEXITCODE) { throw "release build failed" }
$app = Join-Path $tdir 'release\padmix.exe'

Write-Host "-- installer"
$env:PADMIX_PAYLOAD = $app
Push-Location installer
try {
    cargo build --release --target-dir (Join-Path $tdir 'installer')
    if ($LASTEXITCODE) { throw "installer build failed" }
} finally { Pop-Location; Remove-Item Env:PADMIX_PAYLOAD }
$setup = Join-Path $tdir 'installer\release\padmix-setup.exe'

New-Item -ItemType Directory -Force dist | Out-Null
Copy-Item -Force $app "dist\PadMix-$version-portable.exe"
Copy-Item -Force $setup "dist\PadMix-$version-Setup.exe"

Write-Host "-- dist"
Get-ChildItem dist -Filter "PadMix-$version-*.exe" | ForEach-Object {
    $h = (Get-FileHash $_.FullName -Algorithm SHA256).Hash
    "{0,-30} {1,12:N0} bytes  SHA256 {2}" -f $_.Name, $_.Length, $h
} | Tee-Object -FilePath "dist\PadMix-$version-SHA256.txt"
