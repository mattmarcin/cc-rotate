//! The per-user service that keeps the relay running (relay spec, section 10): a LaunchAgent on
//! macOS, a systemd user unit on Linux, a Task Scheduler task on Windows.
//!
//! Every command and every file goes through a `Service`, whose runner and folder tests replace,
//! so no test ever touches the real service manager or the real agents folder.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};

/// The LaunchAgent's label, and the name of its plist without `.plist`.
pub const LAUNCH_AGENT_LABEL: &str = "cc-rotate.relay";
/// The systemd user unit's name.
pub const SYSTEMD_UNIT: &str = "cc-rotate-relay.service";
/// The Task Scheduler task's name.
pub const TASK_NAME: &str = "cc-rotate-relay";

/// Whether the relay's service is installed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceState {
    Installed,
    Missing,
}

/// Runs the service manager's commands.
pub trait Runner: Send + Sync {
    /// Run `program` with `args`: `Ok(true)` when it exits with 0, `Ok(false)` when it exits
    /// otherwise, an error when it cannot be started.
    fn run(&self, program: &str, args: &[String]) -> Result<bool>;
}

/// Runs commands for real, their output thrown away.
pub struct SystemRunner;

impl Runner for SystemRunner {
    fn run(&self, program: &str, args: &[String]) -> Result<bool> {
        let status = std::process::Command::new(program)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .with_context(|| format!("running {program}"))?;
        Ok(status.success())
    }
}

/// Which service manager runs the relay.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// macOS: a LaunchAgent in the user's GUI domain.
    LaunchAgent { uid: u32 },
    /// Linux: a systemd user unit.
    Systemd,
    /// Windows: a Task Scheduler task.
    TaskScheduler,
}

/// The relay's service: where its definition goes and how commands run.
#[derive(Clone)]
pub struct Service {
    pub kind: Kind,
    /// The LaunchAgents folder, the systemd user unit folder, or on Windows a folder for the
    /// task's definition while it is created.
    pub dir: PathBuf,
    pub runner: Arc<dyn Runner>,
}

const LAUNCHCTL: &str = "/bin/launchctl";
const SYSTEMCTL: &str = "systemctl";
const SCHTASKS: &str = "schtasks";

/// The LaunchAgent's plist: `cc-rotate-ctl relay run`, started at login and again whenever it
/// ends, with `env` set.
pub fn launch_agent_plist(ctl: &Path, env: &[(&str, String)]) -> String {
    let mut out = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <plist version=\"1.0\">\n<dict>\n",
    );
    out.push_str(&format!(
        "\t<key>Label</key>\n\t<string>{LAUNCH_AGENT_LABEL}</string>\n"
    ));
    out.push_str("\t<key>ProgramArguments</key>\n\t<array>\n");
    for arg in [ctl.to_string_lossy().as_ref(), "relay", "run"] {
        out.push_str(&format!("\t\t<string>{}</string>\n", xml_escape(arg)));
    }
    out.push_str("\t</array>\n");
    if !env.is_empty() {
        out.push_str("\t<key>EnvironmentVariables</key>\n\t<dict>\n");
        for (name, value) in env {
            out.push_str(&format!(
                "\t\t<key>{}</key>\n\t\t<string>{}</string>\n",
                xml_escape(name),
                xml_escape(value)
            ));
        }
        out.push_str("\t</dict>\n");
    }
    out.push_str("\t<key>RunAtLoad</key>\n\t<true/>\n");
    out.push_str("\t<key>KeepAlive</key>\n\t<true/>\n");
    out.push_str("\t<key>ProcessType</key>\n\t<string>Background</string>\n");
    out.push_str("</dict>\n</plist>\n");
    out
}

/// The systemd user unit: `cc-rotate-ctl relay run`, started at login and again whenever it
/// ends, with `env` set.
pub fn systemd_unit(ctl: &Path, env: &[(&str, String)]) -> String {
    let mut out = String::from("[Unit]\nDescription=cc-rotate relay\n\n[Service]\n");
    out.push_str(&format!(
        "ExecStart={} relay run\n",
        systemd_quote(&ctl.to_string_lossy(), true)
    ));
    for (name, value) in env {
        out.push_str(&format!(
            "Environment={}\n",
            systemd_quote(&format!("{name}={value}"), false)
        ));
    }
    out.push_str("Restart=always\nRestartSec=2\n\n[Install]\nWantedBy=default.target\n");
    out
}

