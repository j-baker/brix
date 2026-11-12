//! Nix's progress bar (`src/libmain/progress-bar.cc`) for fetches and builds: a
//! status in brackets and the newest activity,
//!
//! ```text
//! [2/1/3 fetched, 12.3/45.0 MiB DL] downloading 'https://github.com/…'
//! ```
//!
//! (running, done and started fetches; bytes received and announced).
//! Unlike Nix's, it is drawn only while the evaluator waits for a fetch,
//! store operation or compilation ([`brix_flake::Waiting`]) and an activity
//! has run for a moment. It is erased as soon as the wait ends. Evaluation
//! output comes outside those waits; fetches that carry on in the background
//! are not shown until the evaluator waits for one. Build logs and other
//! messages logged through
//! [`brix::log::stderr`] during a wait are written above the bar: it is
//! the facade's logger ([`brix::log::set_logger`]).

mod daemon;

use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use brix::log::{filter_ansi_escapes_str, write_stderr};
use brix_flake::{ActivityId, ActivityKind, Progress};

/// The progress bar, for [`brix::Config::progress`]. There is one per
/// process, as there is one stderr; from the first call on,
/// [`brix::log::stderr`] writes messages above it.
pub fn progress_bar() -> Arc<ProgressBar> {
    BAR.get_or_init(|| {
        let bar = Arc::new(ProgressBar::default());
        brix::log::set_logger(bar.clone())
            .unwrap_or_else(|_| panic!("the progress bar is the only logger brix installs"));
        bar
    })
    .clone()
}

static BAR: OnceLock<Arc<ProgressBar>> = OnceLock::new();

/// How long an activity runs before the bar is drawn for it: a fetch that
/// is over sooner is not worth a flicker.
const SHOW_AFTER: Duration = Duration::from_millis(100);
/// How long an activity runs before its text replaces an older one's
/// (Nix's delay, so that very short activities do not flash past).
const TEXT_AFTER: Duration = Duration::from_millis(10);
/// The redraw interval while the bar is up (Nix's).
const TICK: Duration = Duration::from_millis(50);

const ANSI_NORMAL: &str = "\x1b[0m";
const ANSI_GREEN: &str = "\x1b[32;1m";
const ANSI_BLUE: &str = "\x1b[34;1m";

/// The state behind [`progress_bar`].
#[derive(Debug, Default)]
pub(crate) struct ProgressBar {
    state: Mutex<BarState>,
    wake: Condvar,
}

#[derive(Debug, Default)]
struct BarState {
    next_id: u64,
    daemon: daemon::State,
    /// In the order they started.
    running: Vec<(ActivityId, Running)>,
    fetches_done: u64,
    /// The final sizes of finished transfers.
    bytes_done: u64,
    /// Threads waiting for activities (see [`Progress::begin_wait`]).
    waits: usize,
    /// The line on the screen, if any.
    drawn: Option<String>,
    /// Whether the drawing thread is running.
    ticking: bool,
    /// Where the bar and the messages above it are written.
    out: Output,
}

/// What the bar writes bytes with.
type Sink = dyn Fn(&[u8]) + Send;

/// Where the bar writes: stderr, or a test's buffer.
struct Output(Box<Sink>);

impl Output {
    fn write(&self, text: &[u8]) {
        (self.0)(text)
    }
}

impl Default for Output {
    fn default() -> Self {
        Output(Box::new(write_stderr))
    }
}

impl std::fmt::Debug for Output {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Output")
    }
}

#[derive(Debug)]
struct Running {
    kind: ActivityKind,
    text: String,
    done: u64,
    expected: u64,
    started: Instant,
}

impl ProgressBar {
    fn lock(&self) -> MutexGuard<'_, BarState> {
        // A panic while drawing leaves nothing half-updated that matters.
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// The drawing thread: redraws every [`TICK`] while there is
    /// something to show, and sleeps otherwise.
    fn run(&self) {
        let mut state = self.lock();
        loop {
            draw(&mut state, Instant::now());
            let idle = state.waits == 0
                || (state.running.is_empty() && state.daemon.activities.is_empty());
            state = if idle {
                self.wake.wait(state).unwrap_or_else(|p| p.into_inner())
            } else {
                self.wake
                    .wait_timeout(state, TICK)
                    .unwrap_or_else(|p| p.into_inner())
                    .0
            };
        }
    }

