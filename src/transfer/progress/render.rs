//! The progress display (milestone 2, item 5 of docs/triage/README.md; #12, #44): a
//! [`ProgressSink`] that draws a transfer on stderr.
//!
//! - One file ([`ProgressDisplay::single`]): a bar with the bytes done of the size, the rate and
//!   the time left.
//! - A recursive transfer ([`ProgressDisplay::recursive`]): a line for the run (files done of
//!   those found, bytes done of the total, the rate, the time left, the failures), and below it a
//!   line per file in transfer, at most `--jobs`. While the walk still finds files the totals are
//!   `?`: [`ProgressEvent::SourceDone`] or [`ProgressEvent::Planned`] makes them known.
//!
//! The bytes are the confirmed bytes the jobs report: written to disk for a download,
//! acknowledged by Drive for an upload. A file that ends without being transferred (unchanged,
//! skipped, failed, cancelled) takes the bytes it did not transfer out of the total, and a job
//! that reported fewer bytes than its outcome counts the rest when it ends.
//!
//! The events only update a [`State`] (a lock and some arithmetic, cheap enough for any task);
//! a thread draws its [`View`] with indicatif ten times a second. The bars are indicatif's
//! `MultiProgress`, which [`super::terminal::suspend`] takes off the terminal while other output
//! is printed.
#![expect(
    dead_code,
    reason = "the commands draw progress from the next commit on"
)]

use std::{
    collections::{BTreeMap, VecDeque},
    io,
    num::NonZeroUsize,
    sync::{mpsc, Arc, Mutex, MutexGuard, PoisonError},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use indicatif::{MultiProgress, ProgressBar, ProgressDrawTarget, ProgressStyle};

use super::{
    terminal::{self, Drawing, Mode, Plain, Quiet, Stderr, Terminal},
    JobId, JobProgress, NoProgress, ProgressEvent, ProgressSink,
};
use crate::{
    common::human_size::human_size,
    transfer::scheduler::{JobKind, Outcome},
};

/// How often a terminal is redrawn.
const LIVE_HZ: u8 = 10;
/// How often the one line of `--progress` on something else than a terminal is rewritten.
const LINE_HZ: u8 = 1;
/// The first drawing waits this long, so that a transfer that ends at once draws nothing.
const FIRST_DRAW: Duration = Duration::from_millis(200);
/// The characters of a file's name or path on its line; a longer one keeps its end.
const LABEL_WIDTH: usize = 48;

/// What a display shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Layout {
    /// One file: its bar.
    Single,
    /// A recursive transfer: the run's line and the lines of up to `files` files in transfer.
    Run { files: usize },
}

/// Draws the progress of a transfer on stderr while it lives; see the module documentation.
/// Dropping it (or [`Self::finish`]) stops the drawing and takes the bars off the terminal.
pub(crate) struct ProgressDisplay {
    sink: Arc<dyn ProgressSink>,
    /// `None` when nothing is drawn.
    drawer: Option<Drawer>,
}

impl ProgressDisplay {
    /// The display of one transfer of `size` bytes (`None`: unknown), per `mode`, and the
    /// transfer's [`JobProgress`]. `content_on_stdout`: the file is written to stdout.
    #[must_use]
    pub(crate) fn single(
        mode: Mode,
        content_on_stdout: bool,
        size: Option<u64>,
    ) -> (ProgressDisplay, JobProgress) {
        let drawing = mode.drawing(Terminal::detect(), content_on_stdout);
        let display = Self::on_stderr(Layout::Single, drawing);
        let progress = display.start_single(size);
        (display, progress)
    }

    /// The display of a recursive transfer of `jobs` files at once, per `mode`; its events come
    /// from the scheduler ([`Self::sink`]).
    #[must_use]
    pub(crate) fn recursive(mode: Mode, jobs: NonZeroUsize) -> ProgressDisplay {
        let drawing = mode.drawing(Terminal::detect(), false);
        Self::on_stderr(Layout::Run { files: jobs.get() }, drawing)
    }

    /// The sink the events go to.
    #[must_use]
    pub(crate) fn sink(&self) -> Arc<dyn ProgressSink> {
        Arc::clone(&self.sink)
    }

