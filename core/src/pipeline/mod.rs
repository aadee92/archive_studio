//! The processing pipeline: what happens to a file once it is in the library.
//!
//! ```text
//! file appears ─ register ─ orient ─┬─ faces ─────┬─ identify (library-wide, when quiet)
//!                                    ├─ signature └─ sync sidecars (when quiet)
//!                                    └─ describe
//! ```
//!
//! An [`Engine`] runs the pipeline on a thread for as long as its host (the GUI, the
//! interactive CLI) is open. On start it catches up with what changed on disk while nothing
//! was running, then watches the library and keeps draining whatever is due. What is due
//! comes from the catalog ([`catalog::runs`]): a stage is due on an asset when it hasn't
//! run on the current file (size and mtime) or ran with an older version. So the engine can
//! stop at any moment: unfinished work is simply still due next time.
//!
//! - **register**: new, changed and deleted files update the catalog (deleted assets are
//!   kept, marked missing, as they hold the user's tags and faces).
//! - **orient**: the orientation model's 90°-step prediction. A confident one rotates the
//!   file itself (see [`rotate`]), along with any face and tag regions; an unsure one, or one
//!   for a format that can't be rewritten losslessly yet, waits for the user's review.
//! - **faces**: detection and embedding into the catalog (after orient, so boxes are
//!   upright).
//! - **signature**: a perceptual hash and colour histogram for finding near-duplicates
//!   ([`catalog::similar`]); needs no model.
//! - **describe**: a CLIP embedding, for search by description ([`catalog::semantic`]), and
//!   the scene, object and event tags it suggests ([`describe`]).
//! - **identify**: suggestions from the confirmed faces and clusters of the unknown ones,
//!   after a batch rather than per face.
//! - **sync**: tags into XMP sidecars.
//!
//! The user's word is final: a rotation they chose is never redone by the model, and faces
//! they reviewed survive rescans.
//!
//! One engine per library at a time: [`Engine::start`] takes a lock in the library's
//! `.archive-studio` directory, released when the engine (or its process) ends.

pub mod describe;
pub mod rotate;

use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::{File, OpenOptions};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use image::{DynamicImage, ImageDecoder, ImageReader};
use notify::{RecursiveMode, Watcher};

use crate::catalog::faces::{self as catalog_faces, NewFace, DEFAULT_MIN_SIMILARITY};
use crate::catalog::similar::Signature;
use crate::catalog::runs::{self, Due, Retry, Run, RunCounts, RunStatus, StageQuery};
use crate::catalog::{AssetId, Catalog, Detection, DEFAULT_MIN_CONFIDENCE};
use crate::media::{classify, MediaKind};

pub const ORIENT: &str = "orient";
/// The orientation model's name; a different one makes every orient run due again.
pub const ORIENT_VERSION: &str = "dio-v2-0.9882";
pub const FACES: &str = "faces";
/// Near-duplicate signatures ([`crate::catalog::similar`]).
pub const SIGNATURE: &str = "signature";
/// Changing how signatures are computed makes every signature run due again.
pub const SIGNATURE_VERSION: &str = "dhash-hist-v1";
/// CLIP embeddings and the tags they suggest ([`describe`]).
pub const DESCRIBE: &str = "describe";
/// Confidence from which a predicted rotation is applied without asking.
pub const AUTO_ROTATE: f32 = 0.85;

/// How long a file must stay unchanged after the watcher reports it before it is taken in
/// (it may still be being copied).
const SETTLE: Duration = Duration::from_secs(1);
/// How long the per-file stages must have been idle before the library-wide ones run.
const QUIET: Duration = Duration::from_secs(3);
/// Jobs fetched from the catalog per refill.
const BATCH: usize = 256;
/// `pipeline_state` key: faces stored since identify last ran.
const FACES_SINCE_IDENTIFY: &str = "faces_since_identify";

/// Where the models are: `$ARCHIVE_STUDIO_MODELS`, else `~/.local/share/archive-studio/models`
/// (`$XDG_DATA_HOME` respected). A stage whose model is missing is skipped.
#[derive(Clone, Debug, Default)]
pub struct Models {
    /// `orientation.onnx`.
    pub orientation: Option<PathBuf>,
    /// The `buffalo_l` directory (`det_10g.onnx`, `w600k_r50.onnx`).
    pub faces: Option<PathBuf>,
    /// The `tinyclip-8m` directory (`visual.onnx`, `textual.onnx`).
    pub describe: Option<PathBuf>,
}

impl Models {
    pub fn dir() -> Option<PathBuf> {
        if let Some(dir) = std::env::var_os("ARCHIVE_STUDIO_MODELS") {
            return Some(dir.into());
        }
        let data = std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|home| Path::new(&home).join(".local/share")))?;
        Some(data.join("archive-studio/models"))
    }

    pub fn locate() -> Self {
        let Some(dir) = Self::dir() else { return Self::default() };
        let orientation = dir.join("orientation.onnx");
        let faces = dir.join("buffalo_l");
        let describe = dir.join("tinyclip-8m");
        Self {
            orientation: orientation.is_file().then_some(orientation),
            faces: faces.join("det_10g.onnx").is_file().then_some(faces),
            describe: describe.join("visual.onnx").is_file().then_some(describe),
        }
    }
}

#[derive(Clone, Debug)]
pub struct Config {
    /// Worker threads for the per-file stages.
    pub threads: usize,
    pub models: Models,
}

