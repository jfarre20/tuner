# Build a portable release zip: both exes, the FFmpeg runtime DLLs they load, README.
# Usage: .\dist.ps1            -> dist\tuner-<version>-win64.zip
param([string]$FfmpegDir = $env:FFMPEG_DIR)

$ErrorActionPreference = 'Stop'
Set-Location $PSScriptRoot
if (-not $FfmpegDir) {
    $cfg = Get-Content .cargo\config.toml -Raw
    if ($cfg -match 'FFMPEG_DIR\s*=\s*"([^"]+)"') { $FfmpegDir = $Matches[1] }
}
if (-not $FfmpegDir -or -not (Test-Path "$FfmpegDir\bin")) { throw "FFMPEG_DIR not set and not found in .cargo\config.toml" }

$ver = (Select-String -Path Cargo.toml -Pattern '^version\s*=\s*"([^"]+)"' | Select-Object -First 1).Matches[0].Groups[1].Value
cargo build --release
if ($LASTEXITCODE -ne 0) { throw "build failed" }

$stage = "dist\tuner-$ver-win64"
if (Test-Path $stage) { Remove-Item -Recurse -Force $stage }
New-Item -ItemType Directory -Force "$stage\bin" | Out-Null
# Real exes and their DLLs live in bin\; two copies of the launcher stub sit on top so the folder
# reads "TUNER" / "TUNER Setup" and nothing else.
Copy-Item target\release\tuner.exe, target\release\tuner-setup.exe "$stage\bin"
Copy-Item target\release\launcher.exe "$stage\TUNER.exe"
Copy-Item target\release\launcher.exe "$stage\TUNER Setup.exe"
# Only the libraries the exes import (and what those import), not the whole FFmpeg bin folder.
foreach ($dll in @('avcodec', 'avformat', 'avutil', 'swresample', 'swscale', 'avdevice', 'avfilter', 'postproc')) {
    Get-ChildItem "$FfmpegDir\bin\$dll-*.dll" -ErrorAction SilentlyContinue | Copy-Item -Destination "$stage\bin"
}
Copy-Item README.md $stage
Copy-Item assets\headlines.txt "$stage\bin"
@"
TUNER $ver

Double-click TUNER.exe. The first run opens the set up wizard: pick your
video folder, optionally logo / ident / music folders and a WeatherStar
checkout, then Finish & launch. "TUNER Setup.exe" reopens the editor later.
Press H in the tuner for the remote legend, S for the set up menu.

Everything else is in bin\ (the real exes, FFmpeg DLLs, headlines.txt).
Settings live in bin\tuner_settings.json and bin\tuner_lineup.json
(or %LOCALAPPDATA%\tuner when this folder isn't writable).

Needs: Windows 10+, a D3D11 GPU. Optional: WebView2 runtime (ships with Edge)
and Node.js for the WeatherStar channel.
"@ | Set-Content -Encoding utf8 "$stage\START HERE.txt"

$zip = "dist\tuner-$ver-win64.zip"
if (Test-Path $zip) { Remove-Item $zip }
Compress-Archive -Path "$stage\*" -DestinationPath $zip
$size = [math]::Round((Get-Item $zip).Length / 1MB, 1)
Write-Host "built $zip ($size MB)"
