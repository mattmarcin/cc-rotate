# Contributing

Thanks for helping out.
Bug reports, fixes and new platform support are all welcome.
For a security problem, follow [SECURITY.md](SECURITY.md) instead of opening an issue.

## Building

You need [Rust](https://rustup.rs) 1.85 or newer and [Node.js](https://nodejs.org) 20 or newer, on Windows, Linux or macOS.
On Linux, the desktop app also needs the WebKitGTK and tray libraries (see **Requirements** in the [README](README.md)).
The desktop app crate embeds the built frontend from `app/dist`, so build it once before running any `cargo` command:

```sh
cd app && npm ci && npm run build && cd ..
cd tests/interop && npm ci && cd ../..   # enables the lock tests against the real proper-lockfile package
```

To try the app without real accounts, use the demo store described under **Development** in the [README](README.md).

## Checks

CI runs these on Windows, Linux and macOS for every pull request, and all of them have to pass:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
cd app && npm run build   # also type-checks the frontend
```

`tests/e2e/live-swap.mjs` swaps between two real accounts in your rotation profile.
It is not part of CI; run it by hand when you change the swap, lock or login code.

## Pull requests

- Keep each pull request to one change, and add or update tests for it.
- Use [Conventional Commits](https://www.conventionalcommits.org) for commit messages, for example `fix: keep the lock fresh during long swaps`.
- Update the README when you change a command, flag or setting, and the design spec in `docs/specs` when you change how swaps, locks or logins work.
- User-facing text (README, app copy, CLI output) uses plain hyphens, never em dashes.
- Never commit real tokens, `.credentials.json` files or store databases, not even in test fixtures.

By contributing, you agree that your contributions are licensed under the [MIT License](LICENSE).
