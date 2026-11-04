//! The scheduler with fake jobs on a paused clock: sleeps stand for transfers, so the tests are
//! fast and their timing is exact.

use std::{
    io,
    num::NonZeroUsize,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

use futures::{future, stream, Stream, StreamExt};
use tokio::{sync::Semaphore, time::sleep};

use super::{
    capture::{output, Captured},
    Counts, Job, JobKind, JobOutput, Options, Outcome, Printer, Report, Scheduler, SkipReason,
    Subtree,
};
use crate::{
    common::table::FailingWriter,
    transfer::progress::{JobId, ProgressEvent, ProgressSink},
};

fn options(jobs: usize, continue_on_error: bool) -> Options {
    Options::new(NonZeroUsize::new(jobs).unwrap(), continue_on_error)
}

/// Counts the jobs that run at once.
#[derive(Clone, Default)]
struct Gauge {
    now: Arc<AtomicUsize>,
    max: Arc<AtomicUsize>,
}

impl Gauge {
    fn enter(&self) -> GaugeGuard {
        let now = self.now.fetch_add(1, Ordering::SeqCst) + 1;
        self.max.fetch_max(now, Ordering::SeqCst);
        GaugeGuard(Arc::clone(&self.now))
    }

    fn max(&self) -> usize {
        self.max.load(Ordering::SeqCst)
    }
}

struct GaugeGuard(Arc<AtomicUsize>);

impl Drop for GaugeGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// A media job of `bytes` that takes `duration`, measured by `gauge`.
fn transfer(label: &str, bytes: u64, duration: Duration, gauge: &Gauge) -> Job {
    let gauge = gauge.clone();
    Job::new(JobKind::Media, label, move |_| async move {
        let _running = gauge.enter();
        sleep(duration).await;
        Outcome::Transferred { bytes }.into()
    })
    .with_size(bytes)
}

/// A media job that fails after `duration`.
fn failure(label: &str, duration: Duration) -> Job {
    Job::new(JobKind::Media, label, move |_| async move {
        sleep(duration).await;
        Outcome::failed(io::Error::other("disk on fire")).into()
    })
}

/// A media job that takes long and counts its cleanup in `cleaned`.
fn slow_with_cleanup(label: &str, cleaned: &Arc<AtomicUsize>) -> Job {
    let cleaned = Arc::clone(cleaned);
    Job::new(JobKind::Media, label, |_| async {
        sleep(Duration::from_secs(3600)).await;
        Outcome::Transferred { bytes: 1 }.into()
    })
    .on_cancel(move || async move {
        cleaned.fetch_add(1, Ordering::SeqCst);
    })
}

/// `jobs` as a source that counts how many were taken.
fn counted(jobs: Vec<Job>, taken: &Arc<AtomicUsize>) -> impl Stream<Item = Job> + Send {
    let taken = Arc::clone(taken);
    stream::iter(jobs).inspect(move |_| {
        taken.fetch_add(1, Ordering::SeqCst);
    })
}

async fn run(options: Options, printer: &Printer, jobs: Vec<Job>) -> Report {
    Scheduler::new(options, printer.clone())
        .run(stream::iter(jobs), future::pending())
        .await
}

const MS: Duration = Duration::from_millis(1);

#[tokio::test(start_paused = true)]
async fn media_jobs_never_exceed_the_limit_and_use_it() {
    let output = output();
    let gauge = Gauge::default();
    let jobs = (0..20)
        .map(|i| transfer(&format!("f{i}"), 10, 20 * MS, &gauge))
        .collect();

    let started = tokio::time::Instant::now();
    let report = run(options(3, false), &output.printer, jobs).await;

    assert_eq!(gauge.max(), 3);
    assert_eq!(
        report.files,
        Counts {
            transferred: 20,
            bytes: 200,
            ..Counts::default()
        }
    );
    // 20 jobs of 20 ms, 3 at a time: 7 rounds.
    assert_eq!(started.elapsed(), 140 * MS);
    assert!(report.check().is_ok());
}

#[tokio::test(start_paused = true)]
async fn folders_and_files_have_separate_limits() {
    let output = output();
    let files = Gauge::default();
    let folders = Gauge::default();
    let mut jobs = Vec::new();
    for i in 0..6 {
        let folders = folders.clone();
        jobs.push(Job::new(
            JobKind::Metadata,
            format!("d{i}"),
            |_| async move {
                let _running = folders.enter();
                sleep(10 * MS).await;
                Outcome::Transferred { bytes: 0 }.into()
            },
        ));
        jobs.push(transfer(&format!("f{i}"), 1, 10 * MS, &files));
    }
    let mut options = options(4, false);
    options.metadata_jobs = NonZeroUsize::new(1).unwrap();

    let report = run(options, &output.printer, jobs).await;

    assert_eq!((folders.max(), files.max()), (1, 4));
    assert_eq!(
        (report.folders.transferred, report.files.transferred),
        (6, 6)
    );
}

#[tokio::test(start_paused = true)]
async fn a_full_queue_stops_taking_jobs_from_the_source() {
    let output = output();
    let gate = Arc::new(Semaphore::new(0));
    let started = Arc::new(AtomicUsize::new(0));
    let jobs = (0..100)
        .map(|i| {
            let gate = Arc::clone(&gate);
            let started = Arc::clone(&started);
            Job::new(JobKind::Media, format!("f{i}"), move |_| async move {
                started.fetch_add(1, Ordering::SeqCst);
                let _permit = gate.acquire().await;
                Outcome::Transferred { bytes: 1 }.into()
            })
        })
        .collect();
    let taken = Arc::new(AtomicUsize::new(0));
    let source = counted(jobs, &taken);
    let options = options(2, false);
    assert_eq!(options.queue.get(), 4);

    let printer = output.printer.clone();
    let run = tokio::spawn(async move {
        Scheduler::new(options, printer)
            .run(source, future::pending())
            .await
    });
    sleep(Duration::from_secs(1)).await;

    // Two run, four wait in the queue, the other 94 are still in the source.
    assert_eq!(started.load(Ordering::SeqCst), 2);
    assert_eq!(taken.load(Ordering::SeqCst), 6);

    gate.add_permits(100);
    let report = run.await.unwrap();
    assert_eq!(report.files.transferred, 100);
    assert_eq!(taken.load(Ordering::SeqCst), 100);
}

#[tokio::test(start_paused = true)]
async fn the_first_failure_stops_the_run_and_cleans_up_the_running_jobs() {
    let output = output();
    let cleaned = Arc::new(AtomicUsize::new(0));
    let gauge = Gauge::default();
    let mut jobs = vec![failure("bad", 10 * MS), slow_with_cleanup("slow", &cleaned)];
    jobs.extend((0..20).map(|i| transfer(&format!("f{i}"), 1, 100 * MS, &gauge)));
    let taken = Arc::new(AtomicUsize::new(0));
    let source = counted(jobs, &taken);

    let report = Scheduler::new(options(2, false), output.printer.clone())
        .run(source, future::pending())
        .await;
    output.printer.flush().await.unwrap();

    assert!(report.stopped);
    assert_eq!(report.files.failed, 1);
    assert_eq!(report.files.transferred, 0, "no other job started");
    assert_eq!(gauge.max(), 0);
    // The slow job was aborted and the four queued ones never started; the rest stayed in the
    // source.
    assert_eq!(report.files.cancelled, 5);
    assert_eq!(taken.load(Ordering::SeqCst), 6);
    assert_eq!(
        cleaned.load(Ordering::SeqCst),
        1,
        "the aborted job's cleanup ran"
    );
    assert_eq!(output.err.lines(), ["Error: disk on fire"]);
    let error = report.check().unwrap_err().to_string();
    assert_eq!(
        error,
        "1 file failed, 5 cancelled; use --continue-on-error to finish the other transfers \
        after a failure"
    );
}

#[tokio::test(start_paused = true)]
async fn continue_on_error_finishes_the_other_jobs_and_still_fails() {
    let output = output();
    let gauge = Gauge::default();
    let mut jobs = vec![failure("bad", 10 * MS)];
    jobs.extend((0..9).map(|i| transfer(&format!("f{i}"), 2, 30 * MS, &gauge)));
    jobs.push(failure("worse", 10 * MS));

    let report = run(options(2, true), &output.printer, jobs).await;
    output.printer.flush().await.unwrap();

    assert!(!report.stopped);
    assert_eq!(
        report.files,
        Counts {
            transferred: 9,
            bytes: 18,
            failed: 2,
            ..Counts::default()
        }
    );
    assert_eq!(output.err.lines().len(), 2);
    assert_eq!(report.check().unwrap_err().to_string(), "2 files failed");
}

#[tokio::test(start_paused = true)]
async fn cancel_aborts_the_running_jobs_and_runs_their_cleanup() {
    let output = output();
    let cleaned = Arc::new(AtomicUsize::new(0));
    let gauge = Gauge::default();
    let mut jobs = vec![transfer("quick", 1, 10 * MS, &gauge)];
    jobs.extend((0..5).map(|i| slow_with_cleanup(&format!("slow{i}"), &cleaned)));

    let report = Scheduler::new(options(3, true), output.printer.clone())
        .run(stream::iter(jobs), sleep(50 * MS))
        .await;
    output.printer.flush().await.unwrap();

    assert!(report.interrupted);
    assert!(!report.stopped);
    assert_eq!(report.files.transferred, 1);
    // Three slow jobs ran (one took the quick job's slot) and two were still queued.
    assert_eq!(report.files.cancelled, 5);
    assert_eq!(cleaned.load(Ordering::SeqCst), 3);
    assert_eq!(
        output.err.lines(),
        ["Interrupted: cancelling the transfers in progress"]
    );
    assert_eq!(
        report.check().unwrap_err().to_string(),
        "interrupted, 5 cancelled"
    );
    assert!(report.summary().ends_with("; interrupted"));
}

#[tokio::test(start_paused = true)]
async fn a_panicking_job_fails_and_is_cleaned_up() {
    let output = output();
    let cleaned = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&cleaned);
    let job = Job::new(JobKind::Media, "boom", |_| async {
        sleep(MS).await;
        panic!("a bug");
    })
    .on_cancel(move || async move {
        counter.fetch_add(1, Ordering::SeqCst);
    });

    let report = run(options(1, true), &output.printer, vec![job]).await;
    output.printer.flush().await.unwrap();

    assert_eq!(report.files.failed, 1);
    assert_eq!(cleaned.load(Ordering::SeqCst), 1);
    assert_eq!(
        output.err.lines(),
        ["Error: internal error while transferring 'boom'"]
    );
}