/// The Task Scheduler task: `cc-rotate-ctl relay run` at the user's logon, restarted every
/// minute after it fails.
/// A task cannot set environment variables, so on Windows the relay finds the store and profile
/// as other commands do.
pub fn task_xml(ctl: &Path) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-16\"?>\n\
         <Task version=\"1.2\" xmlns=\"http://schemas.microsoft.com/windows/2004/02/mit/task\">\n\
         \x20 <RegistrationInfo>\n    <Description>cc-rotate relay</Description>\n  </RegistrationInfo>\n\
         \x20 <Triggers>\n    <LogonTrigger>\n      <Enabled>true</Enabled>\n    </LogonTrigger>\n  </Triggers>\n\
         \x20 <Principals>\n    <Principal id=\"Author\">\n      <LogonType>InteractiveToken</LogonType>\n      <RunLevel>LeastPrivilege</RunLevel>\n    </Principal>\n  </Principals>\n\
         \x20 <Settings>\n\
         \x20   <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>\n\
         \x20   <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>\n\
         \x20   <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>\n\
         \x20   <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>\n\
         \x20   <RestartOnFailure>\n      <Interval>PT1M</Interval>\n      <Count>999</Count>\n    </RestartOnFailure>\n\
         \x20 </Settings>\n\
         \x20 <Actions Context=\"Author\">\n    <Exec>\n      <Command>{}</Command>\n      <Arguments>relay run</Arguments>\n    </Exec>\n  </Actions>\n\
         </Task>\n",
        xml_escape(&ctl.to_string_lossy())
    )
}

/// `schtasks` arguments that create the task from its definition in `xml`, replacing one there.
pub fn schtasks_args(xml: &Path) -> Vec<String> {
    ["/Create", "/F", "/TN", TASK_NAME, "/XML"]
        .iter()
        .map(|s| s.to_string())
        .chain(std::iter::once(xml.to_string_lossy().into_owned()))
        .collect()
}

/// The `CC_ROTATE_DATA_DIR` a service definition written by `install` sets, if any.
fn defined_data_dir(kind: Kind, text: &str) -> Option<String> {
    match kind {
        Kind::LaunchAgent { .. } => {
            let mut lines = text.lines();
            lines.find(|l| l.trim() == "<key>CC_ROTATE_DATA_DIR</key>")?;
            let value = lines
                .next()?
                .trim()
                .strip_prefix("<string>")?
                .strip_suffix("</string>")?;
            Some(xml_unescape(value))
        }
        Kind::Systemd => {
            let line = text
                .lines()
                .find_map(|l| l.strip_prefix("Environment=\"CC_ROTATE_DATA_DIR="))?;
            Some(systemd_unquote(line.strip_suffix('"')?))
        }
        Kind::TaskScheduler => None,
    }
}

fn xml_unescape(s: &str) -> String {
    s.replace("&quot;", "\"")
        .replace("&gt;", ">")
        .replace("&lt;", "<")
        .replace("&amp;", "&")
}

/// The inside of a word `systemd_quote` wrote for an `Environment` line.
fn systemd_unquote(s: &str) -> String {
    let mut out = String::new();
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => out.extend(chars.next()),
            '%' => {
                out.push('%');
                chars.next();
            }
            c => out.push(c),
        }
    }
    out
}

