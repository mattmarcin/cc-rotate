//! The in-app login (spec section 10): run `claude auth login` with piped stdio, report its
//! progress to the UI, accept a pasted code, and import the result.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::process::{ChildStdin, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use ccr_core::login::{self, LoginRequest, Staging};
use ccr_core::process::Tree;
use ccr_core::store::Account;
use serde::Serialize;
use tauri::{AppHandle, Emitter, State};

use crate::commands::{CmdResult, with_store};

const TIMEOUT: Duration = Duration::from_secs(10 * 60);
pub const EVENT: &str = "login-progress";

struct Running {
    /// `claude auth login` and whatever it starts (an npm shim's `node`), so stopping it stops
    /// them all; on Windows they also end with the app.
    child: Arc<Mutex<Tree>>,
    stdin: Option<ChildStdin>,
    cancelled: Arc<AtomicBool>,
    /// The sign-in page Claude Code announced, the only page `open_login_page` opens.
    url: Arc<Mutex<Option<String>>>,
}

/// How long a line Claude Code writes on stderr waits before it is shown: a line it ends the
/// sign-in with shows with the exit instead, not as something to fix and send again.
const NOTICE_GRACE: Duration = Duration::from_millis(400);
/// How long a failed sign-in waits for the rest of Claude Code's output before saying why.
const DRAIN: Duration = Duration::from_secs(1);

#[derive(Default)]
pub struct Logins(Mutex<HashMap<String, Running>>);

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Stage {
    Starting,
    Browser,
    /// Something Claude Code said on stderr while waiting, such as a pasted code it rejected
    /// ("Invalid code. ..."); it keeps waiting for another.
    Notice,
    Importing,
    Done,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Serialize)]
pub struct Progress {
    id: String,
    stage: Stage,
    url: Option<String>,
    message: Option<String>,
    account: Option<Account>,
    activated: bool,
}

impl Progress {
    fn new(id: &str, stage: Stage) -> Self {
        Self {
            id: id.to_string(),
            stage,
            url: None,
            message: None,
            account: None,
            activated: false,
        }
    }
}

fn emit(app: &AppHandle, p: Progress) {
    let _ = app.emit(EVENT, p);
}

fn strip_ansi(text: &str) -> String {
    static ANSI: OnceLock<regex::Regex> = OnceLock::new();
    let re = ANSI.get_or_init(|| regex::Regex::new(r"\x1b\[[0-9;?]*[ -/]*[@-~]").unwrap());
    re.replace_all(text, "").into_owned()
}

#[tauri::command]
pub fn start_login(
    app: AppHandle,
    logins: State<'_, Logins>,
    email: Option<String>,
    label: Option<String>,
    relogin: Option<i64>,
    expect_email: Option<String>,
) -> CmdResult<String> {
    let (claude, staging, email) = with_store(|store, paths| {
        let target = relogin.map(|id| store.account(id)).transpose()?.flatten();
        let email = email
            .filter(|e| !e.trim().is_empty())
            .or_else(|| target.and_then(|a| a.email));
        Ok((
            login::claude_executable(store)?,
            Staging::create(paths)?,
            email,
        ))
    })?;

    let mut cmd = login::login_command(&claude, staging.dir(), email.as_deref());
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    let mut child = Tree::spawn(&mut cmd).map_err(|e| {
        format!("Couldn't start Claude Code's sign-in: {e}. Check that Claude Code is installed.")
    })?;
    let stdout = child.child_mut().stdout.take();
    let stderr = child.child_mut().stderr.take();
    let stdin = child.child_mut().stdin.take();
    let child = Arc::new(Mutex::new(child));
    let cancelled = Arc::new(AtomicBool::new(false));
    let url = Arc::new(Mutex::new(None));

    let id = format!("login-{}", ccr_core::time::now_ms());
    logins.0.lock().unwrap().insert(
        id.clone(),
        Running {
            child: child.clone(),
            stdin,
            cancelled: cancelled.clone(),
            url: url.clone(),
        },
    );
    emit(&app, Progress::new(&id, Stage::Starting));

    let output = Arc::new(Mutex::new(Vec::new()));
    let mut readers = Vec::new();
    for (pipe, is_stderr) in [
        (stdout.map(|p| Box::new(p) as Box<dyn Read + Send>), false),
        (stderr.map(|p| Box::new(p) as Box<dyn Read + Send>), true),
    ] {
        let Some(pipe) = pipe else { continue };
        let reader = Reader {
            is_stderr,
            output: output.clone(),
            url: url.clone(),
            child: child.clone(),
            app: app.clone(),
            id: id.clone(),
        };
        readers.push(std::thread::spawn(move || reader.read(pipe)));
    }

    let request = LoginRequest {
        email,
        label: label.filter(|l| !l.trim().is_empty()),
        relogin,
        expect_email: expect_email.filter(|e| !e.trim().is_empty()),
    };
    let app2 = app.clone();
    let id2 = id.clone();
    std::thread::spawn(move || {
        let mut progress =
            wait_and_import(&app2, &child, staging, &request, &output, readers, &id2);
        if cancelled.load(Ordering::Acquire) {
            progress = Progress::new(&id2, Stage::Cancelled);
        }
        if let Some(state) = tauri::Manager::try_state::<Logins>(&app2) {
            state.0.lock().unwrap().remove(&id2);
        }
        emit(&app2, progress);
    });
    Ok(id)
}

