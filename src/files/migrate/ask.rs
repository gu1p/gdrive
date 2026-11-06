//! `--ask`: the question about one conflict, and the terminal it is asked on.
//!
//! ```text
//!   job: a conflict that a replace can settle -> the two files as Drive has them now
//!     -> Ask::ask(question): one at a time; the printer is held, so that nothing else is
//!        written between the question and its answer
//!        -> the keyboard thread writes the question to stderr and reads a line from stdin
//! ```
//!
//! The answers are read on a thread of their own: a read of the keyboard cannot be cancelled,
//! and a thread of the runtime that waits in one would keep the process from ending after
//! Ctrl-C. That thread ends with the process. A question whose job went away (the run stopped)
//! is abandoned: its hold is released, and its answer goes nowhere. Ctrl-C releases the hold of
//! the open question by itself, so that the run can print and stop even when lines wait behind
//! it.

use std::{
    fmt::{Debug, Write as _},
    io::{self, BufRead, IsTerminal, Write},
    sync::mpsc,
};

use chrono::{DateTime, Utc};
use futures::future::BoxFuture;
use google_drive3::api::File;
use tokio::sync::oneshot;

use crate::{
    common::{human_size::human_size, local_name},
    transfer::scheduler::Printer,
};

/// What is read of a file for its question: fresh, not from a listing.
pub(super) const FIELDS: &str =
    "id,size,md5Checksum,modifiedTime,trashed,lastModifyingUser(displayName)";

/// Asks the user about a conflict.
pub(super) trait Ask: Debug + Send + Sync {
    /// Shows `question`, which ends with its prompt; whether the answer was yes.
    fn ask(&self, question: String) -> BoxFuture<'_, bool>;
}

/// What the user is shown of one of the two files of a conflict.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct Side {
    pub(super) size: Option<u64>,
    pub(super) modified: Option<DateTime<Utc>>,
    /// Who changed it last, as Drive names them.
    pub(super) by: Option<String>,
}

impl Side {
    pub(super) fn of(file: &File) -> Self {
        Side {
            size: file.size.and_then(|size| u64::try_from(size).ok()),
            modified: file.modified_time,
            by: file
                .last_modifying_user
                .as_ref()
                .and_then(|user| user.display_name.clone()),
        }
    }

    /// For example `1.2 MB, changed 2026-11-05 14:03 UTC by Maria`.
    fn describe(&self) -> String {
        let mut text = match self.size {
            Some(size) => human_size(size),
            None => String::from("size unknown"),
        };
        if let Some(modified) = &self.modified {
            let _ = write!(text, ", changed {}", modified.format("%Y-%m-%d %H:%M UTC"));
            if let Some(by) = &self.by {
                let _ = write!(text, " by {}", local_name::printable(by));
            }
        }
        text
    }
}

/// The question about the item `label`, whose content (`new`, called `what`: the source's file,
/// the new archive) is other than that of the destination's file of its name (`old`).
pub(super) fn question(label: &str, what: &str, new: &Side, old: &Side) -> String {
    let width = what.len().max("destination".len()) + 1;
    format!(
        "\n{label} is in the destination with other content:\n  {:<width$} {}\n  {:<width$} {}\n\
         Replace the destination's file? [y/N] ",
        format!("{what}:"),
        new.describe(),
        "destination:",
        old.describe(),
    )
}

/// Whether `answer`, a line the user typed, is a yes. Anything else is a no, and so is no
/// answer at all.
fn is_yes(answer: &str) -> bool {
    matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

/// The terminal of the process: questions on stderr, answers from stdin.
#[derive(Debug)]
pub(super) struct Terminal {
    printer: Printer,
    /// The keyboard, for one question at a time; none until the first one.
    keyboard: tokio::sync::Mutex<Option<Keyboard>>,
    /// Starts the keyboard's thread.
    start: fn() -> Option<Keyboard>,
}

impl Terminal {
    pub(super) fn new(printer: Printer) -> Self {
        Terminal {
            printer,
            keyboard: tokio::sync::Mutex::new(None),
            start: || Keyboard::start(io::BufReader::new(io::stdin()), io::stderr()),
        }
    }

    /// Whether a user is there to ask: stdin and stderr are terminals.
    pub(super) fn available() -> bool {
        io::stdin().is_terminal() && io::stderr().is_terminal()
    }
}

impl Ask for Terminal {
    fn ask(&self, question: String) -> BoxFuture<'_, bool> {
        Box::pin(async move {
            let mut keyboard = self.keyboard.lock().await;
            if keyboard.is_none() {
                *keyboard = (self.start)();
            }
            let Some(keyboard) = keyboard.as_ref() else {
                return false;
            };
            let answer = {
                // What was printed so far is on the terminal, and nothing follows the question.
                let _hold = self.printer.hold().await;
                tokio::select! {
                    answer = keyboard.answer(question) => Some(answer),
                    _ = tokio::signal::ctrl_c() => None,
                }
            };
            match answer {
                Some(answer) => answer.is_some_and(|answer| is_yes(&answer)),
                // Ctrl-C: the output is free again for what the run prints while it stops,
                // which a full queue behind the question would keep it from. The question has
                // no answer, and the run drops this job.
                None => std::future::pending().await,
            }
        })
    }
}