/// A metadata job that records when it ran and releases `follow_ups`.
fn folder(label: &str, log: &Arc<Mutex<Vec<String>>>, follow_ups: Vec<Job>) -> Job {
    let log = Arc::clone(log);
    let name = label.to_owned();
    Job::new(JobKind::Metadata, label, move |_| async move {
        sleep(10 * MS).await;
        log.lock().unwrap().push(format!("created {name}"));
        JobOutput::from(Outcome::Transferred { bytes: 0 }).follow_up(follow_ups)
    })
}

fn logged_file(label: &str, log: &Arc<Mutex<Vec<String>>>) -> Job {
    let log = Arc::clone(log);
    let name = label.to_owned();
    Job::new(JobKind::Media, label, move |_| async move {
        log.lock().unwrap().push(format!("uploading {name}"));
        sleep(10 * MS).await;
        Outcome::Transferred { bytes: 5 }.into()
    })
}

#[tokio::test(start_paused = true)]
async fn follow_ups_start_after_their_job_and_die_with_it() {
    let output = output();
    let log = Arc::new(Mutex::new(Vec::new()));
    let inner = folder("a/b", &log, vec![logged_file("a/b/2", &log)]);
    let a = folder("a", &log, vec![logged_file("a/1", &log), inner]);
    // A folder that cannot be created: its four files and one folder are cancelled with it.
    let broken = Job::new(JobKind::Metadata, "c", |_| async {
        sleep(5 * MS).await;
        Outcome::failed(io::Error::other("no such parent")).into()
    })
    .with_subtree(Subtree {
        files: 4,
        folders: 1,
    });

    let report = run(options(4, true), &output.printer, vec![a, broken]).await;

    let log = log.lock().unwrap().clone();
    let position = |entry: &str| log.iter().position(|logged| logged == entry).unwrap();
    assert!(position("created a") < position("uploading a/1"));
    assert!(position("created a") < position("created a/b"));
    assert!(position("created a/b") < position("uploading a/b/2"));
    assert_eq!(
        report.folders,
        Counts {
            transferred: 2,
            failed: 1,
            cancelled: 1,
            ..Counts::default()
        }
    );
    assert_eq!(
        report.files,
        Counts {
            transferred: 2,
            bytes: 10,
            cancelled: 4,
            ..Counts::default()
        }
    );
    assert_eq!(
        report.summary(),
        "Summary: 2 transferred (10 B), 0 unchanged, 0 skipped, 0 failed, 4 cancelled; \
        1 folder failed"
    );
    assert_eq!(
        report.check().unwrap_err().to_string(),
        "1 folder failed, 5 cancelled"
    );
}

