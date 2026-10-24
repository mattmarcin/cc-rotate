<#
.SYNOPSIS
  Build cc-rotate from source and install it.

.DESCRIPTION
  Builds cc-rotate, cc-rotate-ctl and the desktop app in release mode, copies them into -BinDir,
  then runs `cc-rotate-ctl setup` to create the rotation profile from your existing Claude Code
  profile. Safe to run again to upgrade: an existing rotation profile and its logins are kept.

.PARAMETER BinDir
  Where to install the executables. Defaults to %USERPROFILE%\.local\bin (where Claude Code's
  native installer puts `claude`).

.PARAMETER Template
  The Claude Code profile to copy settings, hooks, skills and plugins from. Defaults to ~\.claude.

.PARAMETER SkipApp
  Install only the command-line tools, without building the desktop app.
#>
param(
  [string]$BinDir = (Join-Path $env:USERPROFILE ".local\bin"),
  [string]$Template = "",
  [switch]$SkipApp
)

$ErrorActionPreference = "Stop"
$repo = Split-Path -Parent $PSScriptRoot

function Require($command, $hint) {
  if (-not (Get-Command $command -ErrorAction SilentlyContinue)) {
    throw "$command was not found. $hint"
  }
}

# A running cc-rotate.exe (one per open session) cannot be overwritten, but it can be renamed.
function Install-Exe($name) {
  $source = Join-Path $repo "target\release\$name"
  $target = Join-Path $BinDir $name
  try {
    Copy-Item $source $target -Force
  } catch {
    $old = "$target.old"
    Remove-Item $old -Force -ErrorAction SilentlyContinue
    Move-Item $target $old -Force
    Copy-Item $source $target -Force
  }
}

Require cargo "Install Rust from https://rustup.rs and open a new terminal."
Require claude "Install Claude Code first: https://code.claude.com/docs/en/setup"
if (-not $SkipApp) { Require npm "Install Node.js 20 or newer from https://nodejs.org (or pass -SkipApp)." }

Write-Host "Building cc-rotate and cc-rotate-ctl..."
cargo build --release --locked -p ccr-cli --manifest-path (Join-Path $repo "Cargo.toml")
if ($LASTEXITCODE -ne 0) { throw "cargo build failed" }

if (-not $SkipApp) {
  Write-Host "Building the desktop app..."
  Push-Location (Join-Path $repo "app")
  try {
    npm ci --no-audit --no-fund
    if ($LASTEXITCODE -ne 0) { throw "npm ci failed" }
    npx tauri build --no-bundle
    if ($LASTEXITCODE -ne 0) { throw "tauri build failed" }
  } finally {
    Pop-Location
  }
}

New-Item -ItemType Directory -Force -Path $BinDir | Out-Null
$running = Get-Process cc-rotate-app -ErrorAction SilentlyContinue
if ($running) {
  Write-Host "Closing the running cc-rotate app so it can be updated..."
  $running | Stop-Process -Force
  Start-Sleep -Milliseconds 800
}
Install-Exe "cc-rotate.exe"
Install-Exe "cc-rotate-ctl.exe"
if (-not $SkipApp) { Install-Exe "cc-rotate-app.exe" }
$ctl = Join-Path $BinDir "cc-rotate-ctl.exe"
Write-Host "Installed to $BinDir"

# --remove-alias drops the cc-rotate PowerShell function older versions added; it would hide
# the cc-rotate command.
$profileDir = (& $ctl config profile_dir 2>$null | Select-Object -First 1)
if ($profileDir -and (Test-Path (Join-Path $profileDir "settings.json"))) {
  Write-Host "The rotation profile at $profileDir is already set up; leaving it as it is."
  Write-Host "To rebuild it from your template profile, run: cc-rotate-ctl setup --force"
  & $ctl setup --remove-alias
  if ($LASTEXITCODE -ne 0) { throw "cc-rotate-ctl setup --remove-alias failed" }
} else {
  $setupArgs = @("setup", "--remove-alias")
  if ($Template) { $setupArgs += @("--from", $Template) }
  & $ctl @setupArgs
  if ($LASTEXITCODE -ne 0) { throw "cc-rotate-ctl setup failed" }
}

$pathDirs = $env:PATH -split ";" | ForEach-Object { $_.TrimEnd("\") }
if ($pathDirs -notcontains $BinDir.TrimEnd("\")) {
  Write-Warning "$BinDir is not on your PATH. Add it so the cc-rotate and cc-rotate-ctl commands work."
}

Write-Host ""
Write-Host "Next steps:"
Write-Host "  1. Open a new terminal."
Write-Host "  2. Add each account: cc-rotate-ctl login   (or use Add account in the app: cc-rotate-ctl dashboard)"
Write-Host "  3. Start working in a project folder: cc-rotate"
