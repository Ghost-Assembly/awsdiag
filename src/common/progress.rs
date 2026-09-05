//! Progress reporting on stderr.
//!
//! # The constraint this exists under
//!
//! The primary consumer of this tool parses JSON from **stdout**. A spinner
//! written there would corrupt it. So every byte this module emits goes to
//! stderr, and the invariant — stdout is byte-for-byte identical whether
//! progress is on or off — is asserted by an integration test rather than
//! left to care.
//!
//! # When it draws
//!
//! `auto` (the default) delegates to indicatif, which hides itself when
//! stderr is not user-attended or `TERM` is unset or `dumb`. That is exactly
//! the wanted behaviour — piping or redirecting produces no escape codes —
//! and it is the library's own safeguard rather than a reimplementation.
//!
//! `always` bypasses that check, which is what lets the stdout-invariance
//! test exercise a live progress path instead of a hidden no-op.

use indicatif::{MultiProgress, ProgressBar, ProgressDrawTarget, ProgressStyle, TermLike};
use std::io::{IsTerminal, Write};
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum, Default)]
pub enum ProgressMode {
    /// Draw only when stderr is an interactive terminal.
    #[default]
    Auto,
    /// Draw even when stderr is redirected.
    Always,
    /// Never draw.
    Never,
}

/// A handle that is a no-op when progress is disabled.
///
/// Every method is safe to call unconditionally, so call sites carry no
/// `if enabled` branches and cannot drift out of step with the setting.
pub struct Progress {
    inner: Option<Inner>,
}

struct Inner {
    multi: MultiProgress,
    main: ProgressBar,
}

/// One tracked unit of work — a log group being scanned, a profile being
/// resolved.
pub struct Task {
    bar: Option<ProgressBar>,
}

const TICK: Duration = Duration::from_millis(90);

impl Progress {
    pub fn new(mode: ProgressMode) -> Self {
        let target = match mode {
            ProgressMode::Never => return Self { inner: None },
            // Hides itself on a non-terminal or a dumb terminal.
            ProgressMode::Auto => ProgressDrawTarget::stderr(),
            ProgressMode::Always => {
                ProgressDrawTarget::term_like_with_hz(Box::new(PlainStderr), 15)
            }
        };
        if target.is_hidden() {
            return Self { inner: None };
        }

        let multi = MultiProgress::with_draw_target(target);
        let main = multi.add(ProgressBar::new_spinner());
        main.set_style(spinner_style());
        main.enable_steady_tick(TICK);
        Self {
            inner: Some(Inner { multi, main }),
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.inner.is_some()
    }

    /// Set the headline message.
    pub fn phase(&self, message: impl Into<String>) {
        if let Some(i) = &self.inner {
            i.main.set_message(message.into());
        }
    }

    /// Add a sub-task line beneath the headline.
    pub fn task(&self, label: impl Into<String>) -> Task {
        let Some(i) = &self.inner else {
            return Task { bar: None };
        };
        let bar = i.multi.add(ProgressBar::new_spinner());
        bar.set_style(task_style());
        bar.enable_steady_tick(TICK);
        bar.set_message(label.into());
        Task { bar: Some(bar) }
    }

    /// Print a line above the bars without disturbing them.
    pub fn note(&self, line: impl AsRef<str>) {
        if let Some(i) = &self.inner {
            let _ = i.multi.println(line);
        }
    }

    /// Clear everything and print a closing summary.
    ///
    /// Called before the result goes to stdout, so a terminal shows the
    /// summary above the data rather than a spinner frozen mid-draw.
    pub fn finish(&self, summary: impl Into<String>) {
        if let Some(i) = &self.inner {
            i.main.finish_and_clear();
            let _ = i.multi.println(summary.into());
            let _ = i.multi.clear();
        }
    }

    /// Clear without a summary, for the failure path.
    pub fn abandon(&self) {
        if let Some(i) = &self.inner {
            i.main.finish_and_clear();
            let _ = i.multi.clear();
        }
    }
}

impl Task {
    pub fn update(&self, message: impl Into<String>) {
        if let Some(b) = &self.bar {
            b.set_message(message.into());
        }
    }

    /// Mark the task complete, leaving one settled line behind.
    pub fn done(&self, message: impl Into<String>) {
        if let Some(b) = &self.bar {
            b.set_style(done_style());
            b.finish_with_message(message.into());
        }
    }