    /// A display that draws nothing and reports to `sink`, for tests of what a command reports.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn reporting_to(sink: Arc<dyn ProgressSink>) -> ProgressDisplay {
        ProgressDisplay { sink, drawer: None }
    }

    /// A recursive display that draws its plain line ([`Drawing::Line`]) on `out`, for tests of
    /// what a command writes. Outside [`terminal::suspend`].
    #[cfg(test)]
    #[must_use]
    pub(crate) fn plain_on<W>(out: W) -> ProgressDisplay
    where
        W: io::Write + Send + 'static,
    {
        Self::open(Layout::Run { files: 0 }, Drawing::Line, false, move |hz| {
            plain_target(out, 80, hz)
        })
    }

    /// Stops drawing and takes the bars off the terminal.
    pub(crate) fn finish(mut self) {
        if let Some(drawer) = &mut self.drawer {
            drawer.stop();
        }
    }

    fn on_stderr(layout: Layout, drawing: Drawing) -> ProgressDisplay {
        Self::open(layout, drawing, true, |hz| match drawing {
            Drawing::Line => plain_target(io::stderr(), terminal::columns(), hz),
            Drawing::Live | Drawing::Off => {
                ProgressDrawTarget::term_like_with_hz(Box::new(Quiet::new(Stderr::new())), hz)
            }
        })
    }

    /// A display per `drawing` on the target that `target` makes for a refresh rate. `active`:
    /// [`terminal::suspend`] takes its bars off the terminal.
    fn open(
        layout: Layout,
        drawing: Drawing,
        active: bool,
        target: impl FnOnce(u8) -> ProgressDrawTarget,
    ) -> ProgressDisplay {
        let (layout, hz) = match drawing {
            Drawing::Off => {
                return ProgressDisplay {
                    sink: Arc::new(NoProgress),
                    drawer: None,
                }
            }
            Drawing::Live => (layout, LIVE_HZ),
            // One line only: no cursor movements that a log could not follow.
            Drawing::Line => match layout {
                Layout::Single => (layout, LINE_HZ),
                Layout::Run { .. } => (Layout::Run { files: 0 }, LINE_HZ),
            },
        };
        let shared = Arc::new(Shared {
            layout,
            state: Mutex::new(State::new(layout, Instant::now())),
        });
        let tick = Duration::from_millis(1000 / u64::from(hz));
        let drawer = Drawer::start(Arc::clone(&shared), target(hz), tick, active);
        ProgressDisplay {
            sink: shared,
            drawer,
        }
    }

    /// The one job of a single transfer, queued and started.
    fn start_single(&self, size: Option<u64>) -> JobProgress {
        let job = JobId(0);
        self.sink.event(&ProgressEvent::Queued {
            job,
            kind: JobKind::Media,
            label: "",
            size,
        });
        self.sink.event(&ProgressEvent::Started { job });
        JobProgress::new(job, self.sink())
    }
}

/// The target of [`Drawing::Line`]: `out`, `width` columns wide, with nothing but text, spaces
/// and carriage returns.
fn plain_target<W>(out: W, width: u16, hz: u8) -> ProgressDrawTarget
where
    W: io::Write + Send + 'static,
{
    ProgressDrawTarget::term_like_with_hz(Box::new(Quiet::new(Plain::new(out, width))), hz)
}

/// The sink of a display: the events update the state that the drawing thread reads.
struct Shared {
    layout: Layout,
    state: Mutex<State>,
}

impl Shared {
    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn view(&self, now: Instant) -> View {
        self.state().view(self.layout, now)
    }
}

impl ProgressSink for Shared {
    fn event(&self, event: &ProgressEvent<'_>) {
        self.state().apply(event, Instant::now());
    }
}

/// What the events said so far.
#[derive(Debug)]
struct State {
    /// The walk may find more files: until [`ProgressEvent::SourceDone`], unless the totals are
    /// planned. Never for a single file.
    discovering: bool,
    planned: Option<Planned>,
    /// Files queued.
    files_found: u64,
    /// Files that ended, however.
    files_done: u64,
    /// Files and folders that failed.
    failed: u64,
    bytes_done: u64,
    /// The sizes of the files queued (grown with content larger than listed).
    bytes_found: u64,
    /// What the files that did not end yet still have to transfer, as far as their sizes are
    /// known.
    bytes_pending: u64,
    /// Files that did not end yet and have no size.
    sizeless: u64,
    /// The files queued that did not end yet.
    files: BTreeMap<JobId, File>,
    rate: Rate,
}