impl Default for Config {
    fn default() -> Self {
        let cores = thread::available_parallelism().map_or(1, |n| n.get());
        Self { threads: cores.saturating_sub(1).max(1), models: Models::locate() }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// All worker threads.
    Normal,
    /// One worker, e.g. while a device is capturing.
    Background,
    /// Nothing new is started; the watcher keeps recording changes.
    Paused,
}

/// What the engine reports, on its own thread: hand off anything slow.
#[derive(Clone, Debug)]
pub enum Event {
    /// Startup catch-up with the files on disk is done.
    CatchUp { added: usize, changed: usize, missing: usize },
    /// Which stages can run, once the models are loaded.
    Ready(Availability),
    Rotated { path: PathBuf, cw: u16, by_user: bool },
    /// The stage wants the user to look at this file.
    Review { path: PathBuf, stage: &'static str, detail: String },
    Failed { path: PathBuf, stage: &'static str, error: String },
    /// The asset's file or catalog data changed (registered, gone, faces found).
    Updated(PathBuf),
    Identified { suggested: usize, clusters: usize },
    Synced { written: usize },
    Warning(String),
    /// Work started after being idle.
    Busy,
    /// Nothing left to do.
    Idle,
}

/// Which model-backed stages can run.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Availability {
    pub orient: bool,
    pub faces: bool,
}

/// How each stage stands, for a status display.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Status {
    pub orient: RunCounts,
    pub faces: RunCounts,
    /// Faces stored since identify last ran.
    pub faces_since_identify: i64,
    pub needs_sync: usize,
}

fn orient_query() -> StageQuery<'static> {
    StageQuery { stage: ORIENT, version: ORIENT_VERSION, kind: MediaKind::Image, after: None }
}

fn faces_query(availability: Availability) -> StageQuery<'static> {
    // Without the orientation model, faces don't wait for it.
    let after = availability.orient.then_some(ORIENT);
    StageQuery { stage: FACES, version: catalog_faces::MODEL, kind: MediaKind::Image, after }
}

fn signature_query(availability: Availability) -> StageQuery<'static> {
    // Upright first, like faces: rotated copies of a photo should match.
    let after = availability.orient.then_some(ORIENT);
    StageQuery { stage: SIGNATURE, version: SIGNATURE_VERSION, kind: MediaKind::Image, after }
}

/// How the signature stage stands (kept out of [`Status`] so hosts building one by hand
/// keep compiling).
pub fn signature_status(catalog: &Catalog, availability: Availability) -> crate::catalog::Result<RunCounts> {
    catalog.run_counts(&signature_query(availability))
}

fn describe_query(availability: Availability) -> StageQuery<'static> {
    // Upright first: the model sees the picture as it is meant to be seen.
    let after = availability.orient.then_some(ORIENT);
    StageQuery { stage: DESCRIBE, version: describe::VERSION, kind: MediaKind::Image, after }
}

/// How the describe stage stands (kept out of [`Status`], like [`signature_status`]). It
/// runs when [`Models::describe`] is found and loads.
pub fn describe_status(catalog: &Catalog, availability: Availability) -> crate::catalog::Result<RunCounts> {
    catalog.run_counts(&describe_query(availability))
}

/// Rotates the asset's file `cw` degrees clockwise (90, 180 or 270) as the user's decision,
/// without an engine: rewrites the file losslessly, turns its face and tag regions, and
/// records the rotation as the user's, so the orientation model never redoes it. Safe while
/// an engine runs on the library (its in-flight work on the old file is discarded), though
/// a host with an engine can as well use [`Engine::rotate`].
pub fn rotate_file(catalog: &Catalog, path: &Path, cw: u16) -> Result<(), String> {
    let cw = cw % 360;
    if cw % 90 != 0 || cw == 0 {
        return Err("rotate by 90, 180 or 270 degrees".into());
    }
    let asset = catalog.find_asset(path).map_err(|e| e.to_string())?.ok_or("not in the library")?;
    if !rotate::can_rotate(path) {
        let ext = path.extension().map_or_else(String::new, |e| e.to_string_lossy().to_uppercase());
        return Err(format!("{ext} files can't be rotated losslessly yet"));
    }
    let temp = rotate::temp_path(path);
    rotate::rotate_into(path, &temp, cw).map_err(|e| {
        let _ = std::fs::remove_file(&temp);
        e.to_string()
    })?;
    let input = replace_rotated(catalog, asset, path, &temp, cw)?;
    let detail = format!("turned {cw}° by you");
    let run = Run { stage: ORIENT, version: ORIENT_VERSION, input: &input, status: RunStatus::User, value: Some(cw as i64), detail: &detail };
    catalog.set_run(asset, &run).map_err(|e| e.to_string())
}

/// Moves the rotated `temp` over `path` and updates the catalog to match. Returns the new
/// fingerprint.
fn replace_rotated(catalog: &Catalog, asset: AssetId, path: &Path, temp: &Path, cw: u16) -> Result<String, String> {
    std::fs::rename(temp, path).map_err(|e| {
        let _ = std::fs::remove_file(temp);
        format!("replace {}: {e}", path.display())
    })?;
    let (size, mtime) = runs::stat(path).map_err(|e| e.to_string())?;
    // Recorded now, so the watcher's report of our own write finds nothing new.
    catalog.record_file(path, MediaKind::Image, size, mtime).map_err(|e| e.to_string())?;
    catalog.rotate_regions(asset, cw).map_err(|e| e.to_string())?;
    Ok(runs::fingerprint(size, mtime))
}

/// The pipeline's standing in `catalog`, read on any connection.
pub fn status(catalog: &Catalog, availability: Availability) -> crate::catalog::Result<Status> {
    Ok(Status {
        orient: catalog.run_counts(&orient_query())?,
        faces: catalog.run_counts(&faces_query(availability))?,
        faces_since_identify: catalog.pipeline_value(FACES_SINCE_IDENTIFY)?,
        needs_sync: catalog.needing_sync()?.len(),
    })
}

/// The pipeline running on its own thread; dropping it stops the pipeline.
pub struct Engine {
    send: mpsc::Sender<Msg>,
    thread: Option<JoinHandle<()>>,
    availability: Arc<Mutex<Option<Availability>>>,
    /// The face models the workers loaded, shared with hosts ([`Engine::face_models`]).
    face_models: Arc<Mutex<Option<Arc<faces::Faces>>>>,
    /// The describe stage's text model, likewise ([`Engine::text_search`]).
    text_search: Arc<Mutex<Option<Arc<describe::TextSearch>>>>,
}

type Reply = mpsc::Sender<Result<(), String>>;

enum Command {
    Add(PathBuf),
    Rotate(PathBuf, u16, Reply),
    /// Applies the rotation orient proposed for review.
    Accept(PathBuf, Reply),
    /// The file is upright as it is.
    Keep(PathBuf, Reply),
    /// Forgets these failed runs and runs the stages again; replies with how many.
    Retry(Retry, Option<mpsc::Sender<Result<usize, String>>>),
    SetMode(Mode),
    Shutdown,
}