#[tokio::test(start_paused = true)]
async fn aborted_folders_cancel_their_subtree() {
    let output = output();
    let log = Arc::new(Mutex::new(Vec::new()));
    let a = folder(
        "a",
        &log,
        vec![logged_file("a/1", &log), logged_file("a/2", &log)],
    )
    .with_subtree(Subtree {
        files: 2,
        folders: 0,
    });
    let b = folder("b", &log, Vec::new()).with_subtree(Subtree {
        files: 7,
        folders: 1,
    });
    let jobs = vec![a, b, failure("bad", MS)];
    let mut options = options(1, false);
    options.metadata_jobs = NonZeroUsize::new(2).unwrap();

    let report = run(options, &output.printer, jobs).await;

    // `a` and `b` were aborted; nothing they would have released ran.
    assert!(log.lock().unwrap().is_empty());
    assert_eq!(report.files.failed, 1);
    assert_eq!(report.files.cancelled, 9);
    assert_eq!(report.folders.cancelled, 3);
}

#[tokio::test(start_paused = true)]
async fn decided_items_take_no_slot_and_count_by_reason() {
    let output = output();
    let gate = Arc::new(Semaphore::new(0));
    let blocked = {
        let gate = Arc::clone(&gate);
        Job::new(JobKind::Media, "blocked", move |_| async move {
            let _permit = gate.acquire().await;
            Outcome::Unchanged.into()
        })
    };
    let skipped =
        |label: &str, reason| Job::done(JobKind::Media, label, Outcome::Skipped { reason });
    let jobs = vec![
        blocked,
        skipped("doc1", SkipReason::GoogleDocument),
        skipped("doc2", SkipReason::GoogleDocument),
        skipped("link", SkipReason::Shortcut),
        Job::done(JobKind::Media, "gone", Outcome::Cancelled),
    ];

    let printer = output.printer.clone();
    let handle = tokio::spawn(async move { run(options(1, false), &printer, jobs).await });
    sleep(Duration::from_secs(1)).await;
    assert!(!handle.is_finished(), "the blocked job holds the only slot");
    gate.add_permits(1);
    let report = handle.await.unwrap();

    assert_eq!(
        report.files,
        Counts {
            unchanged: 1,
            skipped: 3,
            cancelled: 1,
            ..Counts::default()
        }
    );
    assert_eq!(report.skipped(SkipReason::GoogleDocument), 2);
    assert_eq!(report.skipped(SkipReason::Shortcut), 1);
    assert_eq!(
        report.summary(),
        "Summary: 0 transferred (0 B), 1 unchanged, 3 skipped, 0 failed, 1 cancelled"
    );
    assert!(
        report.check().is_ok(),
        "cancelled items alone are no failure"
    );
}