/// Reads one of Claude Code's pipes into `output`. Claude Code prints the sign-in page on stdout,
/// which announces it; after that, what it says on stderr while still running is a notice.
struct Reader {
    is_stderr: bool,
    output: Arc<Mutex<Vec<u8>>>,
    url: Arc<Mutex<Option<String>>>,
    child: Arc<Mutex<Tree>>,
    app: AppHandle,
    id: String,
}

impl Reader {
    fn read(self, mut pipe: Box<dyn Read + Send>) {
        let mut buf = [0u8; 4096];
        // This pipe's bytes not yet used: stdout's until the page is found, stderr's since its
        // last full line. Bytes, so a character split between reads stays whole.
        let mut pending = Vec::new();
        let mut announced = false;
        while let Ok(n) = pipe.read(&mut buf) {
            if n == 0 {
                break;
            }
            self.output.lock().unwrap().extend_from_slice(&buf[..n]);
            if self.is_stderr {
                pending.extend_from_slice(&buf[..n]);
                self.notice(&mut pending);
            } else if !announced {
                pending.extend_from_slice(&buf[..n]);
                let text = strip_ansi(&String::from_utf8_lossy(&pending));
                if let Some(found) = login::find_sign_in_url(&text) {
                    announced = true;
                    pending = Vec::new();
                    *self.url.lock().unwrap() = Some(found.clone());
                    let mut p = Progress::new(&self.id, Stage::Browser);
                    p.url = Some(found);
                    emit(&self.app, p);
                }
            }
        }
    }

    /// Pass on stderr's full lines, if the page is up and Claude Code is still waiting.
    fn notice(&self, pending: &mut Vec<u8>) {
        let Some(end) = pending.iter().rposition(|&b| b == b'\n') else {
            return;
        };
        let lines: Vec<u8> = pending.drain(..=end).collect();
        let said = strip_ansi(&String::from_utf8_lossy(&lines))
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .collect::<Vec<_>>()
            .join(" ");
        // Before the page is up, a failure shows with the exit instead.
        if said.is_empty() || self.url.lock().unwrap().is_none() {
            return;
        }
        std::thread::sleep(NOTICE_GRACE);
        if !matches!(self.child.lock().unwrap().try_wait(), Ok(None)) {
            return;
        }
        let mut p = Progress::new(&self.id, Stage::Notice);
        p.message = Some(said);
        emit(&self.app, p);
    }
}