enum Msg {
    Command(Command),
    Fs(notify::Result<notify::Event>),
    Done(Outcome),
}

impl Engine {
    /// Starts the pipeline for the library at `root`. Fails if another engine holds the
    /// library, or the catalog can't be opened.
    pub fn start(root: &Path, config: Config, sink: impl Fn(Event) + Send + Sync + 'static) -> Result<Self, String> {
        let catalog = Catalog::open(&Catalog::default_path(root), root).map_err(|e| e.to_string())?;
        let lock = Lock::acquire(&root.join(".archive-studio").join("engine.lock"))
            .map_err(|e| format!("engine lock: {e}"))?
            .ok_or("another Archive Studio is already processing this library")?;

        let (send, receive) = mpsc::channel();
        let watch = send.clone();
        let mut watcher = notify::recommended_watcher(move |event| {
            let _ = watch.send(Msg::Fs(event));
        })
        .map_err(|e| format!("file watcher: {e}"))?;
        watcher.watch(catalog.root(), RecursiveMode::Recursive).map_err(|e| format!("file watcher: {e}"))?;

        let availability = Arc::new(Mutex::new(None));
        let shared = availability.clone();
        let face_models = Arc::new(Mutex::new(None));
        let shared_faces = face_models.clone();
        let text_search = Arc::new(Mutex::new(None));
        let shared_text = text_search.clone();
        let done = send.clone();
        let thread = thread::Builder::new()
            .name("pipeline".into())
            .spawn(move || {
                let _keep = (lock, watcher);
                let sink: Arc<dyn Fn(Event) + Send + Sync> = Arc::new(sink);
                let workers = Workers::start(&config, done, &sink);
                *shared_faces.lock().unwrap() = workers.faces.clone();
                *shared_text.lock().unwrap() = workers.text_search.clone();
                *shared.lock().unwrap() = Some(workers.availability);
                sink(Event::Ready(workers.availability));
                Pipeline::new(catalog, workers, sink).run(receive);
            })
            .map_err(|e| e.to_string())?;
        Ok(Self { send, thread: Some(thread), availability, face_models, text_search })
    }

    /// Which stages can run; `None` while the models are loading.
    pub fn availability(&self) -> Option<Availability> {
        *self.availability.lock().unwrap()
    }

    /// The face models the engine loaded (`None` while loading, or without them), for a host
    /// to embed a face the user boxed ([`ops::add_face`](crate::ops::add_face)) without loading
    /// them a second time.
    pub fn face_models(&self) -> Option<Arc<faces::Faces>> {
        self.face_models.lock().unwrap().clone()
    }

    /// The describe stage's text model (`None` while loading, or without it), for a host to
    /// search by description ([`ops::text_search`](crate::ops::text_search)) without loading
    /// it a second time.
    pub fn text_search(&self) -> Option<Arc<describe::TextSearch>> {
        self.text_search.lock().unwrap().clone()
    }

    /// Takes in a file known to be complete (e.g. a capture just saved) without waiting
    /// for it to settle.
    pub fn add(&self, path: impl Into<PathBuf>) {
        let _ = self.send.send(Msg::Command(Command::Add(path.into())));
    }

    /// Rotates the file `cw` degrees clockwise (90, 180 or 270) as the user's decision.
    pub fn rotate(&self, path: impl Into<PathBuf>, cw: u16) -> Result<(), String> {
        let path = path.into();
        self.ask(|reply| Command::Rotate(path, cw, reply))
    }

    /// Applies the rotation that orient proposed for review.
    pub fn accept(&self, path: impl Into<PathBuf>) -> Result<(), String> {
        let path = path.into();
        self.ask(|reply| Command::Accept(path, reply))
    }

    /// Settles a review: the file is upright as it is.
    pub fn keep(&self, path: impl Into<PathBuf>) -> Result<(), String> {
        let path = path.into();
        self.ask(|reply| Command::Keep(path, reply))
    }

    /// Runs the failed stages again.
    pub fn retry_failed(&self) {
        let _ = self.send.send(Msg::Command(Command::Retry(Retry::All, None)));
    }

    /// Runs the failed stages `which` names again ([`Catalog::retry`] on the engine's
    /// thread, which then looks for work at once). Returns how many runs were forgotten.
    pub fn retry(&self, which: Retry) -> Result<usize, String> {
        let (reply, answer) = mpsc::channel();
        self.send.send(Msg::Command(Command::Retry(which, Some(reply)))).map_err(|_| "the pipeline has stopped".to_owned())?;
        answer.recv().map_err(|_| "the pipeline has stopped".to_owned())?
    }

    pub fn set_mode(&self, mode: Mode) {
        let _ = self.send.send(Msg::Command(Command::SetMode(mode)));
    }

    fn ask(&self, command: impl FnOnce(Reply) -> Command) -> Result<(), String> {
        let (reply, answer) = mpsc::channel();
        self.send.send(Msg::Command(command(reply))).map_err(|_| "the pipeline has stopped".to_owned())?;
        answer.recv().map_err(|_| "the pipeline has stopped".to_owned())?
    }
}