    fn start_ticking(&self, state: &mut BarState) {
        if !state.ticking {
            // Only the process-wide bar draws; it lives as long as the
            // process.
            if let Some(bar) = BAR.get().filter(|bar| std::ptr::eq(&***bar, self)) {
                let bar = bar.clone();
                state.ticking = std::thread::Builder::new()
                    .name("brix-progress".into())
                    .spawn(move || bar.run())
                    .is_ok();
            }
        }
        self.wake.notify_all();
    }

    fn write_above(&self, text: &[u8]) {
        let mut state = self.lock();
        clear(&mut state);
        state.out.write(text);
        draw(&mut state, Instant::now());
    }
}

impl Progress for ProgressBar {
    fn start(&self, kind: ActivityKind, text: &str) -> ActivityId {
        let mut state = self.lock();
        state.next_id += 1;
        let id = ActivityId(state.next_id);
        state.running.push((
            id,
            Running {
                kind,
                text: text.to_owned(),
                done: 0,
                expected: 0,
                started: Instant::now(),
            },
        ));
        self.start_ticking(&mut state);
        id
    }

    fn update(&self, id: ActivityId, done: u64, expected: u64) {
        let mut state = self.lock();
        if let Some((_, r)) = state.running.iter_mut().find(|(i, _)| *i == id) {
            r.done = done;
            r.expected = expected;
        }
    }

    fn stop(&self, id: ActivityId) {
        let mut state = self.lock();
        let Some(at) = state.running.iter().position(|(i, _)| *i == id) else {
            return;
        };
        let (_, r) = state.running.remove(at);
        match r.kind {
            ActivityKind::Fetch => state.fetches_done += 1,
            ActivityKind::FileTransfer => state.bytes_done += r.done,
            ActivityKind::Other => {}
        }
        if state.running.is_empty() && state.daemon.activities.is_empty() {
            clear(&mut state);
        }
    }

    fn begin_wait(&self) {
        self.lock().waits += 1;
        self.wake.notify_all();
    }

    fn end_wait(&self) {
        let mut state = self.lock();
        state.waits = state.waits.saturating_sub(1);
        if state.waits == 0 {
            clear(&mut state);
        }
    }
}

impl brix::store::BuildProgress for ProgressBar {
    fn start(&self, activity: &brix_daemon::logger::Activity) {
        let mut state = self.lock();
        state.daemon.start(activity);
        self.start_ticking(&mut state);
    }
    fn stop(&self, id: brix_daemon::logger::ActivityId) {
        let mut state = self.lock();
        state.daemon.stop(id);
        if state.running.is_empty() && state.daemon.activities.is_empty() {
            clear(&mut state);
        }
        self.wake.notify_all();
    }
    fn result(
        &self,
        id: brix_daemon::logger::ActivityId,
        kind: brix_daemon::logger::ResultType,
        fields: &[brix_daemon::logger::Field],
    ) {
        self.lock().daemon.result(id, kind, fields);
        self.wake.notify_all();
    }
}

/// Draws the bar if it should be up (see the module documentation), or
/// erases it.
fn draw(state: &mut BarState, now: Instant) {
    let show = state.waits > 0
        && (state
            .running
            .iter()
            .any(|(_, r)| now.saturating_duration_since(r.started) >= SHOW_AFTER)
            || state
                .daemon
                .activities
                .iter()
                .any(|(_, r)| now.saturating_duration_since(r.started) >= SHOW_AFTER));
    if !show {
        clear(state);
        return;
    }
    let line = render_line(state, now, terminal_width());
    if state.drawn.as_deref() != Some(line.as_str()) {
        state.out.write(format!("\r{line}\x1b[K").as_bytes());
        state.drawn = Some(line);
    }
}

fn clear(state: &mut BarState) {
    if state.drawn.take().is_some() {
        state.out.write(b"\r\x1b[K");
    }
}

impl brix::log::Logger for ProgressBar {
    fn write_line(&self, line: &[u8]) {
        self.write_above(line);
    }
}

