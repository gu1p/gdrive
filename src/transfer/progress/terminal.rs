//! Whether progress is drawn, on what, and how other output gets past the bars.
#![expect(
    dead_code,
    reason = "the commands draw progress from the next commit on"
)]
//!
//! - [`Mode`]: `--progress`, `--no-progress`, `GDRIVE_PROGRESS`, or automatic: on when stderr is
//!   a terminal.
//! - [`Drawing`]: what a mode means on the process's terminal ([`Terminal`]).
//! - [`suspend`], and the [`eprintln!`] and [`println!`] that use it: while a display draws, a
//!   line printed to stdout or stderr goes above the bars, whole. The run's
//!   [`Printer`](crate::transfer::scheduler::Printer) writes through it; code that prints while a
//!   transfer runs imports these macros in place of the standard ones.

use std::{
    ffi::OsStr,
    fmt,
    io::{self, IsTerminal},
    sync::{
        atomic::{AtomicBool, Ordering},
        Mutex, PoisonError,
    },
};

use indicatif::{MultiProgress, TermLike};

/// The environment variable that sets the [`Mode`] when no flag does: `1` on, `0` off.
pub(crate) const ENV: &str = "GDRIVE_PROGRESS";

/// Whether a transfer shows its progress on stderr.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum Mode {
    /// When stderr is a terminal, and, for a download to stdout, stdout is not one.
    #[default]
    Auto,
    /// Also when stderr is not a terminal (`--progress`, `GDRIVE_PROGRESS=1`).
    Always,
    /// Never (`--no-progress`, `GDRIVE_PROGRESS=0`).
    Never,
}

impl Mode {
    /// The mode of the flags, else of `GDRIVE_PROGRESS`, else automatic. At most one of the
    /// flags is set (the last one given wins). An unknown value of the variable is warned about
    /// and ignored.
    #[must_use]
    pub(crate) fn from_flags(progress: bool, no_progress: bool) -> Mode {
        let env = std::env::var_os(ENV);
        Mode::choose(progress, no_progress, env.as_deref()).unwrap_or_else(|value| {
            eprintln!("Warning: {ENV} should be 0 or 1, ignoring '{value}'");
            Mode::Auto
        })
    }

    /// [`Self::from_flags`] with the value of the variable; the error is a value it does not
    /// know, escaped for the terminal.
    fn choose(progress: bool, no_progress: bool, env: Option<&OsStr>) -> Result<Mode, String> {
        if progress {
            return Ok(Mode::Always);
        }
        if no_progress {
            return Ok(Mode::Never);
        }
        let Some(value) = env else {
            return Ok(Mode::Auto);
        };
        match value.to_str() {
            Some("") => Ok(Mode::Auto),
            Some("1") => Ok(Mode::Always),
            Some("0") => Ok(Mode::Never),
            _ => Err(value.to_string_lossy().escape_debug().to_string()),
        }
    }

    /// How a display of this mode draws on `terminal`. `content_on_stdout`: the transfer writes
    /// the file to stdout (`files download --stdout`), where bars on the same terminal would
    /// mix with it.
    #[must_use]
    pub(crate) fn drawing(self, terminal: Terminal, content_on_stdout: bool) -> Drawing {
        match self {
            Mode::Always if terminal.stderr => Drawing::Live,
            Mode::Always => Drawing::Line,
            Mode::Auto if terminal.stderr && !(content_on_stdout && terminal.stdout) => {
                Drawing::Live
            }
            Mode::Auto | Mode::Never => Drawing::Off,
        }
    }
}

/// What stdout and stderr of the process are.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Terminal {
    /// stderr is a terminal that takes cursor movements (`TERM` is not `dumb`).
    pub(crate) stderr: bool,
    /// stdout is a terminal.
    pub(crate) stdout: bool,
}

impl Terminal {
    #[must_use]
    pub(crate) fn detect() -> Terminal {
        let dumb = std::env::var_os("TERM").is_some_and(|term| term == "dumb");
        Terminal {
            stderr: io::stderr().is_terminal() && !dumb,
            stdout: io::stdout().is_terminal(),
        }
    }
}

/// How a display draws.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Drawing {
    /// Not at all: the events are dropped.
    Off,
    /// On a terminal: every line, redrawn ten times a second.
    Live,
    /// Where stderr is not a terminal (`--progress` into a CI log or `script`): one line,
    /// rewritten after a carriage return once a second.
    Line,
}