impl Drop for Engine {
    /// Stops taking work and returns once the engine thread has; jobs still running finish
    /// on their own and are discarded (they will be due next time).
    fn drop(&mut self) {
        let _ = self.send.send(Msg::Command(Command::Shutdown));
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// An exclusive `flock` on a file, released when dropped or when the process ends.
struct Lock(#[allow(dead_code)] File);

impl Lock {
    fn acquire(path: &Path) -> std::io::Result<Option<Self>> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let file = OpenOptions::new().create(true).truncate(false).write(true).open(path)?;
        // SAFETY: flock on a descriptor we own.
        let locked = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0;
        Ok(locked.then_some(Self(file)))
    }
}

// ---------------------------------------------------------------------------------------
// Workers: the per-file stages, on as many threads as configured.

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Stage {
    Orient,
    Faces,
    Signature,
    Describe,
}

impl Stage {
    fn name(self) -> &'static str {
        match self {
            Self::Orient => ORIENT,
            Self::Faces => FACES,
            Self::Signature => SIGNATURE,
            Self::Describe => DESCRIBE,
        }
    }
}

struct Job {
    stage: Stage,
    due: Due,
}

struct Prediction {
    cw: u16,
    confidence: f32,
}

enum Outcome {
    /// The prediction, and the rotated file written next to the original if it applies.
    Orient(Due, Result<(Prediction, Option<PathBuf>), String>),
    Faces(Due, Result<Vec<NewFace>, String>),
    Signature(Due, Result<Signature, String>),
    Describe(Due, Result<describe::Description, String>),
}

struct Loaded {
    orientation: Option<orientation::OrientationDetector>,
    faces: Option<Arc<faces::Faces>>,
    describer: Option<describe::Describer>,
}

struct Workers {
    jobs: mpsc::Sender<Job>,
    /// Workers with an index below this take jobs.
    active: Arc<AtomicUsize>,
    threads: usize,
    availability: Availability,
    /// The describe stage's model loaded.
    describe: bool,
    faces: Option<Arc<faces::Faces>>,
    text_search: Option<Arc<describe::TextSearch>>,
}

impl Workers {
    fn start(config: &Config, done: mpsc::Sender<Msg>, sink: &Arc<dyn Fn(Event) + Send + Sync>) -> Self {
        let warn = |what: &str, e: &dyn std::fmt::Display| sink(Event::Warning(format!("{what} model: {e}; skipping that stage")));
        let orientation = config.models.orientation.as_ref().and_then(|path| {
            orientation::OrientationDetector::new(path).map_err(|e| warn("orientation", &e)).ok()
        });
        let faces = config.models.faces.as_ref().and_then(|dir| faces::Faces::from_dir(dir).map_err(|e| warn("face", &e)).ok()).map(Arc::new);
        let describer = config.models.describe.as_ref().and_then(|dir| {
            let started = Instant::now();
            let describer = describe::Describer::from_dir(dir).map_err(|e| warn("CLIP", &e)).ok();
            log::debug!("CLIP model and tag vocabulary loaded in {:?}", started.elapsed());
            describer
        });
        let availability = Availability { orient: orientation.is_some(), faces: faces.is_some() };
        let describe = describer.is_some();
        let text_search = describer.as_ref().map(describe::Describer::text_search);
        let models = Arc::new(Loaded { orientation, faces: faces.clone(), describer });

        let (jobs, receive) = mpsc::channel::<Job>();
        let receive = Arc::new(Mutex::new(receive));
        let threads = config.threads.max(1);
        let active = Arc::new(AtomicUsize::new(threads));
        for index in 0..threads {
            let (receive, done, models, active) = (receive.clone(), done.clone(), models.clone(), active.clone());
            let _ = thread::Builder::new().name(format!("pipeline-{index}")).spawn(move || loop {
                while index >= active.load(Ordering::Relaxed) {
                    thread::sleep(Duration::from_millis(200));
                }
                // Its own statement, so the lock is released before the work.
                let Ok(job) = receive.lock().unwrap().recv() else { return };
                if done.send(Msg::Done(work(job, &models))).is_err() {
                    return;
                }
            });
        }
        Self { jobs, active, threads, availability, describe, faces, text_search }
    }
}

fn work(job: Job, models: &Loaded) -> Outcome {
    let Job { stage, due } = job;
    match stage {
        Stage::Orient => {
            let result = orient(&due.path, models);
            Outcome::Orient(due, result)
        }
        Stage::Faces => {
            let result = match &models.faces {
                Some(faces) => catalog_faces::scan_image(faces, &due.path),
                None => Err("no face model".into()),
            };
            Outcome::Faces(due, result)
        }
        Stage::Signature => {
            let result = Signature::of_file(&due.path);
            Outcome::Signature(due, result)
        }
        Stage::Describe => {
            let result = match &models.describer {
                Some(describer) => describer.describe_file(&due.path),
                None => Err("no CLIP model".into()),
            };
            Outcome::Describe(due, result)
        }
    }
}

fn orient(path: &Path, models: &Loaded) -> Result<(Prediction, Option<PathBuf>), String> {
    let detector = models.orientation.as_ref().ok_or("no orientation model")?;
    let image = decode_upright(path).map_err(|e| format!("decode: {e}"))?;
    let prediction = detector.detect(&image).map_err(|e| format!("orientation model: {e}"))?;
    let prediction = Prediction { cw: prediction.rotate_cw, confidence: prediction.confidence };
    if prediction.cw == 0 || prediction.confidence < AUTO_ROTATE || !rotate::can_rotate(path) {
        return Ok((prediction, None));
    }
    let temp = rotate::temp_path(path);
    match rotate::rotate_into(path, &temp, prediction.cw) {
        Ok(()) => Ok((prediction, Some(temp))),
        Err(e) => {
            let _ = std::fs::remove_file(&temp);
            Err(format!("rotate: {e}"))
        }
    }
}

/// The image as it is meant to be seen (EXIF orientation applied).
fn decode_upright(path: &Path) -> image::ImageResult<DynamicImage> {
    let mut decoder = ImageReader::open(path)?.with_guessed_format()?.into_decoder()?;
    let orientation = decoder.orientation()?;
    let mut image = DynamicImage::from_decoder(decoder)?;
    image.apply_orientation(orientation);
    Ok(image)
}

// ---------------------------------------------------------------------------------------
// The engine thread: catalog writes, scheduling, the watcher, library-wide stages.

/// A path the watcher reported, waiting to settle.
struct Pending {
    since: Instant,
    last: Option<(u64, i64)>,
    /// Known complete ([`Engine::add`]): no need to wait.
    trusted: bool,
}

struct Pipeline {
    catalog: Catalog,
    workers: Workers,
    sink: Arc<dyn Fn(Event) + Send + Sync>,
    mode: Mode,
    queues: HashMap<Stage, VecDeque<Due>>,
    /// Queued or running.
    known: HashSet<(AssetId, Stage)>,
    /// Stages whose last refill found nothing new; cleared whenever something changes.
    exhausted: HashSet<Stage>,
    running: usize,
    pending: HashMap<PathBuf, Pending>,
    last_work: Instant,
    idle: bool,
    /// Assets whose sidecar couldn't be written; not retried until something else needs it.
    sync_failed: HashSet<AssetId>,
}

impl Pipeline {
    fn new(catalog: Catalog, workers: Workers, sink: Arc<dyn Fn(Event) + Send + Sync>) -> Self {
        Self {
            catalog,
            workers,
            sink,
            mode: Mode::Normal,
            queues: HashMap::new(),
            known: HashSet::new(),
            exhausted: HashSet::new(),
            running: 0,
            pending: HashMap::new(),
            last_work: Instant::now(),
            idle: false,
            sync_failed: HashSet::new(),
        }
    }