/// `p` as a full path: the service starts in another folder, where a relative path would point
/// elsewhere.
fn full_path(p: &Path) -> String {
    std::path::absolute(p)
        .unwrap_or_else(|_| p.to_path_buf())
        .to_string_lossy()
        .into_owned()
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// One word for a systemd unit line, in double quotes: `%` doubled (a specifier), and on an
/// `ExecStart` line `$` too (a variable).
fn systemd_quote(s: &str, exec: bool) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '%' => out.push_str("%%"),
            '$' if exec => out.push_str("$$"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// What the service needs to serve the store and profile `relay on` used: `CC_ROTATE_DATA_DIR`
/// and `CC_ROTATE_PROFILE` when set, the profile `--profile` named first.
pub fn service_env(profile_flag: Option<&Path>) -> Vec<(&'static str, String)> {
    service_env_from(
        std::env::var_os("CC_ROTATE_DATA_DIR"),
        std::env::var_os("CC_ROTATE_PROFILE"),
        profile_flag,
    )
}

fn service_env_from(
    data: Option<OsString>,
    profile: Option<OsString>,
    profile_flag: Option<&Path>,
) -> Vec<(&'static str, String)> {
    let mut env = Vec::new();
    if let Some(data) = data.filter(|v| !v.is_empty()) {
        env.push(("CC_ROTATE_DATA_DIR", full_path(Path::new(&data))));
    }
    let profile = profile_flag
        .map(Path::to_path_buf)
        .or_else(|| profile.filter(|v| !v.is_empty()).map(PathBuf::from));
    if let Some(profile) = profile {
        env.push(("CC_ROTATE_PROFILE", full_path(&profile)));
    }
    env
}

/// Install the relay's service for `ctl`, with the environment `relay on` runs in, and start it.
pub fn install(ctl: &Path) -> Result<()> {
    Service::real()?.install(ctl, &service_env(None))
}

/// Stop the relay's service and remove it.
pub fn remove() -> Result<()> {
    Service::real()?.remove()
}

/// Whether the relay's service is installed.
pub fn status() -> ServiceState {
    Service::real().map_or(ServiceState::Missing, |s| s.status())
}

impl Service {
    /// This user's service manager and folder.
    /// Never in this crate's tests: they use a service of their own.
    pub fn real() -> Result<Service> {
        if cfg!(test) {
            bail!("tests never touch the real service");
        }
        let runner: Arc<dyn Runner> = Arc::new(SystemRunner);
        if cfg!(target_os = "macos") {
            Ok(Service {
                kind: Kind::LaunchAgent { uid: uid() },
                dir: crate::paths::home_dir()?
                    .join("Library")
                    .join("LaunchAgents"),
                runner,
            })
        } else if cfg!(windows) {
            Ok(Service {
                kind: Kind::TaskScheduler,
                dir: crate::Paths::default_data_dir()?,
                runner,
            })
        } else {
            let config = dirs::config_dir().context("could not determine the config folder")?;
            Ok(Service {
                kind: Kind::Systemd,
                dir: config.join("systemd").join("user"),
                runner,
            })
        }
    }

    /// The service's definition file.
    pub fn file(&self) -> PathBuf {
        match self.kind {
            Kind::LaunchAgent { .. } => self.dir.join(format!("{LAUNCH_AGENT_LABEL}.plist")),
            Kind::Systemd => self.dir.join(SYSTEMD_UNIT),
            Kind::TaskScheduler => self.dir.join(format!("{TASK_NAME}.xml")),
        }
    }

    /// The service, for the user.
    pub fn describe(&self) -> String {
        match self.kind {
            Kind::LaunchAgent { .. } => {
                format!("the relay's LaunchAgent, {}", self.file().display())
            }
            Kind::Systemd => format!("the relay's systemd user unit, {}", self.file().display()),
            Kind::TaskScheduler => format!("the relay's scheduled task, {TASK_NAME}"),
        }
    }

    /// Write the service's definition for `ctl` with `env` and start it; a definition that
    /// changed replaces the running relay.
    pub fn install(&self, ctl: &Path, env: &[(&str, String)]) -> Result<()> {
        match self.kind {
            Kind::LaunchAgent { uid } => {
                let text = launch_agent_plist(ctl, env);
                let file = self.file();
                let target = format!("gui/{uid}/{LAUNCH_AGENT_LABEL}");
                let same = std::fs::read_to_string(&file).is_ok_and(|old| old == text);
                if same && self.run(LAUNCHCTL, &["print", &target])? {
                    return Ok(());
                }
                self.write(&file, text.as_bytes())?;
                // Unloaded first, so the new definition is the one loaded; fails when none is.
                self.run(LAUNCHCTL, &["bootout", &target])?;
                let domain = format!("gui/{uid}");
                let path = file.to_string_lossy();
                // launchd may still be unloading the old one for a moment.
                for attempt in 0..10 {
                    if self.run(LAUNCHCTL, &["bootstrap", &domain, &path])? {
                        return Ok(());
                    }
                    if attempt < 9 {
                        std::thread::sleep(Duration::from_millis(200));
                    }
                }
                bail!("launchctl bootstrap {domain} {path} failed")
            }
            Kind::Systemd => {
                let text = systemd_unit(ctl, env);
                let file = self.file();
                let changed = std::fs::read_to_string(&file).is_ok_and(|old| old != text);
                self.write(&file, text.as_bytes())?;
                self.must(SYSTEMCTL, &["--user", "daemon-reload"])?;
                self.must(SYSTEMCTL, &["--user", "enable", "--now", SYSTEMD_UNIT])?;
                if changed {
                    self.must(SYSTEMCTL, &["--user", "restart", SYSTEMD_UNIT])?;
                }
                Ok(())
            }
            Kind::TaskScheduler => {
                let file = self.file();
                // Task Scheduler reads its definitions as UTF-16.
                let mut bytes = vec![0xFF, 0xFE];
                for unit in task_xml(ctl).encode_utf16() {
                    bytes.extend_from_slice(&unit.to_le_bytes());
                }
                self.write(&file, &bytes)?;
                let args = schtasks_args(&file);
                let args: Vec<&str> = args.iter().map(String::as_str).collect();
                let created = self.must(SCHTASKS, &args);
                let _ = std::fs::remove_file(&file);
                created?;
                self.must(SCHTASKS, &["/Run", "/TN", TASK_NAME])
            }
        }
    }

    /// Stop the relay's service and remove it; nothing to do when it is not there.
    ///
    /// The relay calls this itself when it leaves (spec 10), and stopping the service ends the
    /// relay: so the definition goes first and the stop comes last.
    pub fn remove(&self) -> Result<()> {
        match self.kind {
            Kind::LaunchAgent { uid } => {
                self.delete(&self.file())?;
                // Fails when it is not loaded.
                self.run(
                    LAUNCHCTL,
                    &["bootout", &format!("gui/{uid}/{LAUNCH_AGENT_LABEL}")],
                )?;
                Ok(())
            }
            Kind::Systemd => {
                let file = self.file();
                if !file.exists() {
                    return Ok(());
                }
                // Each fails when there is nothing to do, which is fine.
                self.run(SYSTEMCTL, &["--user", "disable", SYSTEMD_UNIT])?;
                self.delete(&file)?;
                self.run(SYSTEMCTL, &["--user", "daemon-reload"])?;
                self.run(SYSTEMCTL, &["--user", "stop", SYSTEMD_UNIT])?;
                Ok(())
            }
            Kind::TaskScheduler => {
                if self.status() == ServiceState::Missing {
                    return Ok(());
                }
                self.must(SCHTASKS, &["/Delete", "/F", "/TN", TASK_NAME])?;
                // Deleting a task leaves its running instance: it fails when none runs.
                self.run(SCHTASKS, &["/End", "/TN", TASK_NAME])?;
                Ok(())
            }
        }
    }

    /// Whether the installed service serves the data folder `data_dir` names (the value of
    /// `CC_ROTATE_DATA_DIR` where the relay runs, `None` for the default folder), so a relay of
    /// another data folder (a test's) never removes it.
    /// A Task Scheduler task carries no environment: it serves the default folder.
    pub fn serves(&self, data_dir: Option<&str>) -> bool {
        let defined = match self.kind {
            Kind::TaskScheduler => None,
            Kind::LaunchAgent { .. } | Kind::Systemd => {
                let Ok(text) = std::fs::read_to_string(self.file()) else {
                    return false;
                };
                defined_data_dir(self.kind, &text)
            }
        };
        let ours = data_dir
            .filter(|d| !d.is_empty())
            .map(|d| full_path(Path::new(d)));
        defined == ours
    }

    /// Whether the service is installed: its definition is there.
    pub fn status(&self) -> ServiceState {
        let installed = match self.kind {
            Kind::LaunchAgent { .. } | Kind::Systemd => self.file().exists(),
            Kind::TaskScheduler => self
                .run(SCHTASKS, &["/Query", "/TN", TASK_NAME])
                .unwrap_or(false),
        };
        if installed {
            ServiceState::Installed
        } else {
            ServiceState::Missing
        }
    }

    fn run(&self, program: &str, args: &[&str]) -> Result<bool> {
        let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        self.runner.run(program, &args)
    }

    /// Run a command that must succeed.
    fn must(&self, program: &str, args: &[&str]) -> Result<()> {
        if self.run(program, args)? {
            Ok(())
        } else {
            bail!("{program} {} failed", args.join(" "))
        }
    }

    fn write(&self, file: &Path, bytes: &[u8]) -> Result<()> {
        std::fs::create_dir_all(&self.dir)
            .with_context(|| format!("creating {}", self.dir.display()))?;
        crate::credfile::atomic_write(file, bytes)
            .with_context(|| format!("writing {}", file.display()))
    }

    fn delete(&self, file: &Path) -> Result<()> {
        match std::fs::remove_file(file) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e).with_context(|| format!("deleting {}", file.display())),
        }
    }
}

