<#
.SYNOPSIS
  Build cc-rotate from source and install it.

.DESCRIPTION
  Builds cc-rotate-ctl and the desktop app in release mode, copies both into -BinDir, then runs
  `cc-rotate-ctl setup` to create the rotation profile from your existing Claude Code profile.
  Safe to run again to upgrade: an existing rotation profile and its logins are kept.

.PARAMETER BinDir
  Where to install the executables. Defaults to %USERPROFILE%\.local\bin (where Claude Code's
  native installer puts `claude`).

.PARAMETER Template
  The Claude Code profile to copy settings, hooks, skills and plugins from. Defaults to ~\.claude.

.PARAMETER NoAlias
  Do not add the `cc-rotate` function to your PowerShell profile.

.PARAMETER SkipApp
  Install only the command-line tool, without building the desktop app.
#>
param(
  [string]$BinDir = (Join-Path $env:USERPROFILE ".local\bin"),
  [string]$Template = "",
  [switch]$NoAlias,
  [switch]$SkipApp
)

$ErrorActionPreference = "Stop"
$repo = Split-Path -Parent $PSScriptRoot

function Require($command, $hint) {
  if (-not (Get-Command $command -ErrorAction SilentlyContinue)) {
    throw "$command was not found. $hint"
  }
}

Require cargo "Install Rust from https://rustup.rs and open a new terminal."
Require claude "Install Claude Code first: https://code.claude.com/docs/en/setup"
if (-not $SkipApp) { Require npm "Install Node.js 20 or newer from https://nodejs.org (or pass -SkipApp)." }

Write-Host "Building cc-rotate-ctl..."
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
Copy-Item (Join-Path $repo "target\release\cc-rotate-ctl.exe") $BinDir -Force
if (-not $SkipApp) { Copy-Item (Join-Path $repo "target\release\cc-rotate-app.exe") $BinDir -Force }
$ctl = Join-Path $BinDir "cc-rotate-ctl.exe"
Write-Host "Installed to $BinDir"

$profileDir = (& $ctl config profile_dir 2>$null | Select-Object -First 1)
if ($profileDir -and $profileDir -ne "null" -and (Test-Path (Join-Path $profileDir "settings.json"))) {
  Write-Host "The rotation profile at $profileDir is already set up; leaving it as it is."
  Write-Host "To rebuild it from your template profile, run: cc-rotate-ctl setup --force"
} else {
  $setupArgs = @("setup")
  if ($Template) { $setupArgs += @("--from", $Template) }
  if (-not $NoAlias) { $setupArgs += "--install-alias" }
  & $ctl @setupArgs
  if ($LASTEXITCODE -ne 0) { throw "cc-rotate-ctl setup failed" }
}

$pathDirs = $env:PATH -split ";" | ForEach-Object { $_.TrimEnd("\") }
if ($pathDirs -notcontains $BinDir.TrimEnd("\")) {
  Write-Warning "$BinDir is not on your PATH. Add it so `cc-rotate-ctl` and the cc-rotate alias work."
}

Write-Host ""
Write-Host "Next steps:"
Write-Host "  1. Open a new terminal so the cc-rotate alias loads."
Write-Host "  2. Add each account: cc-rotate-ctl login   (or use Add account in the app: cc-rotate-ctl dashboard)"
Write-Host "  3. Start working: cc-rotate"
