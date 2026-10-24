# cc-rotate

Run one Claude Code profile on a pool of your own Claude accounts.
When the account in use gets close to its 5-hour or weekly limit, cc-rotate switches the profile to another account in place.
Running sessions keep going on their next request, with no restart and no proxy.

Because every session lives in one profile, sessions can see and message each other, and their history lives in one place.

![The cc-rotate dashboard](docs/screenshot.png)

> **Unofficial.** cc-rotate is a community project and is not affiliated with or endorsed by Anthropic.
> It is meant for people who personally hold more than one Claude subscription.
> You are responsible for using your accounts in line with [Anthropic's terms](https://www.anthropic.com/legal/consumer-terms) and the [Claude Code usage policy](https://code.claude.com/docs/en/legal-and-compliance).

## How it works

- Each account signs in once through Claude Code's own login page (`claude auth login`), in a scratch folder.
  cc-rotate saves that login in a local store, encrypted with Windows DPAPI so only your Windows user can read it.
- A dedicated profile (`%USERPROFILE%\.claude-rotate`) holds the account in use.
  cc-rotate copies your existing settings, hooks, skills and plugins into it.
- Every Claude Code session in that profile runs `cc-rotate-ctl statusline` after every reply and every 30 seconds.
  It records the account's usage, copies renewed logins back into the store, and starts a swap at 98% of either limit.
- A swap takes the same lock Claude Code uses when it renews a login, saves the outgoing account's latest login, and writes the next account's login into the profile.
  Claude Code notices the change and uses the new account on its next request.
- If a request already hit the limit, Claude Code waits and retries on its own, and the retry goes out on the new account.

cc-rotate never talks to Anthropic itself.
All traffic is the unmodified Claude Code talking to Anthropic, exactly as it does today.

The design, with the Claude Code behavior it relies on, is in [docs/specs/2026-10-24-cc-rotate.md](docs/specs/2026-10-24-cc-rotate.md).

## Requirements

- Windows 10 or 11.
- Claude Code, native install (`claude --version`). Tested with 2.1.310.
- To build from source: [Rust](https://rustup.rs) 1.85 or newer, and [Node.js](https://nodejs.org) 20 or newer for the desktop app.

## Install

```powershell
git clone https://github.com/OWNER/cc-rotate
cd cc-rotate
.\scripts\install.ps1
```

The script builds `cc-rotate-ctl.exe` and `cc-rotate-app.exe`, copies them to `%USERPROFILE%\.local\bin`, and creates the rotation profile from `%USERPROFILE%\.claude`.
It also adds a `cc-rotate` function to your PowerShell profile.

| Option | What it does |
|---|---|
| `-BinDir <dir>` | Install somewhere else. Make sure that folder is on your `PATH`. |
| `-Template <dir>` | Copy settings from a different Claude Code profile. |
| `-NoAlias` | Do not touch your PowerShell profile. |
| `-SkipApp` | Install only the command-line tool (no Node.js needed). |

Running the script again upgrades the executables and leaves your rotation profile and logins alone.

## Add your accounts

Add each account once.
Either open the app (`cc-rotate-ctl dashboard`) and choose **Add account**, or use the terminal:

```powershell
cc-rotate-ctl login --email you@example.com --label work
```

A browser window opens on Claude's sign-in page.
Your browser signs in as whichever claude.ai account it is currently logged in to, so switch accounts there first if needed.
A separate browser profile per account makes this painless.

The first account you add becomes the active one.

These are fresh logins, separate from any other Claude Code profiles you already use, so adding an account here never logs you out anywhere else.

## Daily use

Open a new terminal and start Claude Code on the pool:

```powershell
cc-rotate
```

`cc-rotate` passes its arguments through to `claude`, so `cc-rotate --resume` and the rest work as usual.
The status line starts with the active account's label, and your own status line still shows after it.

The desktop app shows every account, how much of each limit is used, when each resets, usage over the last day or week with swaps marked, and an activity log.
It lives in the tray, starts when you sign in to Windows (you can turn that off in **Health**), and sends a notification when an account needs a new login or no account is left to swap to.

### Commands

| Command | What it does |
|---|---|
| `cc-rotate-ctl status` | Every account with its usage and reset times. |
| `cc-rotate-ctl swap` | Swap now to the account with the most room. `--to <label>` picks one. |
| `cc-rotate-ctl login` | Add an account. `--relogin <label>` signs an existing account in again. |
| `cc-rotate-ctl pause <label>` / `resume <label>` | Keep an account out of rotation, or bring it back. |
| `cc-rotate-ctl rename <label> <new>` / `remove <label> --yes` | Rename or delete an account. |
| `cc-rotate-ctl events` | Recent swaps, logins and problems. |
| `cc-rotate-ctl doctor` | Check the profile, the store and Claude Code. |
| `cc-rotate-ctl config [key] [value]` | Show or change settings. |
| `cc-rotate-ctl dashboard` | Open the desktop app. |

### Settings

| Setting | Default | Meaning |
|---|---|---|
| `swap_threshold_5h` | 98 | Swap when the 5-hour limit reaches this percentage. |
| `swap_threshold_7d` | 98 | Swap when the weekly limit reaches this percentage. |
| `swap_cooldown_s` | 120 | Minimum seconds between swaps caused by usage readings. |
| `lock_timeout_ms` | 30000 | How long a swap waits for Claude Code to finish renewing a login. |
| `sample_retention_days` | 90 | How long usage history is kept. |
| `statusline_downstream` | your old status line | The status line command shown after cc-rotate's label. |
| `claude_path` | found on `PATH` | The `claude` executable to use for logins. |

### Tracking your other profiles too

If you keep using separate profiles for some accounts, cc-rotate can record their usage so the dashboard and the swap logic see it.
In that profile's `settings.json`, set the status line to:

```json
"statusLine": { "type": "command", "command": "cc-rotate-ctl statusline --observe", "refreshInterval": 30 }
```

To keep your own status line, add it with `--then`, for example `cc-rotate-ctl statusline --observe --then \"node C:/path/to/statusline.js\"`.

Observe mode only records usage for accounts that are also in the pool.
It never touches that profile's login and never swaps anything.

## What it does not cover

- **The Claude desktop app.** Its Code tab passes its own login to Claude Code directly, so swapping the profile's login cannot reach it.
- **macOS and Linux.** Not supported yet. The core is written so they can be added; contributions are welcome.
- **Claude Code updates.** cc-rotate relies on how Claude Code reloads its login file and locks login renewal, which is not a documented interface.
  `cc-rotate-ctl doctor` and the app warn when your Claude Code version has not been tested.

## Troubleshooting

- Run `cc-rotate-ctl doctor` first.
- Errors from the status line and hooks go to `%LOCALAPPDATA%\cc-rotate\logs\cc-rotate.log`.
- An account marked **needs login** had its login rejected by Anthropic. Sign it in again with `cc-rotate-ctl login --relogin <label>`.
- A daily encrypted backup of the profile's login file is kept in `%LOCALAPPDATA%\cc-rotate\backups`.

## Uninstall

1. Quit the app from its tray icon, and turn off **Start when I sign in** in **Health** first if you enabled it.
2. Delete `cc-rotate-ctl.exe` and `cc-rotate-app.exe` from `%USERPROFILE%\.local\bin`.
3. Remove the block between `# >>> cc-rotate >>>` and `# <<< cc-rotate <<<` from your PowerShell profile.
4. Delete `%USERPROFILE%\.claude-rotate` (the profile) and `%LOCALAPPDATA%\cc-rotate` (the store, with every saved login).

## Development

```powershell
cargo test --workspace                 # unit and integration tests
cd tests\interop; npm ci; cd ..\..      # once: enables the lock tests against the real proper-lockfile package
cd app; npm ci; npx tauri dev           # run the app with live reload
```

To try the app without real accounts, generate a demo store and point cc-rotate at it:

```powershell
$env:CC_ROTATE_DATA_DIR = "$env:TEMP\cc-rotate-demo"
cargo run -p ccr-core --example demo_store
cd app; npx tauri dev
```

The workspace has three parts: `crates/ccr-core` (store, locks, swap protocol, login import), `crates/ccr-cli` (`cc-rotate-ctl`), and `app` (the Tauri desktop app).

## License

[MIT](LICENSE)
