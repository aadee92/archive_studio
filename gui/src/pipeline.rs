//! Hosts the processing pipeline ([`archive_core::pipeline`]) for as long as the window is
//! open: the [`Engine`] is started on launch for the library root and dropped on exit. One
//! engine per library: if another window (or the `archive` shell) holds it, the GUI runs
//! without one and says so.
//!
//! The engine reports on its own thread, and during catch-up that can be thousands of
//! events a second. Its sink only forwards them to a relay thread, which takes each burst in
//! one go (~100 ms), reads the files it names (stat, tags) into review-grid updates, counts
//! what is left ([`pipeline::status`], at most twice a second while busy and once on idle)
//! and posts the lot to the UI thread as one event-loop callback.
//!
//! Captures: the acquire sink calls [`Handle::saved`] for each saved file, so the engine
//! takes it in at once (no settle delay) and the grid shows it even with no engine here.
//! While any device is capturing the engine runs in [`Mode::Background`] (one worker), so a
//! real-time DV capture isn't starved.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use slint::{ComponentHandle, Timer, TimerMode};

use archive_core::catalog::Catalog;
use archive_core::pipeline::{self, Availability, Config, Engine, Event, Mode, Models, Status, FACES, ORIENT};

use crate::review::{self, FileUpdate};
use crate::{details, people, AppWindow, PipelineBridge};

/// A burst of engine events is gathered this long before it is passed on.
const GATHER: Duration = Duration::from_millis(100);
/// Least time between two status counts while the engine is busy.
const STATUS_EVERY: Duration = Duration::from_millis(500);
/// How long a warning or failure stays in the status line.
const MESSAGE_FOR: Duration = Duration::from_secs(6);

/// What reaches the relay thread.
enum Note {
    Event(Event),
    /// A capture was saved (see [`Handle::saved`]).
    Saved(PathBuf),
    /// [`Engine::start`]'s outcome.
    Started(Result<(), String>),
    /// Something changed the counts without an engine event (see [`Handle::recount`]).
    Recount,
}

/// The engine, shareable with device threads and workers. Cheap to clone.
#[derive(Clone)]
pub struct Handle {
    engine: Arc<Mutex<Option<Arc<Engine>>>>,
    notes: mpsc::Sender<Note>,
}

impl Handle {
    /// The engine, if it runs in this window (and has started).
    pub fn engine(&self) -> Option<Arc<Engine>> {
        self.engine.lock().unwrap().clone()
    }

    /// A capture was saved and closed at `path`: the engine takes it in now rather than
    /// when the watcher has seen it settle, and the review grid shows it. Any thread.
    pub fn saved(&self, path: &Path) {
        if let Some(engine) = self.engine() {
            engine.add(path);
        }
        let _ = self.notes.send(Note::Saved(path.to_owned()));
    }