#[cfg(unix)]
fn uid() -> u32 {
    // SAFETY: getuid cannot fail.
    unsafe { libc::getuid() }
}

#[cfg(not(unix))]
fn uid() -> u32 {
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Records every command and answers with `ok` unless a command starts with one of `fail`.
    #[derive(Default)]
    struct Fake {
        calls: Mutex<Vec<String>>,
        fail: Mutex<Vec<String>>,
    }

    impl Fake {
        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
        fn failing(&self, prefix: &str) {
            self.fail.lock().unwrap().push(prefix.to_string());
        }
    }

    impl Runner for Fake {
        fn run(&self, program: &str, args: &[String]) -> Result<bool> {
            let line = std::iter::once(program.to_string())
                .chain(args.iter().cloned())
                .collect::<Vec<_>>()
                .join(" ");
            let ok = !self
                .fail
                .lock()
                .unwrap()
                .iter()
                .any(|p| line.starts_with(p.as_str()));
            self.calls.lock().unwrap().push(line);
            Ok(ok)
        }
    }

    fn service(kind: Kind) -> (tempfile::TempDir, Service, Arc<Fake>) {
        let root = tempfile::tempdir().unwrap();
        let fake = Arc::new(Fake::default());
        let service = Service {
            kind,
            dir: root.path().join("agents"),
            runner: fake.clone(),
        };
        (root, service, fake)
    }

    fn env() -> Vec<(&'static str, String)> {
        vec![("CC_ROTATE_DATA_DIR", "/data & <more>".to_string())]
    }

    #[test]
    fn plist_text() {
        let ctl = Path::new("/Users/me/.local/bin/cc-rotate-ctl");
        let text = launch_agent_plist(ctl, &env());
        assert!(text.starts_with("<?xml"), "{text}");
        assert!(
            text.contains("<key>Label</key>\n\t<string>cc-rotate.relay</string>"),
            "{text}"
        );
        assert!(
            text.contains(
                "<key>ProgramArguments</key>\n\t<array>\n\t\t<string>/Users/me/.local/bin/cc-rotate-ctl</string>\n\t\t<string>relay</string>\n\t\t<string>run</string>\n\t</array>"
            ),
            "{text}"
        );
        assert!(text.contains("<key>RunAtLoad</key>\n\t<true/>"), "{text}");
        assert!(text.contains("<key>KeepAlive</key>\n\t<true/>"), "{text}");
        assert!(
            text.contains(
                "<key>EnvironmentVariables</key>\n\t<dict>\n\t\t<key>CC_ROTATE_DATA_DIR</key>\n\t\t<string>/data &amp; &lt;more&gt;</string>\n\t</dict>"
            ),
            "{text}"
        );
        let bare = launch_agent_plist(ctl, &[]);
        assert!(!bare.contains("EnvironmentVariables"), "{bare}");
    }

    #[test]
    fn unit_text() {
        let ctl = Path::new("/home/me/my tools/cc-rotate-ctl");
        let text = systemd_unit(ctl, &env());
        assert!(text.contains("\n[Service]\n"), "{text}");
        assert!(
            text.contains("\nExecStart=\"/home/me/my tools/cc-rotate-ctl\" relay run\n"),
            "{text}"
        );
        assert!(text.contains("\nRestart=always\n"), "{text}");
        assert!(
            text.contains("\nEnvironment=\"CC_ROTATE_DATA_DIR=/data & <more>\"\n"),
            "{text}"
        );
        assert!(
            text.contains("\n[Install]\nWantedBy=default.target\n"),
            "{text}"
        );
        // systemd expands `%` and `$`, and a quote or backslash ends or escapes the word.
        let odd = systemd_unit(Path::new("/opt/a%b$c\"d\\e/cc-rotate-ctl"), &[]);
        assert!(
            odd.contains("\nExecStart=\"/opt/a%%b$$c\\\"d\\\\e/cc-rotate-ctl\" relay run\n"),
            "{odd}"
        );
    }

    #[test]
    fn task_text() {
        let ctl = Path::new(r"C:\Users\me\bin\cc-rotate-ctl.exe");
        let xml = task_xml(ctl);
        assert!(xml.contains("<LogonTrigger>"), "{xml}");
        assert!(
            xml.contains("<RestartOnFailure>\n      <Interval>PT1M</Interval>"),
            "{xml}"
        );
        assert!(
            xml.contains(r"<Command>C:\Users\me\bin\cc-rotate-ctl.exe</Command>"),
            "{xml}"
        );
        assert!(xml.contains("<Arguments>relay run</Arguments>"), "{xml}");
        assert_eq!(
            schtasks_args(Path::new(r"C:\t\task.xml")),
            [
                "/Create",
                "/F",
                "/TN",
                "cc-rotate-relay",
                "/XML",
                r"C:\t\task.xml"
            ]
        );
    }

    #[test]
    fn launch_agent_install_and_remove() {
        let (_root, service, fake) = service(Kind::LaunchAgent { uid: 501 });
        let ctl = Path::new("/bin/cc-rotate-ctl");
        assert_eq!(service.status(), ServiceState::Missing);
        // Not loaded yet: `print` fails.
        fake.failing("/bin/launchctl print");
        service.install(ctl, &env()).unwrap();
        let plist = service.dir.join("cc-rotate.relay.plist");
        assert_eq!(
            std::fs::read_to_string(&plist).unwrap(),
            launch_agent_plist(ctl, &env())
        );
        assert_eq!(service.status(), ServiceState::Installed);
        let path = plist.display().to_string();
        assert_eq!(
            fake.calls(),
            [
                "/bin/launchctl bootout gui/501/cc-rotate.relay".to_string(),
                format!("/bin/launchctl bootstrap gui/501 {path}"),
            ]
        );

        // The same definition, loaded: nothing to do.
        fake.fail.lock().unwrap().clear();
        fake.calls.lock().unwrap().clear();
        service.install(ctl, &env()).unwrap();
        assert_eq!(
            fake.calls(),
            ["/bin/launchctl print gui/501/cc-rotate.relay"]
        );

        // The file goes first, so the service is gone even if unloading ends this process.
        fake.calls.lock().unwrap().clear();
        service.remove().unwrap();
        assert!(!plist.exists());
        assert_eq!(service.status(), ServiceState::Missing);
        assert_eq!(
            fake.calls(),
            ["/bin/launchctl bootout gui/501/cc-rotate.relay"]
        );
        // Removing what is not there is fine.
        service.remove().unwrap();
    }

    #[test]
    fn launch_agent_install_fails_when_launchd_refuses() {
        let (_root, service, fake) = service(Kind::LaunchAgent { uid: 501 });
        fake.failing("/bin/launchctl bootstrap");
        let err = service
            .install(Path::new("/bin/cc-rotate-ctl"), &[])
            .unwrap_err();
        assert!(format!("{err:#}").contains("launchctl"), "{err:#}");
    }

    #[test]
    fn systemd_install_and_remove() {
        let (_root, service, fake) = service(Kind::Systemd);
        let ctl = Path::new("/bin/cc-rotate-ctl");
        service.install(ctl, &env()).unwrap();
        let unit = service.dir.join("cc-rotate-relay.service");
        assert_eq!(
            std::fs::read_to_string(&unit).unwrap(),
            systemd_unit(ctl, &env())
        );
        assert_eq!(service.status(), ServiceState::Installed);
        assert_eq!(
            fake.calls(),
            [
                "systemctl --user daemon-reload",
                "systemctl --user enable --now cc-rotate-relay.service",
            ]
        );

        // A changed definition restarts the relay on it.
        fake.calls.lock().unwrap().clear();
        service
            .install(Path::new("/new/cc-rotate-ctl"), &env())
            .unwrap();
        assert_eq!(
            fake.calls(),
            [
                "systemctl --user daemon-reload",
                "systemctl --user enable --now cc-rotate-relay.service",
                "systemctl --user restart cc-rotate-relay.service",
            ]
        );

        fake.calls.lock().unwrap().clear();
        service.remove().unwrap();
        assert!(!unit.exists());
        assert_eq!(
            fake.calls(),
            [
                "systemctl --user disable cc-rotate-relay.service",
                "systemctl --user daemon-reload",
                "systemctl --user stop cc-rotate-relay.service",
            ]
        );
    }

    #[test]
    fn systemd_install_fails_when_systemd_refuses() {
        let (_root, service, fake) = service(Kind::Systemd);
        fake.failing("systemctl --user enable");
        assert!(
            service
                .install(Path::new("/bin/cc-rotate-ctl"), &[])
                .is_err()
        );
    }

    #[test]
    fn task_install_and_remove() {
        let (_root, service, fake) = service(Kind::TaskScheduler);
        fake.failing("schtasks /Query");
        assert_eq!(service.status(), ServiceState::Missing);
        fake.fail.lock().unwrap().clear();
        fake.calls.lock().unwrap().clear();
        service
            .install(Path::new(r"C:\bin\cc-rotate-ctl.exe"), &[])
            .unwrap();
        let xml = service
            .dir
            .join("cc-rotate-relay.xml")
            .display()
            .to_string();
        assert_eq!(
            fake.calls(),
            [
                format!("schtasks /Create /F /TN cc-rotate-relay /XML {xml}"),
                "schtasks /Run /TN cc-rotate-relay".to_string(),
            ]
        );
        assert!(!service.dir.join("cc-rotate-relay.xml").exists());
        assert_eq!(service.status(), ServiceState::Installed);
        fake.calls.lock().unwrap().clear();
        service.remove().unwrap();
        assert_eq!(
            fake.calls(),
            [
                "schtasks /Query /TN cc-rotate-relay",
                "schtasks /Delete /F /TN cc-rotate-relay",
                "schtasks /End /TN cc-rotate-relay",
            ]
        );
    }

    #[test]
    fn a_service_serves_the_data_folder_it_names() {
        for kind in [Kind::LaunchAgent { uid: 501 }, Kind::Systemd] {
            let (_root, service, _) = service(kind);
            assert!(!service.serves(None), "nothing installed");
            let ctl = Path::new("/bin/cc-rotate-ctl");
            service.install(ctl, &[]).unwrap();
            assert!(service.serves(None));
            assert!(service.serves(Some("")));
            assert!(!service.serves(Some("/tmp/test-data")));
            let odd = "/data & <%more \"x\">";
            service
                .install(ctl, &[("CC_ROTATE_DATA_DIR", odd.to_string())])
                .unwrap();
            assert!(service.serves(Some(odd)), "{kind:?}");
            assert!(!service.serves(None));
            assert!(!service.serves(Some("/data")));
        }
        let (_root, task, _) = service(Kind::TaskScheduler);
        assert!(task.serves(None));
        assert!(!task.serves(Some("/tmp/test-data")));
    }

    #[test]
    fn the_real_service_is_never_used_in_tests() {
        assert!(Service::real().is_err());
        assert!(install(Path::new("/bin/cc-rotate-ctl")).is_err());
        assert!(remove().is_err());
        assert_eq!(status(), ServiceState::Missing);
    }

    #[test]
    fn env_names_the_store_in_use() {
        let env = service_env_from(Some("data".into()), None, None);
        assert_eq!(env.len(), 1);
        assert_eq!(env[0].0, "CC_ROTATE_DATA_DIR");
        assert!(Path::new(&env[0].1).is_absolute(), "{env:?}");
        assert!(env[0].1.ends_with("data"), "{env:?}");
        let env = service_env_from(
            None,
            Some("/env/profile".into()),
            Some(Path::new("/flag/profile")),
        );
        assert_eq!(env, [("CC_ROTATE_PROFILE", "/flag/profile".to_string())]);
        let env = service_env_from(Some("".into()), Some("/env/profile".into()), None);
        assert_eq!(env, [("CC_ROTATE_PROFILE", "/env/profile".to_string())]);
    }
}
