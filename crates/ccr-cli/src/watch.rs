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
//! left over, so nothing flickers.

use std::io::{IsTerminal, Write};
use std::time::{Duration, Instant};

use anyhow::Result;
use ccr_core::{Paths, Store, snapshot, time};
use crossterm::cursor::{Hide, MoveTo, Show};
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::terminal::{self, Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen};
use crossterm::{Command, execute};

use crate::format;

const DIM: &str = "\x1b[2m";
const RESET: &str = "\x1b[0m";

/// The most events shown below the accounts.
const MAX_EVENTS: usize = 8;

pub fn run(store: &Store, paths: &Paths, interval: Duration) -> Result<()> {
    let mut out = std::io::stdout();
    if !out.is_terminal() {
        // Piped or redirected: one frame, no cursor movement.
        out.write_all(frame(store, paths, interval, false, None)?.as_bytes())?;
        return Ok(());
    }
    let _terminal = TerminalGuard::enter()?;
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
        if wait(interval)? == Wake::Quit {
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

/// "2s", "1m 30s".
fn every(interval: Duration) -> String {
    let s = interval.as_secs();
    match (s / 60, s % 60) {
        (0, s) => format!("{s}s"),
        (m, 0) => format!("{m}m"),
        (m, s) => format!("{m}m {s}s"),
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

/// Cut `line` to `width` visible characters, keeping its color codes whole; a cut line ends by
/// resetting its colors.
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
        if visible == width {
            if styled {
                out.push_str(RESET);
            }
            return out;
        }
        out.push(c);
        visible += 1;
    }
    out
}

#[derive(Debug, PartialEq, Eq)]
enum Wake {
    Redraw,
    Quit,
}

/// Wait for the next frame: `interval`, or less when the window is resized; `Quit` on q, Esc
/// or Ctrl-C (raw mode delivers Ctrl-C as a key, on Windows too).
fn wait(interval: Duration) -> Result<Wake> {
    let deadline = Instant::now() + interval;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() || !event::poll(left)? {
            return Ok(Wake::Redraw);
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

/// Raw mode, the alternate screen and a hidden cursor while it lives; all three are restored
/// when it drops, on q, Ctrl-C, an error, or a panic.
struct TerminalGuard;

impl TerminalGuard {
    fn enter() -> Result<Self> {
        // Turns on a Windows console's ANSI support (Windows 10 and later), which the frames use.
        #[cfg(windows)]
        crossterm::ansi_support::supports_ansi();
        terminal::enable_raw_mode()?;
        // From here on, dropping the guard restores whatever was changed.
        let guard = TerminalGuard;
        execute!(std::io::stdout(), EnterAlternateScreen, Hide)?;
        Ok(guard)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = execute!(std::io::stdout(), Show, LeaveAlternateScreen);
        let _ = terminal::disable_raw_mode();
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
    fn intervals_read_like_durations() {
        assert_eq!(every(Duration::from_secs(2)), "2s");
        assert_eq!(every(Duration::from_secs(60)), "1m");
        assert_eq!(every(Duration::from_secs(90)), "1m 30s");
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
