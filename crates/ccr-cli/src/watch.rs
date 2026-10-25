//! `cc-rotate-ctl watch`: `status` redrawn in place every few seconds, for SSH sessions and
//! terminal panes where there's no app or tray.
//!
//! Read-only: every frame comes from `snapshot::snapshot` and the events table, which only read
//! the store. Unlike `status`, it never syncs the profile's login, swaps, settles a login or
//! starts Claude Code; the status line and the app keep the store current.
//!
//! Drawing uses crossterm, which handles raw mode, keys, the terminal size and the alternate
//! screen the same way on Windows consoles and Unix terminals (including over SSH). Each frame
//! moves the cursor home and overwrites the previous one line by line, clearing only what's
//! left over, so nothing flickers. The terminal is put back however it ends: q, Esc, Ctrl-C, an
//! error, a panic (before the panic message is printed, so it stays readable), and on Unix
//! SIGTERM, SIGHUP and SIGQUIT, after which the process ends as the signal would have ended it.

use std::io::{ErrorKind, IsTerminal, Write};
use std::panic::PanicHookInfo;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use anyhow::Result;
use ccr_core::{Paths, Store, snapshot, time};
use crossterm::cursor::{Hide, MoveTo, Show};
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::terminal::{self, Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen};
use crossterm::{Command, execute};
use unicode_width::UnicodeWidthChar;

use crate::format;

const DIM: &str = "\x1b[2m";
const RESET: &str = "\x1b[0m";

/// The most events shown below the accounts.
const MAX_EVENTS: usize = 8;

/// How often the wait between frames checks for a termination signal.
const STOP_CHECK: Duration = Duration::from_millis(100);

pub fn run(store: &Store, paths: &Paths, interval: Duration) -> Result<()> {
    let mut out = std::io::stdout();
    // Piped or redirected: one frame, no cursor movement. That includes input that isn't a
    // terminal (`watch < /dev/null`): keys would then come from /dev/tty, which crossterm can't
    // poll on macOS, so one frame is the same on every platform and never hangs.
    if !out.is_terminal() || !std::io::stdin().is_terminal() {
        out.write_all(frame(store, paths, interval, false, None)?.as_bytes())?;
        return Ok(());
    }
    let stop = Stop::register()?;
    let outcome = live(store, paths, interval, &stop);
    // The terminal is restored by now: end the way a termination signal asked to, if one did.
    stop.finish();
    outcome
}

/// Redraw until q, Esc, Ctrl-C or a termination signal. The terminal is restored when it
/// returns, whichever way.
fn live(store: &Store, paths: &Paths, interval: Duration, stop: &Stop) -> Result<()> {
    let _terminal = TerminalGuard::enter()?;
    let mut out = std::io::stdout();
    loop {
        // Some terminals (a serial console, an SSH client that sends no size) report 0 x 0.
        let (cols, rows) = match terminal::size() {
            Ok((cols, rows)) if cols > 0 && rows > 0 => (cols, rows),
            _ => (80, 24),
        };
        // A frame that can't be built (the store busy for too long) says why and tries again.
        let text = frame(store, paths, interval, true, Some(rows as usize)).unwrap_or_else(|e| {
            format!(
                "{}\n\nCouldn't read cc-rotate's data: {e:#}\n",
                header(interval, true)
            )
        });
        draw(&mut out, &text, cols, rows)?;
        if wait(interval, stop)? == Wake::Quit {
            return Ok(());
        }
    }
}

/// The header line: when the frame was built, and how to leave.
fn header(interval: Duration, live: bool) -> String {
    let at = time::local(time::now_s(), "%H:%M:%S");
    if live {
        format!(
            "{DIM}cc-rotate  Updated {at}. Refreshes every {}. Press q to quit.{RESET}",
            every(interval)
        )
    } else {
        format!("{DIM}cc-rotate  Updated {at}.{RESET}")
    }
}

/// "2s", "1m 30s", "1h".
fn every(interval: Duration) -> String {
    let s = interval.as_secs();
    let parts: Vec<String> = [(s / 3600, "h"), (s % 3600 / 60, "m"), (s % 60, "s")]
        .into_iter()
        .filter(|&(n, _)| n > 0)
        .map(|(n, unit)| format!("{n}{unit}"))
        .collect();
    if parts.is_empty() {
        "0s".into()
    } else {
        parts.join(" ")
    }
}