/// stderr as the bars draw on it.
#[derive(Debug)]
pub(super) struct Stderr {
    term: console::Term,
    /// The width when stderr is not a terminal: `COLUMNS`, or 80.
    columns: Option<u16>,
}

impl Stderr {
    pub(super) fn new() -> Self {
        let term = console::Term::buffered_stderr();
        let columns = (!term.is_term()).then(|| {
            std::env::var("COLUMNS")
                .ok()
                .and_then(|columns| columns.trim().parse().ok())
                .filter(|columns| *columns > 0)
                .unwrap_or(80)
        });
        Stderr { term, columns }
    }
}

impl TermLike for Stderr {
    fn width(&self) -> u16 {
        self.columns.unwrap_or_else(|| self.term.size().1)
    }

    fn height(&self) -> u16 {
        self.term.size().0
    }

    fn move_cursor_up(&self, n: usize) -> io::Result<()> {
        self.term.move_cursor_up(n)
    }

    fn move_cursor_down(&self, n: usize) -> io::Result<()> {
        self.term.move_cursor_down(n)
    }

    fn move_cursor_right(&self, n: usize) -> io::Result<()> {
        self.term.move_cursor_right(n)
    }

    fn move_cursor_left(&self, n: usize) -> io::Result<()> {
        self.term.move_cursor_left(n)
    }

    fn write_line(&self, s: &str) -> io::Result<()> {
        self.term.write_line(s)
    }

    fn write_str(&self, s: &str) -> io::Result<()> {
        self.term.write_str(s)
    }

    fn clear_line(&self) -> io::Result<()> {
        self.term.clear_line()
    }

    fn flush(&self) -> io::Result<()> {
        self.term.flush()
    }
}

/// A terminal whose first failed write ends the drawing on it, quietly: indicatif's `suspend`
/// panics on an error of its terminal (stderr closed behind `--progress 2>&1 | head`), and the
/// bars are not worth failing a transfer for.
pub(super) struct Quiet<T> {
    inner: T,
    failed: AtomicBool,
}

impl<T> Quiet<T> {
    pub(super) fn new(inner: T) -> Self {
        Quiet {
            inner,
            failed: AtomicBool::new(false),
        }
    }

    /// Writes with `write` until a write fails.
    fn write(&self, write: impl FnOnce(&T) -> io::Result<()>) {
        if !self.failed.load(Ordering::Relaxed) && write(&self.inner).is_err() {
            self.failed.store(true, Ordering::Relaxed);
        }
    }
}

impl<T: fmt::Debug> fmt::Debug for Quiet<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Quiet")
            .field("inner", &self.inner)
            .field("failed", &self.failed.load(Ordering::Relaxed))
            .finish()
    }
}

impl<T: TermLike> TermLike for Quiet<T> {
    fn width(&self) -> u16 {
        self.inner.width()
    }

    fn height(&self) -> u16 {
        self.inner.height()
    }

    fn move_cursor_up(&self, n: usize) -> io::Result<()> {
        self.write(|term| term.move_cursor_up(n));
        Ok(())
    }

    fn move_cursor_down(&self, n: usize) -> io::Result<()> {
        self.write(|term| term.move_cursor_down(n));
        Ok(())
    }

    fn move_cursor_right(&self, n: usize) -> io::Result<()> {
        self.write(|term| term.move_cursor_right(n));
        Ok(())
    }

    fn move_cursor_left(&self, n: usize) -> io::Result<()> {
        self.write(|term| term.move_cursor_left(n));
        Ok(())
    }

    fn write_line(&self, s: &str) -> io::Result<()> {
        self.write(|term| term.write_line(s));
        Ok(())
    }

    fn write_str(&self, s: &str) -> io::Result<()> {
        self.write(|term| term.write_str(s));
        Ok(())
    }

    fn clear_line(&self) -> io::Result<()> {
        self.write(TermLike::clear_line);
        Ok(())
    }

    fn flush(&self) -> io::Result<()> {
        self.write(TermLike::flush);
        Ok(())
    }
}

/// The bars that draw now, if any.
static ACTIVE: Mutex<Option<MultiProgress>> = Mutex::new(None);

/// Runs `f`, which prints to stdout or stderr, with the bars of the display that draws now taken
/// off the terminal and drawn again below what `f` printed. Without a display, only runs `f`.
///
/// `f` must not call `suspend` itself, nor wait for anything that does: the bars stay locked
/// while it runs.
pub(crate) fn suspend<R>(f: impl FnOnce() -> R) -> R {
    let active = ACTIVE
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone();
    match active {
        Some(bars) => bars.suspend(f),
        None => f(),
    }
}

