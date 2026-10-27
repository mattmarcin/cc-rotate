//! `cc-rotate-ctl uninstall`: show what removing cc-rotate does, ask, then do it
//! (`ccr_core::uninstall`).

use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use ccr_core::uninstall::{self, KeychainItem, Plan, RealSystem, Removal, Restore};
use ccr_core::{Paths, Store, time};

/// What the user types to go ahead.
const CONFIRM: &str = "uninstall";

pub fn run(profile: Option<PathBuf>, dry_run: bool, yes: bool, force: bool) -> Result<ExitCode> {
    let data_dir = Paths::default_data_dir()?;
    let store_path = data_dir.join("store.db");
    // Read-only: nothing changes before the user says so.
    let store = if store_path.exists() {
        Some(Store::open_read_only(&store_path).context(
            "the store was made by another version of cc-rotate; install this version first, or delete the files by hand (README, Uninstall)",
        )?)
    } else {
        None
    };
    let from_env = std::env::var_os("CC_ROTATE_PROFILE")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from);
    // As `ccr_core::open` finds it.
    let profile_dir = match profile.or(from_env) {
        Some(dir) => dir,
        None => match store
            .as_ref()
            .map(Store::settings)
            .transpose()?
            .and_then(|s| s.profile_dir)
        {
            Some(dir) if !std::path::Path::new(&dir).is_absolute() => {
                bail!("profile_dir must be a full path; pass --profile <full path>")
            }
            Some(dir) => PathBuf::from(dir),
            None => Paths::default_profile_dir()?,
        },
    };
    let paths = Paths::new(data_dir, profile_dir);
    let exe = std::env::current_exe().context("finding this program")?;
    let exe = dunce::canonicalize(&exe).unwrap_or(exe);
    let system = RealSystem::installed(&paths.home);
    let plan = uninstall::plan(&system, &paths, store.as_ref(), &exe, time::now_ms())?;
    drop(store);

    print_plan(&plan);
    let blocked = blocked(&plan, force);
    if dry_run {
        if let Some(why) = blocked {
            println!();
            println!("It would not go ahead now: {why}");
        }
        return Ok(ExitCode::SUCCESS);
    }
    if let Some(why) = blocked {
        bail!(why);
    }
    if !yes {
        if !std::io::stdin().is_terminal() {
            bail!("run cc-rotate-ctl uninstall in a terminal to confirm, or pass --yes");
        }
        print!("\nType \"{CONFIRM}\" to go ahead: ");
        std::io::stdout().flush()?;
        let mut answer = String::new();
        std::io::stdin().read_line(&mut answer)?;
        if answer.trim() != CONFIRM {
            println!("Nothing was changed.");
            return Ok(ExitCode::FAILURE);
        }
    }

    let out = uninstall::run(&system, &plan, &exe);
    println!();
    for line in &out.done {
        println!("{line}");
    }
    for line in &out.failed {
        eprintln!("error: {line}");
    }
    if !out.deleted_later.is_empty() {
        println!(
            "Windows lets these go only once they stop running; they are deleted a few seconds after this command ends:"
        );
        for file in &out.deleted_later {
            println!("  {}", file.display());
        }
    }
    if !out.left.is_empty() {
        println!(
            "Still running (an open session?), so renamed aside; delete what is left of these once they stop:"
        );
        for file in &out.left {
            println!("  {}", file.display());
        }
    }
    let by_hand: Vec<_> = plan
        .observe
        .iter()
        .filter(|o| o.restore == Restore::ByHand)
        .collect();
    if !by_hand.is_empty() {
        println!("Change the status line in these by hand, keeping what runs after --then:");
        for o in by_hand {
            println!("  {}", o.settings.display());
        }
    }
    if matches!(
        ccr_core::herdr::detect(),
        Ok(ccr_core::herdr::Detected::Installed(_) | ccr_core::herdr::Detected::InPane(_))
    ) {
        println!(
            "herdr panes that resume a session with `cc-rotate --resume` can't anymore: quit those sessions, or close the panes."
        );
    }
    if out.failed.is_empty() {
        println!("cc-rotate is uninstalled.");
        Ok(ExitCode::SUCCESS)
    } else {
        Ok(ExitCode::FAILURE)
    }
}

/// Why the plan cannot be carried out now, if it cannot.
fn blocked(plan: &Plan, force: bool) -> Option<String> {
    if let KeychainItem::Unreachable(service) = &plan.keychain {
        return Some(format!(
            "this session can't use the macOS Keychain, which holds the cc-rotate profile's login (\"{service}\"). Run cc-rotate-ctl uninstall in Terminal on the Mac itself, not over SSH."
        ));
    }
    if plan.sessions_open && !force {
        return Some(
            "a Claude Code session in the cc-rotate profile ran in the last 2 minutes. Quit your cc-rotate sessions first, or pass --force."
                .into(),
        );
    }
    None
}

fn print_plan(plan: &Plan) {
    let mut remove = vec!["Quit the cc-rotate app, if it runs".to_string()];
    let mut keep = Vec::new();
    if let Some(entry) = &plan.autostart {
        remove.push(format!("Stop it starting at login: {entry}"));
    }
    if let Some(entry) = &plan.autostart_foreign {
        keep.push(format!("{entry}: it does not start the cc-rotate app"));
    }
    for o in &plan.observe {
        match &o.restore {
            Restore::Remove => remove.push(format!(
                "Take observe mode out of {} (its status line goes)",
                o.settings.display()
            )),
            Restore::Command(command) => remove.push(format!(
                "Take observe mode out of {} (its status line runs `{command}` again)",
                o.settings.display()
            )),
            Restore::ByHand => keep.push(format!(
                "{}: its status line runs cc-rotate in observe mode in a way this can't take apart; change it by hand",
                o.settings.display()
            )),
        }
    }
    for file in &plan.legacy_alias {
        remove.push(format!(
            "Remove the old cc-rotate function from {}",
            file.display()
        ));
    }
    match &plan.keychain {
        KeychainItem::Present(service) => remove.push(format!(
            "Delete the macOS Keychain item \"{service}\" (the cc-rotate profile's login)"
        )),
        KeychainItem::Unreachable(service) => keep.push(format!(
            "The macOS Keychain item \"{service}\": this session can't reach the Keychain"
        )),
        KeychainItem::None => {}
    }
    for (what, removal) in [
        ("the cc-rotate profile", &plan.profile_dir),
        (
            "the data folder (the store, its key and every saved login)",
            &plan.data_dir,
        ),
    ] {
        match removal {
            Removal::Delete(dir) => remove.push(format!("Delete {what}: {}", dir.display())),
            Removal::Keep(dir, why) => keep.push(format!("{}: {why}", dir.display())),
            Removal::Absent => {}
        }
    }
    for program in &plan.programs {
        remove.push(format!("Delete {}", program.display()));
    }
    if let Some(why) = &plan.programs_kept {
        keep.push(format!("The programs: {why}"));
    }

    println!("This removes cc-rotate:");
    for line in &remove {
        println!("  - {line}");
    }
    if !keep.is_empty() {
        println!("It leaves alone:");
        for line in &keep {
            println!("  - {line}");
        }
    }
}