fn wait_and_import(
    app: &AppHandle,
    child: &Mutex<Tree>,
    staging: Staging,
    request: &LoginRequest,
    output: &Mutex<Vec<u8>>,
    readers: Vec<JoinHandle<()>>,
    id: &str,
) -> Progress {
    let started = Instant::now();
    let status = loop {
        // Not in the match itself: the lock would be held through its arms and the sleep.
        let polled = child.lock().unwrap().try_wait();
        match polled {
            Ok(Some(status)) => break Some(status),
            Ok(None) if started.elapsed() > TIMEOUT => {
                child.lock().unwrap().kill();
                let mut p = Progress::new(id, Stage::Failed);
                p.message =
                    Some("Sign-in timed out after 10 minutes. Try again when you're ready.".into());
                return p;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(200)),
            Err(_) => break None,
        }
    };
    match status {
        Some(s) if s.success() => {}
        other => {
            // Its last words may still be in a pipe; the pipes stay open while anything Claude
            // Code started holds them, so this waits only so long.
            let deadline = Instant::now() + DRAIN;
            while readers.iter().any(|r| !r.is_finished()) && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(20));
            }
            let tail = strip_ansi(&String::from_utf8_lossy(&output.lock().unwrap()));
            let last = tail
                .lines()
                .rev()
                .find(|l| !l.trim().is_empty())
                .unwrap_or("")
                .trim()
                .to_string();
            let mut p = Progress::new(id, Stage::Failed);
            p.message = Some(match other {
                Some(s) if !last.is_empty() => {
                    format!("Claude Code ended the sign-in ({s}): {last}")
                }
                Some(s) => format!("Claude Code ended the sign-in ({s})."),
                None => "Lost track of the sign-in. Try again.".into(),
            });
            return p;
        }
    }

    emit(app, Progress::new(id, Stage::Importing));
    match with_store(|store, paths| login::import(store, paths, staging.dir(), request)) {
        Ok(imported) => {
            let mut p = Progress::new(id, Stage::Done);
            p.activated = imported.activated;
            p.account = Some(imported.account);
            p
        }
        Err(message) => {
            let mut p = Progress::new(id, Stage::Failed);
            p.message = Some(message);
            p
        }
    }
}

#[tauri::command]
pub fn submit_login_code(logins: State<'_, Logins>, id: String, code: String) -> CmdResult<()> {
    let mut map = logins.0.lock().unwrap();
    let running = map
        .get_mut(&id)
        .ok_or("That sign-in has already finished.")?;
    let stdin = running
        .stdin
        .as_mut()
        .ok_or("This sign-in isn't waiting for a code.")?;
    let line = login::code_line(&code, running.url.lock().unwrap().as_deref())
        .ok_or("That doesn't look like a sign-in code. Copy it again from the browser.")?;
    writeln!(stdin, "{line}").map_err(|e| e.to_string())?;
    stdin.flush().map_err(|e| e.to_string())
}

/// Open a running sign-in's page again: only the one Claude Code announced for it.
#[tauri::command]
pub fn open_login_page(app: AppHandle, logins: State<'_, Logins>, id: String) -> CmdResult<()> {
    let url = logins
        .0
        .lock()
        .unwrap()
        .get(&id)
        .and_then(|r| r.url.lock().unwrap().clone())
        .ok_or("That sign-in has already finished.")?;
    tauri_plugin_opener::OpenerExt::opener(&app)
        .open_url(url, None::<&str>)
        .map_err(|e| e.to_string())
}

/// End every running sign-in as the app exits, so none can still finish in the browser and
/// leave a login behind with no one to import or delete it. Their staging folders are swept
/// on the next start (`usage_check::sweep_logins`).
pub fn stop_all(app: &AppHandle) {
    let Some(logins) = tauri::Manager::try_state::<Logins>(app) else {
        return;
    };
    for running in logins.0.lock().unwrap().values() {
        running.cancelled.store(true, Ordering::Release);
        running.child.lock().unwrap().kill();
    }
}

/// Stop a running sign-in. The waiting thread reports `cancelled` once the process exits.
#[tauri::command]
pub fn cancel_login(logins: State<'_, Logins>, id: String) -> CmdResult<()> {
    if let Some(running) = logins.0.lock().unwrap().get(&id) {
        running.cancelled.store(true, Ordering::Release);
        running.child.lock().unwrap().kill();
    }
    Ok(())
}
