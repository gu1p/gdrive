//! The single writer to stdout and stderr of a run.

use std::{
    error::Error,
    fmt::{self, Display, Formatter},
    io::{self, Write},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex, PoisonError,
    },
};

use tokio::sync::{mpsc, oneshot};

use crate::transfer::progress;

/// The only writer to stdout and stderr while a run goes on: one task writes the lines that the
/// clones send, each whole and in the order received, above the progress bars when a display
/// draws ([`progress::suspend`]). Cloning is cheap; the task ends when the last clone is
/// dropped.
///
/// A stream that fails takes no further lines. A closed pipe (the reader went away, as in
/// `| head`) ends that output quietly, as it ends a listing; any other failure (a full disk) is
/// kept: [`Self::failed`] tells the scheduler, which stops the run unless it continues on
/// errors, and [`Self::flush`] returns it, so that the command fails.
#[derive(Debug, Clone)]
pub(crate) struct Printer {
    sender: mpsc::Sender<Message>,
    state: Arc<State>,
}

#[derive(Debug)]
enum Message {
    Line {
        target: Target,
        text: String,
    },
    Flush(oneshot::Sender<()>),
    /// Everything before is written, which `held` is told; nothing more is written until
    /// `released` resolves or its sender is dropped.
    Hold {
        held: oneshot::Sender<()>,
        released: oneshot::Receiver<()>,
    },
}

/// Keeps a [`Printer`] from writing until it is dropped ([`Printer::hold`]).
#[derive(Debug)]
pub(crate) struct Hold {
    _release: oneshot::Sender<()>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Target {
    Stdout,
    Stderr,
}

impl Display for Target {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Target::Stdout => "stdout",
            Target::Stderr => "stderr",
        })
    }
}

/// The first failure of a printer's output.
#[derive(Debug, Clone)]
pub(crate) struct OutputError {
    target: Target,
    source: Arc<io::Error>,
}

impl Display for OutputError {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "unable to write to {}", self.target)
    }
}

impl Error for OutputError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(self.source.as_ref())
    }
}

#[derive(Debug, Default)]
struct State {
    failed: AtomicBool,
    error: Mutex<Option<OutputError>>,
}

impl State {
    fn record(&self, error: OutputError) {
        let mut first = self.error.lock().unwrap_or_else(PoisonError::into_inner);
        first.get_or_insert(error);
        self.failed.store(true, Ordering::SeqCst);
    }
}

/// One stream, as the printer's task writes it.
struct Output<W> {
    writer: W,
    target: Target,
    open: bool,
}

impl<W: Write> Output<W> {
    fn new(writer: W, target: Target) -> Self {
        Output {
            writer,
            target,
            open: true,
        }
    }

    fn line(&mut self, text: &str, state: &State) {
        if self.open {
            let result = writeln!(self.writer, "{text}").and_then(|()| self.writer.flush());
            self.check(result, state);
        }
    }

    fn flush(&mut self, state: &State) {
        if self.open {
            let result = self.writer.flush();
            self.check(result, state);
        }
    }

    fn check(&mut self, result: io::Result<()>, state: &State) {
        let Err(error) = result else {
            return;
        };
        self.open = false;
        if error.kind() != io::ErrorKind::BrokenPipe {
            state.record(OutputError {
                target: self.target,
                source: Arc::new(error),
            });
        }
    }
}

impl Printer {
    /// Lines waiting for the writer before a sender waits.
    const CAPACITY: usize = 256;
    /// Lines written at once, when that many wait.
    const BATCH: usize = 64;

    /// A printer to the process's stdout and stderr, on the current runtime.
    #[must_use]
    pub(crate) fn stdio() -> Printer {
        Printer::to(io::stdout(), io::stderr())
    }

    /// A printer to `out` and `err`, on the current runtime.
    #[must_use]
    pub(crate) fn to<O, E>(out: O, err: E) -> Printer
    where
        O: Write + Send + 'static,
        E: Write + Send + 'static,
    {
        let (sender, mut messages) = mpsc::channel(Self::CAPACITY);
        let state = Arc::new(State::default());
        let task_state = Arc::clone(&state);
        tokio::spawn(async move {
            let state = task_state;
            let mut out = Output::new(out, Target::Stdout);
            let mut err = Output::new(err, Target::Stderr);
            let mut batch = Vec::with_capacity(Self::BATCH);
            while let Some(message) = messages.recv().await {
                // A hold ends its batch: it is waited for once what came before is written.
                let mut hold = None;
                let mut next = Some(message);
                while let Some(message) = next.take() {
                    match message {
                        Message::Hold { held, released } => hold = Some((held, released)),
                        message => {
                            batch.push(message);
                            if batch.len() < Self::BATCH {
                                next = messages.try_recv().ok();
                            }
                        }
                    }
                }
                // Above the progress bars, if any: they are taken off the terminal once per
                // batch, not per line. A hold that came alone has nothing to write before it.
                if !batch.is_empty() {
                    progress::suspend(|| {
                        for message in batch.drain(..) {
                            match message {
                                Message::Line {
                                    target: Target::Stdout,
                                    text,
                                } => out.line(&text, &state),
                                Message::Line {
                                    target: Target::Stderr,
                                    text,
                                } => err.line(&text, &state),
                                Message::Flush(done) => {
                                    out.flush(&state);
                                    err.flush(&state);
                                    let _ = done.send(());
                                }
                                // Never in a batch.
                                Message::Hold { .. } => {}
                            }
                        }
                    });
                }
                if let Some((held, released)) = hold {
                    out.flush(&state);
                    err.flush(&state);
                    // Until the holder lets go; one that went away holds nothing.
                    if held.send(()).is_ok() {
                        let _ = released.await;
                    }
                }
            }
        });
        Printer { sender, state }
    }

    /// Prints `line` and a newline to stdout.
    pub(crate) async fn out(&self, line: impl Into<String>) {
        self.send(Target::Stdout, line.into()).await;
    }

    /// Prints `line` and a newline to stderr.
    pub(crate) async fn err(&self, line: impl Into<String>) {
        self.send(Target::Stderr, line.into()).await;
    }

    /// Whether writing failed, other than by a closed pipe.
    #[must_use]
    pub(crate) fn failed(&self) -> bool {
        self.state.failed.load(Ordering::SeqCst)
    }

    /// Waits until every line sent before is written, so that output printed directly
    /// afterwards follows it; the first failure of the output, if any, is the error.
    pub(crate) async fn flush(&self) -> Result<(), OutputError> {
        let (done, written) = oneshot::channel();
        if self.sender.send(Message::Flush(done)).await.is_ok() {
            let _ = written.await;
        }
        let error = self
            .state
            .error
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        match &*error {
            Some(error) => Err(error.clone()),
            None => Ok(()),
        }
    }

    /// Waits until every line sent before is written, and keeps the lines sent from then on
    /// from being written until the returned guard is dropped: for a question to the user,
    /// which nothing may follow on the terminal before its answer. The lines wait in order, and
    /// their senders with them once [`Self::CAPACITY`] of them wait.
    pub(crate) async fn hold(&self) -> Hold {
        let (held, holding) = oneshot::channel();
        let (release, released) = oneshot::channel();
        if self
            .sender
            .send(Message::Hold { held, released })
            .await
            .is_ok()
        {
            let _ = holding.await;
        }
        Hold { _release: release }
    }

    async fn send(&self, target: Target, text: String) {
        // The task only ends with the last sender, so this cannot fail while `self` lives.
        let _ = self.sender.send(Message::Line { target, text }).await;
    }
}
