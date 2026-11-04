//! The accounting of the display on events with chosen times, and its drawing on a model of a
//! terminal that the bars and the printer share.

use std::{
    future,
    io::{self, Write},
    num::NonZeroUsize,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use futures::stream;
use indicatif::{ProgressDrawTarget, TermLike};

use super::{
    eta_text, file_line, plain_target, run_line, shortened, single_line, FileView, Layout,
    ProgressDisplay, Rate, Shared, State, View, FIRST_DRAW,
};
use crate::transfer::{
    progress::{
        terminal::{Drawing, Mode, Terminal},
        JobId, ProgressEvent, ProgressSink,
    },
    scheduler::{
        capture::{output, Captured},
        Job, JobKind, Options, Outcome, Printer, Scheduler, SkipReason,
    },
};

const MB: u64 = 1_000_000;

/// Events at chosen times, in milliseconds after the start.
struct Feed {
    state: State,
    layout: Layout,
    start: Instant,
}

impl Feed {
    fn new(layout: Layout) -> Self {
        let start = Instant::now();
        Feed {
            state: State::new(layout, start),
            layout,
            start,
        }
    }

    fn at(&self, millis: u64) -> Instant {
        self.start + Duration::from_millis(millis)
    }

    fn event(&mut self, millis: u64, event: &ProgressEvent<'_>) {
        let now = self.at(millis);
        self.state.apply(event, now);
    }

    fn queued(&mut self, job: u64, label: &str, size: Option<u64>) {
        self.event(
            0,
            &ProgressEvent::Queued {
                job: JobId(job),
                kind: JobKind::Media,
                label,
                size,
            },
        );
    }

    fn started(&mut self, millis: u64, job: u64) {
        self.event(millis, &ProgressEvent::Started { job: JobId(job) });
    }

    fn bytes(&mut self, millis: u64, job: u64, bytes: u64) {
        self.event(
            millis,
            &ProgressEvent::Bytes {
                job: JobId(job),
                bytes,
            },
        );
    }

    fn finished(&mut self, millis: u64, job: u64, kind: JobKind, outcome: &Outcome) {
        self.event(
            millis,
            &ProgressEvent::Finished {
                job: JobId(job),
                kind,
                label: "",
                outcome,
            },
        );
    }

    fn view(&self, millis: u64) -> View {
        self.state.view(self.layout, self.at(millis))
    }
}

fn failure() -> Outcome {
    Outcome::failed(io::Error::other("lost"))
}

#[test]
fn totals_are_unknown_until_the_walk_ends() {
    let mut feed = Feed::new(Layout::Run { files: 4 });
    feed.queued(0, "a/1.bin", Some(10 * MB));
    feed.queued(1, "a/2.bin", Some(20 * MB));
    feed.started(0, 0);
    feed.started(0, 1);
    feed.bytes(500, 0, 4 * MB);
    feed.bytes(1_000, 1, 6 * MB);

    let view = feed.view(2_000);
    assert!(view.discovering);
    assert_eq!(
        (view.files_done, view.files_found, view.files_total),
        (0, 2, None)
    );
    assert_eq!((view.bytes_done, view.bytes_known), (10 * MB, 30 * MB));
    assert_eq!(view.bytes_total, None);
    assert_eq!(view.eta, None, "no time left without a total");
    assert_eq!(
        run_line(&view),
        "0/? files (2 found), 10 MB/? (30 MB found), 5 MB/s"
    );

    feed.queued(2, "b/3.bin", Some(30 * MB));
    feed.event(2_000, &ProgressEvent::SourceDone);
    let view = feed.view(2_000);
    assert!(!view.discovering);
    assert_eq!(view.files_total, Some(3));
    assert_eq!(view.bytes_total, Some(60 * MB));
    // 50 MB left at 5 MB/s.
    assert_eq!(view.eta, Some(Duration::from_secs(10)));
    assert_eq!(
        run_line(&view),
        "0/3 files, 10 MB/60 MB (16%), 5 MB/s, ETA 10s"
    );
}

#[test]
fn files_that_end_without_a_transfer_leave_the_total() {
    let mut feed = Feed::new(Layout::Run { files: 4 });
    for (job, size) in [(0, 10), (1, 20), (2, 30), (3, 40)] {
        feed.queued(job, &format!("{job}.bin"), Some(size * MB));
        feed.started(0, job);
    }
    // A document, decided without running.
    feed.queued(4, "notes", None);
    feed.event(0, &ProgressEvent::SourceDone);

    feed.bytes(100, 0, 10 * MB);
    feed.finished(
        100,
        0,
        JobKind::Media,
        &Outcome::Transferred { bytes: 10 * MB },
    );
    feed.finished(200, 1, JobKind::Media, &Outcome::Unchanged);
    feed.bytes(300, 2, 5 * MB);
    feed.finished(300, 2, JobKind::Media, &failure());
    feed.finished(
        300,
        4,
        JobKind::Media,
        &Outcome::Skipped {
            reason: SkipReason::GoogleDocument,
        },
    );
    feed.finished(300, 5, JobKind::Metadata, &failure());

    let view = feed.view(400);
    assert_eq!((view.files_done, view.files_total), (4, Some(5)));
    assert_eq!(view.failed, 2, "a file and a folder");
    // Done: 10 + the 5 of the failed file. Left: the 40 of the running one.
    assert_eq!(view.bytes_done, 15 * MB);
    assert_eq!(view.bytes_total, Some(55 * MB));
    let running = view.files.iter().map(|file| file.job).collect::<Vec<_>>();
    assert_eq!(running, [JobId(3)]);
}

#[test]
fn bytes_reported_only_at_the_end_count_then() {
    let mut feed = Feed::new(Layout::Run { files: 1 });
    feed.queued(0, "a.bin", Some(10 * MB));
    feed.queued(1, "doc.pdf", None);
    feed.event(0, &ProgressEvent::SourceDone);
    feed.started(0, 0);
    feed.started(0, 1);
    feed.bytes(500, 0, 2 * MB);
    feed.bytes(500, 1, 3 * MB);

    let view = feed.view(500);
    assert_eq!(view.bytes_total, None, "a file without a size is running");
    assert_eq!(view.bytes_known, 13 * MB);
    assert_eq!(
        run_line(&view),
        "0/2 files, 5 MB/13 MB+",
        "no rate before a second"
    );

    feed.finished(
        1_000,
        0,
        JobKind::Media,
        &Outcome::Transferred { bytes: 10 * MB },
    );
    feed.finished(
        1_000,
        1,
        JobKind::Media,
        &Outcome::Transferred { bytes: 4 * MB },
    );
    let view = feed.view(1_000);
    assert_eq!(view.bytes_done, 14 * MB);
    assert_eq!(view.bytes_total, Some(14 * MB));
    assert_eq!(view.files, []);
}

#[test]
fn a_file_larger_than_listed_grows_the_total() {
    let mut feed = Feed::new(Layout::Single);
    feed.queued(0, "", Some(10 * MB));
    feed.started(0, 0);
    feed.bytes(500, 0, 8 * MB);
    feed.bytes(1_000, 0, 4 * MB);

    let view = feed.view(1_000);
    assert_eq!(view.bytes_done, 12 * MB);
    assert_eq!(view.bytes_total, Some(12 * MB));
    assert_eq!(single_line(&view), "12 MB/12 MB (100%), 12 MB/s, ETA 0s");
}

#[test]
fn planned_totals_are_known_before_the_jobs() {
    let mut feed = Feed::new(Layout::Run { files: 2 });
    feed.event(
        0,
        &ProgressEvent::Planned {
            files: 3,
            bytes: 60 * MB,
        },
    );
    // The upload's top folder: the source is done, its files come later.
    feed.event(0, &ProgressEvent::SourceDone);
    let view = feed.view(0);
    assert_eq!(view.files_total, Some(3));
    assert_eq!(view.bytes_total, Some(60 * MB));

    feed.queued(1, "dir/a.bin", Some(10 * MB));
    feed.started(0, 1);
    feed.bytes(2_000, 1, 10 * MB);
    feed.finished(
        2_000,
        1,
        JobKind::Media,
        &Outcome::Transferred { bytes: 10 * MB },
    );
    let view = feed.view(2_000);
    assert_eq!((view.files_done, view.files_total), (1, Some(3)));
    assert_eq!(
        (view.bytes_done, view.bytes_total),
        (10 * MB, Some(60 * MB))
    );
    assert_eq!(
        run_line(&view),
        "1/3 files, 10 MB/60 MB (16%), 5 MB/s, ETA 10s"
    );
}

#[test]
fn files_below_a_failed_folder_leave_the_planned_totals() {
    let mut feed = Feed::new(Layout::Run { files: 2 });
    feed.event(
        0,
        &ProgressEvent::Planned {
            files: 6,
            bytes: 20,
        },
    );
    feed.event(0, &ProgressEvent::SourceDone);
    feed.queued(1, "photos/a.txt", Some(4));
    feed.started(0, 1);
    feed.bytes(1_000, 1, 4);
    feed.finished(1_000, 1, JobKind::Media, &Outcome::Transferred { bytes: 4 });
    feed.finished(1_000, 2, JobKind::Metadata, &failure());
    feed.event(
        1_000,
        &ProgressEvent::SubtreeCancelled {
            files: 3,
            bytes: 13,
        },
    );

    let view = feed.view(1_000);
    assert_eq!((view.files_done, view.files_total), (1, Some(3)));
    assert_eq!((view.bytes_done, view.bytes_total), (4, Some(7)));
    assert_eq!(view.failed, 1);
    // 3 bytes left at 4 B/s.
    assert_eq!(view.eta, Some(Duration::from_secs(1)));
}

#[test]
fn file_lines_are_the_running_files_in_queue_order_up_to_the_jobs() {
    let mut feed = Feed::new(Layout::Run { files: 2 });
    for job in 0..4 {
        feed.queued(job, &format!("f{job}"), Some(MB));
    }
    feed.started(0, 2);
    feed.started(0, 0);
    feed.started(0, 3);
    feed.bytes(2_000, 0, MB / 2);

    let view = feed.view(2_000);
    assert_eq!(
        view.files,
        [
            FileView {
                job: JobId(0),
                label: String::from("f0"),
                done: MB / 2,
                size: Some(MB),
                rate: Some(MB / 4),
            },
            FileView {
                job: JobId(2),
                label: String::from("f2"),
                done: 0,
                size: Some(MB),
                rate: Some(0),
            },
        ]
    );

    assert_eq!(file_line(&view.files[0]), "500 KB/1 MB, 250 KB/s");
    let unsized_file = FileView {
        size: None,
        rate: None,
        ..view.files[0].clone()
    };
    assert_eq!(file_line(&unsized_file), "500 KB");

    let single = Feed::new(Layout::Single);
    assert_eq!(single.view(0).files, []);
}

#[test]
fn labels_cannot_move_the_cursor() {
    let mut feed = Feed::new(Layout::Run { files: 1 });
    feed.queued(0, "evil\u{1b}[2J\nname", Some(1));
    feed.started(0, 0);
    assert_eq!(feed.view(0).files[0].label, "evil?[2J?name");

    let long = format!("{}/file.bin", "d".repeat(60));
    let short = shortened(&long);
    assert_eq!(short.chars().count(), super::LABEL_WIDTH);
    assert!(short.starts_with("...") && short.ends_with("/file.bin"));
    assert_eq!(shortened("a/b.bin"), "a/b.bin");
}

#[test]
fn a_steady_rate_is_measured_over_the_last_seconds() {
    let start = Instant::now();
    let at = |millis: u64| start + Duration::from_millis(millis);
    let mut rate = Rate::new(start);

    assert_eq!(rate.per_sec(at(500)), None);
    // 1 MB/s for 10 s, in pieces of 10 KB.
    for tick in 1..=1_000 {
        rate.record(at(tick * 10), tick * 10_000);
    }
    assert_eq!(rate.per_sec(at(10_000)), Some(MB));
    assert!(rate.points.len() <= 100, "{} points", rate.points.len());

    // Then 3 MB/s for 8 s: the rate is that of the last seconds only.
    for tick in 1..=800 {
        rate.record(at(10_000 + tick * 10), 10 * MB + tick * 30_000);
    }
    let per_sec = rate.per_sec(at(18_000)).unwrap();
    assert!((2_900_000..=3_000_000).contains(&per_sec), "{per_sec}");
}

#[test]
fn a_rate_in_steps_does_not_drop_to_zero_between_them() {
    let start = Instant::now();
    let at = |secs: u64| start + Duration::from_secs(secs);
    let mut rate = Rate::new(start);

    // An upload that Drive confirms 32 MB at a time, every 32 s: 1 MB/s.
    rate.record(at(32), 32 * MB);
    assert_eq!(rate.per_sec(at(40)), Some(32 * MB / 40));
    rate.record(at(64), 64 * MB);
    assert_eq!(rate.per_sec(at(64)), Some(MB));
    assert_eq!(rate.per_sec(at(80)), Some(32 * MB / 48));
}

#[test]
fn times_left_read_in_the_largest_units() {
    assert_eq!(eta_text(Duration::from_secs(0)), "ETA 0s");
    assert_eq!(eta_text(Duration::from_secs(59)), "ETA 59s");
    assert_eq!(eta_text(Duration::from_secs(185)), "ETA 3m 05s");
    assert_eq!(eta_text(Duration::from_secs(7_620)), "ETA 2h 07m");
}

/// A scheduler run reports to the display's sink.
#[tokio::test]
async fn the_display_follows_a_scheduler_run() {
    let output = output();
    let shared = Arc::new(Shared {
        layout: Layout::Run { files: 2 },
        state: Mutex::new(State::new(Layout::Run { files: 2 }, Instant::now())),
    });
    let file = |name: &str, size: u64| {
        Job::new(JobKind::Media, name, move |context| async move {
            context.progress.bytes(size / 2);
            context.progress.bytes(size - size / 2);
            Outcome::Transferred { bytes: size }.into()
        })
        .with_size(size)
    };
    let broken = Job::new(JobKind::Media, "broken", |context| async move {
        context.progress.bytes(1);
        failure().into()
    })
    .with_size(5);
    let options = Options::new(NonZeroUsize::new(2).unwrap(), true);

    let report = Scheduler::new(options, output.printer.clone())
        .with_describe(ToString::to_string)
        .with_progress(Arc::clone(&shared) as Arc<dyn ProgressSink>)
        .run(
            stream::iter([file("a", 7), broken, file("b", 9)]),
            future::pending(),
        )
        .await;

    assert_eq!(report.files.transferred, 2);
    let view = shared.view(Instant::now());
    assert_eq!((view.files_done, view.files_total), (3, Some(3)));
    assert_eq!(view.failed, 1);
    assert_eq!((view.bytes_done, view.bytes_total), (17, Some(17)));
    assert_eq!(view.files, []);
}

/// A terminal as the user sees it: rows of text and a cursor, written by the bars (as a
/// `TermLike`) and by the printer (as a `Write`). Like a real one, it wraps a line that reaches
/// its width when the next character comes: indicatif fills each line to the width instead of
/// ending it.
#[derive(Debug, Clone, Default)]
struct Screen(Arc<Mutex<Model>>);

#[derive(Debug, Default)]
struct Model {
    rows: Vec<Vec<char>>,
    row: usize,
    column: usize,
    /// Everything written, cursor movements included.
    writes: usize,
}

impl Model {
    fn current(&mut self) -> &mut Vec<char> {
        if self.rows.len() <= self.row {
            self.rows.resize(self.row + 1, Vec::new());
        }
        &mut self.rows[self.row]
    }

    fn put(&mut self, text: &str) {
        self.writes += 1;
        for c in text.chars() {
            match c {
                '\n' => {
                    self.row += 1;
                    self.column = 0;
                }
                '\r' => self.column = 0,
                c => {
                    if self.column >= WIDTH {
                        self.row += 1;
                        self.column = 0;
                    }
                    let column = self.column;
                    let row = self.current();
                    if row.len() <= column {
                        row.resize(column + 1, ' ');
                    }
                    row[column] = c;
                    self.column += 1;
                }
            }
        }
    }
}

impl Screen {
    fn model(&self) -> std::sync::MutexGuard<'_, Model> {
        self.0.lock().unwrap()
    }

    /// The rows with text, trimmed.
    fn rows(&self) -> Vec<String> {
        let model = self.model();
        let mut rows = model
            .rows
            .iter()
            .map(|row| row.iter().collect::<String>().trim_end().to_owned())
            .collect::<Vec<_>>();
        while rows.last().is_some_and(String::is_empty) {
            rows.pop();
        }
        rows
    }

    fn writes(&self) -> usize {
        self.model().writes
    }
}

