mod desktop_launch;
mod format;
mod herdr;
mod interrupts;
mod process;
mod ranking;
mod stop_failure;
mod uninstall;
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
        /// Renew the one account's login with Claude Code in a scratch profile (used by the local
        /// proxy while the app account is on).
        #[arg(long, hide = true)]
        renew: bool,
    },
    /// Start an account's 5-hour window now: Claude Code sends one short prompt to the smallest
    /// model with its login, then reads its usage.
    WarmUp {
        /// The account (label, email or id).
        account: String,
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
    /// Show or set whether cc-rotate starts an account's next 5-hour window with a short request
    /// as soon as one ends while it's not in use: yes or no (the default).
    KeepRunning {
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
    /// Remove cc-rotate: start at login, observe mode in your other profiles, the macOS Keychain
    /// item, the programs, the cc-rotate profile and the data folder with every saved login.
    /// Shows what it will do and asks first.
    Uninstall {
        /// Only show what would be removed.
        #[arg(long)]
        dry_run: bool,
        /// Don't ask before going ahead.
        #[arg(long)]
        yes: bool,
        /// Go ahead even though a session in the cc-rotate profile ran in the last 2 minutes.
        #[arg(long)]
        force: bool,
    },
    /// Show whether the Claude desktop app's Code sessions run on the pool, and the commands that
    /// turn that on or off (macOS and Windows; they need an administrator).
    Desktop {
        /// Print the macOS configuration profile that turns on the app's launcher policy.
        #[arg(long)]
        mobileconfig: bool,
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
    /// The launcher the Claude desktop app runs for each Code session, named in Claude Code's
    /// managed settings (`processWrapper`): it puts desktop sessions on the cc-rotate profile.
    #[command(hide = true)]
    DesktopLaunch {
        /// Claude Code and its arguments, after `--`.
        #[arg(last = true, required = true, value_parser = clap::value_parser!(std::ffi::OsString))]
        command: Vec<std::ffi::OsString>,
    },
    /// Link the transcripts of desktop sessions on the pool into the app's profile, so the app
    /// finds them after it restarts (started in the background by `desktop-launch`). With
    /// `--watch-pid`, keep looking while that Claude Code runs, until its new transcript is
    /// linked.
    #[command(hide = true)]
    DesktopLinkTranscripts {
        #[arg(long, value_name = "DIR")]
        app_profile: PathBuf,
        #[arg(long, value_name = "PID")]
        watch_pid: Option<u32>,
    },
    /// Show the app account, or set it: sessions act as the account the Claude desktop app is
    /// signed in to (so artifacts open in the app's artifact pane), while usage still rotates
    /// across the pool through a local proxy.
    ///
    /// `cc-rotate-ctl app-account <account>` turns it on, `cc-rotate-ctl app-account off` turns
    /// it off, and `cc-rotate-ctl app-account` shows it.
    AppAccount {
        /// The account the desktop app is signed in to (label, email or id), or `off` to turn the
        /// app account off. `off` always means off: name an account labelled "off" by its email
        /// or id.
        #[arg(value_name = "ACCOUNT")]
        account: Option<String>,
        /// Everything but installing the local proxy's service (for tests, which run the local
        /// proxy themselves).
        #[arg(long, hide = true, requires = "account")]
        no_service: bool,
    },
    /// The local proxy behind the app account (`relay run`, started by the launchers and its
    /// service).
    #[command(hide = true)]
    Relay {
        #[command(subcommand)]
        action: Option<RelayAction>,
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
enum RelayAction {
    /// Serve the relay on the port in `relay.json` (started by the launchers and the relay's
    /// service); exits at once when another relay runs.
    #[command(hide = true)]
    Run,
}

#[derive(Subcommand)]
enum HookCommand {
    /// A turn stopped at a usage limit: swap, then wake the session once the profile holds an
    /// account with room.
    StopFailure,
    /// Report `cc-rotate --resume <id>` to herdr when the session runs in a herdr pane.
    SessionStart,
    /// A turn ended normally: a session stopped at a usage limit is working again.
    Stop,
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
        // Before anything opens the store for writing.
        Command::Uninstall {
            dry_run,
            yes,
            force,
        } => uninstall::run(cli.profile, dry_run, yes, force),
        // Reads system settings only.
        Command::Desktop { mobileconfig: true } => {
            print!("{}", ccr_core::desktop::mobileconfig());
            Ok(ExitCode::SUCCESS)
        }
        // Reads system settings, and the store only for the app account.
        Command::Desktop {
            mobileconfig: false,
        } => {
            let setup = ccr_core::desktop::read_setup();
            let home = relay_home_label();
            print!(
                "{}",
                format::desktop(&setup, &dunce_exe()?, home.as_deref())
            );
            Ok(ExitCode::SUCCESS)
        }
        // Starts Claude Code for the desktop app: nothing here may write to the store.
        Command::DesktopLaunch { command } => Ok(desktop_launch::run(cli.profile, command)),
        Command::DesktopLinkTranscripts {
            app_profile,
            watch_pid,
        } => {
            desktop_launch::link_transcripts(cli.profile, &app_profile, watch_pid);
            Ok(ExitCode::SUCCESS)
        }
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
            hook: HookCommand::Stop,
        } => {
            // Never fails the session: errors only go to the log.
            if let Err(e) = stop_hook(cli.profile, &read_stdin()) {
                process::log_error("hook stop", &e);
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
        // Opens the store and resolves the profile itself, after taking the relay's lock.
        Command::Relay {
            action: Some(RelayAction::Run),
        } => Ok(relay_run()),
        // `relay` alone is no longer a user command: point at its replacement.
        Command::Relay { action: None } => {
            eprintln!(
                "error: the app account is shown and set with `cc-rotate-ctl app-account` (see `cc-rotate-ctl app-account --help`)."
            );
            Ok(ExitCode::from(2))
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
            let (store, paths) = ccr_core::open(cli.profile.clone())?;
            dispatch(command, &store, &paths, cli.profile)
        }
    }
}

/// `profile_flag` is `--profile`, which `open` already used for `paths`.
fn dispatch(
    command: Command,
    store: &Store,
    paths: &Paths,
    profile_flag: Option<PathBuf>,
) -> Result<ExitCode> {
    match command {
        Command::Status { json } => {
            let _ = ccr_core::budget::refresh_rates(store, time::now_ms());
            if let Some(req) = statusline::sync(store, paths)? {
                swap::run(store, paths, &req)?;
            }
            if statusline::needs_settling(store, paths, time::now_ms())?
                && let Ok(_lock) = ccr_core::lock::DirLock::acquire(
                    &paths.swap_lock(),
                    ccr_core::lock::LockOptions::claude(Duration::ZERO),
                )
            {
                swap::settle_profile(store, paths)?;
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
        Command::Check {
            accounts,
            json,
            renew,
        } => check_cmd(store, paths, &accounts, json, renew)?,
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
            // Before asking for --yes: there is nothing to confirm.
            ccr_core::relay::refuse_removal(store, &a)?;
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
        Command::KeepRunning { account, value } => {
            ranking::keep_running(store, paths, &account, value.as_deref())?
        }
        Command::WarmUp { account, json } => {
            usage_check::sweep(store, paths)?;
            let account = store.find_account(&account)?;
            let outcome =
                usage_check::warm_up(store, paths, account.id, usage_check::Trigger::Manual)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&outcome)?);
            } else {
                println!(
                    "{}: {}",
                    account.label,
                    usage_check::describe_warm_up(&outcome)
                );
            }
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
        Command::AppAccount { account: None, .. } => {
            print!("{}", format::relay(&relay_view(store, paths)?))
        }
        // `off` is a keyword: it always means off, whatever an account is called.
        Command::AppAccount {
            account: Some(off), ..
        } if off == "off" => {
            ccr_core::relay::turn_off(store, paths)?;
            println!(
                "The app account is off. New sessions start without the local proxy; it serves the sessions started while the app account was on until it has had no request for an hour, then removes itself."
            );
            println!(
                "Restart the sessions started while the app account was on within the hour: once the local proxy has removed itself, they get connection errors."
            );
        }
        Command::AppAccount {
            account: Some(home),
            no_service,
        } => {
            let ctl = dunce_exe()?;
            let profile_flag = profile_flag.as_deref();
            let (account, cfg) = ccr_core::relay::on(store, paths, &home, || {
                if no_service {
                    return Ok(());
                }
                let env = ccr_core::relay_service::service_env(profile_flag);
                ccr_core::relay_service::Service::real()?.install(&ctl, &env)
            })?;
            println!(
                "The app account is {0}: sessions act as {0} (the desktop app's account); usage still rotates across the pool.",
                account.label
            );
            println!(
                "Sessions started from now on go through the local proxy on 127.0.0.1:{}; restart running ones to move them.",
                cfg.port
            );
        }
        Command::Statusline { .. }
        | Command::Hook { .. }
        | Command::HerdrReport { .. }
        | Command::Relay { .. }
        | Command::DesktopLaunch { .. }
        | Command::DesktopLinkTranscripts { .. }
        | Command::Desktop { .. }
        | Command::Uninstall { .. } => {
            unreachable!()
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// How long a relay that cannot start waits before it exits, so its service (which restarts it
/// whenever it ends) restarts it at that pace rather than in a tight loop.
const RELAY_START_FAILED_WAIT: Duration = Duration::from_secs(10);

/// `cc-rotate-ctl relay run` (spec sections 5 and 10): hold `relay.lock`, resolve the store and
/// profile, bind 127.0.0.1 on the port in `relay.json`, and serve until the relay leaves (an
/// upgrade, or an hour without requests once relay mode is off).
/// Another relay holding the lock, or another program listening on the port, means there is
/// nothing to do: it exits at once and quietly, the taken port noted in `relay.log`.
/// A relay that cannot start otherwise says why in `relay.log` and exits with an error after a
/// wait.
fn relay_run() -> ExitCode {
    // The program as it is now, before any wait: an upgrade while this relay waits for the lock
    // is still one, and it leaves at its first check.
    let program = std::env::current_exe().ok();
    let program_at_start = program.as_deref().and_then(ccr_relay::fingerprint);
    // Read once, and not passed on to what the relay starts (Claude Code for renewals).
    let from_service =
        std::env::var_os(ccr_core::relay_service::SERVICE_MARK_ENV).is_some_and(|v| !v.is_empty());
    // SAFETY: no other thread runs yet, so none reads the environment meanwhile.
    unsafe { std::env::remove_var(ccr_core::relay_service::SERVICE_MARK_ENV) };
    // The lock and `relay.json` are in the data folder; the profile is resolved below.
    let data = match Paths::default_data_dir() {
        Ok(dir) => Paths::new(dir, PathBuf::new()),
        Err(e) => return relay_start_failed(None, &e),
    };
    // An OS file lock, held for the life of the process: however the process ends (a crash,
    // SIGKILL, SIGTERM from its service), the system releases it and the next relay can start.
    // One the service started waits for it, and takes over when the relay a launcher started
    // ends: exiting would only have the service start it again and again.
    let _lock = match relay_lock(&data, from_service) {
        Ok(Some(lock)) => lock,
        Ok(None) => return ExitCode::SUCCESS,
        Err(e) => return relay_start_failed(Some(&data), &e),
    };
    let ready = match relay_ready(&data, program, program_at_start) {
        Ok(Some(ready)) => ready,
        // Another relay (one from another data folder, say) already serves this port.
        Ok(None) => return ExitCode::SUCCESS,
        Err(e) => return relay_start_failed(Some(&data), &e),
    };
    match ready() {
        Ok(()) => ExitCode::SUCCESS,
        // The runtime would not start, or the relay refused its address: before it served.
        Err(e) => relay_start_failed(Some(&data), &e),
    }
}

/// Everything the relay needs, resolved before it takes the port, and then the port: what
/// serves until the relay leaves. `None` when something else listens on the port, which is
/// written to `relay.log`.
fn relay_ready(
    data: &Paths,
    program: Option<PathBuf>,
    program_at_start: Option<ccr_relay::Fingerprint>,
) -> Result<Option<impl FnOnce() -> Result<()> + use<>>> {
    use ccr_relay::RelayEvents;
    use std::sync::Arc;

    // The relay runs swaps, which need the profile as any swap does: the one every other command
    // resolves (`CC_ROTATE_PROFILE`, the `profile_dir` setting, `~/.claude-rotate`).
    let (store, paths) = ccr_core::open(None).context("opening the store and the profile")?;
    let cfg = ccr_core::relay::RelayConfig::read(&data.relay_config())?
        .with_context(|| format!("{} does not exist", data.relay_config().display()))?;
    let upstream = relay_upstream()?;
    let events_store = Store::open(&paths.store_db())?;
    let listener = match std::net::TcpListener::bind(("127.0.0.1", cfg.port)) {
        Ok(listener) => listener,
        // Exits quietly (a relay of another data folder may serve it), but says so for `doctor`.
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
            ccr_relay::log_now(
                &data.relay_log(),
                &format!(
                    "cannot start: 127.0.0.1:{} is taken by another program",
                    cfg.port
                ),
            );
            return Ok(None);
        }
        Err(e) => {
            return Err(e).with_context(|| format!("listening on 127.0.0.1:{}", cfg.port));
        }
    };
    Ok(Some(move || {
        let log = Arc::new(ccr_relay::FileLog::new(paths.relay_log()));
        log.log(&format!(
            "started, version {}, port {}",
            env!("CARGO_PKG_VERSION"),
            cfg.port
        ));
        // Readings, threshold swaps and swaps at a refused limit (spec 7 and 8). Each part has a
        // store connection of its own, and every swap opens another.
        let events = Arc::new(ccr_relay::StoreEvents::new(
            events_store,
            Arc::new(ccr_relay::StoreSwaps::new(paths.clone())),
            log.clone(),
        ));
        // Renewals run a check on a store of their own (spec 9).
        let renewals = Arc::new(ccr_relay::StoreRenewals::new(paths.clone(), log));
        let options = ccr_relay::Options {
            program,
            program_at_start,
            lifecycle: Arc::new(RelayLifecycle {
                paths: paths.clone(),
            }),
            ..ccr_relay::Options::default()
        };
        ccr_relay::run_blocking(
            listener,
            cfg,
            options,
            upstream,
            Arc::new(ccr_relay::StoreLogins::new(store, renewals)),
            events,
        )
    }))
}

/// One line in `relay.log` (or cc-rotate's log, without a data folder), then the wait, then an
/// error exit.
fn relay_start_failed(data: Option<&Paths>, e: &anyhow::Error) -> ExitCode {
    match data {
        Some(data) => ccr_relay::log_now(&data.relay_log(), &format!("cannot start: {e:#}")),
        None => process::log_error("relay run", e),
    }
    std::thread::sleep(RELAY_START_FAILED_WAIT);
    ExitCode::FAILURE
}

/// Whether relay mode is on, and what the relay removes when it leaves for good (spec 10).
struct RelayLifecycle {
    paths: Paths,
}

impl RelayLifecycle {
    fn home(&self) -> Result<Option<i64>> {
        ccr_core::relay::home(&Store::open(&self.paths.store_db())?)
    }
}

impl ccr_relay::Lifecycle for RelayLifecycle {
    fn mode_on(&self) -> bool {
        // Unsure counts as on: the relay stays.
        !matches!(self.home(), Ok(None))
    }

    fn retire(&self) -> Result<()> {
        // `app-account <account>` may have run since the last look.
        if self.home()?.is_some() {
            bail!("the app account is on again");
        }
        let service = ccr_core::relay_service::Service::real()?;
        let data_dir = std::env::var("CC_ROTATE_DATA_DIR").ok();
        ccr_core::relay_service::retire(&service, &self.paths.relay_config(), data_dir.as_deref())
    }
}

/// The home account's label while relay mode is on, for `desktop`, which otherwise reads system
/// settings only: `None` when relay mode is off, or the store isn't there or can't be read.
/// Read-only: `desktop` never writes to, or migrates, the store.
fn relay_home_label() -> Option<String> {
    use ccr_core::relay::{SETTING_HOME, SETTING_MODE};
    let path = Paths::default_data_dir().ok()?.join("store.db");
    if !path.exists() {
        return None;
    }
    if Store::read_setting_any_schema(&path, SETTING_MODE)
        .ok()??
        .as_str()
        != "on"
    {
        return None;
    }
    let id: i64 = Store::read_setting_any_schema(&path, SETTING_HOME)
        .ok()??
        .trim()
        .parse()
        .ok()?;
    // The label needs this build's schema; without it, the id still says which account.
    let label = Store::open_read_only(&path)
        .ok()
        .and_then(|store| store.account(id).ok().flatten())
        .map(|a| a.label);
    Some(label.unwrap_or_else(|| format!("account {id}")))
}

/// What `cc-rotate-ctl app-account` shows.
fn relay_view(store: &Store, paths: &Paths) -> Result<format::RelayView> {
    let home = match ccr_core::relay::home(store)? {
        Some(id) => Some(
            store
                .account(id)?
                .map_or_else(|| format!("account {id}"), |a| a.label),
        ),
        None => None,
    };
    let port = ccr_core::relay::RelayConfig::read(&paths.relay_config())?.map(|cfg| cfg.port);
    Ok(format::RelayView {
        home,
        port,
        answers: port.is_some_and(ccr_core::relay::answers),
        service: ccr_core::relay_service::status(),
        bypassing: ccr_core::relay::bypass_count(store, time::now_ms())?,
    })
}

/// The relay's lock file, locked: `None` when another process holds it.
/// With `wait`, waits for the process that holds it to let go instead.
fn relay_lock(data: &Paths, wait: bool) -> Result<Option<std::fs::File>> {
    let path = data.relay_lock();
    let path = path.as_path();
    if let Some(dir) = path.parent() {
        ccr_core::secret::create_private_dir(dir)?;
    }
    let mut options = std::fs::OpenOptions::new();
    options.create(true).truncate(false).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options
        .open(path)
        .with_context(|| format!("opening {}", path.display()))?;
    match file.try_lock() {
        Ok(()) => Ok(Some(file)),
        Err(std::fs::TryLockError::WouldBlock) if wait => {
            ccr_relay::log_now(
                &data.relay_log(),
                "another local proxy runs; waiting for it to end",
            );
            file.lock()
                .with_context(|| format!("locking {}", path.display()))?;
            Ok(Some(file))
        }
        Err(std::fs::TryLockError::WouldBlock) => Ok(None),
        Err(std::fs::TryLockError::Error(e)) => {
            Err(e).with_context(|| format!("locking {}", path.display()))
        }
    }
}

/// Anthropic's API; a test build can name a local server in `CC_ROTATE_RELAY_TEST_UPSTREAM`.
fn relay_upstream() -> Result<ccr_relay::Upstream> {
    #[cfg(feature = "relay-test-upstream")]
    if let Some(base) = std::env::var_os("CC_ROTATE_RELAY_TEST_UPSTREAM").filter(|v| !v.is_empty())
    {
        let base = base
            .to_str()
            .context("CC_ROTATE_RELAY_TEST_UPSTREAM is not valid UTF-8")?
            .parse()
            .context("CC_ROTATE_RELAY_TEST_UPSTREAM is not a URL")?;
        return ccr_relay::Upstream::for_test(base);
    }
    ccr_relay::Upstream::anthropic()
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

fn check_cmd(
    store: &Store,
    paths: &Paths,
    references: &[String],
    json: bool,
    renew: bool,
) -> Result<()> {
    if renew && references.len() != 1 {
        bail!("--renew takes exactly one account");
    }
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
        let outcome = if renew {
            usage_check::renew(store, paths, a.id)?
        } else {
            usage_check::check(store, paths, a.id, usage_check::Trigger::Manual)?
        };
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

    let mut child = login::login_command(&claude, staging.dir(), email.as_deref())
        .spawn()
        .context("running `claude auth login`")?;
    // Ctrl+C reaches Claude Code too, which ends the sign-in; this process waits for it so the
    // staging folder, which may already hold the new login, is removed on the way out.
    let status = {
        let held = interrupts::Held::new();
        loop {
            if let Some(status) = child.try_wait().context("running `claude auth login`")? {
                break status;
            }
            if held.ended() {
                // Asked to end, and Claude Code wasn't: stop it, and let the staging folder go.
                let _ = child.kill();
                let _ = child.wait();
                // Removed while signals are still held, so a second one doesn't cut it short.
                drop(staging);
                bail!("Stopped the sign-in.");
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    };
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

/// `hook stop` (spec section 24.3). Runs after every turn of every session in the profile, so it
/// does nothing beyond reading the store unless the session has an open stop.
fn stop_hook(profile: Option<PathBuf>, input: &str) -> Result<()> {
    let payload: serde_json::Value = serde_json::from_str(input).unwrap_or_default();
    let Some(session) = payload
        .get("session_id")
        .and_then(serde_json::Value::as_str)
        .filter(|s| !s.is_empty() && s.len() <= 128)
    else {
        return Ok(());
    };
    let (store, _) = ccr_core::open(profile)?;
    ccr_core::waiting::note_turn_ended(&store, session, time::now_ms())?;
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
    let mut input = StatusInput::parse(&raw);
    input.claude_pid = statusline::claude_pid_from_env();
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
        let result = statusline::tick(&store, &paths, &input, time::now_ms(), |name| {
            std::env::var(name).ok()
        })?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    fn parse(args: &[&str]) -> Command {
        let argv = std::iter::once("cc-rotate-ctl").chain(args.iter().copied());
        Cli::try_parse_from(argv)
            .unwrap_or_else(|e| panic!("{args:?}: {e}"))
            .command
    }

    fn refused(args: &[&str]) -> bool {
        Cli::try_parse_from(std::iter::once("cc-rotate-ctl").chain(args.iter().copied())).is_err()
    }

    #[test]
    fn the_command_line_is_well_formed() {
        Cli::command().debug_assert();
    }

    #[test]
    fn app_account_and_the_hidden_relay_run_parse() {
        assert!(matches!(
            parse(&["app-account"]),
            Command::AppAccount {
                account: None,
                no_service: false
            }
        ));
        assert!(matches!(
            parse(&["app-account", "work", "--no-service"]),
            Command::AppAccount { account: Some(a), no_service: true } if a == "work"
        ));
        assert!(matches!(
            parse(&["app-account", "off"]),
            Command::AppAccount { account: Some(a), no_service: false } if a == "off"
        ));
        // The installed service runs exactly this.
        assert!(matches!(
            parse(&["relay", "run"]),
            Command::Relay {
                action: Some(RelayAction::Run)
            }
        ));
        assert!(refused(&["app-account", "--no-service"]));
        assert!(refused(&["relay", "on", "--home", "work"]));
        assert!(refused(&["relay", "off"]));
    }

    #[test]
    fn help_lists_app_account_and_not_relay() {
        let help = Cli::command().render_help().to_string();
        assert!(help.contains("app-account"), "{help}");
        assert!(!help.to_lowercase().contains("relay"), "{help}");

        let mut cli = Cli::command();
        let app_account = cli.find_subcommand_mut("app-account").unwrap();
        let help = app_account.render_long_help().to_string();
        assert!(help.contains("off"), "{help}");
        assert!(!help.to_lowercase().contains("relay"), "{help}");
        assert!(!help.contains("--no-service"), "{help}");
        assert!(!help.contains('\u{2014}'), "no em dashes: {help}");
    }
}