#[tokio::test(start_paused = true)]
async fn a_failure_decided_without_running_stops_the_run_too() {
    let output = output();
    let cleaned = Arc::new(AtomicUsize::new(0));
    let jobs = vec![
        slow_with_cleanup("slow", &cleaned),
        Job::done(
            JobKind::Metadata,
            "folder",
            Outcome::failed(io::Error::other("cannot list")),
        ),
    ];

    let report = run(options(2, false), &output.printer, jobs).await;

    assert!(report.stopped);
    assert_eq!(report.folders.failed, 1);
    assert_eq!(report.files.cancelled, 1);
    assert_eq!(cleaned.load(Ordering::SeqCst), 1);
}

#[tokio::test(start_paused = true)]
async fn lines_of_parallel_jobs_stay_whole_and_in_order() {
    let output = output();
    let jobs = (0..30)
        .map(|i| {
            Job::new(JobKind::Media, format!("f{i}"), move |context| async move {
                context
                    .printer
                    .out(format!("Downloading file 'f{i}'"))
                    .await;
                sleep(MS * (30 - i)).await;
                context.printer.out(format!("Done 'f{i}'")).await;
                Outcome::Transferred { bytes: 1 }.into()
            })
        })
        .collect();

    run(options(8, false), &output.printer, jobs).await;
    output.printer.flush().await.unwrap();

    let lines = output.out.lines();
    assert_eq!(lines.len(), 60);
    for i in 0..30 {
        let start = lines
            .iter()
            .position(|line| *line == format!("Downloading file 'f{i}'"));
        let end = lines
            .iter()
            .position(|line| *line == format!("Done 'f{i}'"));
        assert!(start.unwrap() < end.unwrap(), "{lines:?}");
    }
}