/// [`ProgressEvent::Planned`].
#[derive(Debug, Clone, Copy)]
struct Planned {
    files: u64,
    bytes: u64,
}

#[derive(Debug)]
struct File {
    label: String,
    size: Option<u64>,
    done: u64,
    /// Since the file started; `None` while it waits.
    rate: Option<Rate>,
}

impl File {
    fn remaining(&self) -> u64 {
        self.size.map_or(0, |size| size.saturating_sub(self.done))
    }
}

impl State {
    fn new(layout: Layout, now: Instant) -> Self {
        State {
            discovering: matches!(layout, Layout::Run { .. }),
            planned: None,
            files_found: 0,
            files_done: 0,
            failed: 0,
            bytes_done: 0,
            bytes_found: 0,
            bytes_pending: 0,
            sizeless: 0,
            files: BTreeMap::new(),
            rate: Rate::new(now),
        }
    }

    fn apply(&mut self, event: &ProgressEvent<'_>, now: Instant) {
        match *event {
            ProgressEvent::Planned { files, bytes } => {
                self.planned = Some(Planned { files, bytes });
                self.discovering = false;
            }
            ProgressEvent::Queued {
                job,
                kind: JobKind::Media,
                label,
                size,
            } => {
                self.files_found += 1;
                match size {
                    Some(size) => {
                        self.bytes_found += size;
                        self.bytes_pending += size;
                    }
                    None => self.sizeless += 1,
                }
                let file = File {
                    label: printable(label),
                    size,
                    done: 0,
                    rate: None,
                };
                self.files.insert(job, file);
            }
            // Folders count only when they fail.
            ProgressEvent::Queued { .. } => {}
            ProgressEvent::Started { job } => {
                if let Some(file) = self.files.get_mut(&job) {
                    file.rate = Some(Rate::new(now));
                }
            }
            ProgressEvent::Bytes { job, bytes } => self.add_bytes(job, bytes, now),
            ProgressEvent::Finished {
                job, kind, outcome, ..
            } => self.finish(job, kind, outcome, now),
            ProgressEvent::SourceDone => self.discovering = false,
            // Planned files below a folder that failed: they will never come.
            ProgressEvent::SubtreeCancelled { files, bytes, .. } => {
                if let Some(planned) = &mut self.planned {
                    planned.files = planned.files.saturating_sub(files);
                    planned.bytes = planned.bytes.saturating_sub(bytes);
                }
            }
        }
    }

    fn add_bytes(&mut self, job: JobId, bytes: u64, now: Instant) {
        self.bytes_done += bytes;
        self.rate.record(now, self.bytes_done);
        let Some(file) = self.files.get_mut(&job) else {
            return;
        };
        let remaining = file.remaining();
        file.done += bytes;
        if let Some(size) = file.size {
            if file.done > size {
                // More content than listed (the file changed): its size grows with it.
                self.bytes_found += file.done - size;
                file.size = Some(file.done);
            }
        }
        self.bytes_pending = self
            .bytes_pending
            .saturating_sub(remaining.saturating_sub(file.remaining()));
        if let Some(rate) = &mut file.rate {
            rate.record(now, file.done);
        }
    }

    fn finish(&mut self, job: JobId, kind: JobKind, outcome: &Outcome, now: Instant) {
        if matches!(outcome, Outcome::Failed { .. }) {
            self.failed += 1;
        }
        if kind != JobKind::Media {
            return;
        }
        self.files_done += 1;
        let Some(file) = self.files.remove(&job) else {
            return;
        };
        // What it did not transfer leaves the total.
        self.bytes_pending = self.bytes_pending.saturating_sub(file.remaining());
        if file.size.is_none() {
            self.sizeless = self.sizeless.saturating_sub(1);
        }
        // Bytes it transferred without reporting them on the way count now.
        if let Outcome::Transferred { bytes } = *outcome {
            if bytes > file.done {
                self.bytes_done += bytes - file.done;
                self.rate.record(now, self.bytes_done);
            }
        }
    }