    /// Mark the task failed.
    pub fn failed(&self, message: impl Into<String>) {
        if let Some(b) = &self.bar {
            b.set_style(failed_style());
            b.finish_with_message(message.into());
        }
    }
}

/// `NO_COLOR` is honoured for colour only. It asks for no colour, not for no
/// progress, so the bars still draw — in plain text.
fn colour() -> bool {
    std::env::var("NO_COLOR").is_err_and(|_| true)
}

fn spinner_style() -> ProgressStyle {
    let t = if colour() {
        "{spinner:.cyan} {msg}"
    } else {
        "{spinner} {msg}"
    };
    ProgressStyle::with_template(t)
        .unwrap_or_else(|_| ProgressStyle::default_spinner())
        .tick_strings(&["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏", "⠿"])
}

fn task_style() -> ProgressStyle {
    let t = if colour() {
        "  {spinner:.dim} {msg:.dim}"
    } else {
        "  {spinner} {msg}"
    };
    ProgressStyle::with_template(t).unwrap_or_else(|_| ProgressStyle::default_spinner())
}

fn done_style() -> ProgressStyle {
    let t = if colour() {
        "  {prefix:.green}{msg:.dim}"
    } else {
        "  {prefix}{msg}"
    };
    ProgressStyle::with_template(t)
        .unwrap_or_else(|_| ProgressStyle::default_spinner())
        .tick_strings(&["✓ ", "✓ "])
}

fn failed_style() -> ProgressStyle {
    let t = if colour() {
        "  {prefix:.red}{msg}"
    } else {
        "  {prefix}{msg}"
    };
    ProgressStyle::with_template(t)
        .unwrap_or_else(|_| ProgressStyle::default_spinner())
        .tick_strings(&["✗ ", "✗ "])
}

/// A minimal stderr terminal for `--progress always`.
///
/// indicatif deliberately hides itself when stderr is not a terminal, to keep
/// escape codes out of redirected output. `always` opts out of that, which is
/// only appropriate because the caller asked for it explicitly.
#[derive(Debug)]
struct PlainStderr;

impl PlainStderr {
    fn write(&self, s: &str) -> std::io::Result<()> {
        let mut err = std::io::stderr();
        err.write_all(s.as_bytes())?;
        err.flush()
    }
}

impl TermLike for PlainStderr {
    fn width(&self) -> u16 {
        // Wide enough not to wrap the lines this module emits, without
        // pretending to know a redirected stream's dimensions.
        120
    }
    fn move_cursor_up(&self, n: usize) -> std::io::Result<()> {
        if std::io::stderr().is_terminal() {
            self.write(&format!("\x1b[{n}A"))
        } else {
            Ok(())
        }
    }
    fn move_cursor_down(&self, n: usize) -> std::io::Result<()> {
        if std::io::stderr().is_terminal() {
            self.write(&format!("\x1b[{n}B"))
        } else {
            Ok(())
        }
    }
    fn move_cursor_right(&self, n: usize) -> std::io::Result<()> {
        if std::io::stderr().is_terminal() {
            self.write(&format!("\x1b[{n}C"))
        } else {
            Ok(())
        }
    }
    fn move_cursor_left(&self, n: usize) -> std::io::Result<()> {
        if std::io::stderr().is_terminal() {
            self.write(&format!("\x1b[{n}D"))
        } else {
            Ok(())
        }
    }
    fn write_line(&self, s: &str) -> std::io::Result<()> {
        self.write(&format!("{s}\n"))
    }
    fn write_str(&self, s: &str) -> std::io::Result<()> {
        self.write(s)
    }
    fn clear_line(&self) -> std::io::Result<()> {
        if std::io::stderr().is_terminal() {
            self.write("\r\x1b[2K")
        } else {
            Ok(())
        }
    }
    fn flush(&self) -> std::io::Result<()> {
        std::io::stderr().flush()
    }
}

/// Format a count with thousands separators: 12400 -> "12,400".
pub fn thousands(n: usize) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_disabled_handle_accepts_every_call_without_drawing() {
        // Call sites must never need an `if enabled` branch, or they drift.
        let p = Progress::new(ProgressMode::Never);
        assert!(!p.is_enabled());
        p.phase("scanning");
        p.note("a note");
        let t = p.task("group");
        t.update("page 2");
        t.done("finished");
        let t2 = p.task("other");
        t2.failed("nope");
        p.finish("done");
        p.abandon();
    }

    #[test]
    fn auto_is_disabled_when_stderr_is_not_a_terminal() {
        // The test harness captures stderr, so this exercises the real path a
        // piped or redirected invocation takes.
        assert!(
            !std::io::stderr().is_terminal(),
            "precondition for this test"
        );
        assert!(!Progress::new(ProgressMode::Auto).is_enabled());
    }

    #[test]
    fn thousands_separates_groups_of_three() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1_000), "1,000");
        assert_eq!(thousands(12_400), "12,400");
        assert_eq!(thousands(1_234_567), "1,234,567");
    }
}