/// Records the events of a run as text.
#[derive(Default)]
struct Recorder(Mutex<Vec<String>>);

impl ProgressSink for Recorder {
    fn event(&self, event: &ProgressEvent<'_>) {
        let text = match event {
            ProgressEvent::Queued {
                job: JobId(job),
                kind,
                label,
                size,
            } => format!("queued {job} {kind:?} {label} {size:?}"),
            ProgressEvent::Started { job: JobId(job) } => format!("started {job}"),
            ProgressEvent::Bytes {
                job: JobId(job),
                bytes,
            } => format!("bytes {job} {bytes}"),
            ProgressEvent::Finished {
                job: JobId(job),
                kind,
                label,
                outcome,
            } => format!("finished {job} {kind:?} {label} {outcome:?}"),
            ProgressEvent::SourceDone => String::from("source done"),
        };
        self.0.lock().unwrap().push(text);
    }
}

#[tokio::test(start_paused = true)]
async fn progress_events_follow_every_job() {
    let output = output();
    let recorder = Arc::new(Recorder::default());
    let job = Job::new(JobKind::Media, "a.bin", |context| async move {
        context.progress.bytes(3);
        context.progress.bytes(0);
        context.progress.bytes(4);
        Outcome::Transferred { bytes: 7 }.into()
    })
    .with_size(7);
    let doc = Job::done(
        JobKind::Media,
        "notes",
        Outcome::Skipped {
            reason: SkipReason::GoogleDocument,
        },
    );

    Scheduler::new(options(1, false), output.printer.clone())
        .with_progress(Arc::clone(&recorder) as Arc<dyn ProgressSink>)
        .run(stream::iter([job, doc]), future::pending())
        .await;

    let events = recorder.0.lock().unwrap().clone();
    let of = |job: &str| {
        events
            .iter()
            .filter(|event| event.split(' ').nth(1) == Some(job))
            .cloned()
            .collect::<Vec<_>>()
    };
    assert_eq!(
        of("0"),
        [
            "queued 0 Media a.bin Some(7)",
            "started 0",
            "bytes 0 3",
            "bytes 0 4",
            "finished 0 Media a.bin Transferred { bytes: 7 }",
        ]
    );
    assert_eq!(
        of("1"),
        [
            "queued 1 Media notes None",
            "finished 1 Media notes Skipped { reason: GoogleDocument }",
        ]
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| *event == "source done")
            .count(),
        1
    );
}

#[test]
fn options_scale_the_queue_with_the_jobs() {
    let options = Options::default();
    assert_eq!(
        (
            options.jobs.get(),
            options.metadata_jobs.get(),
            options.queue.get()
        ),
        (4, 2, 8)
    );
    assert!(!options.continue_on_error);
}