    fn emit(&self, event: Event) {
        log_event(&event);
        (self.sink)(event);
    }

    fn run(mut self, receive: mpsc::Receiver<Msg>) {
        log::info!("pipeline engine for {}: {} workers, {:?}", self.catalog.root().display(), self.workers.threads, self.workers.availability);
        let started = Instant::now();
        let caught_up = catch_up(&self.catalog);
        log::debug!("catch-up took {:?}", started.elapsed());
        match caught_up {
            Ok((added, changed, missing)) => self.emit(Event::CatchUp { added, changed, missing }),
            Err(e) => self.emit(Event::Warning(format!("catch-up: {e}"))),
        }
        loop {
            self.dispatch();
            let wait = if !self.pending.is_empty() {
                Duration::from_millis(250)
            } else if self.running == 0 && !self.idle {
                QUIET.saturating_sub(self.last_work.elapsed()).max(Duration::from_millis(50))
            } else {
                Duration::from_secs(60)
            };
            let first = match receive.recv_timeout(wait) {
                Ok(msg) => Some(msg),
                Err(mpsc::RecvTimeoutError::Timeout) => None,
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    log::info!("pipeline engine stopped (channel closed)");
                    return;
                }
            };
            for msg in first.into_iter().chain(receive.try_iter()) {
                match msg {
                    Msg::Command(Command::Shutdown) => {
                        log::info!("pipeline engine stopped");
                        return;
                    }
                    Msg::Command(command) => self.command(command),
                    Msg::Fs(Ok(event)) => self.watched(event),
                    Msg::Fs(Err(e)) => self.emit(Event::Warning(format!("file watcher: {e}"))),
                    Msg::Done(outcome) => self.finished(outcome),
                }
            }
            self.settle();
            if self.running == 0 && self.queues.values().all(VecDeque::is_empty) && self.last_work.elapsed() >= QUIET {
                self.library_stages();
            }
        }
    }

