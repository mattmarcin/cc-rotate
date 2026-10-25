mod format;
mod herdr;
mod process;
mod ranking;

use std::io::{IsTerminal, Read, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use ccr_core::scheduler::Reason;
use ccr_core::statusline::StatusInput;
use ccr_core::store::{AccountState, kind};
use ccr_core::swap::{SwapOutcome, SwapRequest};
use ccr_core::{Paths, Store, doctor, login, profiles, setup, snapshot, statusline, swap, time};
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "cc-rotate-ctl",
    version,
    about = "Run one Claude Code profile on a pool of accounts"
)]
struct Cli {
    /// Rotation profile directory (defaults to the one recorded by `setup`).
    #[arg(long, global = true, value_name = "DIR")]
    profile: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Show every account, its usage, and the pool state.
    Status {
        #[arg(long)]
        json: bool,
    },
    /// Add an account, or re-login one, through Claude Code's own sign-in flow.
    Login {
        /// Pre-fill the sign-in page with this email.
        #[arg(long)]
        email: Option<String>,
        /// Label for a new account (defaults to the email's local part).
        #[arg(long)]
        label: Option<String>,
        /// Re-login this existing account (label, email or id).
        #[arg(long, value_name = "ACCOUNT")]
        relogin: Option<String>,
        /// Sign in fresh as the account another profile uses (see `profiles`); its email and
        /// name are pre-filled, and a different account signing in is refused.
        #[arg(long, value_name = "NAME", conflicts_with_all = ["email", "relogin"])]
        from_profile: Option<String>,
    },
    /// List your other Claude Code profiles and the account each belongs to.
    Profiles,
    /// Show the account the next swap would pick, and why.
    Next {
        /// List every account in the order they would be used, with a reason for each.
        #[arg(long)]
        explain: bool,
        #[arg(long)]
        json: bool,
    },
    /// Swap the profile to another account now.
    Swap {
        /// Swap to this account instead of the scheduler's pick.
        #[arg(long, value_name = "ACCOUNT")]
        to: Option<String>,
        /// Only swap if this account id is still active (used by the statusline).
        #[arg(long, hide = true)]
        from_id: Option<i64>,
        #[arg(long, hide = true, default_value = "manual")]
        reason: String,
        /// Skip instead of waiting when another swap is running.
        #[arg(long, hide = true)]
        no_wait: bool,
        #[arg(long, hide = true)]
        quiet: bool,
    },
    /// Keep an account out of automatic selection.
    Pause { account: String },
    /// Return a paused account to the pool.
    Resume { account: String },
    /// Remove an account and its stored login.
    Remove {
        account: String,
        #[arg(long)]
        yes: bool,
    },
    /// Change an account's label.
    Rename { account: String, label: String },
    /// Show the order accounts are used in, or put the listed accounts first, in that order.
    Order { accounts: Vec<String> },
    /// Move an account to a place in the order (1 is used first).
    Move {
        account: String,
        #[arg(long, value_name = "N")]
        to: usize,
    },
    /// Show or set whether an account has a weekly limit: auto (detect it), yes, or no.
    WeeklyLimit {
        account: String,
        #[arg(value_parser = ["auto", "yes", "no"])]
        value: Option<String>,
    },
    /// Show recent swaps, logins and errors.
    Events {
        #[arg(long, default_value_t = 30)]
        limit: usize,
        #[arg(long)]
        json: bool,
    },
    /// Show or change settings.
    Config {
        key: Option<String>,
        value: Option<String>,
        #[arg(long)]
        unset: bool,
    },
    /// Create the rotation profile from an existing Claude Code profile.
    Setup {
        /// Profile to copy settings, hooks, skills and plugins from (default: ~/.claude).
        #[arg(long, value_name = "DIR")]
        from: Option<PathBuf>,
        /// Rebuild an existing rotation profile (its login is kept).
        #[arg(long)]
        force: bool,
        /// Remove the `cc-rotate` PowerShell function older versions added to your PowerShell
        /// profile; it would hide the `cc-rotate` command. With an existing profile and no
        /// --force, this is all setup does.
        #[arg(long)]
        remove_alias: bool,
    },
    /// Check the store, the profile and Claude Code.
    Doctor {
        #[arg(long)]
        json: bool,
    },
    /// Print a daily backup of the profile's login file, decrypted (it contains tokens).
    Backup {
        /// A `credentials-<day>.ccr` file from the backups folder.
        file: PathBuf,
    },
    /// Open the cc-rotate desktop app.
    Dashboard,
    /// Statusline entry point (configured by `setup`; reads Claude Code's JSON on stdin).
    Statusline {
        /// Only record usage for a profile outside the pool; never swaps.
        #[arg(long)]
        observe: bool,
        /// Status line command to run after cc-rotate's (overrides the `statusline_downstream` setting).
        #[arg(long, value_name = "COMMAND")]
        then: Option<String>,
    },
    /// Hook entry points (configured by `setup`; read Claude Code's JSON on stdin).
    Hook {
        #[command(subcommand)]
        hook: HookCommand,
    },
    /// Tell herdr how to resume a session (started in the background by `hook session-start`).
    #[command(hide = true)]
    HerdrReport {
        /// Orders cc-rotate's reports for the pane; herdr ignores an older one.
        #[arg(long)]
        seq: u64,
        /// Give up after this many milliseconds of herdr not accepting the report yet.
        #[arg(long, default_value_t = herdr::DEFAULT_TIMEOUT.as_millis() as u64)]
        timeout_ms: u64,
        pane: String,
        session: String,
    },
}

