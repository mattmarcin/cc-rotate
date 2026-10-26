# Security policy

cc-rotate holds the Claude Code logins (OAuth access and refresh tokens) of every account in your pool, so security reports are very welcome.

## Reporting a vulnerability

Please report vulnerabilities privately through GitHub: open the repository's **Security** tab and choose **Report a vulnerability** ([direct link](https://github.com/mattmarcin/cc-rotate/security/advisories/new)).
Do not open a public issue, pull request or discussion for a security problem.

Include what you found, the steps to reproduce it, and the cc-rotate commit and Claude Code version you used.
Never include real tokens, `.credentials.json` files or store databases in a report.

You can expect a first reply within a week.
Fixes land on `main`, and the advisory is published once a fix is available.
There are no versioned releases yet, so only the latest `main` is supported.

## How cc-rotate stores logins

- Each account's login is stored in `store.db` in cc-rotate's data folder (`%LOCALAPPDATA%\cc-rotate` on Windows, `~/.local/share/cc-rotate` on Linux, `~/Library/Application Support/cc-rotate` on macOS), encrypted with XChaCha20-Poly1305.
  Every login gets a fresh random nonce, and is bound to its account, so a login copied into another account's row fails to decrypt instead of being used.
- The 256-bit key is in `master.key` in the same folder.
  cc-rotate creates it on first use and never logs it or prints it.
  On Linux and macOS the file and the data folder are readable only by you (modes 600 and 700).
  On Windows the file inherits your user profile's permissions, which allow your account, SYSTEM and administrators.
- The key is read without any prompt, because the status line runs every few seconds in every session, including over SSH.
  It keeps the store safe from casual reads, and a copy of `store.db` on its own (in a support request, say) reveals no login.
  A copy of the whole data folder holds the key too, and reveals every stored login: a Time Machine or other backup of it, or a zipped `%LOCALAPPDATA%` or `~/Library/Application Support`.
  Keep the folder out of backups that others can read, and never send it in full.
  The key does not protect against a program running as your user, which can read `master.key` just as it can read Claude Code's own plaintext `.credentials.json`.
  Without the key the stored logins cannot be recovered, and each account has to sign in again.
- The daily backups of the profile's login file in the `backups` folder are encrypted with the same key.
  `cc-rotate-ctl backup <file>` prints one decrypted.
- Stores from older versions, encrypted with Windows DPAPI, are converted to this format the first time a new version opens them.
- The rotation profile's `.credentials.json` holds the active account's login in plain text, exactly as Claude Code itself stores it (mode 600 on Linux and macOS).
  On macOS, where Claude Code keeps the login in the Keychain, cc-rotate writes it to the same Keychain item through Apple's `security` tool, like Claude Code does.
  Like Claude Code, it passes the login to `security` on standard input, and on the command line only when the document is too long for that (over about 2 KB, for example with many MCP logins).
  A command line is not visible to other users on macOS, but it is to your own programs, to root, and to security software that records the commands programs run.
- A new login is made in a temporary staging folder under `staging` in the data folder, which is deleted when the login finishes, fails, is cancelled or times out.
  One left behind when cc-rotate was quit, crashed or interrupted with Ctrl+C is deleted the next time a sign-in starts or the app starts, with its Keychain item on macOS.
- While a usage check or warm-up runs, the account it uses has its login in plain text in a scratch folder under `staging`, the way Claude Code needs it; it is deleted when the check ends, and one interrupted by a crash is settled and deleted later.
- On Linux and macOS, the data folder and the rotation profile are made readable only by you (mode 700) each time cc-rotate opens them, the store, logs and settings it writes only by you (600), even when an older version or `CC_ROTATE_DATA_DIR` made them with wider permissions.
- cc-rotate makes no network requests of its own, and the desktop app opens no network port.
- The desktop app's window can change only the settings shown in it: the ones that name a program to run or where the login is written (`claude_path`, `statusline_downstream`, `profile_dir`) are set with `cc-rotate-ctl config` alone, and the window can reopen only the sign-in page Claude Code announced, not any address.
- `claude` is found in the `claude_path` setting, which must be a full path, or in the absolute folders on `PATH`, then in the folders Claude Code's installers use (`~/.local/bin`, Homebrew's, npm's): a relative `PATH` entry such as `.` is skipped, so a project folder can't put its own `claude` in reach of a login.
- Your other Claude Code profiles are only read, to find out which account each belongs to; their logins are never imported or changed.

## Scope

In scope:

- Tokens written to disk unencrypted anywhere other than the rotation profile's `.credentials.json`, or written to logs, events or error messages.
- A swap or login that leaves a token readable by another user, or that leaks a token through a process argument, environment variable or temporary file.
- Ways for another local program or a web page to drive the desktop app or the CLI into revealing, replacing or exporting a login.
- Staging folders or backups that are not cleaned up or not encrypted as described above.
- Supply-chain issues in this repository, such as the install script, the build, or the CI workflow.

Out of scope:

- Malware or another process already running as your user.
  Neither cc-rotate's key file nor Claude Code's own credential file can protect against code with your own permissions.
- Vulnerabilities in Claude Code, the Anthropic API or claude.ai.
  Report those to Anthropic.
- Whether a particular way of using your accounts is allowed by Anthropic's terms.