/// A question and where its answer goes: the line typed, or `None` at the end of stdin.
type Request = (String, oneshot::Sender<Option<String>>);

/// The thread that writes the questions and reads the answers.
#[derive(Debug)]
struct Keyboard {
    requests: mpsc::Sender<Request>,
}

impl Keyboard {
    /// Starts the thread, which writes to `screen` and reads from `keys`; `None` when it
    /// cannot be started.
    fn start<R, W>(mut keys: R, mut screen: W) -> Option<Keyboard>
    where
        R: BufRead + Send + 'static,
        W: Write + Send + 'static,
    {
        let (requests, receiver) = mpsc::channel::<Request>();
        std::thread::Builder::new()
            .name(String::from("gdrive-ask"))
            .spawn(move || {
                for (question, reply) in receiver {
                    let _ = write!(screen, "{question}");
                    let _ = screen.flush();
                    let mut line = String::new();
                    let answer = match keys.read_line(&mut line) {
                        Ok(read) if read > 0 => Some(line),
                        _ => None,
                    };
                    let _ = reply.send(answer);
                }
            })
            .ok()?;
        Some(Keyboard { requests })
    }

    async fn answer(&self, question: String) -> Option<String> {
        let (reply, answer) = oneshot::channel();
        self.requests.send((question, reply)).ok()?;
        answer.await.ok().flatten()
    }
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;

    #[test]
    fn a_question_shows_both_files_and_asks_for_a_yes() {
        let source = Side {
            size: Some(1_200_000),
            modified: Some(Utc.with_ymd_and_hms(2026, 11, 5, 14, 3, 59).unwrap()),
            by: Some(String::from("Maria\u{1b}[31m")),
        };
        let destination = Side {
            size: Some(1_100_000),
            modified: Some(Utc.with_ymd_and_hms(2026, 10, 28, 9, 15, 0).unwrap()),
            by: None,
        };

        assert_eq!(
            question("photos/report.xlsx", "source", &source, &destination),
            format!(
                "\nphotos/report.xlsx is in the destination with other content:\n  \
                 source:      {}, changed 2026-11-05 14:03 UTC by Maria\\u{{1b}}[31m\n  \
                 destination: {}, changed 2026-10-28 09:15 UTC\n\
                 Replace the destination's file? [y/N] ",
                human_size(1_200_000),
                human_size(1_100_000)
            )
        );
        // An archive that is made now has a size only.
        let archive = Side {
            size: Some(2048),
            ..Side::default()
        };
        let asked = question("photos.tar", "new archive", &archive, &Side::default());
        assert!(
            asked.contains(&format!(
                "  new archive: {}\n  destination: size unknown\n",
                human_size(2048)
            )),
            "{asked}"
        );
    }

    /// Keys that are typed when the test says so: a line per message.
    struct Typed {
        lines: mpsc::Receiver<&'static str>,
        line: io::Cursor<&'static str>,
    }