    fn view(&self, layout: Layout, now: Instant) -> View {
        let planned_rest = self
            .planned
            .map_or(0, |planned| planned.bytes.saturating_sub(self.bytes_found));
        let bytes_known = self.bytes_done + self.bytes_pending + planned_rest;
        let files_total = (!self.discovering).then(|| {
            self.planned.map_or(self.files_found, |planned| {
                planned.files.max(self.files_found)
            })
        });
        let bytes_total = (!self.discovering && self.sizeless == 0).then_some(bytes_known);
        let rate = self.rate.per_sec(now);
        let eta = match (bytes_total, rate) {
            (Some(total), Some(rate)) if rate > 0 => Some(Duration::from_secs(
                total.saturating_sub(self.bytes_done).div_ceil(rate),
            )),
            _ => None,
        };
        let lines = match layout {
            Layout::Single => 0,
            Layout::Run { files } => files,
        };
        let files = self
            .files
            .iter()
            .filter_map(|(job, file)| {
                let rate = file.rate.as_ref()?;
                Some(FileView {
                    job: *job,
                    label: file.label.clone(),
                    done: file.done,
                    size: file.size,
                    rate: rate.per_sec(now),
                })
            })
            .take(lines)
            .collect();
        View {
            discovering: self.discovering,
            files_done: self.files_done,
            files_found: self.files_found,
            files_total,
            bytes_done: self.bytes_done,
            bytes_known,
            bytes_total,
            failed: self.failed,
            rate,
            eta,
            files,
        }
    }
}

/// What a display draws, at one moment.
#[derive(Debug, Clone, PartialEq, Eq)]
struct View {
    discovering: bool,
    files_done: u64,
    files_found: u64,
    /// `None` while the walk may find more files.
    files_total: Option<u64>,
    bytes_done: u64,
    /// Done, and what the files found still have to transfer as far as known.
    bytes_known: u64,
    /// `None` while the walk may find more files or a file without a size did not end.
    bytes_total: Option<u64>,
    failed: u64,
    /// Bytes per second.
    rate: Option<u64>,
    eta: Option<Duration>,
    /// The files in transfer, in the order they started, at most the layout's lines.
    files: Vec<FileView>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FileView {
    job: JobId,
    label: String,
    done: u64,
    size: Option<u64>,
    rate: Option<u64>,
}

/// The rate at which a count grows: over the last [`Rate::WINDOW`], and at least since the
/// change before the last one, so that a count that grows in steps (an upload confirms a chunk
/// at a time) does not read as stalled between them.
#[derive(Debug, Clone)]
struct Rate {
    /// When the count changed, and to what; the first is the start (0). At most one point per
    /// [`Rate::RESOLUTION`].
    points: VecDeque<(Instant, u64)>,
}

impl Rate {
    const WINDOW: Duration = Duration::from_secs(8);
    const RESOLUTION: Duration = Duration::from_millis(100);
    /// No rate before there is this much time to measure over.
    const MIN_SPAN: Duration = Duration::from_secs(1);

    fn new(now: Instant) -> Self {
        Rate {
            points: VecDeque::from([(now, 0)]),
        }
    }

    fn record(&mut self, now: Instant, count: u64) {
        let len = self.points.len();
        let close =
            len > 1 && now.saturating_duration_since(self.points[len - 2].0) < Self::RESOLUTION;
        if close {
            self.points.pop_back();
        }
        self.points.push_back((now, count));
        while self.points.len() > 2
            && now.saturating_duration_since(self.points[1].0) >= Self::WINDOW
        {
            self.points.pop_front();
        }
    }