/// One screen: the header, `status`, and as many of the latest events as fit in `rows`.
fn frame(
    store: &Store,
    paths: &Paths,
    interval: Duration,
    live: bool,
    rows: Option<usize>,
) -> Result<String> {
    let snap = snapshot::snapshot(store, paths)?;
    let mut text = format!("{}\n\n{}", header(interval, live), format::status(&snap));
    if snap.accounts.is_empty() {
        return Ok(text);
    }
    // The heading and a blank line above it.
    let room = rows.map_or(MAX_EVENTS, |r| r.saturating_sub(text.lines().count() + 2));
    let events = snapshot::recent_events(store, room.min(MAX_EVENTS))?;
    if !events.is_empty() {
        text.push_str("\nActivity\n");
        text.push_str(&format::events(store, &events));
    }
    Ok(text)
}

/// Overwrite the screen with `text` from the top-left corner: each line is cut to the width and
/// its leftovers cleared, then everything below the last line. Lines past the bottom are left
/// out, so the screen never scrolls. One write per frame.
fn draw(out: &mut impl Write, text: &str, cols: u16, rows: u16) -> Result<()> {
    out.write_all(render(text, cols, rows)?.as_bytes())?;
    out.flush()?;
    Ok(())
}

/// The bytes `draw` writes. The status text already uses ANSI colors, so the cursor moves are
/// ANSI too (`TerminalGuard::enter` makes sure a Windows console understands them).
fn render(text: &str, cols: u16, rows: u16) -> Result<String> {
    let mut buf = String::with_capacity(text.len() + 256);
    // The last column is left empty: writing in the last column of the last row makes some
    // terminals scroll.
    let width = usize::from(cols.saturating_sub(1)).max(1);
    let mut row = 0u16;
    for line in text.lines().take(usize::from(rows)) {
        MoveTo(0, row).write_ansi(&mut buf)?;
        buf.push_str(&fit(line, width));
        Clear(ClearType::UntilNewLine).write_ansi(&mut buf)?;
        row += 1;
    }
    if row < rows {
        MoveTo(0, row).write_ansi(&mut buf)?;
        Clear(ClearType::FromCursorDown).write_ansi(&mut buf)?;
    }
    Ok(buf)
}

/// Cut `line` to `width` terminal columns, keeping its color codes whole; a cut line ends by
/// resetting its colors. Wide characters (CJK, most emoji) take two columns, and one that would
/// cross the edge is left out with the rest, so a line never wraps.
pub fn fit(line: &str, width: usize) -> String {
    let mut out = String::with_capacity(line.len());
    let mut visible = 0;
    let mut chars = line.chars().peekable();
    let mut styled = false;
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            // A CSI sequence: ESC [ parameters, then a final byte in @..~.
            out.push(c);
            if chars.peek() == Some(&'[') {
                out.push(chars.next().unwrap_or('['));
                for c in chars.by_ref() {
                    out.push(c);
                    if ('@'..='~').contains(&c) {
                        break;
                    }
                }
            }
            styled = true;
            continue;
        }
        let columns = c.width().unwrap_or(0);
        if visible + columns > width {
            if styled {
                out.push_str(RESET);
            }
            return out;
        }
        out.push(c);
        visible += columns;
    }
    out
}

#[derive(Debug, PartialEq, Eq)]
enum Wake {
    Redraw,
    Quit,
}

/// Wait for the next frame: `interval`, or less when the window is resized; `Quit` on q, Esc,
/// Ctrl-C (raw mode delivers Ctrl-C as a key, on Windows too) or a termination signal.
fn wait(interval: Duration, stop: &Stop) -> Result<Wake> {
    let deadline = Instant::now() + interval;
    loop {
        if stop.requested().is_some() {
            return Ok(Wake::Quit);
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Ok(Wake::Redraw);
        }
        match event::poll(left.min(STOP_CHECK)) {
            Ok(true) => {}
            Ok(false) => continue,
            // A signal arrived while waiting: the check above handles it.
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(e) => return Err(e.into()),
        }
        match event::read()? {
            Event::Key(key) if key.kind != KeyEventKind::Release => match key.code {
                KeyCode::Char('q' | 'Q') | KeyCode::Esc => return Ok(Wake::Quit),
                KeyCode::Char('c' | 'C') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    return Ok(Wake::Quit);
                }
                _ => {}
            },
            Event::Resize(..) => return Ok(Wake::Redraw),
            _ => {}
        }
    }
}

/// The termination signals that should end `watch` with the terminal restored: SIGTERM, SIGHUP
/// and SIGQUIT on Unix (Ctrl-C arrives as a key in raw mode). Their default action would end the
/// process at once, leaving the terminal in raw mode on the alternate screen; while this is
/// registered they only set a flag that `wait` checks.
struct Stop {
    /// The number of the signal that arrived, 0 for none.
    signal: Arc<AtomicUsize>,
    #[cfg(unix)]
    ids: Vec<signal_hook::SigId>,
}