    fn stage_query(&self, stage: Stage) -> StageQuery<'static> {
        match stage {
            Stage::Orient => orient_query(),
            Stage::Faces => faces_query(self.workers.availability),
            Stage::Signature => signature_query(self.workers.availability),
            Stage::Describe => describe_query(self.workers.availability),
        }
    }

    /// Something changed: every stage may have new work.
    fn changed(&mut self) {
        self.exhausted.clear();
    }

    /// Keeps up to two jobs per active worker in flight, orient before faces.
    fn dispatch(&mut self) {
        let capacity = match self.mode {
            Mode::Paused => 0,
            Mode::Background => 2,
            Mode::Normal => self.workers.threads * 2,
        };
        let availability = self.workers.availability;
        let stages = [
            (Stage::Orient, availability.orient),
            (Stage::Faces, availability.faces),
            (Stage::Signature, true),
            (Stage::Describe, self.workers.describe),
        ];
        for (stage, available) in stages {
            if !available {
                continue;
            }
            while self.running < capacity {
                if self.queues.get(&stage).is_none_or(VecDeque::is_empty) && !self.refill(stage) {
                    break;
                }
                let due = self.queues.get_mut(&stage).and_then(VecDeque::pop_front).expect("refilled");
                if self.workers.jobs.send(Job { stage, due }).is_err() {
                    return;
                }
                self.running += 1;
                if self.idle {
                    self.idle = false;
                    self.emit(Event::Busy);
                }
            }
        }
    }

    /// Fetches the next due assets for `stage`; false if there are none new.
    fn refill(&mut self, stage: Stage) -> bool {
        if self.exhausted.contains(&stage) {
            return false;
        }
        let query = self.stage_query(stage);
        let due = match self.catalog.due(&query, BATCH + self.known.len()) {
            Ok(due) => due,
            Err(e) => {
                self.emit(Event::Warning(format!("{}: {e}", stage.name())));
                Vec::new()
            }
        };
        let queue = self.queues.entry(stage).or_default();
        for due in due {
            if self.known.insert((due.asset, stage)) {
                queue.push_back(due);
            }
        }
        if queue.is_empty() {
            self.exhausted.insert(stage);
            return false;
        }
        true
    }

    /// Whether `due` still describes the file: it may have changed while the job ran.
    fn still_current(&self, due: &Due) -> bool {
        let catalog = self.catalog.current_input(due.asset).ok().flatten();
        let disk = runs::stat(&due.path).ok().map(|(size, mtime)| runs::fingerprint(size, mtime));
        catalog.as_deref() == Some(due.input.as_str()) && disk.as_deref() == Some(due.input.as_str())
    }

    fn finished(&mut self, outcome: Outcome) {
        self.running -= 1;
        self.last_work = Instant::now();
        // Faces may now be due after orient; either may have new work after a failure.
        self.changed();
        match outcome {
            Outcome::Orient(due, result) => {
                self.known.remove(&(due.asset, Stage::Orient));
                match &result {
                    Ok((p, temp)) => log::debug!("orient {}: {}° ({:.0}%){}", due.path.display(), p.cw, p.confidence * 100.0, if temp.is_some() { ", rewritten" } else { "" }),
                    Err(e) => log::debug!("orient {}: error {e}", due.path.display()),
                }
                if !self.still_current(&due) {
                    log::debug!("orient {}: file changed meanwhile, result dropped", due.path.display());
                    if let Ok((_, Some(temp))) = result {
                        let _ = std::fs::remove_file(temp);
                    }
                    return;
                }
                if let Err(e) = self.oriented(&due, result) {
                    self.fail(&due, Stage::Orient, e);
                }
            }
            Outcome::Faces(due, result) => {
                self.known.remove(&(due.asset, Stage::Faces));
                match &result {
                    Ok(found) => log::debug!("faces {}: {} found", due.path.display(), found.len()),
                    Err(e) => log::debug!("faces {}: error {e}", due.path.display()),
                }
                if !self.still_current(&due) {
                    log::debug!("faces {}: file changed meanwhile, result dropped", due.path.display());
                    return;
                }
                let stored = result.and_then(|found| {
                    self.catalog.set_faces(due.asset, catalog_faces::MODEL, &found).map_err(|e| e.to_string())?;
                    Ok(found.len())
                });
                match stored {
                    Ok(n) => {
                        let run = Run {
                            stage: FACES,
                            version: catalog_faces::MODEL,
                            input: &due.input,
                            status: RunStatus::Done,
                            value: Some(n as i64),
                            detail: "",
                        };
                        self.record(due.asset, &run);
                        if n > 0 {
                            let _ = self.catalog.bump_pipeline_value(FACES_SINCE_IDENTIFY, n as i64, false);
                            self.emit(Event::Updated(due.path));
                        }
                    }
                    Err(e) => self.fail(&due, Stage::Faces, e),
                }
            }
            Outcome::Signature(due, result) => {
                self.known.remove(&(due.asset, Stage::Signature));
                if let Err(e) = &result {
                    log::debug!("signature {}: error {e}", due.path.display());
                } else {
                    log::trace!("signature {}: done", due.path.display());
                }
                if !self.still_current(&due) {
                    log::debug!("signature {}: file changed meanwhile, result dropped", due.path.display());
                    return;
                }
                let stored = result.and_then(|signature| self.catalog.set_signature(due.asset, &signature).map_err(|e| e.to_string()));
                match stored {
                    Ok(()) => {
                        let run = Run {
                            stage: SIGNATURE,
                            version: SIGNATURE_VERSION,
                            input: &due.input,
                            status: RunStatus::Done,
                            value: None,
                            detail: "",
                        };
                        self.record(due.asset, &run);
                    }
                    Err(e) => self.fail(&due, Stage::Signature, e),
                }
            }
            Outcome::Describe(due, result) => {
                self.known.remove(&(due.asset, Stage::Describe));
                match &result {
                    Ok(d) => {
                        let tags: Vec<String> = d.tags.iter().map(|t| format!("{} {:.0}%", t.path, t.confidence * 100.0)).collect();
                        log::debug!("describe {}: [{}]", due.path.display(), tags.join(", "));
                    }
                    Err(e) => log::debug!("describe {}: error {e}", due.path.display()),
                }
                if !self.still_current(&due) {
                    log::debug!("describe {}: file changed meanwhile, result dropped", due.path.display());
                    return;
                }
                match result.and_then(|description| self.store_description(due.asset, &description)) {
                    Ok(n) => {
                        let run = Run {
                            stage: DESCRIBE,
                            version: describe::VERSION,
                            input: &due.input,
                            status: RunStatus::Done,
                            value: Some(n as i64),
                            detail: "",
                        };
                        self.record(due.asset, &run);
                        if n > 0 {
                            self.emit(Event::Updated(due.path));
                        }
                    }
                    Err(e) => self.fail(&due, Stage::Describe, e),
                }
            }
        }
    }

    /// Stores the embedding and replaces the stage's tags on the asset. Returns how many
    /// tags it proposed.
    fn store_description(&mut self, asset: AssetId, description: &describe::Description) -> Result<usize, String> {
        self.catalog.set_image_embedding(asset, clip::MODEL, &description.embedding).map_err(|e| e.to_string())?;
        let mut found = Vec::new();
        for tag in &description.tags {
            // A tag the user already has under that path with another kind keeps theirs.
            match self.catalog.tag(&tag.path, tag.kind) {
                Ok(id) => found.push(Detection { tag: id, confidence: tag.confidence, region: None }),
                Err(crate::catalog::Error::Invalid(e)) => log::debug!("describe: {}: {e}", tag.path),
                Err(e) => return Err(e.to_string()),
            }
        }
        self.catalog.set_detections(asset, describe::DETECTOR, &found).map_err(|e| e.to_string())?;
        Ok(found.len())
    }

    fn oriented(&mut self, due: &Due, result: Result<(Prediction, Option<PathBuf>), String>) -> Result<(), String> {
        let (prediction, temp) = result?;
        let (cw, percent) = (prediction.cw, prediction.confidence * 100.0);
        let mut run = Run {
            stage: ORIENT,
            version: ORIENT_VERSION,
            input: &due.input,
            status: RunStatus::Done,
            value: Some(cw as i64),
            detail: "",
        };
        if let Some(temp) = temp {
            let input = self.replace_rotated(due.asset, &due.path, &temp, cw)?;
            let detail = format!("rotated {cw}° ({percent:.0}%)");
            self.record(due.asset, &Run { input: &input, detail: &detail, ..run });
            self.emit(Event::Rotated { path: due.path.clone(), cw, by_user: false });
            return Ok(());
        }
        let detail = if cw == 0 {
            format!("upright ({percent:.0}%)")
        } else if !rotate::can_rotate(&due.path) {
            let ext = due.path.extension().map_or_else(String::new, |e| e.to_string_lossy().to_uppercase());
            format!("rotate {cw}° ({percent:.0}%); {ext} files can't be rotated losslessly yet")
        } else {
            format!("rotate {cw}°? ({percent:.0}% sure)")
        };
        if cw != 0 {
            run.status = RunStatus::Review;
            self.emit(Event::Review { path: due.path.clone(), stage: ORIENT, detail: detail.clone() });
        }
        self.record(due.asset, &Run { detail: &detail, ..run });
        Ok(())
    }

    /// Moves the rotated `temp` over `path` and updates the catalog to match. Returns the
    /// new fingerprint.
    fn replace_rotated(&mut self, asset: AssetId, path: &Path, temp: &Path, cw: u16) -> Result<String, String> {
        let input = replace_rotated(&self.catalog, asset, path, temp, cw)?;
        self.changed();
        Ok(input)
    }

    fn record(&self, asset: AssetId, run: &Run) {
        if let Err(e) = self.catalog.set_run(asset, run) {
            self.emit(Event::Warning(format!("{}: {e}", run.stage)));
        }
    }

    fn fail(&self, due: &Due, stage: Stage, error: String) {
        let version = match stage {
            Stage::Orient => ORIENT_VERSION,
            Stage::Faces => catalog_faces::MODEL,
            Stage::Signature => SIGNATURE_VERSION,
            Stage::Describe => describe::VERSION,
        };
        let run = Run { stage: stage.name(), version, input: &due.input, status: RunStatus::Failed, value: None, detail: &error };
        self.record(due.asset, &run);
        self.emit(Event::Failed { path: due.path.clone(), stage: stage.name(), error });
    }

    fn command(&mut self, command: Command) {
        match &command {
            Command::Add(path) => log::debug!("command: add {}", path.display()),
            Command::Rotate(path, cw, _) => log::debug!("command: rotate {} {cw}°", path.display()),
            Command::Accept(path, _) => log::debug!("command: accept {}", path.display()),
            Command::Keep(path, _) => log::debug!("command: keep {}", path.display()),
            Command::Retry(which, _) => log::info!("command: retry {which:?}"),
            Command::SetMode(mode) => log::debug!("command: mode {mode:?}"),
            Command::Shutdown => {}
        }
        match command {
            Command::Add(path) => {
                let since = Instant::now().checked_sub(SETTLE).unwrap_or_else(Instant::now);
                self.pending.insert(path, Pending { since, last: None, trusted: true });
            }
            Command::Rotate(path, cw, reply) => {
                let result = self.user_rotate(&path, cw);
                log::debug!("rotate {} {cw}°: {result:?}", path.display());
                let _ = reply.send(result);
            }
            Command::Accept(path, reply) => {
                let proposed = self.review_of(&path).and_then(|(_, cw)| cw.ok_or_else(|| "nothing to accept".to_owned()));
                let result = proposed.and_then(|cw| self.user_rotate(&path, cw));
                log::debug!("accept {}: {result:?}", path.display());
                let _ = reply.send(result);
            }
            Command::Keep(path, reply) => {
                let result = self.review_of(&path).and_then(|(asset, _)| {
                    let input = self.catalog.current_input(asset).map_err(|e| e.to_string())?.ok_or("file not seen yet")?;
                    let run = Run {
                        stage: ORIENT,
                        version: ORIENT_VERSION,
                        input: &input,
                        status: RunStatus::User,
                        value: Some(0),
                        detail: "kept as is",
                    };
                    self.catalog.set_run(asset, &run).map_err(|e| e.to_string())
                });
                log::debug!("keep {}: {result:?}", path.display());
                let _ = reply.send(result);
            }
            Command::Retry(which, reply) => {
                let result = self.catalog.retry(&which).map_err(|e| e.to_string());
                if let (Err(e), None) = (&result, &reply) {
                    self.emit(Event::Warning(format!("retry: {e}")));
                }
                if which == Retry::All {
                    self.sync_failed.clear();
                }
                self.changed();
                if let Some(reply) = reply {
                    let _ = reply.send(result);
                }
            }
            Command::SetMode(mode) => {
                self.mode = mode;
                let active = match mode {
                    Mode::Normal => self.workers.threads,
                    Mode::Background => 1,
                    Mode::Paused => 0,
                };
                self.workers.active.store(active, Ordering::Relaxed);
            }
            Command::Shutdown => {}
        }
    }

    /// The asset at `path` and the rotation orient proposed for it, if it is in review.
    fn review_of(&self, path: &Path) -> Result<(AssetId, Option<u16>), String> {
        let asset = self.catalog.find_asset(path).map_err(|e| e.to_string())?.ok_or("not in the library")?;
        match self.catalog.run_of(asset, ORIENT).map_err(|e| e.to_string())? {
            Some((RunStatus::Review, value, _)) => Ok((asset, value.map(|v| v as u16))),
            _ => Err("no rotation to review for this file".into()),
        }
    }

    fn user_rotate(&mut self, path: &Path, cw: u16) -> Result<(), String> {
        rotate_file(&self.catalog, path, cw)?;
        self.changed();
        self.emit(Event::Rotated { path: path.to_owned(), cw: cw % 360, by_user: true });
        Ok(())
    }

    fn watched(&mut self, event: notify::Event) {
        if event.kind.is_access() {
            return;
        }
        for path in event.paths {
            if ignored(self.catalog.root(), &path) {
                continue;
            }
            // Files copied in with a new directory may predate the watch on it.
            let files = if path.is_dir() { walk(&path) } else { vec![path] };
            for file in files {
                self.watched_file(file);
            }
        }
    }

    /// Waits for `path` to settle, unless [`Engine::add`] already vouched for it: the
    /// watcher's report of that same write mustn't restart the wait.
    fn watched_file(&mut self, path: PathBuf) {
        let now = Instant::now();
        match self.pending.get_mut(&path) {
            Some(entry) if entry.trusted => {}
            Some(entry) => (entry.since, entry.last) = (now, None),
            None => {
                self.pending.insert(path, Pending { since: now, last: None, trusted: false });
            }
        }
    }

    /// Takes in the watched paths that stopped changing.
    fn settle(&mut self) {
        let now = Instant::now();
        let ready: Vec<PathBuf> =
            self.pending.iter().filter(|(_, p)| now.duration_since(p.since) >= SETTLE).map(|(path, _)| path.clone()).collect();
        for path in ready {
            let Some(kind) = classify(&path) else {
                self.pending.remove(&path);
                continue;
            };
            match runs::stat(&path) {
                Err(_) => {
                    self.pending.remove(&path);
                    if let Ok(Some(state)) = self.catalog.file_state(&path) {
                        if !state.missing {
                            let _ = self.catalog.set_missing(state.asset, true);
                            self.emit(Event::Updated(path));
                        }
                    }
                }
                Ok(seen) => {
                    let entry = self.pending.get_mut(&path).expect("listed");
                    if !entry.trusted && entry.last != Some(seen) {
                        (entry.last, entry.since) = (Some(seen), now);
                        continue;
                    }
                    self.pending.remove(&path);
                    match self.catalog.record_file(&path, kind, seen.0, seen.1) {
                        Ok((_, true)) => {
                            self.changed();
                            self.emit(Event::Updated(path));
                        }
                        Ok((_, false)) => {}
                        Err(e) => self.emit(Event::Warning(format!("{}: {e}", path.display()))),
                    }
                }
            }
        }
    }

    /// Identify and sidecar sync, once the per-file stages have been quiet for a while.
    fn library_stages(&mut self) {
        if self.workers.availability.faces && self.catalog.pipeline_value(FACES_SINCE_IDENTIFY).unwrap_or(0) > 0 {
            let started = Instant::now();
            let identified = (|| {
                let suggested = self.catalog.refresh_suggestions(DEFAULT_MIN_SIMILARITY)?;
                let params = faces::cluster::Params { min_faces: 2, ..Default::default() };
                let clusters = self.catalog.cluster_faces(&params)?;
                self.catalog.bump_pipeline_value(FACES_SINCE_IDENTIFY, 0, true)?;
                Ok::<_, crate::catalog::Error>((suggested, clusters))
            })();
            match identified {
                Ok((suggested, clusters)) => {
                    log::debug!("identify took {:?}", started.elapsed());
                    self.emit(Event::Identified { suggested, clusters })
                }
                Err(e) => self.emit(Event::Warning(format!("identify: {e}"))),
            }
        }

        let needs_sync = self.catalog.needing_sync().unwrap_or_default();
        if needs_sync.iter().any(|asset| !self.sync_failed.contains(asset)) {
            match self.catalog.sync_sidecars(DEFAULT_MIN_CONFIDENCE) {
                Ok(report) => {
                    for (path, error) in report.failed {
                        if let Ok(Some(asset)) = self.catalog.find_asset(&path) {
                            self.sync_failed.insert(asset);
                        }
                        self.emit(Event::Failed { path, stage: "sync", error });
                    }
                    if report.written > 0 {
                        self.emit(Event::Synced { written: report.written });
                    }
                }
                Err(e) => self.emit(Event::Warning(format!("sync: {e}"))),
            }
        }

        if !self.idle && self.pending.is_empty() {
            self.idle = true;
            self.emit(Event::Idle);
        }
    }
}

