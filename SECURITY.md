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

- Each account's login is stored in `store.db` in cc-rotate's data folder (`%LOCALAPPDATA%\cc-rotate` on Windows), encrypted with XChaCha20-Poly1305.
  Every login gets a fresh random nonce, and is bound to its account, so a login copied into another account's row fails to decrypt instead of being used.
- The 256-bit key is in `master.key` in the same folder.
  cc-rotate creates it on first use and never logs it or prints it.
  On Linux and macOS the file and the data folder are readable only by you (modes 600 and 700).
  On Windows the file inherits your user profile's permissions, which allow your account, SYSTEM and administrators.
- The key is read without any prompt, because the status line runs every few seconds in every session, including over SSH.
  It keeps the store safe from casual reads, and a copy of `store.db` on its own (in a backup or a support request) reveals no login.
  It does not protect against a program running as your user, which can read `master.key` just as it can read Claude Code's own plaintext `.credentials.json`.
  Keep `master.key` out of shared backups; without it the stored logins cannot be recovered, and each account has to sign in again.
- The daily backups of the profile's login file in the `backups` folder are encrypted with the same key.
  `cc-rotate-ctl backup <file>` prints one decrypted.
- Stores from older versions, encrypted with Windows DPAPI, are converted to this format the first time a new version opens them.
- The rotation profile's `.credentials.json` holds the active account's login in plain text, exactly as Claude Code itself stores it.
- A new login is made in a temporary staging folder under `%LOCALAPPDATA%\cc-rotate\staging`, which is deleted when the login finishes, fails, is cancelled or times out.
- cc-rotate makes no network requests of its own, and the desktop app opens no network port.
- Your other Claude Code profiles are only read, to find out which account each belongs to; their logins are never imported or changed.

## Scope

In scope:

- Tokens written to disk unencrypted anywhere other than the rotation profile's `.credentials.json`, or written to logs, events or error messages.
- A swap or login that leaves a token readable by another Windows user, or that leaks a token through a process argument, environment variable or temporary file.
- Ways for another local program or a web page to drive the desktop app or the CLI into revealing, replacing or exporting a login.
- Staging folders or backups that are not cleaned up or not encrypted as described above.
- Supply-chain issues in this repository, such as the install script, the build, or the CI workflow.

Out of scope:

- Malware or another process already running as your user.
  Neither cc-rotate's key file nor Claude Code's own credential file can protect against code with your own permissions.
- Vulnerabilities in Claude Code, the Anthropic API or claude.ai.
  Report those to Anthropic.
- Whether a particular way of using your accounts is allowed by Anthropic's terms.