/// The terminal's width in columns (80 if it cannot be told).
fn terminal_width() -> usize {
    #[cfg(unix)]
    {
        if let Ok(size) = rustix::termios::tcgetwinsize(std::io::stderr()) {
            if size.ws_col > 0 {
                return size.ws_col as usize;
            }
        }
    }
    80
}

/// `ProgressBar::draw`'s line: `[status] text`, cut to `width` columns.
fn render_line(state: &BarState, now: Instant, width: usize) -> String {
    let mut line = String::new();
    let status = render_status(state);
    if !status.is_empty() {
        line = format!("[{status}]");
    }
    // The newest activity that has run long enough to be named.
    let newest = state
        .running
        .iter()
        .rev()
        .find(|(_, r)| now.duration_since(r.started) >= TEXT_AFTER);
    let text = state
        .daemon
        .latest(now)
        .or_else(|| newest.map(|(_, r)| r.text.clone()));
    if let Some(text) = text {
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(&text);
    }
    format!(
        "{}{ANSI_NORMAL}",
        filter_ansi_escapes_str(&line, false, width)
    )
}

/// `ProgressBar::getStatus` for what fetching reports: input fetches
/// counted like Nix's builds (running/done/started) and transfers in
/// bytes like its downloads.
fn render_status(state: &BarState) -> String {
    let running = state
        .running
        .iter()
        .filter(|(_, r)| r.kind == ActivityKind::Fetch)
        .count() as u64;
    let done = state.fetches_done;
    let fetches = render_count(running, done, running + done);
    let (daemon_done, daemon_expected) = state.daemon.download_totals();
    let (mut dl_done, mut dl_expected) = (
        state.bytes_done + daemon_done,
        state.bytes_done + daemon_expected,
    );
    for (_, r) in &state.running {
        if r.kind == ActivityKind::FileTransfer {
            dl_done += r.done;
            dl_expected += r.expected;
        }
    }
    let transfers = render_size(dl_done, dl_expected);
    let mut parts = state.daemon.status();
    if !fetches.is_empty() {
        parts.push(format!("{fetches} fetched"));
    }
    if !transfers.is_empty() {
        parts.push(format!("{transfers} DL"));
    }
    parts.join(", ")
}

/// `renderActivity`'s numbers: `running/done/expected` with running in
/// blue and done in green, shortened when parts are zero.
fn render_count(running: u64, done: u64, expected: u64) -> String {
    if running == 0 && done == 0 && expected == 0 {
        return String::new();
    }
    if running > 0 {
        if expected != 0 {
            format!("{ANSI_BLUE}{running}{ANSI_NORMAL}/{ANSI_GREEN}{done}{ANSI_NORMAL}/{expected}")
        } else {
            format!("{ANSI_BLUE}{running}{ANSI_NORMAL}/{ANSI_GREEN}{done}{ANSI_NORMAL}")
        }
    } else if expected != done {
        if expected != 0 {
            format!("{ANSI_GREEN}{done}{ANSI_NORMAL}/{expected}")
        } else {
            format!("{ANSI_GREEN}{done}{ANSI_NORMAL}")
        }
    } else if done != 0 {
        format!("{ANSI_GREEN}{done}{ANSI_NORMAL}")
    } else {
        done.to_string()
    }
}

/// `renderSizeActivity` for transfers (which report nothing as running):
/// `done/expected` in one unit, `done` alone when the sizes agree or are
/// unknown.
fn render_size(done: u64, expected: u64) -> String {
    if done == 0 && expected == 0 {
        return String::new();
    }
    if expected != done && expected != 0 {
        let (du, eu) = (size_unit(done), size_unit(expected));
        if du == eu {
            format!(
                "{ANSI_GREEN}{}{ANSI_NORMAL}/{} {}iB",
                size_without_unit(done, du),
                size_without_unit(expected, du),
                SIZE_SUFFIXES[du]
            )
        } else {
            format!(
                "{ANSI_GREEN}{}{ANSI_NORMAL}/{}",
                render_size_with_unit(done),
                render_size_with_unit(expected)
            )
        }
    } else {
        let unit = size_unit(done);
        let value = size_without_unit(done, unit);
        if done != 0 {
            format!("{ANSI_GREEN}{value}{ANSI_NORMAL} {}iB", SIZE_SUFFIXES[unit])
        } else {
            format!("{value} {}iB", SIZE_SUFFIXES[unit])
        }
    }
}