#[derive(Subcommand)]
enum HookCommand {
    StopFailure,
    /// Report `cc-rotate --resume <id>` to herdr when the session runs in a herdr pane.
    SessionStart,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> Result<ExitCode> {
    match cli.command {
        Command::Statusline { observe, then } => {
            statusline_entry(cli.profile, observe, then);
            Ok(ExitCode::SUCCESS)
        }
        Command::Hook {
            hook: HookCommand::StopFailure,
        } => {
            if let Err(e) = stop_failure(cli.profile) {
                process::log_error("hook stop-failure", &e);
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::Hook {
            hook: HookCommand::SessionStart,
        } => {
            // Never fails Claude Code's startup: errors only go to the log.
            if let Err(e) = herdr::session_start(read_stdin) {
                process::log_error("hook session-start", &e);
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::HerdrReport {
            seq,
            timeout_ms,
            pane,
            session,
        } => match herdr::report(&pane, &session, seq, Duration::from_millis(timeout_ms)) {
            Ok(()) => Ok(ExitCode::SUCCESS),
            Err(e) => {
                process::log_error("herdr report", &e);
                Ok(ExitCode::FAILURE)
            }
        },
        command => {
            let (store, paths) = ccr_core::open(cli.profile)?;
            dispatch(command, &store, &paths)
        }
    }
}

fn dispatch(command: Command, store: &Store, paths: &Paths) -> Result<ExitCode> {
    match command {
        Command::Status { json } => {
            if let Some(req) = statusline::sync(store, paths)? {
                swap::run(store, paths, &req)?;
            }
            let snap = snapshot::snapshot(store, paths)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&snap)?);
            } else {
                print!("{}", format::status(&snap));
            }
        }
        Command::Login {
            email,
            label,
            relogin,
            from_profile,
        } => match from_profile {
            Some(name) => {
                let found = profiles::discover(store, paths)?
                    .into_iter()
                    .find(|p| p.name.eq_ignore_ascii_case(&name))
                    .with_context(|| {
                        format!("no profile named {name:?}; see `cc-rotate-ctl profiles`")
                    })?;
                if let Some(label) = &found.in_pool_as {
                    bail!(
                        "{} is already in the pool as {label}",
                        found.email.as_deref().unwrap_or(&name)
                    );
                }
                let label = label.or_else(|| {
                    Some(found.name.clone())
                        .filter(|l| store.account_by_label(l).ok().flatten().is_none())
                });
                login_cmd(store, paths, found.email.clone(), label, None, found.email)?
            }
            None => login_cmd(store, paths, email, label, relogin, None)?,
        },
        Command::Profiles => {
            let found = profiles::discover(store, paths)?;
            if found.is_empty() {
                println!("No other Claude Code profiles found in your home folder.");
            }
            for p in &found {
                let status = match &p.in_pool_as {
                    Some(label) => format!("in the pool as {label}"),
                    None => format!("add with: cc-rotate-ctl login --from-profile {}", p.name),
                };
                println!(
                    "{:<10} {:<32} {}",
                    p.name,
                    p.email.as_deref().unwrap_or("unknown email"),
                    status
                );
            }
        }
        Command::Swap {
            to,
            from_id,
            reason,
            no_wait,
            quiet,
        } => {
            let reason =
                Reason::parse(&reason).with_context(|| format!("unknown reason {reason:?}"))?;
            let to = to
                .map(|r| store.find_account(&r))
                .transpose()?
                .map(|a| a.id);
            let outcome = swap::run(
                store,
                paths,
                &SwapRequest {
                    from: from_id,
                    to,
                    reason,
                    wait: !no_wait,
                },
            )?;
            if !quiet {
                match outcome {
                    SwapOutcome::Swapped { from, to } => {
                        let label = |id: Option<i64>| {
                            id.and_then(|i| store.account(i).ok().flatten())
                                .map(|a| a.label)
                                .unwrap_or_else(|| "none".into())
                        };
                        println!("Swapped {} -> {}", label(from), label(Some(to)));
                    }
                    SwapOutcome::Skipped { why } => println!("No swap: {why}"),
                    SwapOutcome::PoolExhausted => {
                        println!(
                            "No swap: every other account is paused, needs a login, or is over a threshold"
                        );
                    }
                }
            }
        }
        Command::Pause { account } => set_state(store, &account, AccountState::Paused)?,
        Command::Resume { account } => set_state(store, &account, AccountState::Parked)?,
        Command::Remove { account, yes } => {
            let a = store.find_account(&account)?;
            if a.state == AccountState::Active {
                bail!("{} is active; swap to another account first", a.label);
            }
            if !yes {
                bail!(
                    "this deletes {} and its stored login; re-run with --yes to confirm",
                    a.label
                );
            }
            store.remove_account(a.id)?;
            store.log_event(
                kind::STATE,
                Some(a.id),
                None,
                Some("removed"),
                Some(serde_json::json!({"label": a.label})),
            )?;
            println!("Removed {}", a.label);
        }
        Command::Rename { account, label } => {
            let a = store.find_account(&account)?;
            store.rename_account(a.id, &label)?;
            println!("Renamed {} to {label}", a.label);
        }
        Command::Next { explain, json } => ranking::next(store, paths, explain, json)?,
        Command::Order { accounts } => ranking::order(store, &accounts)?,
        Command::Move { account, to } => ranking::move_to(store, &account, to)?,
        Command::WeeklyLimit { account, value } => {
            ranking::weekly_limit(store, &account, value.as_deref())?
        }
        Command::Events { limit, json } => {
            let events = store.events(limit)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&events)?);
            } else {
                print!("{}", format::events(store, &events));
            }
        }
        Command::Config { key, value, unset } => config_cmd(store, key, value, unset)?,
        Command::Setup {
            from,
            force,
            remove_alias,
        } => {
            if remove_alias && !force && paths.settings_file().exists() {
                for file in setup::remove_powershell_alias()? {
                    println!("Removed the old cc-rotate function from {}", file.display());
                }
                return Ok(ExitCode::SUCCESS);
            }
            let template = match from {
                Some(dir) => dir,
                None => ccr_core::paths::default_template_dir()?,
            };
            let ctl_path = dunce_exe()?;
            let report = setup::run(
                store,
                &setup::SetupOptions {
                    template,
                    profile: paths.profile_dir.clone(),
                    ctl_path,
                    force,
                    remove_alias,
                },
            )?;
            println!("Rotation profile ready at {}", report.profile.display());
            println!("  copied: {}", report.copied.join(", "));
            if let Some(how) = &report.claude_md {
                println!("  CLAUDE.md: {how}");
            }
            match &report.statusline_downstream {
                Some(cmd) => println!("  your statusline still runs after cc-rotate's: {cmd}"),
                None => println!("  no existing statusline to chain"),
            }
            if !report.mcp_servers.is_empty() {
                println!("  MCP servers: {}", report.mcp_servers.join(", "));
                println!(
                    "  (servers that sign in with OAuth need one /mcp login in the cc-rotate profile)"
                );
            }
            for file in &report.alias_removed_from {
                println!(
                    "  removed the old cc-rotate function from {}",
                    file.display()
                );
            }
            if store.accounts()?.is_empty() {
                println!(
                    "\nNext: add your accounts with `cc-rotate-ctl login` (once per account)."
                );
            } else {
                println!("\nStart a new `cc-rotate` session to pick up the changes.");
            }
        }
        Command::Doctor { json } => {
            let checks = doctor::run(store, paths);
            if json {
                println!("{}", serde_json::to_string_pretty(&checks)?);
            } else {
                print!("{}", format::doctor(&checks));
            }
            if checks.iter().any(|c| c.status == doctor::Status::Fail) {
                return Ok(ExitCode::FAILURE);
            }
        }
        Command::Backup { file } => {
            let plain = swap::open_backup(store, &file)?;
            std::io::stdout().write_all(&plain)?;
            println!();
        }
        Command::Dashboard => process::launch_app()?,
        Command::Statusline { .. } | Command::Hook { .. } | Command::HerdrReport { .. } => {
            unreachable!()
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn dunce_exe() -> Result<PathBuf> {
    let exe = std::env::current_exe()?;
    Ok(std::fs::canonicalize(&exe)
        .map(|p| strip_verbatim(&p))
        .unwrap_or(exe))
}

fn strip_verbatim(p: &std::path::Path) -> PathBuf {
    let s = p.display().to_string();
    PathBuf::from(s.strip_prefix(r"\\?\").unwrap_or(&s))
}

fn set_state(store: &Store, reference: &str, state: AccountState) -> Result<()> {
    let a = store.find_account(reference)?;
    match (a.state, state) {
        (AccountState::Active, AccountState::Paused) => {
            bail!(
                "{} is active; swap to another account first, then pause it",
                a.label
            )
        }
        (AccountState::NeedsLogin, AccountState::Parked) => {
            bail!(
                "{} needs a new login: cc-rotate-ctl login --relogin {}",
                a.label,
                a.label
            )
        }
        (AccountState::Active, _) => bail!("{} is active", a.label),
        _ => {}
    }
    store.set_state(a.id, state)?;
    store.log_event(kind::STATE, Some(a.id), None, Some(state.as_str()), None)?;
    println!("{} is now {}", a.label, state.as_str().replace('_', " "));
    Ok(())
}

/// A setting's value as typed: strings without JSON quoting or escaping, null as empty.
fn plain(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::Null => String::new(),
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn config_cmd(
    store: &Store,
    key: Option<String>,
    value: Option<String>,
    unset: bool,
) -> Result<()> {
    match (key, value) {
        (None, _) => {
            let settings = serde_json::to_value(store.settings()?)?;
            for (k, v) in settings.as_object().into_iter().flatten() {
                let shown = if v.is_null() {
                    "(unset)".to_string()
                } else {
                    plain(v)
                };
                println!("{k} = {shown}");
            }
        }
        (Some(key), None) if unset => {
            store.set_setting(&key, None)?;
            println!("{key} unset");
        }
        (Some(key), None) => {
            let settings = serde_json::to_value(store.settings()?)?;
            match settings.get(&key) {
                // Unset prints nothing, so scripts can test for an empty result.
                Some(v) => println!("{}", plain(v)),
                None => bail!("unknown setting {key:?}"),
            }
        }
        (Some(key), Some(value)) => {
            store.set_setting(&key, Some(&value))?;
            println!("{key} = {value}");
        }
    }
    Ok(())
}

fn login_cmd(
    store: &Store,
    paths: &Paths,
    email: Option<String>,
    label: Option<String>,
    relogin: Option<String>,
    expect_email: Option<String>,
) -> Result<()> {
    let relogin = relogin.map(|r| store.find_account(&r)).transpose()?;
    let email = email.or_else(|| relogin.as_ref().and_then(|a| a.email.clone()));
    let claude = login::claude_executable(store)?;
    let staging = login::Staging::create(paths)?;

    match (&relogin, &email) {
        (Some(a), Some(e)) => println!(
            "Re-logging {} ({e}). Sign in to that account in your browser.",
            a.label
        ),
        (None, Some(e)) => println!("Adding {e}. Sign in to that account in your browser."),
        _ => println!("Adding an account. Sign in to the account you want to add in your browser."),
    }
    println!("Tip: your browser signs in as whichever claude.ai account it is logged into.\n");

    let status = login::login_command(&claude, staging.dir(), email.as_deref())
        .status()
        .context("running `claude auth login`")?;
    if !status.success() {
        bail!("`claude auth login` exited with {status}");
    }
    let imported = login::import(
        store,
        paths,
        staging.dir(),
        &login::LoginRequest {
            email,
            label,
            relogin: relogin.map(|a| a.id),
            expect_email,
        },
    )?;
    let a = &imported.account;
    println!(
        "\n{} {} ({}){}",
        if imported.created { "Added" } else { "Updated" },
        a.label,
        a.email.as_deref().unwrap_or("unknown email"),
        if imported.activated {
            "; it is now the active account"
        } else {
            ""
        },
    );
    Ok(())
}

fn read_stdin() -> String {
    let mut text = String::new();
    if !std::io::stdin().is_terminal() {
        let _ = std::io::stdin().read_to_string(&mut text);
    }
    text
}

/// Statusline: never fails visibly. Errors go to the log and the downstream statusline still runs.
fn statusline_entry(profile: Option<PathBuf>, observe: bool, then: Option<String>) {
    let raw = read_stdin();
    let input = StatusInput::parse(&raw);
    let mut prefix = String::new();

    let outcome: Result<Option<String>> = (|| {
        let (store, paths) = ccr_core::open(profile.clone())?;
        if observe {
            let dir = match &profile {
                Some(p) => p.clone(),
                None => std::env::var_os("CLAUDE_CONFIG_DIR")
                    .map(PathBuf::from)
                    .unwrap_or(ccr_core::paths::default_template_dir()?),
            };
            statusline::observe(&store, &dir, &input, time::now_ms())?;
            return Ok(None);
        }
        let result = statusline::tick(&store, &paths, &input, time::now_ms())?;
        if let Some(req) = &result.swap {
            process::spawn_swap(req, profile.as_deref())?;
        }
        Ok(result
            .active
            .map(|a| format::statusline_prefix(&a.label, result.pool_exhausted)))
    })();
    match outcome {
        Ok(Some(p)) => prefix = p,
        Ok(None) => {}
        Err(e) => process::log_error("statusline", &e),
    }

    let downstream = match (then, observe) {
        (Some(cmd), _) => Some(cmd).filter(|c| !c.trim().is_empty()),
        (None, true) => None,
        (None, false) => ccr_core::open(profile)
            .ok()
            .and_then(|(s, _)| s.settings().ok())
            .and_then(|s| s.statusline_downstream),
    };
    let body = downstream
        .and_then(|cmd| process::run_downstream(&cmd, &raw))
        .unwrap_or_default();

    let mut out = std::io::stdout().lock();
    let mut lines = body.lines();
    match (prefix.is_empty(), lines.next()) {
        (true, Some(first)) => {
            let _ = writeln!(out, "{first}");
        }
        (false, Some(first)) => {
            let _ = writeln!(out, "{prefix} {first}");
        }
        (false, None) => {
            let _ = writeln!(out, "{prefix}");
        }
        (true, None) => {}
    }
    for line in lines {
        let _ = writeln!(out, "{line}");
    }
}

fn stop_failure(profile: Option<PathBuf>) -> Result<()> {
    let payload: serde_json::Value = serde_json::from_str(&read_stdin()).unwrap_or_default();
    if payload.get("error").and_then(|e| e.as_str()) != Some("rate_limit") {
        return Ok(());
    }
    let (store, paths) = ccr_core::open(profile)?;
    let Some(active) = store.active_account()? else {
        return Ok(());
    };
    swap::run(
        &store,
        &paths,
        &SwapRequest {
            from: Some(active.id),
            to: None,
            reason: Reason::StopFailure,
            wait: false,
        },
    )?;
    Ok(())
}