const WIDTH: usize = 100;

impl TermLike for Screen {
    fn width(&self) -> u16 {
        u16::try_from(WIDTH).unwrap()
    }

    fn height(&self) -> u16 {
        30
    }

    fn move_cursor_up(&self, n: usize) -> io::Result<()> {
        let mut model = self.model();
        model.writes += 1;
        model.row = model.row.saturating_sub(n);
        model.column = model.column.min(WIDTH - 1);
        Ok(())
    }

    fn move_cursor_down(&self, n: usize) -> io::Result<()> {
        let mut model = self.model();
        model.writes += 1;
        model.row += n;
        model.column = model.column.min(WIDTH - 1);
        Ok(())
    }

    fn move_cursor_right(&self, n: usize) -> io::Result<()> {
        let mut model = self.model();
        model.writes += 1;
        model.column += n;
        Ok(())
    }

    fn move_cursor_left(&self, n: usize) -> io::Result<()> {
        let mut model = self.model();
        model.writes += 1;
        model.column = model.column.saturating_sub(n);
        Ok(())
    }

    fn write_line(&self, s: &str) -> io::Result<()> {
        self.model().put(&format!("{s}\n"));
        Ok(())
    }

    fn write_str(&self, s: &str) -> io::Result<()> {
        self.model().put(s);
        Ok(())
    }