    /// Refreshes the status counts, e.g. after a review was settled (which the engine
    /// doesn't report). Any thread.
    pub fn recount(&self) {
        let _ = self.notes.send(Note::Recount);
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum EngineState {
    Starting,
    Running,
    /// Another window or the shell has the library's engine.
    Elsewhere,
    Off(String),
}

/// Everything the relay thread hands the UI thread at once.
#[derive(Default)]
struct Batch {
    started: Option<Result<(), String>>,
    ready: Option<Availability>,
    files: Vec<FileUpdate>,
    /// Files whose pixels the engine changed.
    rotated: Vec<PathBuf>,
    identified: bool,
    /// The last of `Busy` / `Idle`.
    busy: Option<bool>,
    status: Option<Status>,
    message: Option<String>,
}

/// The UI thread's side.
struct Host {
    ui: slint::Weak<AppWindow>,
    handle: Handle,
    state: RefCell<EngineState>,
    availability: Cell<Availability>,
    busy: Cell<bool>,
    capturing: Cell<bool>,
    paused: Cell<bool>,
    /// The mode last sent to the engine.
    mode: Cell<Option<Mode>>,
    status: RefCell<Option<Status>>,
    message: RefCell<Option<String>>,
    message_timer: Timer,
}

thread_local! {
    static HOST: RefCell<Option<Rc<Host>>> = const { RefCell::new(None) };
}

fn with_host(f: impl FnOnce(&Host)) {
    HOST.with_borrow(|host| {
        if let Some(host) = host {
            f(host)
        }
    });
}

/// Starts the engine for `root` (on a thread: opening the catalog and watching the tree can
/// take a moment) and the relay that brings its reports to the UI.
pub fn install(ui: &AppWindow, root: PathBuf) -> Handle {
    let (notes, receive) = mpsc::channel();
    let handle = Handle { engine: Arc::default(), notes };

    let (relay_root, weak) = (root.clone(), ui.as_weak());
    let _ = thread::Builder::new().name("pipeline-relay".into()).spawn(move || relay(&relay_root, receive, weak));

    let starter = handle.clone();
    let _ = thread::Builder::new().name("pipeline-start".into()).spawn(move || {
        let events = starter.notes.clone();
        let started = Engine::start(&root, Config::default(), move |event| {
            let _ = events.send(Note::Event(event));
        });
        let result = started.map(|engine| *starter.engine.lock().unwrap() = Some(Arc::new(engine)));
        let _ = starter.notes.send(Note::Started(result));
    });

    let models = Models::locate();
    let host = Rc::new(Host {
        ui: ui.as_weak(),
        handle: handle.clone(),
        state: RefCell::new(EngineState::Starting),
        availability: Cell::new(Availability { orient: models.orientation.is_some(), faces: models.faces.is_some() }),
        busy: Cell::new(false),
        capturing: Cell::new(false),
        paused: Cell::new(false),
        mode: Cell::new(None),
        status: RefCell::default(),
        message: RefCell::default(),
        message_timer: Timer::default(),
    });
    HOST.set(Some(host.clone()));
    ui.global::<PipelineBridge>().on_toggle_pause(|| {
        with_host(|h| {
            h.paused.set(!h.paused.get());
            h.apply_mode();
        })
    });
    host.publish();
    handle
}

/// Stops the engine (if it runs here) and waits for its thread. Called at app exit.
pub fn shutdown() {
    let engine = HOST.with_borrow(|host| host.as_ref().and_then(|h| h.handle.engine.lock().unwrap().take()));
    drop(engine);
}

/// Whether any device is capturing; the engine steps back to one worker meanwhile.
/// UI thread; cheap when nothing changed.
pub fn set_capturing(capturing: bool) {
    with_host(|h| {
        if h.capturing.replace(capturing) != capturing {
            h.apply_mode();
        }
    });
}

impl Host {
    fn wanted_mode(&self) -> Mode {
        if self.paused.get() {
            Mode::Paused
        } else if self.capturing.get() {
            Mode::Background
        } else {
            Mode::Normal
        }
    }

    fn apply_mode(&self) {
        let mode = self.wanted_mode();
        if let Some(engine) = self.handle.engine() {
            if self.mode.replace(Some(mode)) != Some(mode) {
                engine.set_mode(mode);
            }
        }
        self.publish();
    }

    fn apply(&self, batch: Batch) {
        let Some(ui) = self.ui.upgrade() else { return };
        if let Some(started) = batch.started {
            *self.state.borrow_mut() = match started {
                Ok(()) => {
                    // Catching up until it says otherwise.
                    self.busy.set(true);
                    EngineState::Running
                }
                Err(e) if e.contains("already processing") => EngineState::Elsewhere,
                Err(e) => {
                    eprintln!("pipeline: not started: {e}");
                    EngineState::Off(e)
                }
            };
            self.apply_mode();
        }
        if let Some(availability) = batch.ready {
            self.availability.set(availability);
        }
        if !batch.files.is_empty() {
            review::update_files(batch.files);
        }
        for path in batch.rotated {
            details::file_changed(&ui, &path);
        }
        if batch.identified {
            people::refresh_if_shown(&ui);
        }
        if let Some(busy) = batch.busy {
            self.busy.set(busy);
        }
        if let Some(status) = batch.status {
            *self.status.borrow_mut() = Some(status);
        }
        if let Some(message) = batch.message {
            *self.message.borrow_mut() = Some(message);
            self.message_timer.start(TimerMode::SingleShot, MESSAGE_FOR, || {
                with_host(|h| {
                    h.message.take();
                    h.publish();
                })
            });
        }
        self.publish();
    }

    fn publish(&self) {
        let Some(ui) = self.ui.upgrade() else { return };
        let state = self.state.borrow();
        let running = *state == EngineState::Running;
        let line = describe(&state, self.busy.get(), self.wanted_mode(), self.status.borrow().as_ref(), self.availability.get());
        let bridge = ui.global::<PipelineBridge>();
        bridge.set_text(self.message.borrow().clone().unwrap_or(line).into());
        bridge.set_alert(self.message.borrow().is_some());
        bridge.set_busy(running && self.busy.get() && !self.paused.get());
        bridge.set_running(running);
        bridge.set_paused(self.paused.get());
    }
}

/// The status line, e.g. "Processing… orient 12 · faces 40 left · 3 rotations to review".
fn describe(state: &EngineState, busy: bool, mode: Mode, status: Option<&Status>, availability: Availability) -> String {
    match state {
        EngineState::Starting => return "Pipeline starting…".into(),
        EngineState::Elsewhere => return "Pipeline running in another window".into(),
        EngineState::Off(e) => return format!("Pipeline off: {e}"),
        EngineState::Running => {}
    }
    let left: Vec<String> = status
        .map(|s| [(ORIENT, availability.orient, s.orient.pending), (FACES, availability.faces, s.faces.pending)])
        .into_iter()
        .flatten()
        .filter(|&(_, available, n)| available && n > 0)
        .map(|(stage, _, n)| format!("{stage} {}", review::grouped(n)))
        .collect();
    let mut line = match mode {
        Mode::Paused => "Paused".to_owned(),
        _ if !busy => "All caught up".to_owned(),
        Mode::Background => "Processing in the background…".to_owned(),
        Mode::Normal => "Processing…".to_owned(),
    };
    if !left.is_empty() && (busy || mode == Mode::Paused) {
        line = format!("{line} {} left", left.join(" · "));
    }
    if let Some(status) = status {
        let plural = |n: usize, one: &str, many: &str| format!("{} {}", review::grouped(n), if n == 1 { one } else { many });
        if status.orient.review > 0 {
            line.push_str(&format!(" · {} to review", plural(status.orient.review, "rotation", "rotations")));
        }
        let failed = status.orient.failed + status.faces.failed;
        if failed > 0 {
            line.push_str(&format!(" · {failed} failed"));
        }
    }
    line
}

/// What one burst of notes amounts to.
#[derive(Default)]
struct Digest {
    /// The engine said anything (so the counts may have moved).
    engine: bool,
    started: Option<Result<(), String>>,
    ready: Option<Availability>,
    /// Path -> whether its pixels changed.
    files: HashMap<PathBuf, bool>,
    rotated: Vec<PathBuf>,
    identified: bool,
    busy: Option<bool>,
    message: Option<String>,
}

impl Digest {
    fn add(&mut self, root: &Path, note: Note) {
        let show = |path: &Path| path.strip_prefix(root).unwrap_or(path).display().to_string();
        let event = match note {
            Note::Saved(path) => {
                self.files.entry(path).or_insert(false);
                return;
            }
            Note::Started(result) => {
                self.engine |= result.is_ok();
                self.started = Some(result);
                return;
            }
            Note::Recount => {
                self.engine = true;
                return;
            }
            Note::Event(event) => event,
        };
        self.engine = true;
        match event {
            Event::CatchUp { added, changed, missing } => {
                if added + changed + missing > 0 {
                    eprintln!("pipeline: catch-up: {added} new, {changed} changed, {missing} missing");
                }
            }
            Event::Ready(availability) => {
                self.ready = Some(availability);
                let missing: Vec<&str> =
                    [(ORIENT, availability.orient), (FACES, availability.faces)].into_iter().filter(|s| !s.1).map(|s| s.0).collect();
                if !missing.is_empty() {
                    let dir = Models::dir().map_or_else(|| "the models directory".to_owned(), |d| d.display().to_string());
                    eprintln!("pipeline: no model for {} in {dir}; skipping", missing.join(", "));
                }
            }
            Event::Rotated { path, .. } => {
                self.files.insert(path.clone(), true);
                self.rotated.push(path);
            }
            Event::Updated(path) => {
                self.files.entry(path).or_insert(false);
            }
            Event::Failed { path, stage, error } => {
                let message = format!("{stage} failed: {}: {error}", show(&path));
                eprintln!("pipeline: {message}");
                self.message = Some(message);
            }
            Event::Warning(warning) => {
                eprintln!("pipeline: warning: {warning}");
                self.message = Some(format!("Warning: {warning}"));
            }
            Event::Identified { .. } => self.identified = true,
            Event::Busy => self.busy = Some(true),
            Event::Idle => self.busy = Some(false),
            Event::Review { .. } | Event::Synced { .. } => {}
        }
    }
}

/// The relay thread: gathers bursts of notes, reads what they name, posts one [`Batch`]
/// per burst. Ends when every sender is gone.
fn relay(root: &Path, notes: mpsc::Receiver<Note>, ui: slint::Weak<AppWindow>) {
    let catalog = Catalog::open(&Catalog::default_path(root), root).ok();
    let mut availability = None;
    // The engine moved since the last count, and when that count was.
    let (mut owed, mut counted) = (false, None::<Instant>);
    let count_due = |counted: Option<Instant>| counted.map_or(Duration::ZERO, |t: Instant| STATUS_EVERY.saturating_sub(t.elapsed()));
    loop {
        let wait = if owed { count_due(counted) } else { Duration::from_secs(3600) };
        let mut digest = Digest::default();
        match notes.recv_timeout(wait) {
            Ok(note) => digest.add(root, note),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
        }
        let deadline = Instant::now() + GATHER;
        while let Ok(note) = notes.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            digest.add(root, note);
        }

        availability = digest.ready.or(availability);
        owed |= digest.engine;
        let mut batch = Batch {
            started: digest.started,
            ready: digest.ready,
            rotated: digest.rotated,
            identified: digest.identified,
            busy: digest.busy,
            message: digest.message,
            ..Batch::default()
        };
        batch.files = digest
            .files
            .into_iter()
            .filter_map(|(path, changed)| FileUpdate::read(&path, root, catalog.as_ref(), changed))
            .collect();
        // Counted at most every STATUS_EVERY while busy, and once more when it goes idle.
        if owed && (batch.busy == Some(false) || count_due(counted).is_zero()) {
            if let (Some(catalog), Some(availability)) = (&catalog, availability) {
                match pipeline::status(catalog, availability) {
                    Ok(status) => batch.status = Some(status),
                    Err(e) => eprintln!("pipeline: status: {e}"),
                }
            }
            (owed, counted) = (false, Some(Instant::now()));
        }

        let nothing = batch.started.is_none()
            && batch.ready.is_none()
            && batch.files.is_empty()
            && batch.rotated.is_empty()
            && !batch.identified
            && batch.busy.is_none()
            && batch.status.is_none()
            && batch.message.is_none();
        if !nothing {
            let _ = ui.upgrade_in_event_loop(move |_| with_host(|h| h.apply(batch)));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use archive_core::catalog::runs::RunCounts;

    #[test]
    fn engine_is_shareable_with_device_threads() {
        fn send_sync<T: Send + Sync>() {}
        send_sync::<Engine>();
        send_sync::<Handle>();
    }

    #[test]
    fn status_line() {
        let both = Availability { orient: true, faces: true };
        let counts = |pending, review, failed| RunCounts { done: 5, review, failed, pending };
        let status = Status { orient: counts(12, 0, 0), faces: counts(40, 0, 0), faces_since_identify: 0, needs_sync: 0 };
        let running = EngineState::Running;
        assert_eq!(describe(&running, true, Mode::Normal, Some(&status), both), "Processing… orient 12 · faces 40 left");
        assert_eq!(
            describe(&running, true, Mode::Normal, Some(&status), Availability { orient: false, faces: true }),
            "Processing… faces 40 left",
            "stages without a model aren't counted"
        );
        assert_eq!(describe(&running, true, Mode::Background, None, both), "Processing in the background…");
        assert_eq!(describe(&running, true, Mode::Paused, Some(&status), both), "Paused orient 12 · faces 40 left");

        let done = Status { orient: counts(0, 3, 1), faces: counts(0, 0, 2), ..status };
        assert_eq!(describe(&running, false, Mode::Normal, Some(&done), both), "All caught up · 3 rotations to review · 3 failed");
        let one = Status { orient: counts(0, 1, 0), faces: counts(0, 0, 0), ..status };
        assert_eq!(describe(&running, false, Mode::Normal, Some(&one), both), "All caught up · 1 rotation to review");

        assert_eq!(describe(&EngineState::Elsewhere, true, Mode::Normal, None, both), "Pipeline running in another window");
        assert_eq!(describe(&EngineState::Starting, false, Mode::Normal, None, both), "Pipeline starting…");
    }

    #[test]
    fn digest_coalesces_a_burst() {
        let root = Path::new("/lib");
        let mut digest = Digest::default();
        for i in 0..1000 {
            digest.add(root, Note::Event(Event::Updated(root.join(format!("{}.png", i % 10)))));
        }
        digest.add(root, Note::Event(Event::Rotated { path: root.join("3.png"), cw: 90, by_user: false }));
        digest.add(root, Note::Event(Event::Updated(root.join("3.png"))));
        digest.add(root, Note::Saved(root.join("scan.png")));
        digest.add(root, Note::Event(Event::Busy));
        digest.add(root, Note::Event(Event::Idle));
        assert_eq!(digest.files.len(), 11, "one update per file");
        assert_eq!(digest.files[&root.join("3.png")], true, "a rotation sticks through later updates");
        assert_eq!(digest.files[&root.join("4.png")], false);
        assert_eq!(digest.rotated, vec![root.join("3.png")]);
        assert_eq!(digest.busy, Some(false), "the last word counts");
        assert!(digest.engine);
    }

    /// What a status count costs on a real library (read only).
    /// `BENCH_DIR=… cargo test --release -- --ignored --nocapture bench_status`
    #[test]
    #[ignore]
    fn bench_status() {
        let root = std::env::var_os("BENCH_DIR").map(PathBuf::from).unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("test_images"));
        let catalog = Catalog::open(&Catalog::default_path(&root), &root).unwrap();
        let both = Availability { orient: true, faces: true };
        let mut times: Vec<Duration> = (0..5)
            .map(|_| {
                let start = Instant::now();
                pipeline::status(&catalog, both).unwrap();
                start.elapsed()
            })
            .collect();
        times.sort();
        println!("status count: median {:?}, max {:?}: {:?}", times[2], times[4], pipeline::status(&catalog, both).unwrap());
    }

    /// The engine hosted as the GUI does, on a library with no models: files are registered
    /// and reported, `add` takes a capture in at once, and the relay turns that into
    /// review-grid updates. Also times the status count.
    #[test]
    fn hosts_an_engine_without_models() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::path::absolute(dir.path()).unwrap();
        image::RgbImage::from_pixel(64, 48, image::Rgb([1, 2, 3])).save(root.join("old.png")).unwrap();

        let (tx, rx) = mpsc::channel();
        let config = Config { threads: 1, models: Models::default() };
        let engine = Engine::start(&root, config, move |e| {
            let _ = tx.send(e);
        })
        .unwrap();
        let second = Engine::start(&root, Config { threads: 1, models: Models::default() }, |_| {});
        assert!(second.err().is_some_and(|e| e.contains("already processing")), "one engine per library");

        let wait = |want: &dyn Fn(&Event) -> bool| {
            let deadline = Instant::now() + Duration::from_secs(20);
            loop {
                let event = rx.recv_timeout(deadline.saturating_duration_since(Instant::now())).expect("engine event");
                if want(&event) {
                    return event;
                }
            }
        };
        wait(&|e| matches!(e, Event::CatchUp { added: 1, .. }));
        wait(&|e| matches!(e, Event::Idle));

        let scan = root.join("scan.png");
        image::RgbImage::from_pixel(64, 48, image::Rgb([9, 9, 9])).save(&scan).unwrap();
        // `add` straight after the write: the watcher's report of it must not restart the wait.
        let start = Instant::now();
        engine.add(&scan);
        let Event::Updated(path) = wait(&|e| matches!(e, Event::Updated(_))) else { unreachable!() };
        assert_eq!(path, scan);
        assert!(start.elapsed() < Duration::from_millis(900), "no settle delay: {:?}", start.elapsed());

        let catalog = Catalog::open(&Catalog::default_path(&root), &root).unwrap();
        let mut digest = Digest::default();
        digest.add(&root, Note::Event(Event::Updated(path)));
        let updates: Vec<FileUpdate> =
            digest.files.into_iter().filter_map(|(p, changed)| FileUpdate::read(&p, &root, Some(&catalog), changed)).collect();
        assert!(matches!(&updates[..], [FileUpdate::Present(..)]));

        let start = Instant::now();
        let status = pipeline::status(&catalog, Availability { orient: true, faces: true }).unwrap();
        eprintln!("status count: {:?}", start.elapsed());
        assert_eq!(status.orient.pending, 2, "both files registered, neither oriented");

        std::fs::remove_file(&scan).unwrap();
        assert!(matches!(FileUpdate::read(&scan, &root, Some(&catalog), false), Some(FileUpdate::Gone(_))));
        drop(engine);
    }
}