/// Media jobs that each print a line to stdout and take 10 ms.
fn printing(count: usize) -> Vec<Job> {
    (0..count)
        .map(|i| {
            Job::new(JobKind::Media, format!("f{i}"), move |context| async move {
                context.printer.out(format!("f{i}: id-{i}")).await;
                sleep(10 * MS).await;
                Outcome::Transferred { bytes: 1 }.into()
            })
        })
        .collect()
}

#[tokio::test(start_paused = true)]
async fn an_output_that_cannot_be_written_stops_the_run_and_fails_it() {
    let err = Captured::default();
    let printer = Printer::to(FailingWriter(io::ErrorKind::StorageFull), err.clone());

    let report = run(options(2, false), &printer, printing(20)).await;

    assert!(report.output_failed);
    assert!(report.files.transferred < 20, "{:?}", report.files);
    assert!(report.files.cancelled > 0, "{:?}", report.files);
    let error = printer.flush().await.unwrap_err();
    assert!(
        crate::error_message(&error).starts_with("Error: unable to write to stdout: "),
        "{error}"
    );
    let incomplete = report.check().unwrap_err().to_string();
    assert!(
        incomplete.starts_with("the output could not be written, "),
        "{incomplete}"
    );
    // stderr still works: the summary of the caller would reach it.
    printer.err("Summary").await;
    assert!(printer.flush().await.is_err(), "the failure stays");
    assert_eq!(err.lines(), ["Summary"]);
}

#[tokio::test(start_paused = true)]
async fn continue_on_error_finishes_the_jobs_when_the_output_fails() {
    let printer = Printer::to(
        FailingWriter(io::ErrorKind::StorageFull),
        Captured::default(),
    );

    let report = run(options(2, true), &printer, printing(10)).await;

    assert_eq!(report.files.transferred, 10);
    assert!(report.output_failed);
    assert!(report.check().is_err());
    assert!(printer.flush().await.is_err());
}

#[tokio::test(start_paused = true)]
async fn a_closed_pipe_ends_the_output_quietly() {
    let err = Captured::default();
    let printer = Printer::to(FailingWriter(io::ErrorKind::BrokenPipe), err.clone());

    let report = run(options(2, false), &printer, printing(10)).await;

    assert_eq!(report.files.transferred, 10);
    assert!(!report.output_failed);
    assert!(report.check().is_ok());
    printer.flush().await.unwrap();
    assert_eq!(err.lines(), Vec::<String>::new());
}

/// `count` jobs of `kind` that each wait for a permit of `gate`, counting the ones that started
/// in `started`, as an iterator that counts the jobs taken from it in `taken`.
fn gated(
    kind: JobKind,
    count: usize,
    gate: &Arc<Semaphore>,
    started: &Arc<AtomicUsize>,
    taken: &Arc<AtomicUsize>,
) -> impl Iterator<Item = Job> + Send + 'static {
    let gate = Arc::clone(gate);
    let started = Arc::clone(started);
    let taken = Arc::clone(taken);
    (0..count).map(move |i| {
        taken.fetch_add(1, Ordering::SeqCst);
        let gate = Arc::clone(&gate);
        let started = Arc::clone(&started);
        Job::new(kind, format!("{kind:?} {i}"), move |_| async move {
            started.fetch_add(1, Ordering::SeqCst);
            let _permit = gate.acquire().await;
            Outcome::Transferred { bytes: 1 }.into()
        })
    })
}

#[tokio::test(start_paused = true)]
async fn a_full_queue_stops_taking_follow_ups() {
    let output = output();
    let gate = Arc::new(Semaphore::new(0));
    let started = Arc::new(AtomicUsize::new(0));
    let taken = Arc::new(AtomicUsize::new(0));
    let files = gated(JobKind::Media, 100, &gate, &started, &taken);
    let folder = Job::new(JobKind::Metadata, "folder", |_| async move {
        JobOutput::from(Outcome::Transferred { bytes: 0 }).follow_up(files)
    });

    let printer = output.printer.clone();
    let handle = tokio::spawn(async move { run(options(2, false), &printer, vec![folder]).await });
    sleep(Duration::from_secs(1)).await;

    // Two run and four wait in the queue; one more was taken to see that it does not fit. The
    // other 93 were never made.
    assert_eq!(started.load(Ordering::SeqCst), 2);
    assert_eq!(taken.load(Ordering::SeqCst), 2 + 4 + 1);

    gate.add_permits(100);
    let report = handle.await.unwrap();
    assert_eq!(report.files.transferred, 100);
    assert_eq!(report.folders.transferred, 1);
}