/// The engine's reports, as they go to the host: summaries at info, stage failures and
/// warnings at warn, per-file news at debug (`Updated` at trace: thousands in a catch-up).
fn log_event(event: &Event) {
    match event {
        Event::CatchUp { added, changed, missing } if added + changed + missing > 0 => {
            log::info!("catch-up: {added} new, {changed} changed, {missing} missing")
        }
        Event::CatchUp { .. } => log::info!("catch-up: library unchanged"),
        Event::Ready(availability) => log::info!("ready: {availability:?}"),
        Event::Rotated { path, cw, by_user } => {
            log::debug!("rotated {} {cw}° ({})", path.display(), if *by_user { "by the user" } else { "by orient" })
        }
        Event::Review { path, stage, detail } => log::debug!("{stage} wants review: {}: {detail}", path.display()),
        Event::Failed { path, stage, error } => log::warn!("{stage} failed: {}: {error}", path.display()),
        Event::Updated(path) => log::trace!("updated {}", path.display()),
        Event::Identified { suggested, clusters } => log::info!("identified: {suggested} faces suggested, {clusters} clusters"),
        Event::Synced { written } => log::info!("synced {written} sidecars"),
        Event::Warning(warning) => log::warn!("{warning}"),
        Event::Busy => log::debug!("busy"),
        Event::Idle => log::info!("idle: all caught up"),
    }
}