/// From now on, [`suspend`] takes `bars` off the terminal.
pub(super) fn activate(bars: &MultiProgress) {
    *ACTIVE.lock().unwrap_or_else(PoisonError::into_inner) = Some(bars.clone());
}

pub(super) fn deactivate() {
    *ACTIVE.lock().unwrap_or_else(PoisonError::into_inner) = None;
}

/// `std::eprintln!`, printed above the progress bars (see [`suspend`]).
macro_rules! eprintln {
    ($($arg:tt)*) => {
        $crate::transfer::progress::suspend(|| ::std::eprintln!($($arg)*))
    };
}

/// `std::println!`, printed above the progress bars (see [`suspend`]).
macro_rules! println {
    ($($arg:tt)*) => {
        $crate::transfer::progress::suspend(|| ::std::println!($($arg)*))
    };
}

pub(crate) use {eprintln, println};

#[cfg(test)]
mod tests {
    use std::{
        ffi::OsStr,
        io,
        sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        },
    };

    use indicatif::TermLike;

    use super::{Drawing, Mode, Quiet, Terminal};

    #[test]
    fn flags_beat_the_variable_which_beats_the_terminal() {
        let choose = |progress, no_progress, env: Option<&str>| {
            Mode::choose(progress, no_progress, env.map(OsStr::new))
        };
        assert_eq!(choose(false, false, None), Ok(Mode::Auto));
        assert_eq!(choose(false, false, Some("")), Ok(Mode::Auto));
        assert_eq!(choose(false, false, Some("1")), Ok(Mode::Always));
        assert_eq!(choose(false, false, Some("0")), Ok(Mode::Never));
        assert_eq!(choose(true, false, Some("0")), Ok(Mode::Always));
        assert_eq!(choose(false, true, Some("1")), Ok(Mode::Never));
        assert_eq!(choose(true, false, Some("yes")), Ok(Mode::Always));
        assert_eq!(
            choose(false, false, Some("yes\n")),
            Err(String::from("yes\\n"))
        );
    }

    #[test]
    fn automatic_progress_needs_a_terminal_on_stderr_and_none_under_the_content() {
        let terminal = |stderr, stdout| Terminal { stderr, stdout };
        for (stderr, stdout) in [(false, false), (false, true), (true, false), (true, true)] {
            for content_on_stdout in [false, true] {
                let terminal = terminal(stderr, stdout);
                assert_eq!(
                    Mode::Never.drawing(terminal, content_on_stdout),
                    Drawing::Off
                );
                let forced = if stderr { Drawing::Live } else { Drawing::Line };
                assert_eq!(Mode::Always.drawing(terminal, content_on_stdout), forced);
                let auto = if stderr && !(content_on_stdout && stdout) {
                    Drawing::Live
                } else {
                    Drawing::Off
                };
                assert_eq!(
                    Mode::Auto.drawing(terminal, content_on_stdout),
                    auto,
                    "{terminal:?}, content on stdout: {content_on_stdout}"
                );
            }
        }
    }

    /// Fails every write and counts them.
    #[derive(Debug, Default)]
    struct Broken(Arc<AtomicUsize>);

    impl TermLike for Broken {
        fn width(&self) -> u16 {
            80
        }

        fn move_cursor_up(&self, _: usize) -> io::Result<()> {
            self.write_str("")
        }

        fn move_cursor_down(&self, _: usize) -> io::Result<()> {
            self.write_str("")
        }

        fn move_cursor_right(&self, _: usize) -> io::Result<()> {
            self.write_str("")
        }

        fn move_cursor_left(&self, _: usize) -> io::Result<()> {
            self.write_str("")
        }

        fn write_line(&self, s: &str) -> io::Result<()> {
            self.write_str(s)
        }

        fn write_str(&self, _: &str) -> io::Result<()> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Err(io::ErrorKind::BrokenPipe.into())
        }

        fn clear_line(&self) -> io::Result<()> {
            self.write_str("")
        }

        fn flush(&self) -> io::Result<()> {
            self.write_str("")
        }
    }

    #[test]
    fn a_terminal_that_fails_is_left_alone() {
        let writes = Arc::new(AtomicUsize::new(0));
        let quiet = Quiet::new(Broken(Arc::clone(&writes)));

        assert!(quiet.write_str("bar").is_ok());
        assert!(quiet.clear_line().is_ok());
        assert!(quiet.flush().is_ok());

        assert_eq!(writes.load(Ordering::SeqCst), 1);
    }
}
