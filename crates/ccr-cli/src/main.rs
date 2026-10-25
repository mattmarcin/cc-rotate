mod format;
mod herdr;
mod process;
mod ranking;
mod stop_failure;
mod watch;

use std::io::{IsTerminal, Read, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use ccr_core::scheduler::Reason;
use ccr_core::statusline::StatusInput;
use ccr_core::store::{AccountState, kind};
use ccr_core::swap::{SwapOutcome, SwapRequest};
use ccr_core::{
    Paths, Store, doctor, history, login, profiles, setup, snapshot, statusline, swap, time,
    usage_check,
};
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "cc-rotate-ctl",
    version,
    about = "Run one Claude Code profile on a pool of accounts"
)]
struct Cli {
    /// cc-rotate profile directory (defaults to the one recorded by `setup`).
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
    /// Add an account, or sign in to one again, through Claude Code's own sign-in.
    Login {
        /// Pre-fill the sign-in page with this email.
        #[arg(long)]
        email: Option<String>,
        /// Label for a new account (defaults to the email's local part).
        #[arg(long)]
        label: Option<String>,
        /// Sign in to this existing account again (label, email or id).
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
    /// Check the usage of accounts not in use with Claude Code's own `/usage` (no turn, no quota).
    Check {
        /// Accounts to check (label, email or id); every account not in use when none are given.
        accounts: Vec<String>,
        #[arg(long)]
        json: bool,
    },
    /// Swap the profile to another account now.
    Swap {
        /// Swap to this account instead of the scheduler's pick.
        #[arg(long, value_name = "ACCOUNT")]
        to: Option<String>,
        /// Only swap if this account id is still in use (used by the status line).
        #[arg(long, hide = true)]
        from_id: Option<i64>,
        /// Swap to this account id (used by the status line).
        #[arg(long, hide = true, conflicts_with = "to")]
        to_id: Option<i64>,
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
    /// Remove an account and its saved login.
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
    /// Show or set the weekly usage (percent) at which rotation stops using an account, keeping
    /// the rest of its week for you: a number from 1 to 100, or off to use the setting.
    WeeklyCap {
        account: String,
        value: Option<String>,
    },
    /// Show the status and recent events live, until you press q.
    ///
    /// Redraws in place every few seconds. Read-only: it never swaps, syncs logins or starts
    /// Claude Code. Without a terminal for input and output, it prints the status once.
    Watch {
        /// Seconds between redraws.
        #[arg(long, value_name = "SECONDS", default_value_t = 2,
              value_parser = clap::value_parser!(u64).range(1..=3600))]
        interval: u64,
    },
    /// Show how much of its limits each account used, week by week.
    Report {
        /// How many weeks back to report.
        #[arg(long, value_name = "N", default_value_t = 4,
              value_parser = clap::value_parser!(u64).range(1..=52))]
        weeks: u64,
        #[arg(long)]
        json: bool,
    },
    /// Show or set whether an account moves ahead when its weekly budget would reset unused:
    /// yes (the default) or no.
    UseBeforeReset {
        account: String,
        #[arg(value_parser = ["yes", "no"])]
        value: Option<String>,
    },
    /// Show recent swaps, sign-ins and errors.
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
    /// Create the cc-rotate profile from an existing Claude Code profile.
    Setup {
        /// Profile to copy settings, hooks, skills and plugins from (default: ~/.claude).
        #[arg(long, value_name = "DIR")]
        from: Option<PathBuf>,
        /// Rebuild an existing cc-rotate profile (its login is kept).
        #[arg(long)]
        force: bool,
        /// Remove the `cc-rotate` PowerShell function older versions added to your PowerShell
        /// profile; it would hide the `cc-rotate` command. With an existing profile and no
        /// --force, this is all setup does.
        #[arg(long)]
        remove_alias: bool,
        /// Keep an existing cc-rotate profile instead of failing, only bringing cc-rotate's own
        /// hooks in it up to date (the installer runs setup on every upgrade).
        #[arg(long, conflicts_with = "force")]
        keep_existing: bool,
    },
    /// Check cc-rotate's data, the profile and Claude Code.
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
    /// Status line entry point (configured by `setup`; reads Claude Code's JSON on stdin).
    Statusline {
        /// Only record usage, for a profile other than the cc-rotate profile; never swaps.
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
    /// Check the usage of the account in use with a login Claude Code can't renew (started in the
    /// background by the status line).
    #[command(hide = true)]
    ConfirmActive,
    /// Identify the profile's login after an interrupted swap, or when it changed without
    /// cc-rotate writing it (started in the background by the status line).
    #[command(hide = true)]
    SettleLogin,
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
    /// A turn stopped at a usage limit: swap, then wake the session once the profile holds an
    /// account with room.
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
        } => Ok(match stop_failure::run(cli.profile, &read_stdin()) {
            Ok(code) => code,
            Err(e) => {
                // Never fails the session: errors only go to the log.
                process::log_error("hook stop-failure", &e);
                ExitCode::SUCCESS
            }
        }),
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
            let _ = ccr_core::budget::refresh_rates(store, time::now_ms());
            if let Some(req) = statusline::sync(store, paths)? {
                swap::run(store, paths, &req)?;
            }
            if statusline::needs_settling(store, paths, time::now_ms())? {
                if let Ok(_lock) = ccr_core::lock::DirLock::acquire(
                    &paths.swap_lock(),
                    ccr_core::lock::LockOptions::claude(Duration::ZERO),
                ) {
                    swap::settle_profile(store, paths)?;
                }
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
                        format!("No profile named {name:?}. See `cc-rotate-ctl profiles`.")
                    })?;
                if let Some(label) = &found.in_pool_as {
                    bail!(
                        "{} is already added as {label}",
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
                    Some(label) => format!("added as {label}"),
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
            to_id,
            reason,
            no_wait,
            quiet,
        } => {
            let reason =
                Reason::parse(&reason).with_context(|| format!("unknown reason {reason:?}"))?;
            let to = match to_id {
                Some(id) => Some(id),
                None => to
                    .map(|r| store.find_account(&r))
                    .transpose()?
                    .map(|a| a.id),
            };
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
                            "No swap: every other account is near a limit, paused or signed out"
                        );
                    }
                }
            }
        }
        Command::Check { accounts, json } => check_cmd(store, paths, &accounts, json)?,
        Command::SettleLogin => {
            let timeout = Duration::from_millis(store.settings()?.lock_timeout_ms);
            let _lock = ccr_core::lock::DirLock::acquire(
                &paths.swap_lock(),
                ccr_core::lock::LockOptions::claude(timeout),
            )?;
            let outcome = swap::settle_profile(store, paths)?;
            println!(
                "{}",
                match outcome {
                    swap::Settle::Nothing => "Nothing to do: the profile's login is known.",
                    swap::Settle::Settled =>
                        "Identified which account the profile's login belongs to.",
                    swap::Settle::Unsure =>
                        "Can't tell yet which account the profile is signed in to. Retrying in a minute.",
                }
            );
        }
        Command::ConfirmActive => match usage_check::confirm_active(store, paths) {
            Ok(outcome) => println!("{}", usage_check::describe(&outcome)),
            Err(e) => process::log_error("confirm-active", &e),
        },
        Command::Pause { account } => set_state(store, &account, AccountState::Paused)?,
        Command::Resume { account } => set_state(store, &account, AccountState::Parked)?,
        Command::Remove { account, yes } => {
            let a = store.find_account(&account)?;
            if a.state == AccountState::Active {
                bail!("{} is in use. Swap to another account first.", a.label);
            }
            if !yes {
                bail!(
                    "This removes {} and its saved login. Run again with --yes to confirm.",
                    a.label
                );
            }
            usage_check::remove_account(store, paths, a.id)?;
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
        Command::WeeklyCap { account, value } => {
            ranking::weekly_cap(store, &account, value.as_deref())?
        }
        Command::UseBeforeReset { account, value } => {
            ranking::use_before_reset(store, &account, value.as_deref())?
        }
        Command::Watch { interval } => {
            watch::run(store, paths, Duration::from_secs(interval))?;
        }
        Command::Report { weeks, json } => report_cmd(store, weeks as usize, json)?,
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
            keep_existing,
        } => {
            if (remove_alias || keep_existing) && !force && paths.settings_file().exists() {
                if keep_existing {
                    // An upgrade: cc-rotate's own hooks follow the new release, the rest stays.
                    // A settings.json it cannot update must not fail the upgrade: doctor says
                    // the hook is outdated until it is fixed.
                    let note = match setup::update_hooks(&paths.profile_dir, &dunce_exe()?) {
                        Ok(true) => {
                            "Updated cc-rotate's hooks in it and left everything else as it is."
                                .to_string()
                        }
                        Ok(false) => "Leaving it as it is.".to_string(),
                        Err(e) => {
                            eprintln!("warning: couldn't update cc-rotate's hooks in it: {e:#}");
                            "Leaving it as it is.".to_string()
                        }
                    };
                    println!(
                        "The cc-rotate profile at {} is already set up. {note}",
                        paths.profile_dir.display(),
                    );
                    println!(
                        "To rebuild it from your template profile, run: cc-rotate-ctl setup --force"
                    );
                }
                if remove_alias {
                    for file in setup::remove_powershell_alias()? {
                        println!("Removed the old cc-rotate function from {}", file.display());
                    }
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
            println!("cc-rotate profile ready at {}", report.profile.display());
            println!("  copied: {}", report.copied.join(", "));
            if let Some(how) = &report.claude_md {
                println!("  CLAUDE.md: {how}");
            }
            match &report.statusline_downstream {
                Some(cmd) => println!("  your status line still runs after cc-rotate's: {cmd}"),
                None => println!("  no existing status line to chain"),
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

fn check_cmd(store: &Store, paths: &Paths, references: &[String], json: bool) -> Result<()> {
    // Settle anything an earlier check left behind first.
    usage_check::sweep(store, paths)?;
    let accounts = if references.is_empty() {
        store
            .accounts()?
            .into_iter()
            .filter(|a| matches!(a.state, AccountState::Parked | AccountState::Paused))
            .collect()
    } else {
        references
            .iter()
            .map(|r| store.find_account(r))
            .collect::<Result<Vec<_>>>()?
    };
    if accounts.is_empty() {
        println!("No account to check: every account is in use or signed out.");
        return Ok(());
    }
    let width = accounts.iter().map(|a| a.label.len()).max().unwrap_or(5);
    let mut results = Vec::new();
    for a in &accounts {
        if !json {
            print!("{:<width$}  ", a.label);
            let _ = std::io::stdout().flush();
        }
        let outcome = usage_check::check(store, paths, a.id, usage_check::Trigger::Manual)?;
        if json {
            results.push(serde_json::json!({ "label": a.label, "result": outcome }));
        } else {
            println!("{}", usage_check::describe(&outcome));
        }
    }
    if json {
        println!("{}", serde_json::to_string_pretty(&results)?);
    }
    Ok(())
}

/// `report [--weeks N] [--json]`: read-only, like `watch`.
fn report_cmd(store: &Store, weeks: usize, json: bool) -> Result<()> {
    let now = time::now_ms();
    let bounds = history::local_weeks(now / 1000, weeks);
    let histories = history::report(store, now, &bounds)?;
    if json {
        let mut rows = Vec::with_capacity(histories.len());
        for h in &histories {
            let mut row = serde_json::to_value(h)?;
            row["summary"] = format::report_summary(h, weeks).into();
            rows.push(row);
        }
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({ "weeks": weeks, "accounts": rows }))?
        );
    } else {
        print!("{}", format::report(&histories, weeks));
    }
    Ok(())
}

fn set_state(store: &Store, reference: &str, state: AccountState) -> Result<()> {
    let a = store.find_account(reference)?;
    match (a.state, state) {
        (AccountState::Active, AccountState::Paused) => {
            bail!(
                "{} is in use. Swap to another account before pausing it.",
                a.label
            )
        }
        (AccountState::NeedsLogin, AccountState::Parked) => {
            bail!(
                "{} is signed out. Sign in again with `cc-rotate-ctl login --relogin {}`.",
                a.label,
                a.label
            )
        }
        (AccountState::Active, _) => bail!("{} is in use", a.label),
        _ => {}
    }
    store.set_state(a.id, state)?;
    store.log_event(kind::STATE, Some(a.id), None, Some(state.as_str()), None)?;
    match state {
        AccountState::Paused => println!("Paused {}", a.label),
        _ => println!("Resumed {}", a.label),
    }
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
            store
                .set_setting(&key, Some(&value))
                .with_context(|| key.clone())?;
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
            "Signing in to {} again ({e}). Sign in to that account in your browser.",
            a.label
        ),
        (None, Some(e)) => println!("Adding {e}. Sign in to that account in your browser."),
        _ => println!("Adding an account. Sign in to the account you want to add in your browser."),
    }
    println!(
        "Your browser signs in with the claude.ai account it's signed in to. Switch accounts there first if needed.\n"
    );

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
        "\n{} {} ({}).{}",
        if imported.created {
            "Added"
        } else {
            "Signed in again to"
        },
        a.label,
        a.email.as_deref().unwrap_or("unknown email"),
        if imported.activated {
            " It's now in use."
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
            statusline::observe(&store, &dir, &paths.home, &input, time::now_ms())?;
            return Ok(None);
        }
        let result = statusline::tick(&store, &paths, &input, time::now_ms())?;
        if let Some(req) = &result.swap {
            process::spawn_swap(req, profile.as_deref())?;
        } else if result.settle_login {
            process::spawn_settle_login(profile.as_deref())?;
        } else if result.confirm {
            process::spawn_confirm(profile.as_deref())?;
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
