//! `cc-rotate-ctl uninstall`: show what removing cc-rotate does, ask, then do it
//! (`ccr_core::uninstall`).

use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use ccr_core::uninstall::{
    self, DataRemoval, KeychainItem, Plan, RealSystem, Removal, Restore, Sessions,
};
use ccr_core::{Paths, Store, time};

/// What the user types to go ahead.
const CONFIRM: &str = "uninstall";

pub fn run(profile: Option<PathBuf>, dry_run: bool, yes: bool, force: bool) -> Result<ExitCode> {
    let data_dir = Paths::default_data_dir()?;
    let store_path = data_dir.join("store.db");
    // Read-only: nothing changes before the user says so. A store that can't be read (another
    // version's, damaged) only leaves open sessions unknown.
    let store = if store_path.exists() {
        match Store::open_read_only(&store_path) {
            Ok(store) => Some(store),
            Err(e) => {
                eprintln!(
                    "warning: couldn't read the store, so open sessions and a profile set with `config profile_dir` can't be told ({e:#})"
                );
                None
            }
        }
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
    if out.programs_kept {
        println!(
            "The programs stay, so run cc-rotate-ctl uninstall again once the errors above are fixed."
        );
    }
    if let Some(dir) = &out.data_left {
        println!(
            "{} held more than cc-rotate's files, so it stays with the rest.",
            dir.display()
        );
    }
    if !out.deleted_later.is_empty() {
        println!("Windows lets these go only once this command ends; they are deleted then:");
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
        println!("Change the status line in these by hand, since cc-rotate-ctl is gone:");
        for o in by_hand {
            println!("  {}: {}", o.settings.display(), o.command);
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
    match plan.sessions {
        Sessions::Open if !force => Some(
            "a Claude Code session in the cc-rotate profile ran in the last 2 minutes. Quit your cc-rotate sessions first, or pass --force."
                .into(),
        ),
        Sessions::Unknown if !force => Some(
            "the store couldn't be read to tell whether cc-rotate sessions are open. Quit them, then pass --force."
                .into(),
        ),
        _ => None,
    }
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
        let settings = o.settings.display();
        match &o.restore {
            Restore::Remove => remove.push(format!(
                "Take observe mode out of {settings} (its status line goes)"
            )),
            Restore::Command(command) => remove.push(format!(
                "Take observe mode out of {settings} (its status line runs `{command}` again)"
            )),
            Restore::ByHand => keep.push(format!(
                "{settings}: its status line runs more than cc-rotate's observe mode (`{}`); change it by hand",
                o.command
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
        KeychainItem::Present(service) if plan.deletes_keychain_item() => remove.push(format!(
            "Delete the macOS Keychain item \"{service}\" (the cc-rotate profile's login)"
        )),
        KeychainItem::Present(service) => keep.push(format!(
            "The macOS Keychain item \"{service}\": it goes with the profile, which stays"
        )),
        KeychainItem::Unreachable(service) => keep.push(format!(
            "The macOS Keychain item \"{service}\": this session can't reach the Keychain"
        )),
        KeychainItem::None => {}
    }
    if !plan.staging_items.is_empty() {
        remove.push(format!(
            "Delete the macOS Keychain items of {} leftover sign-in or usage-check folders",
            plan.staging_items.len()
        ));
    }
    match &plan.profile_dir {
        Removal::Delete(dir) => remove.push(format!(
            "Delete the cc-rotate profile, with the session history and memory Claude Code keeps there (copy its projects folder first to keep them): {}",
            dir.display()
        )),
        Removal::Keep(dir, why) => keep.push(format!("{}: {why}", dir.display())),
        Removal::Absent => {}
    }
    let saved = "the store, its key and every saved login";
    match &plan.data_dir {
        DataRemoval::Whole(dir) => remove.push(format!(
            "Delete the data folder ({saved}): {}",
            dir.display()
        )),
        DataRemoval::Entries(dir, entries) => {
            remove.push(format!(
                "Delete cc-rotate's files from {} ({saved}), and the folder if nothing else is in it:",
                dir.display()
            ));
            for entry in entries {
                remove.push(format!(
                    "  {}",
                    entry.file_name().unwrap_or_default().to_string_lossy()
                ));
            }
        }
        DataRemoval::Keep(dir, why) => keep.push(format!("{}: {why}", dir.display())),
        DataRemoval::Absent => {}
    }
    for dir in &plan.app_data {
        remove.push(format!("Delete the app's webview data: {}", dir.display()));
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