impl Stop {
    fn register() -> Result<Self> {
        let signal = Arc::new(AtomicUsize::new(0));
        #[cfg(unix)]
        let ids = {
            use signal_hook::consts::{SIGHUP, SIGQUIT, SIGTERM};
            [SIGTERM, SIGHUP, SIGQUIT]
                .into_iter()
                .map(|sig| {
                    signal_hook::flag::register_usize(sig, Arc::clone(&signal), sig as usize)
                })
                .collect::<std::io::Result<Vec<_>>>()?
        };
        Ok(Self {
            signal,
            #[cfg(unix)]
            ids,
        })
    }

    /// The termination signal that arrived, if one did.
    fn requested(&self) -> Option<i32> {
        match self.signal.load(Ordering::SeqCst) {
            0 => None,
            sig => Some(sig as i32),
        }
    }

    /// Once the terminal is restored: stop catching the signals, and end the process the way
    /// the one that arrived would have (so `timeout` and shells see the signal, not a normal exit).
    fn finish(self) {
        let requested = self.requested();
        drop(self);
        #[cfg(unix)]
        if let Some(sig) = requested {
            let _ = signal_hook::low_level::emulate_default_handler(sig);
        }
        #[cfg(not(unix))]
        let _ = requested;
    }
}

impl Drop for Stop {
    fn drop(&mut self) {
        #[cfg(unix)]
        for id in self.ids.drain(..) {
            signal_hook::low_level::unregister(id);
        }
    }
}

/// Whether the terminal is in watch's state (raw mode, alternate screen, cursor hidden), so it is
/// restored exactly once, by the panic hook or the guard, whichever comes first.
static TERMINAL_CHANGED: AtomicBool = AtomicBool::new(false);

fn restore_terminal() {
    if TERMINAL_CHANGED.swap(false, Ordering::SeqCst) {
        let _ = execute!(std::io::stdout(), Show, LeaveAlternateScreen);
        let _ = terminal::disable_raw_mode();
    }
}