#[tokio::test(start_paused = true)]
async fn follow_ups_of_one_kind_do_not_hold_back_the_other() {
    let output = output();
    let gate = Arc::new(Semaphore::new(0));
    let started = Arc::new(AtomicUsize::new(0));
    let folders_taken = Arc::new(AtomicUsize::new(0));
    let files_taken = Arc::new(AtomicUsize::new(0));
    let folders = gated(JobKind::Metadata, 20, &gate, &started, &folders_taken);
    let files = gated(JobKind::Media, 20, &gate, &started, &files_taken);
    let folder = Job::new(JobKind::Metadata, "folder", |_| async move {
        JobOutput::from(Outcome::Transferred { bytes: 0 })
            .follow_up(folders)
            .follow_up(files)
    });
    let mut options = options(2, false);
    options.metadata_jobs = NonZeroUsize::new(1).unwrap();

    let printer = output.printer.clone();
    let handle = tokio::spawn(async move { run(options, &printer, vec![folder]).await });
    sleep(Duration::from_secs(1)).await;

    // One folder and two files run; each kind has its own four queued and one seen.
    assert_eq!(started.load(Ordering::SeqCst), 1 + 2);
    assert_eq!(folders_taken.load(Ordering::SeqCst), 1 + 4 + 1);
    assert_eq!(files_taken.load(Ordering::SeqCst), 2 + 4 + 1);

    gate.add_permits(100);
    let report = handle.await.unwrap();
    assert_eq!(
        (report.folders.transferred, report.files.transferred),
        (21, 20)
    );
}

#[tokio::test(start_paused = true)]
async fn a_stop_cancels_the_follow_ups_that_were_not_taken() {
    let output = output();
    let gate = Arc::new(Semaphore::new(0));
    let started = Arc::new(AtomicUsize::new(0));
    let taken = Arc::new(AtomicUsize::new(0));
    let files = gated(JobKind::Media, 50, &gate, &started, &taken);
    let folder = Job::new(JobKind::Metadata, "folder", |_| async move {
        JobOutput::from(Outcome::Transferred { bytes: 0 }).follow_up(files)
    });
    let mut options = options(2, false);
    options.metadata_jobs = NonZeroUsize::new(2).unwrap();

    let report = run(
        options,
        &output.printer,
        vec![folder, failure("bad", 100 * MS)],
    )
    .await;

    assert!(report.stopped);
    assert_eq!(report.files.failed, 1);
    assert_eq!(
        report.files.cancelled, 50,
        "running, queued and never taken"
    );
    assert_eq!(taken.load(Ordering::SeqCst), 50);
}

#[tokio::test(start_paused = true)]
async fn a_failed_job_gets_the_hint_of_the_account_like_a_failed_command() {
    use crate::app_config::account_type::AccountType;

    let output = output();
    let job = Job::new(JobKind::Media, "photos/cat.jpg", |_| async {
        let reply = serde_json::json!({"error": {
            "code": 403,
            "message": "Service Accounts do not have storage quota.",
            "errors": [{"domain": "usageLimits", "reason": "storageQuotaExceeded"}],
        }});
        Outcome::failed(google_drive3::Error::BadRequest(reply)).into()
    });
    let service_account = AccountType::ServiceAccount { subject: None };

    let report = Scheduler::new(options(1, false), output.printer.clone())
        .with_describe(move |error| {
            crate::failure_message_with(error, |error| {
                crate::auth::service::account_hint(error, &service_account)
            })
        })
        .run(stream::iter([job]), future::pending())
        .await;
    output.printer.flush().await.unwrap();

    assert_eq!(report.files.failed, 1);
    let err = output.err.text();
    assert!(err.starts_with("Error: "), "{err}");
    assert!(
        err.contains("\nHint: service accounts have no Drive storage of their own"),
        "{err}"
    );
}
