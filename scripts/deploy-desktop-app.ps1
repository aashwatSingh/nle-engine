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
# Hand-installed tools live under NLE_TOOLS_DIR if it's set, otherwise
# %USERPROFILE%\tools -- the same rule as `integrity::tools_dir`.
$toolsDir = if ($env:NLE_TOOLS_DIR) { $env:NLE_TOOLS_DIR } else { Join-Path $env:USERPROFILE "tools" }
$ffmpegBin = Join-Path $toolsDir "ffmpeg-n7.1-latest-win64-gpl-shared-7.1\bin"
$installDir = "$env:LOCALAPPDATA\Programs\nle-engine"
# Pinned to SHA-256 as vetted: BtbN's ffmpeg-n7.1-latest-win64-gpl-shared-7.1
# build, FFmpeg n7.1.5-12-g1fdbca85aa (2026-08-07). The installed app loads
# these at startup, before any of its own code runs, so it can't check them
# itself — this script is the one place they can be checked. To upgrade FFmpeg
# deliberately, re-pin as described in docs/security.md.
$requiredDlls = [ordered]@{
    "avcodec-61.dll"   = "c57bec1c6c3b4df5b9f04718380e5f3923e30e7234bb4324f631c387606e5261"
    "avdevice-61.dll"  = "c366213df35bf5f8b39594b81253f18bacff27ba8bd05fabf6445c39348ad52c"
    "avfilter-10.dll"  = "e5eb54d9b8978fcd4338d556152da4c029f07dbcd9c816b975ab6f5971f2c2bd"
    "avformat-61.dll"  = "36d9ea76ebe3ee8e484323fc3644c628c1f83c2baef9fc400b644a99553f304b"
    "avutil-59.dll"    = "8a6cfc56b0c28b1d143068b4198fb0f556d73fc0e84ffb76c62a6177b8f23b7a"
    "postproc-58.dll"  = "c5ae8bd2f60791c5b5bc746f8d48ea557c0a30832b8963b814393c4557d95797"
    "swresample-5.dll" = "79d7b27e209976715001525df230fa0f7f5ab2f2dc86810b117699b883031e36"
    "swscale-8.dll"    = "9a379004bf735fffb94aa81d833408119d30e1e5ea782001f4293d7dd7ce1250"
}

function Assert-Sha256($path, $expected) {
    if (-not (Test-Path $path)) {
        throw "missing expected file: $path (has the FFmpeg install moved?)"
    }
    $actual = (Get-FileHash -Algorithm SHA256 $path).Hash.ToLower()
    if ($actual -ne $expected) {
        throw "$path failed its integrity check (expected SHA-256 $expected, found $actual) -- it has changed since it was vetted; reinstall it, or re-pin it deliberately (see docs/security.md)"
    }
}

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

# Checked before anything in the install directory is touched, so a bad DLL
# leaves the previous install working rather than half-replaced.
foreach ($dll in $requiredDlls.Keys) {
    Assert-Sha256 (Join-Path $ffmpegBin $dll) $requiredDlls[$dll]
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
foreach ($dll in $requiredDlls.Keys) {
    Copy-Item (Join-Path $ffmpegBin $dll) $installDir -Force
    # Again after copying: what the app loads is the copy, not the source.
    Assert-Sha256 (Join-Path $installDir $dll) $requiredDlls[$dll]
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