    impl io::Read for Typed {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if self.line.position() == self.line.get_ref().len() as u64 {
                match self.lines.recv() {
                    Ok(line) => self.line = io::Cursor::new(line),
                    // The end of the input.
                    Err(_) => return Ok(0),
                }
            }
            self.line.read(buf)
        }
    }

    async fn until(condition: impl Fn() -> bool) {
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            while !condition() {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("it should happen");
    }

    #[tokio::test]
    async fn nothing_is_printed_between_a_question_and_its_answer() {
        use crate::transfer::scheduler::capture::Captured;

        // One screen for the printer and the questions: what a terminal shows.
        static SCREEN: std::sync::OnceLock<Captured> = std::sync::OnceLock::new();
        static KEYS: std::sync::Mutex<Option<mpsc::Receiver<&'static str>>> =
            std::sync::Mutex::new(None);
        let screen = SCREEN.get_or_init(Captured::default).clone();
        let (typing, lines) = mpsc::channel();
        *KEYS.lock().unwrap() = Some(lines);
        let printer = Printer::to(screen.clone(), screen.clone());
        let terminal = std::sync::Arc::new(Terminal {
            printer: printer.clone(),
            keyboard: tokio::sync::Mutex::new(None),
            start: || {
                let keys = Typed {
                    lines: KEYS.lock().unwrap().take()?,
                    line: io::Cursor::new(""),
                };
                Keyboard::start(io::BufReader::new(keys), SCREEN.get()?.clone())
            },
        });

        printer.err("before").await;
        let asking = {
            let terminal = std::sync::Arc::clone(&terminal);
            tokio::spawn(async move { terminal.ask(String::from("first? ")).await })
        };
        until(|| screen.text().ends_with("first? ")).await;
        // Sent while the question waits: written once it is answered.
        let later = {
            let printer = printer.clone();
            tokio::spawn(async move { printer.err("during").await })
        };
        later.await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(screen.text(), "before\nfirst? ");

        typing.send("y\n").unwrap();
        assert!(asking.await.unwrap());
        printer.flush().await.unwrap();
        assert_eq!(screen.text(), "before\nfirst? during\n");

        // A no, and then the end of the input, which is a no as well.
        let second = {
            let terminal = std::sync::Arc::clone(&terminal);
            tokio::spawn(async move { terminal.ask(String::from("second? ")).await })
        };
        until(|| screen.text().ends_with("second? ")).await;
        typing.send("no\n").unwrap();
        assert!(!second.await.unwrap());
        drop(typing);
        assert!(!terminal.ask(String::from("third? ")).await);
        printer.err("after").await;
        printer.flush().await.unwrap();
        assert_eq!(
            screen.text(),
            "before\nfirst? during\nsecond? third? after\n"
        );
    }

    #[tokio::test]
    async fn a_question_whose_job_went_away_does_not_hold_the_output() {
        use crate::transfer::scheduler::capture::Captured;

        static SCREEN: std::sync::OnceLock<Captured> = std::sync::OnceLock::new();
        // Never typed on, and never closed: the read of the answer does not return.
        static KEYS: std::sync::Mutex<Option<mpsc::Receiver<&'static str>>> =
            std::sync::Mutex::new(None);
        let screen = SCREEN.get_or_init(Captured::default).clone();
        let (typing, lines) = mpsc::channel();
        *KEYS.lock().unwrap() = Some(lines);
        let printer = Printer::to(screen.clone(), screen.clone());
        let terminal = std::sync::Arc::new(Terminal {
            printer: printer.clone(),
            keyboard: tokio::sync::Mutex::new(None),
            start: || {
                let keys = Typed {
                    lines: KEYS.lock().unwrap().take()?,
                    line: io::Cursor::new(""),
                };
                Keyboard::start(io::BufReader::new(keys), SCREEN.get()?.clone())
            },
        });

        let asking = {
            let terminal = std::sync::Arc::clone(&terminal);
            tokio::spawn(async move { terminal.ask(String::from("replace? ")).await })
        };
        until(|| screen.text().ends_with("replace? ")).await;
        // The run stops (Ctrl-C): the job that asked is dropped.
        asking.abort();
        let _ = asking.await;

        printer.err("Summary").await;
        tokio::time::timeout(std::time::Duration::from_secs(10), printer.flush())
            .await
            .expect("the output is free again")
            .unwrap();
        assert_eq!(screen.text(), "replace? Summary\n");
        drop(typing);
    }

    #[test]
    fn only_a_y_or_yes_is_a_yes() {
        for answer in ["y\n", "Y", " yes \r\n", "YES"] {
            assert!(is_yes(answer), "{answer:?}");
        }
        for answer in ["", "\n", "n", "no", "yep", "replace", "s"] {
            assert!(!is_yes(answer), "{answer:?}");
        }
    }
}
