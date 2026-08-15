# Rebuilds nle-engine in release mode and redeploys the standalone desktop
# app: copies the exe + its full FFmpeg DLL dependency closure into
# %LOCALAPPDATA%\Programs\nle-engine, then ensures the Desktop and Start Menu
# shortcuts exist and point at that copy.
#
# Why a bundled copy at all, rather than pointing shortcuts straight at
# target\release\nle.exe: that path needs FFmpeg's bin directory on PATH to
# resolve avcodec/avformat/avutil/etc. at runtime, which is only true in a
# dev shell that has explicitly exported it (see .cargo/config.toml and this
# project's memory notes) — a plain double-click from Explorer has no such
# PATH and fails with STATUS_DLL_NOT_FOUND. Bundling the DLLs next to the exe
# makes it launchable exactly like any other installed Windows app, with no
# environment setup required.
#
# The DLL list below is not a guess — it's the real transitive dependency
# closure, traced with `objdump -p` down through avdevice's own imports
# (avdevice -> avfilter -> postproc), which a shallow "just copy what the exe
# directly imports" pass misses entirely and fails at first launch.
#
# Usage: powershell -File scripts\deploy-desktop-app.ps1
# Run from anywhere; paths below are resolved relative to this script.

$ErrorActionPreference = "Stop"

$repoRoot = Split-Path -Parent $PSScriptRoot
$ffmpegBin = "C:\Users\aashw\tools\ffmpeg-n7.1-latest-win64-gpl-shared-7.1\bin"
$installDir = "$env:LOCALAPPDATA\Programs\nle-engine"
$requiredDlls = @(
    "avcodec-61.dll",
    "avdevice-61.dll",
    "avfilter-10.dll",
    "avformat-61.dll",
    "avutil-59.dll",
    "postproc-58.dll",
    "swresample-5.dll",
    "swscale-8.dll"
)

Write-Host "Building release..."
Push-Location $repoRoot
try {
    cargo build --workspace --release
    if ($LASTEXITCODE -ne 0) { throw "cargo build failed with exit code $LASTEXITCODE" }
} finally {
    Pop-Location
}

$builtExe = Join-Path $repoRoot "target\release\nle.exe"
if (-not (Test-Path $builtExe)) {
    throw "expected $builtExe to exist after a successful build"
}

New-Item -ItemType Directory -Force -Path $installDir | Out-Null

# Stop a running instance first — copying over a locked exe fails outright,
# and silently skipping the copy would leave a stale binary behind with no
# indication anything went wrong.
$running = Get-Process -Name "nle" -ErrorAction SilentlyContinue
if ($running) {
    Write-Host "Stopping running nle-engine instance(s)..."
    $running | Stop-Process -Force
    Start-Sleep -Milliseconds 500
}

Write-Host "Copying exe and FFmpeg dependencies to $installDir..."
Copy-Item $builtExe $installDir -Force
foreach ($dll in $requiredDlls) {
    $src = Join-Path $ffmpegBin $dll
    if (-not (Test-Path $src)) {
        throw "missing expected FFmpeg DLL: $src (has the FFmpeg install moved?)"
    }
    Copy-Item $src $installDir -Force
}

function Set-AppShortcut($path) {
    $shell = New-Object -ComObject WScript.Shell
    $shortcut = $shell.CreateShortcut($path)
    $shortcut.TargetPath = "$installDir\nle.exe"
    $shortcut.WorkingDirectory = $installDir
    $shortcut.IconLocation = "$installDir\nle.exe,0"
    $shortcut.Description = "nle-engine -- non-linear video editor"
    $shortcut.Save()
}

$desktop = [Environment]::GetFolderPath("Desktop")
Set-AppShortcut (Join-Path $desktop "NLE Engine.lnk")

$startMenuPrograms = [Environment]::GetFolderPath("Programs")
Set-AppShortcut (Join-Path $startMenuPrograms "NLE Engine.lnk")

Write-Host "Done. Installed to $installDir; Desktop and Start Menu shortcuts updated."
