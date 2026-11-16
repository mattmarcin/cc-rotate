//! What every test that runs cc-rotate's programs shares.

use std::process::Command;

/// A command for one of cc-rotate's built programs that never installs or removes the relay's
/// real service (`ccr_core::relay_service::NO_SERVICE_ENV`), whatever it runs.
pub fn program(path: &str) -> Command {
    let mut cmd = Command::new(path);
    cmd.env(ccr_core::relay_service::NO_SERVICE_ENV, "1");
    cmd
}