/// Hidden files and directories (the catalog, temporaries) and partial writes.
fn ignored(root: &Path, path: &Path) -> bool {
    let rel = path.strip_prefix(root).unwrap_or(path);
    rel.components().any(|c| c.as_os_str().to_string_lossy().starts_with('.'))
        || path.extension().is_some_and(|e| e == "part")
}

/// The files under `dir`, skipping hidden ones.
fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut dirs = vec![dir.to_owned()];
    while let Some(dir) = dirs.pop() {
        let Ok(read) = std::fs::read_dir(&dir) else { continue };
        for item in read.flatten() {
            if item.file_name().to_string_lossy().starts_with('.') {
                continue;
            }
            match item.file_type() {
                Ok(t) if t.is_dir() => dirs.push(item.path()),
                Ok(t) if t.is_file() => files.push(item.path()),
                _ => {}
            }
        }
    }
    files
}

/// Brings the catalog up to date with the files on disk: (added, changed, missing).
/// Also removes rotation temporaries left by an engine that was stopped mid-write.
pub fn catch_up(catalog: &Catalog) -> crate::catalog::Result<(usize, usize, usize)> {
    let root = catalog.root().to_owned();
    let mut states = catalog.file_states()?;
    let (mut added, mut changed) = (0, 0);
    let mut dirs = vec![root.clone()];
    while let Some(dir) = dirs.pop() {
        let Ok(read) = std::fs::read_dir(&dir) else { continue };
        for item in read.flatten() {
            let name = item.file_name().to_string_lossy().into_owned();
            let path = item.path();
            if name.starts_with('.') {
                if name.ends_with(".rotating") {
                    let _ = std::fs::remove_file(&path);
                }
                continue;
            }
            match item.file_type() {
                Ok(t) if t.is_dir() => {
                    dirs.push(path);
                    continue;
                }
                Ok(t) if t.is_file() => {}
                _ => continue,
            }
            let Some(kind) = classify(&path) else { continue };
            let Ok((size, mtime)) = runs::stat(&path) else { continue };
            let rel = path.strip_prefix(&root).ok().and_then(|r| r.to_str()).map(|r| r.replace('\\', "/"));
            let before = rel.and_then(|rel| states.remove(&rel));
            if before.is_some_and(|s| s.size == Some(size) && s.mtime == Some(mtime) && !s.missing) {
                continue;
            }
            match catalog.record_file(&path, kind, size, mtime) {
                Ok(_) if before.is_none() => added += 1,
                Ok(_) => changed += 1,
                Err(crate::catalog::Error::Path(_)) => {}
                Err(e) => return Err(e),
            }
        }
    }
    // What wasn't found on disk.
    let mut missing = 0;
    for state in states.into_values().filter(|s| !s.missing) {
        catalog.set_missing(state.asset, true)?;
        missing += 1;
    }
    Ok((added, changed, missing))
}

#[cfg(test)]
mod tests;
