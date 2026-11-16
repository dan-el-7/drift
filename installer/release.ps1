# Build a Drift release for Windows: the Rust core, the app (self-contained), then Velopack's
# one-click installer (our animated splash) and a portable zip. Unsigned for now: add --signParams
# or --azureTrustedSignFile to the vpk call once there's a certificate.
#
#   pwsh installer/release.ps1 -Version 0.1.0
#
# Output: dist/releases/Drift-win-Setup.exe, Drift-win-Portable.zip (+ update packages).
# -Clean wipes dist/releases first (rebuilding the same version); real releases keep history for updates.
# -Publish fetches the previous release from GitHub first (for delta updates), then uploads this one as
# release v<Version> (installers, packages and releases.win.json: what the app's updater reads).
# Needs `gh auth login`. The Android APK is added to the same release separately.
param([string]$Version = "0.1.0", [switch]$Clean, [switch]$Publish)
$repo = "https://github.com/dan-el-7/drift"
$ErrorActionPreference = "Stop"
$root = Split-Path $PSScriptRoot
Set-Location $root

# Keep this machine's paths (user name, folders) out of the binaries: Rust embeds source paths for
# panic messages, .NET the PDB path. (Encoded flags: one per -separated item, spaces are fine.)
$env:CARGO_ENCODED_RUSTFLAGS = @("--remap-path-prefix=$env:USERPROFILE=~", "--remap-path-prefix=$root=.") -join [char]0x1f

Write-Host "core: drift_ffi (release)"
cargo build -p drift-ffi --release
if ($LASTEXITCODE) { throw "cargo build failed" }

Write-Host "app: publish (self-contained, win-x64)"
# Outside the repo: MSBuild's publish copy breaks on an apostrophe in the path.
$pub = Join-Path ([IO.Path]::GetTempPath()) "drift-publish"
if (Test-Path $pub) { Remove-Item $pub -Recurse -Force }
dotnet publish ui\windows\Drift.csproj -c Release -p:Platform=x64 -r win-x64 --self-contained -p:Version=$Version -p:PathMap="$root=." -o $pub
if ($LASTEXITCODE) { throw "dotnet publish failed" }
if (-not (Test-Path (Join-Path $pub "drift_ffi.dll"))) { throw "drift_ffi.dll missing from the publish folder" }

Write-Host "splash"
python -I installer\splash.py ui\windows\Assets\Fonts\Geist-Variable.ttf installer\splash.gif
if ($LASTEXITCODE) { throw "splash render failed" }

$releases = Join-Path (Join-Path $root "dist") "releases"
if ($Clean -and (Test-Path $releases)) { Remove-Item $releases -Recurse -Force }
if ($Publish) {
    $token = (gh auth token).Trim()
    Write-Host "previous release (for delta updates)"
    vpk download github --repoUrl $repo --token $token --outputDir $releases
    if ($LASTEXITCODE) { Write-Warning "no previous release to build a delta from" }
}
Write-Host "pack: installer + portable"
vpk pack --packId Drift --packVersion $Version --packDir $pub --mainExe Drift.exe `
    --packTitle Drift --packAuthors Drift `
    --icon ui\windows\Assets\Drift.ico `
    --splashImage installer\splash.gif --splashProgressColor "#FFFFFF" `
    --shortcuts StartMenuRoot `
    --outputDir $releases
if ($LASTEXITCODE) { throw "vpk pack failed" }
Get-ChildItem $releases | Select-Object Name, @{ n = "MB"; e = { [math]::Round($_.Length / 1MB, 1) } }
if ($Publish) {
    Write-Host "upload: GitHub release v$Version"
    vpk upload github --repoUrl $repo --token $token --outputDir $releases --publish --releaseName "Drift $Version" --tag "v$Version"
    if ($LASTEXITCODE) { throw "vpk upload failed" }
}