    fn clear_line(&self) -> io::Result<()> {
        let mut model = self.model();
        model.writes += 1;
        model.current().clear();
        model.column = 0;
        Ok(())
    }

    fn flush(&self) -> io::Result<()> {
        Ok(())
    }
}

impl Write for Screen {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.model().put(std::str::from_utf8(buf).unwrap());
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn on_screen(
    layout: Layout,
    mode: Mode,
    terminal: Terminal,
    screen: &Screen,
    active: bool,
) -> ProgressDisplay {
    let drawing = mode.drawing(terminal, false);
    let screen = screen.clone();
    ProgressDisplay::open(layout, drawing, active, move |hz| {
        ProgressDrawTarget::term_like_with_hz(Box::new(screen), hz)
    })
}

fn feed_some(sink: &dyn ProgressSink) {
    for job in 0..2 {
        let label = format!("dir/f{job}.bin");
        sink.event(&ProgressEvent::Queued {
            job: JobId(job),
            kind: JobKind::Media,
            label: &label,
            size: Some(MB),
        });
        sink.event(&ProgressEvent::Started { job: JobId(job) });
    }
    sink.event(&ProgressEvent::Bytes {
        job: JobId(0),
        bytes: MB / 4,
    });
    sink.event(&ProgressEvent::SourceDone);
}

#[test]
fn nothing_is_drawn_when_progress_is_off() {
    let terminal = Terminal {
        stderr: false,
        stdout: false,
    };
    let tty = Terminal {
        stderr: true,
        stdout: true,
    };
    let screen = Screen::default();
    for (mode, terminal) in [(Mode::Never, tty), (Mode::Auto, terminal)] {
        let display = on_screen(Layout::Run { files: 2 }, mode, terminal, &screen, false);
        feed_some(display.sink().as_ref());
        std::thread::sleep(FIRST_DRAW * 3);
        display.finish();
    }
    assert_eq!(screen.writes(), 0);

    // The same events do draw when it is on.
    let display = on_screen(Layout::Run { files: 2 }, Mode::Auto, tty, &screen, false);
    feed_some(display.sink().as_ref());
    let started = Instant::now();
    while screen.writes() == 0 && started.elapsed() < Duration::from_secs(10) {
        std::thread::sleep(Duration::from_millis(10));
    }
    display.finish();
    assert!(screen.writes() > 0);
    assert_eq!(screen.rows(), Vec::<String>::new(), "finished: cleared");
}

#[test]
fn forced_progress_off_a_terminal_is_one_plain_line() {
    let terminal = Terminal {
        stderr: false,
        stdout: false,
    };
    let drawing = Mode::Always.drawing(terminal, false);
    assert_eq!(drawing, Drawing::Line);
    let out = Captured::default();
    let target = out.clone();
    let display = ProgressDisplay::open(Layout::Run { files: 2 }, drawing, false, move |hz| {
        plain_target(target, 80, hz)
    });
    feed_some(display.sink().as_ref());
    let started = Instant::now();
    while !out.text().contains("0/2 files") && started.elapsed() < Duration::from_secs(10) {
        std::thread::sleep(Duration::from_millis(10));
    }
    display.finish();

    let written = out.text();
    assert!(
        written.contains("0/2 files, 250 KB/2 MB (12%)"),
        "{written:?}"
    );
    assert!(
        !written.contains('\u{1b}'),
        "no escape sequence: {written:?}"
    );
    assert!(!written.contains('\n'), "one line: {written:?}");
    let blank = format!("\r{}\r", " ".repeat(80));
    assert!(written.ends_with(&blank), "erased at the end: {written:?}");
}

/// Waits until `check` holds: other tests that print take the bars off the screen for a moment.
async fn eventually(what: &str, mut check: impl FnMut() -> bool) {
    for _ in 0..1_000 {
        if check() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("{what}");
}

/// The one test that makes its display the one that `suspend` takes off the terminal: the lines
/// of the run's printer end up above the bars, whole, and stay when the bars go.
#[tokio::test]
async fn printed_lines_go_above_the_bars() {
    let screen = Screen::default();
    let printer = Printer::to(screen.clone(), screen.clone());
    let tty = Terminal {
        stderr: true,
        stdout: true,
    };
    let display = on_screen(Layout::Run { files: 2 }, Mode::Auto, tty, &screen, true);
    feed_some(display.sink().as_ref());
    eventually("the bars are drawn", || screen.rows().len() == 3).await;

    let lines = [
        "Downloading file 'dir/f0.bin'",
        "Warning: dir/f1.bin: interrupted, resuming",
        "Downloading file 'dir/f1.bin'",
    ];
    printer.out(lines[0]).await;
    printer.err(lines[1]).await;
    printer.out(lines[2]).await;
    printer.flush().await.unwrap();

    eventually("the lines above the bars", || {
        let rows = screen.rows();
        rows.len() == 6
            && rows[..3] == lines
            && rows[3].contains("0/2 files, 250 KB/2 MB (12%)")
            && rows[4].contains("dir/f0.bin")
            && rows[4].contains("[=====>              ] 250 KB/1 MB")
            && rows[5].contains("dir/f1.bin")
    })
    .await;

    display.finish();
    assert_eq!(screen.rows(), lines);
    // Without a display, the printer writes as before.
    printer.out("Summary").await;
    printer.flush().await.unwrap();
    assert_eq!(screen.rows().last().map(String::as_str), Some("Summary"));
}
