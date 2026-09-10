# Writes .cargo/config.toml for this machine from .cargo/config.toml.example,
# pointing the build at the FFmpeg, libclang and MinGW installs under
# NLE_TOOLS_DIR if it's set, otherwise %USERPROFILE%\tools -- the same rule as
# `integrity::tools_dir`.
#
# The generated file is gitignored because it holds absolute paths, and those
# include the local account name. Run this once per machine, and again if the
# tools move.
#
# Usage: powershell -File scripts\configure-machine.ps1

$ErrorActionPreference = "Stop"

$repoRoot = Split-Path -Parent $PSScriptRoot
$toolsDir = if ($env:NLE_TOOLS_DIR) { $env:NLE_TOOLS_DIR } else { Join-Path $env:USERPROFILE "tools" }

foreach ($required in @(
    "ffmpeg-n7.1-latest-win64-gpl-shared-7.1",
    "libclang-pip\clang\native",
    "mingw64\x86_64-w64-mingw32\include"
)) {
    $path = Join-Path $toolsDir $required
    if (-not (Test-Path $path)) {
        throw "expected $path -- install the tools there, or set NLE_TOOLS_DIR to where they are"
    }
}

$template = Get-Content (Join-Path $repoRoot ".cargo\config.toml.example") -Raw
# Forward slashes: these values go into TOML strings and a clang argument
# list, where backslashes would need escaping.
$config = $template.Replace("{TOOLS_DIR}", ($toolsDir -replace '\\', '/'))
$output = Join-Path $repoRoot ".cargo\config.toml"
# WriteAllText rather than Set-Content: Windows PowerShell's UTF-8 option adds
# a byte-order mark, which cargo's TOML parser rejects.
[System.IO.File]::WriteAllText($output, $config)
Write-Host "Wrote $output for tools in $toolsDir"