    /// Per second.
    fn per_sec(&self, now: Instant) -> Option<u64> {
        let (since, first) = *self.points.front()?;
        let (_, last) = *self.points.back()?;
        let span = now.saturating_duration_since(since);
        if span < Self::MIN_SPAN {
            return None;
        }
        let per_sec = u128::from(last.saturating_sub(first)) * 1_000_000 / span.as_micros();
        Some(u64::try_from(per_sec).unwrap_or(u64::MAX))
    }
}

/// `label` without control characters, which would move the cursor or break the line.
fn printable(label: &str) -> String {
    label
        .chars()
        .map(|c| if c.is_control() { '?' } else { c })
        .collect()
}

/// `label` cut to its last [`LABEL_WIDTH`] characters: the end of a path names the file.
fn shortened(label: &str) -> String {
    let count = label.chars().count();
    if count <= LABEL_WIDTH {
        return label.to_owned();
    }
    let tail = label
        .chars()
        .skip(count - (LABEL_WIDTH - 3))
        .collect::<String>();
    format!("...{tail}")
}

/// `12.3 MB/40 MB (30%)`, or `12.3 MB` without a size.
fn amount(done: u64, size: Option<u64>) -> String {
    match size {
        Some(size) => format!(
            "{}/{} ({}%)",
            human_size(done),
            human_size(size),
            percent(done, size)
        ),
        None => human_size(done),
    }
}

fn percent(done: u64, total: u64) -> u64 {
    if total == 0 {
        return 100;
    }
    let percent = u128::from(done.min(total)) * 100 / u128::from(total);
    u64::try_from(percent).unwrap_or(100)
}

/// `2.1 MB/s`.
fn rate_text(rate: u64) -> String {
    format!("{}/s", human_size(rate))
}

/// `ETA 42s`, `ETA 3m 05s`, `ETA 2h 07m`.
fn eta_text(eta: Duration) -> String {
    let secs = eta.as_secs();
    match secs {
        0..60 => format!("ETA {secs}s"),
        60..3600 => format!("ETA {}m {:02}s", secs / 60, secs % 60),
        _ => format!("ETA {}h {:02}m", secs / 3600, secs % 3600 / 60),
    }
}

/// The text of a single file's bar: `12.3 MB/40 MB (30%), 2.1 MB/s, ETA 13s`.
fn single_line(view: &View) -> String {
    let mut parts = vec![amount(view.bytes_done, view.bytes_total)];
    parts.extend(view.rate.map(rate_text));
    parts.extend(view.eta.map(eta_text));
    parts.join(", ")
}

/// The run's line: `3/12 files, 45.6 MB/120 MB (38%), 4.2 MB/s, ETA 18s, 1 failed`, with `?` for
/// what the walk may still add to: `3/? files (12 found), 45.6 MB/? (120 MB found), ...`.
fn run_line(view: &View) -> String {
    let files = match view.files_total {
        Some(total) => format!("{}/{total} files", view.files_done),
        None => format!("{}/? files ({} found)", view.files_done, view.files_found),
    };
    let bytes = match view.bytes_total {
        Some(total) => amount(view.bytes_done, Some(total)),
        None if view.discovering => format!(
            "{}/? ({} found)",
            human_size(view.bytes_done),
            human_size(view.bytes_known)
        ),
        // Files without a size are still in transfer: at least this much.
        None => format!(
            "{}/{}+",
            human_size(view.bytes_done),
            human_size(view.bytes_known)
        ),
    };
    let mut parts = vec![files, bytes];
    parts.extend(view.rate.map(rate_text));
    parts.extend(view.eta.map(eta_text));
    if view.failed > 0 {
        parts.push(format!("{} failed", view.failed));
    }
    parts.join(", ")
}

/// What follows a file's bar on its line: `12.3 MB/40 MB, 2.1 MB/s`, or `12.3 MB, 2.1 MB/s`
/// without a size. The bar shows the share.
fn file_line(file: &FileView) -> String {
    let amount = match file.size {
        Some(size) => format!("{}/{}", human_size(file.done), human_size(size)),
        None => human_size(file.done),
    };
    let mut parts = vec![amount];
    parts.extend(file.rate.map(rate_text));
    parts.join(", ")
}

/// The looks of the bars.
struct Styles {
    /// The main line with a known total (a bar), and without one (a spinner).
    bar: ProgressStyle,
    spinner: ProgressStyle,
    /// A file's line with a known size, and without one.
    file_bar: ProgressStyle,
    file_spinner: ProgressStyle,
}

impl Styles {
    fn new() -> Self {
        Styles {
            bar: style("[{wide_bar}] {msg}"),
            spinner: style("{spinner} {msg}"),
            // The numbers in a column of their own, so that the bars of the files line up.
            file_bar: style("  {wide_msg} [{bar:20}] {prefix:30}"),
            file_spinner: style("  {wide_msg} {prefix:30}"),
        }
    }
}

/// ASCII only, for any console.
fn style(template: &str) -> ProgressStyle {
    ProgressStyle::with_template(template)
        .unwrap_or_else(|_| ProgressStyle::default_spinner())
        .progress_chars("=> ")
        .tick_chars("-\\|/ ")
}

/// A bar and whether its style is the one for a known total (`None` before it got one).
struct Bar {
    bar: ProgressBar,
    sized: Option<bool>,
}

impl Bar {
    fn new(multi: &MultiProgress, message: String) -> Self {
        let bar =
            ProgressBar::with_draw_target(None, ProgressDrawTarget::hidden()).with_message(message);
        Bar {
            bar: multi.add(bar),
            sized: None,
        }
    }