type PanicHook = Box<dyn Fn(&PanicHookInfo<'_>) + Sync + Send + 'static>;

/// Put a panic hook in front of the current one that restores the terminal first, so the panic
/// message is printed on the normal screen instead of being wiped with the alternate one.
/// Returns the hook it wraps, for `restore_panic_hook`.
fn install_panic_hook() -> Arc<PanicHook> {
    let previous: Arc<PanicHook> = Arc::new(std::panic::take_hook());
    let chained = Arc::clone(&previous);
    std::panic::set_hook(Box::new(move |info| {
        restore_terminal();
        chained(info);
    }));
    previous
}

/// Put back the hook `install_panic_hook` wrapped. Not while panicking, when the hook can't be
/// changed (the process is ending with the hook it has anyway).
fn restore_panic_hook(previous: Arc<PanicHook>) {
    if std::thread::panicking() {
        return;
    }
    // Dropping ours drops its share of `previous`, so the original comes back as it was.
    drop(std::panic::take_hook());
    match Arc::try_unwrap(previous) {
        Ok(original) => std::panic::set_hook(original),
        Err(shared) => std::panic::set_hook(Box::new(move |info| shared(info))),
    }
}

/// Raw mode, the alternate screen and a hidden cursor while it lives. All three are restored
/// when it drops (q, Ctrl-C, a signal, an error), or by the panic hook it installs, before the
/// panic message is printed.
struct TerminalGuard {
    previous_hook: Option<Arc<PanicHook>>,
}

impl TerminalGuard {
    fn enter() -> Result<Self> {
        // Turns on a Windows console's ANSI support (Windows 10 and later), which the frames use.
        #[cfg(windows)]
        crossterm::ansi_support::supports_ansi();
        // From here on, dropping the guard restores whatever was changed.
        let guard = TerminalGuard {
            previous_hook: Some(install_panic_hook()),
        };
        TERMINAL_CHANGED.store(true, Ordering::SeqCst);
        terminal::enable_raw_mode()?;
        execute!(std::io::stdout(), EnterAlternateScreen, Hide)?;
        Ok(guard)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        restore_terminal();
        if let Some(previous) = self.previous_hook.take() {
            restore_panic_hook(previous);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fit_cuts_to_visible_width_and_keeps_colors_whole() {
        assert_eq!(fit("hello", 10), "hello");
        assert_eq!(fit("hello", 3), "hel");
        assert_eq!(fit("\x1b[2mhello\x1b[0m", 10), "\x1b[2mhello\x1b[0m");
        assert_eq!(fit("\x1b[2mhello\x1b[0m", 2), "\x1b[2mhe\x1b[0m");
        assert_eq!(fit("ab\x1b[33mcd", 3), "ab\x1b[33mc\x1b[0m");
        assert_eq!(fit("", 3), "");
    }

    #[test]
    fn fit_counts_wide_characters_as_two_columns() {
        assert_eq!(fit("日本語", 6), "日本語");
        assert_eq!(fit("日本語", 5), "日本", "the third would cross the edge");
        assert_eq!(fit("ab😀c", 3), "ab");
        assert_eq!(fit("ab😀c", 4), "ab😀");
        assert_eq!(fit("\x1b[2m日本\x1b[0m", 3), "\x1b[2m日\x1b[0m");
        // Combining marks take no column.
        assert_eq!(fit("e\u{301}x", 1), "e\u{301}");
    }

    #[test]
    fn intervals_read_like_durations() {
        assert_eq!(every(Duration::from_secs(2)), "2s");
        assert_eq!(every(Duration::from_secs(60)), "1m");
        assert_eq!(every(Duration::from_secs(90)), "1m 30s");
        assert_eq!(every(Duration::from_secs(3600)), "1h");
        assert_eq!(every(Duration::from_secs(3661)), "1h 1m 1s");
    }

    /// The panic hook restores the terminal and then runs the hook it wraps, and dropping the
    /// guard puts that hook back.
    #[test]
    fn the_panic_hook_runs_the_previous_one_and_is_put_back() {
        static CALLS: AtomicUsize = AtomicUsize::new(0);
        let original = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {
            CALLS.fetch_add(1, Ordering::SeqCst);
        }));
        let wrapped = install_panic_hook();
        assert!(std::panic::catch_unwind(|| panic!("in watch")).is_err());
        assert_eq!(
            CALLS.load(Ordering::SeqCst),
            1,
            "the previous hook still runs"
        );
        restore_panic_hook(wrapped);
        assert!(std::panic::catch_unwind(|| panic!("after watch")).is_err());
        assert_eq!(CALLS.load(Ordering::SeqCst), 2);
        std::panic::set_hook(original);
    }

    /// A termination signal makes the wait end with `Quit`, so the guard drops and restores the
    /// terminal, instead of the signal ending the process on the spot.
    #[cfg(unix)]
    #[test]
    fn a_termination_signal_quits() {
        use signal_hook::consts::SIGTERM;
        let stop = Stop::register().unwrap();
        assert_eq!(stop.requested(), None);
        signal_hook::low_level::raise(SIGTERM).unwrap();
        assert_eq!(stop.requested(), Some(SIGTERM), "caught, not fatal");
        assert_eq!(wait(Duration::from_secs(30), &stop).unwrap(), Wake::Quit);
        // Dropping it (rather than `finish`) unregisters without ending the test process.
        drop(stop);
    }

    #[test]
    fn frames_overwrite_in_place_and_never_scroll() {
        let s = render("one\ntwo\nthree\nfour", 20, 3).unwrap();
        // Home, never a full-screen clear (ESC[2J), and only the lines that fit.
        assert!(s.starts_with("\x1b[1;1H"), "{s:?}");
        assert!(!s.contains("\x1b[2J"), "{s:?}");
        assert!(s.contains("three") && !s.contains("four"), "{s:?}");
        assert!(
            !s.contains("\x1b[J"),
            "the screen is full: nothing below to clear"
        );

        let s = render("one", 20, 3).unwrap();
        assert!(
            s.ends_with("\x1b[2;1H\x1b[J"),
            "clears below the last line: {s:?}"
        );
    }

    #[test]
    fn a_frame_shows_the_header_status_and_events() {
        let root = tempfile::tempdir().unwrap();
        let paths = Paths::new(root.path().join("data"), root.path().join("profile"));
        let store = Store::open(&paths.data_dir.join("store.db")).unwrap();
        let empty = frame(&store, &paths, Duration::from_secs(2), true, Some(40)).unwrap();
        assert!(empty.contains("Press q to quit"), "{empty}");
        assert!(empty.contains("No accounts yet"), "{empty}");

        let id = store
            .insert_account(
                "main",
                &ccr_core::store::AccountIdentity {
                    account_uuid: Some("u".into()),
                    ..Default::default()
                },
                ccr_core::store::AccountState::Active,
            )
            .unwrap();
        for _ in 0..20 {
            store
                .log_event("swap", None, Some(id), Some("manual"), None)
                .unwrap();
        }
        let before = store.conn().total_changes();
        let text = frame(&store, &paths, Duration::from_secs(2), false, Some(40)).unwrap();
        assert_eq!(store.conn().total_changes(), before, "read-only");
        assert!(!text.contains("Press q"), "one frame for a pipe: {text}");
        assert!(text.contains("main") && text.contains("Activity"), "{text}");
        assert_eq!(
            text.lines().filter(|l| l.contains("manual")).count(),
            MAX_EVENTS
        );
        // A short screen shows fewer events, so the newest stay in view.
        let short = frame(&store, &paths, Duration::from_secs(2), true, Some(12)).unwrap();
        assert!(short.lines().count() <= 12, "{short}");
    }
}
