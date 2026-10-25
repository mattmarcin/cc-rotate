#!/bin/sh
# Build cc-rotate from source and install it on Linux or macOS.
#
# Builds cc-rotate, cc-rotate-ctl and the desktop app in release mode, copies them into the bin
# directory, then runs `cc-rotate-ctl setup` to create the rotation profile from your existing
# Claude Code profile. Safe to run again to upgrade: an existing rotation profile and its logins
# are kept.
#
# Usage: scripts/install.sh [--bin-dir DIR] [--template DIR] [--skip-app]
#   --bin-dir DIR   Where to install the executables (default: ~/.local/bin, where Claude Code's
#                   native installer puts `claude`).
#   --template DIR  The Claude Code profile to copy settings, hooks, skills and plugins from
#                   (default: ~/.claude).
#   --skip-app      Install only the command-line tools, without building the desktop app.
set -eu

bin_dir="$HOME/.local/bin"
template=""
skip_app=0
while [ $# -gt 0 ]; do
  case "$1" in
    --bin-dir) bin_dir="$2"; shift 2 ;;
    --template) template="$2"; shift 2 ;;
    --skip-app) skip_app=1; shift ;;
    -h|--help) sed -n '2,14p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "unknown option: $1" >&2; exit 2 ;;
  esac
done

repo=$(cd "$(dirname "$0")/.." && pwd)

require() {
  if ! command -v "$1" >/dev/null 2>&1; then
    echo "$1 was not found. $2" >&2
    exit 1
  fi
}

# Copy next to the target, then rename over it: a running executable cannot be overwritten in
# place, but it can be replaced.
install_exe() {
  tmp="$bin_dir/.$1.new"
  cp "$repo/target/release/$1" "$tmp"
  chmod 755 "$tmp"
  mv -f "$tmp" "$bin_dir/$1"
}

require cargo "Install Rust from https://rustup.rs and open a new terminal."
require claude "Install Claude Code first: https://code.claude.com/docs/en/setup"
if [ "$skip_app" -eq 0 ]; then
  require npm "Install Node.js 20 or newer from https://nodejs.org (or pass --skip-app)."
fi

echo "Building cc-rotate and cc-rotate-ctl..."
cargo build --release --locked -p ccr-cli --manifest-path "$repo/Cargo.toml"

if [ "$skip_app" -eq 0 ]; then
  echo "Building the desktop app..."
  (cd "$repo/app" && npm ci --no-audit --no-fund && npx tauri build --no-bundle)
fi

mkdir -p "$bin_dir"
install_exe cc-rotate
install_exe cc-rotate-ctl
if [ "$skip_app" -eq 0 ]; then
  install_exe cc-rotate-app
fi
ctl="$bin_dir/cc-rotate-ctl"
echo "Installed to $bin_dir"

# --keep-existing leaves an existing rotation profile as it is on an upgrade.
if [ -n "$template" ]; then
  "$ctl" setup --keep-existing --from "$template"
else
  "$ctl" setup --keep-existing
fi

case ":$PATH:" in
  *":$bin_dir:"*) ;;
  *) echo "warning: $bin_dir is not on your PATH. Add it so the cc-rotate and cc-rotate-ctl commands work." >&2 ;;
esac

# A running app keeps the build it started with. It is not restarted from here: this may be an SSH
# session, which cannot open windows on the desktop.
if [ "$skip_app" -eq 0 ] && pgrep -x cc-rotate-app >/dev/null 2>&1; then
  echo ""
  echo "The cc-rotate app is still running the previous version."
  echo "Quit it from its tray menu (Quit cc-rotate) and start it again to use the new one."
fi

echo ""
echo "Next steps:"
echo "  1. Add each account: cc-rotate-ctl login   (or use Add account in the app: cc-rotate-ctl dashboard)"
echo "  2. Start working in a project folder: cc-rotate"
