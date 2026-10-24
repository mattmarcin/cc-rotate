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

- Each account's login is stored in `%LOCALAPPDATA%\cc-rotate\store.db`, encrypted with Windows DPAPI in the current-user scope.
  Only processes running as your Windows user can decrypt it.
- The daily backups of the profile's login file in `%LOCALAPPDATA%\cc-rotate\backups` are encrypted the same way.
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

- Malware or another process already running as your Windows user.
  DPAPI cannot protect against code with your own permissions, and neither can Claude Code's own credential file.
- Vulnerabilities in Claude Code, the Anthropic API or claude.ai.
  Report those to Anthropic.
- Whether a particular way of using your accounts is allowed by Anthropic's terms.