/// Nix's `SizeUnit`s by index: bytes are shown in KiB.
const SIZE_SUFFIXES: [char; 9] = ['K', 'K', 'M', 'G', 'T', 'P', 'E', 'Z', 'Y'];

/// `getSizeUnit`.
fn size_unit(value: u64) -> usize {
    let mut unit = 0;
    let mut v = value;
    while v > 1024 && unit < SIZE_SUFFIXES.len() - 1 {
        unit += 1;
        v /= 1024;
    }
    unit
}

/// `renderSizeWithoutUnit`.
fn size_without_unit(value: u64, unit: usize) -> String {
    let power = unit.max(1) as i32;
    format!("{:.1}", value as f64 / 1024f64.powi(power))
}

/// `renderSize`.
fn render_size_with_unit(value: u64) -> String {
    let unit = size_unit(value);
    format!(
        "{} {}iB",
        size_without_unit(value, unit),
        SIZE_SUFFIXES[unit]
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The status with colours spelled out, for readable expectations.
    fn plain(s: &str) -> String {
        s.replace(ANSI_BLUE, "<b>")
            .replace(ANSI_GREEN, "<g>")
            .replace(ANSI_NORMAL, "</>")
    }

    fn running(kind: ActivityKind, text: &str, done: u64, expected: u64, age_ms: u64) -> Running {
        Running {
            kind,
            text: text.to_owned(),
            done,
            expected,
            started: Instant::now() - Duration::from_millis(age_ms),
        }
    }

    #[test]
    fn sizes_render_as_nix_renders_them() {
        assert_eq!(render_size_with_unit(500), "0.5 KiB");
        assert_eq!(render_size_with_unit(1024), "1.0 KiB");
        assert_eq!(render_size_with_unit(1536 * 1024), "1.5 MiB");
        assert_eq!(plain(&render_size(0, 0)), "");
        // Unknown length: the amount so far.
        assert_eq!(
            plain(&render_size(12_900_000, 12_900_000)),
            "<g>12.3</> MiB"
        );
        // Known length, one unit for both.
        assert_eq!(
            plain(&render_size(12_900_000, 47_185_920)),
            "<g>12.3</>/45.0 MiB"
        );
        // Different units: each with its own.
        assert_eq!(
            plain(&render_size(700 * 1024, 3 * 1024 * 1024)),
            "<g>700.0 KiB</>/3.0 MiB"
        );
    }

    #[test]
    fn counts_render_as_nix_renders_them() {
        assert_eq!(plain(&render_count(2, 1, 3)), "<b>2</>/<g>1</>/3");
        assert_eq!(plain(&render_count(0, 3, 3)), "<g>3</>");
        assert_eq!(plain(&render_count(0, 0, 0)), "");
    }

    #[test]
    fn the_line_shows_totals_and_the_newest_activity() {
        let now = Instant::now();
        let mut state = BarState {
            fetches_done: 1,
            bytes_done: 1024 * 1024,
            waits: 1,
            ..BarState::default()
        };
        state.running = vec![
            (
                ActivityId(1),
                running(ActivityKind::Fetch, "fetching 'github:a/b'", 0, 0, 500),
            ),
            (
                ActivityId(2),
                running(
                    ActivityKind::FileTransfer,
                    "downloading 'https://x/a.tar.gz'",
                    1024 * 1024,
                    3 * 1024 * 1024,
                    400,
                ),
            ),
            // Too young to be named yet.
            (
                ActivityId(3),
                running(ActivityKind::Fetch, "fetching 'github:c/d'", 0, 0, 0),
            ),
        ];
        assert_eq!(
            plain(&render_line(&state, now, 200)),
            "[<b>2</>/<g>1</>/3 fetched, <g>2.0</>/4.0 MiB DL] downloading 'https://x/a.tar.gz'</>"
        );
        // Cut to the terminal's width, colour codes not counted.
        assert_eq!(
            plain(&render_line(&state, now, 12)),
            "[<b>2</>/<g>1</>/3 fetch</>"
        );
    }

    /// A bar that writes into a buffer, and the buffer.
    fn bar() -> (ProgressBar, Arc<Mutex<String>>) {
        let written = Arc::new(Mutex::new(String::new()));
        let sink = written.clone();
        let bar = ProgressBar {
            state: Mutex::new(BarState {
                out: Output(Box::new(move |s: &[u8]| {
                    sink.lock().unwrap().push_str(
                        std::str::from_utf8(s).expect("the bar and these messages are text"),
                    )
                })),
                ..BarState::default()
            }),
            wake: Condvar::new(),
        };
        (bar, written)
    }

    fn take(written: &Mutex<String>) -> String {
        plain(&std::mem::take(&mut *written.lock().unwrap()))
    }

    /// Lets the activities seem to have run for a while.
    fn age(bar: &ProgressBar, by: Duration) {
        let mut state = bar.lock();
        for (_, r) in &mut state.running {
            r.started -= by;
        }
        for (_, r) in &mut state.daemon.activities {
            r.started -= by;
        }
    }

    /// The bar is drawn only during a wait and once an activity has run
    /// for a moment, redrawn only when its line changes, kept below
    /// messages written meanwhile, and erased when the wait ends or the
    /// last activity stops.
    #[test]
    fn the_bar_is_drawn_and_erased_like_nix_s() {
        let (bar, written) = bar();
        let fetch = bar.start(ActivityKind::Fetch, "fetching 'github:a/b'");
        age(&bar, Duration::from_secs(1));
        // Not waiting: nothing drawn.
        draw(&mut bar.lock(), Instant::now());
        bar.write_above(b"trace: one\n");
        assert_eq!(take(&written), "trace: one\n");

        bar.begin_wait();
        // Too young to show.
        let transfer = bar.start(ActivityKind::FileTransfer, "downloading 'x'");
        draw(&mut bar.lock(), Instant::now() - Duration::from_secs(1));
        assert_eq!(take(&written), "");
        draw(&mut bar.lock(), Instant::now());
        let line = "\r[<b>1</>/<g>0</>/1 fetched] fetching 'github:a/b'</>\u{1b}[K";
        assert_eq!(take(&written), line);
        // The same line is not written again.
        draw(&mut bar.lock(), Instant::now());
        assert_eq!(take(&written), "");
        // A message goes above it: erase, write, redraw.
        bar.write_above(b"warning: two\n");
        assert_eq!(
            take(&written),
            format!("\r\u{1b}[K{}{line}", "warning: two\n")
        );
        // The newest activity's text once it has run long enough.
        bar.update(transfer, 512, 1024);
        age(&bar, Duration::from_millis(50));
        draw(&mut bar.lock(), Instant::now());
        assert_eq!(
            take(&written),
            "\r[<b>1</>/<g>0</>/1 fetched, <g>0.5</>/1.0 KiB DL] downloading 'x'</>\u{1b}[K"
        );
        // The wait ends: erased, and messages are written as they are.
        bar.end_wait();
        assert_eq!(take(&written), "\r\u{1b}[K");
        bar.write_above(b"trace: three\n");
        assert_eq!(take(&written), "trace: three\n");
        // Waiting again; the last activity stops: erased.
        bar.begin_wait();
        draw(&mut bar.lock(), Instant::now());
        assert!(take(&written).starts_with("\r["));
        bar.stop(transfer);
        draw(&mut bar.lock(), Instant::now());
        // Its bytes count as done.
        assert_eq!(
            take(&written),
            "\r[<b>1</>/<g>0</>/1 fetched, <g>0.5</> KiB DL] fetching 'github:a/b'</>\u{1b}[K"
        );
        bar.stop(fetch);
        assert_eq!(take(&written), "\r\u{1b}[K");
        bar.end_wait();
        assert_eq!(take(&written), "");
    }
    #[test]
    fn daemon_events_reach_the_bar_and_failed_builds_are_not_counted_as_successes() {
        use brix_daemon::logger::{
            Activity, ActivityKind as Kind, ActivityType as Type, Field, ResultType as Res,
        };
        use brix_daemon::Logger;
        let (bar, written) = bar();
        let bar = Arc::new(bar);
        let mut logger = brix::store::BuildLogger::with_output(brix::BuildLogMode::Normal, |_| {
            panic!("activity text belongs in the bar, not permanent log lines")
        })
        .with_progress(Some(bar.clone()));
        let activity = |id, kind, fields| Activity {
            id,
            level: brix_daemon::Verbosity::Info,
            kind: Kind::Typed { kind, fields },
            text: "building hello".into(),
            parent: 0,
        };
        // Daemon and fetch IDs can overlap without replacing each other.
        let fetch = bar.start(ActivityKind::Fetch, "fetching input");
        logger.start_activity(&activity(fetch.0, Type::Builds, vec![]));
        logger.result(
            fetch.0,
            Res::Progress,
            &[Field::Int(1), Field::Int(3), Field::Int(1), Field::Int(1)],
        );
        logger.start_activity(&activity(
            99,
            Type::Build,
            vec![Field::Str("/nix/store/hash-hello-1.drv".into())],
        ));
        logger.result(99, Res::SetPhase, &[Field::Str("buildPhase".into())]);
        logger.result(99, Res::BuildLogLine, &[Field::Str("compiling\n".into())]);
        age(&bar, Duration::from_secs(1));
        draw(&mut bar.lock(), Instant::now());
        assert_eq!(take(&written), "");
        bar.begin_wait();
        draw(&mut bar.lock(), Instant::now());
        let line = take(&written);
        assert!(line.contains("<b>1</>/<g>1</>/3 built"), "{line}");
        assert!(line.contains("1 failed"), "{line}");
        let line = plain(&render_line(&bar.lock(), Instant::now(), 300));
        assert!(
            line.contains("building hello-1 (buildPhase): compiling"),
            "{line}"
        );
        bar.end_wait();
        assert_eq!(take(&written), "\r\u{1b}[K");
        bar.stop(fetch);
        // Even a disconnected daemon (no stop events) clears its activities.
        drop(logger);
        assert!(bar.lock().daemon.activities.is_empty());
        assert!(bar.lock().running.is_empty());
        assert!(plain(&render_status(&bar.lock())).contains("1 failed"));
    }

    #[test]
    fn daemon_transfer_expectations_are_scoped_to_their_owner() {
        use brix::store::BuildProgress;
        use brix_daemon::logger::{
            Activity, ActivityKind as Kind, ActivityType as Type, Field, ResultType as Res,
        };
        let (bar, _) = bar();
        let activity = |id, kind, parent, text: &str| Activity {
            id,
            parent,
            level: brix_daemon::Verbosity::Info,
            kind: Kind::Typed {
                kind,
                fields: vec![],
            },
            text: text.into(),
        };
        BuildProgress::start(&bar, &activity(1, Type::Realise, 0, "building"));
        BuildProgress::result(
            &bar,
            1,
            Res::SetExpected,
            &[Field::Int(101), Field::Int(4096)],
        );
        BuildProgress::start(&bar, &activity(2, Type::CopyPaths, 1, "copying paths"));
        BuildProgress::result(
            &bar,
            2,
            Res::Progress,
            &[Field::Int(1), Field::Int(2), Field::Int(1), Field::Int(0)],
        );
        BuildProgress::start(&bar, &activity(3, Type::CopyPath, 2, "copying a path"));
        BuildProgress::result(
            &bar,
            3,
            Res::Progress,
            &[Field::Int(1024), Field::Int(2048)],
        );
        BuildProgress::start(&bar, &activity(4, Type::FileTransfer, 3, "hidden download"));
        BuildProgress::result(&bar, 4, Res::Progress, &[Field::Int(512), Field::Int(2048)]);
        age(&bar, Duration::from_secs(1));
        let line = plain(&render_line(&bar.lock(), Instant::now(), 300));
        assert!(
            line.contains("1</>/<g>1</>/2 copied (<g>1.0 KiB</>/2.0 KiB)"),
            "{line}"
        );
        assert!(line.contains("<g>0.5 KiB</>/4.0 KiB DL"), "{line}");
        assert!(line.contains("copying a path"), "{line}");
        assert!(!line.contains("hidden download"), "{line}");
        BuildProgress::stop(&bar, 1);
        let line = plain(&render_status(&bar.lock()));
        assert!(line.contains("<g>0.5 KiB</>/2.0 KiB DL"), "{line}");
        BuildProgress::stop(&bar, 4);
        let line = plain(&render_status(&bar.lock()));
        assert!(line.contains("<g>0.5</> KiB DL"), "{line}");
    }
}