    /// Shows `done` of `total`: a bar with a total, a spinner without one.
    fn show(
        &mut self,
        done: u64,
        total: Option<u64>,
        bar: &ProgressStyle,
        spinner: &ProgressStyle,
    ) {
        if self.sized != Some(total.is_some()) {
            let style = if total.is_some() { bar } else { spinner };
            self.bar.set_style(style.clone());
            self.sized = Some(total.is_some());
        }
        match total {
            Some(total) => {
                self.bar.set_length(total);
                self.bar.set_position(done.min(total));
            }
            None => self.bar.tick(),
        }
    }
}

/// The bars of a display, which its drawing thread owns.
struct Bars {
    multi: MultiProgress,
    layout: Layout,
    styles: Styles,
    main: Bar,
    files: BTreeMap<JobId, Bar>,
}

impl Bars {
    fn new(target: ProgressDrawTarget, layout: Layout) -> Self {
        let multi = MultiProgress::with_draw_target(target);
        let main = Bar::new(&multi, String::new());
        Bars {
            multi,
            layout,
            styles: Styles::new(),
            main,
            files: BTreeMap::new(),
        }
    }

    fn draw(&mut self, view: &View) {
        let styles = &self.styles;
        let line = match self.layout {
            Layout::Single => single_line(view),
            Layout::Run { .. } => run_line(view),
        };
        self.main.show(
            view.bytes_done,
            view.bytes_total,
            &styles.bar,
            &styles.spinner,
        );
        self.main.bar.set_message(line);

        let multi = &self.multi;
        self.files.retain(|job, bar| {
            let shown = view.files.iter().any(|file| file.job == *job);
            if !shown {
                multi.remove(&bar.bar);
            }
            shown
        });
        for file in &view.files {
            let bar = self
                .files
                .entry(file.job)
                .or_insert_with(|| Bar::new(multi, shortened(&file.label)));
            bar.show(file.done, file.size, &styles.file_bar, &styles.file_spinner);
            bar.bar.set_prefix(file_line(file));
        }
    }

    /// Takes the bars off the terminal for good.
    fn clear(self) {
        for bar in self.files.values() {
            self.multi.remove(&bar.bar);
        }
        self.multi.remove(&self.main.bar);
        // The terminal is a Quiet one: a failure ends the drawing, and there is nothing to undo.
        let _ = self.multi.clear();
        // A `suspend` that took the bars just before they went away draws nothing either.
        self.multi.set_draw_target(ProgressDrawTarget::hidden());
    }
}

/// The thread that draws a display, and the way to stop it.
struct Drawer {
    stop: mpsc::Sender<()>,
    thread: Option<JoinHandle<()>>,
    /// The bars are the ones [`terminal::suspend`] takes off the terminal.
    active: bool,
}

impl Drawer {
    /// Draws `shared` on `target` every `tick`. `None` when no thread can be started: the
    /// transfer goes on without bars.
    fn start(
        shared: Arc<Shared>,
        target: ProgressDrawTarget,
        tick: Duration,
        active: bool,
    ) -> Option<Drawer> {
        let bars = Bars::new(target, shared.layout);
        if active {
            terminal::activate(&bars.multi);
        }
        let (stop, stopped) = mpsc::channel::<()>();
        let thread = thread::Builder::new()
            .name(String::from("progress"))
            .spawn(move || {
                let mut bars = bars;
                let mut wait = FIRST_DRAW;
                // A message or a closed channel: stop.
                while stopped.recv_timeout(wait) == Err(mpsc::RecvTimeoutError::Timeout) {
                    bars.draw(&shared.view(Instant::now()));
                    wait = tick;
                }
                bars.clear();
            });
        let Ok(thread) = thread else {
            if active {
                terminal::deactivate();
            }
            return None;
        };
        Some(Drawer {
            stop,
            thread: Some(thread),
            active,
        })
    }

    /// Stops the thread once it took the bars off the terminal. Once.
    fn stop(&mut self) {
        let Some(thread) = self.thread.take() else {
            return;
        };
        let _ = self.stop.send(());
        // A drawing thread that panicked left at worst a bar on the screen.
        let _ = thread.join();
        if self.active {
            terminal::deactivate();
        }
    }
}

impl Drop for Drawer {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests;
