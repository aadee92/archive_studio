//! Thumbnail pipeline for the review grid.
//!
//! ```text
//!  UI thread                     CPU workers (cores-1, ≤16)        I/O threads (2 HDD / 8 SSD)
//!  viewport ─schedule(wanted)─▶ lookup: compressed LRU (64 MB) ─miss─▶ read source bytes ─┐
//!   rows, best first             └▶ SQLite thumbs.sqlite              (EXIF preview ─────┐│
//!         ▲                     decode bytes ◀────────────────────────── ranked, bounded ┘│
//!         │                       └▶ writer thread: JPEG encode + batched SQLite insert   │
//!         └── ThumbCache::complete (256 MB LRU of slint::Image) ◀── via event loop ◀──────┘
//! ```
//!
//! - Priority comes from the viewport (see [`sched`]): visible tiles top to bottom, then a
//!   prefetch band ahead in the scroll direction, then a smaller one behind. Tiles that
//!   leave that window before a thread starts on them are cancelled.
//! - Cache hits never wait for the disk: lookups run on the CPU workers. Only misses go to
//!   the few I/O threads, so a spinning disk isn't thrashed by many parallel random reads.
//!   TIFFs are decoded while streaming on the I/O thread (a reduced page may be all that is
//!   read); everything else is read whole and decoded on a CPU worker.
//! - A JPEG's embedded EXIF thumbnail is delivered as soon as its first bytes are read, and
//!   every thumbnail's average color is kept (and persisted), so tiles never sit empty.
//! - Two size [`Tier`]s; a source-file decode produces and persists both (on the writer
//!   thread, after delivery), so zooming never re-decodes.
//! - Pixels cross threads as `SharedPixelBuffer`; the `slint::Image` (not `Send`)
//!   is created on the UI thread.

pub mod cache;
mod decode;
pub mod sched;
mod writer;

#[allow(unused_imports)] // used by capture backends
pub use decode::{decode_preview, fit, write_tiff_with_reduced_page};

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::Duration;

use image::{DynamicImage, RgbImage};
use slint::{Image, Rgb8Pixel, SharedPixelBuffer};

use crate::MediaKind;
use cache::{DiskCache, DiskConn, Fingerprint, Lru};
use sched::{Key, Sched};
use writer::{Produced, Writer};

pub type Pixels = SharedPixelBuffer<Rgb8Pixel>;
/// Compressed JPEG thumbnails and their packed average color, shared by all threads.
type Compressed = Arc<Mutex<Lru<i64, (Arc<[u8]>, u32)>>>;

/// Most tiles wanted at once (visible + prefetch).
const QUEUE_CAP: usize = 512;
/// Decoded RGB thumbnails held by the UI thread.
const DECODED_BUDGET: usize = 256 << 20;
/// EXIF previews held by the UI thread until the real thumbnail replaces them.
const PREVIEW_BUDGET: usize = 16 << 20;
/// Compressed JPEG thumbnails shared by the workers.
const COMPRESSED_BUDGET: usize = 64 << 20;
/// Decoded files waiting for the writer (~0.4 MB each) before writes are dropped.
const WRITE_QUEUE_CAP: usize = 64;
/// Read first, to find a JPEG's EXIF thumbnail (APP1 is at most 64 KB) before the rest.
const HEAD_BYTES: u64 = 128 << 10;
/// Larger files are streamed by the decoder rather than read into memory.
const MAX_READ_BYTES: u64 = 256 << 20;
/// How long app exit waits for queued thumbnail writes.
const EXIT_FLUSH: Duration = Duration::from_secs(2);

/// Thumbnail size class, by longest edge in physical pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Tier {
    Small,
    Large,
}

impl Tier {
    pub const fn edge(self) -> u32 {
        match self {
            Self::Small => 192,
            Self::Large => 384,
        }
    }

    /// The smallest tier covering a tile `px` physical pixels wide.
    pub fn for_tile(px: f32) -> Self {
        if px <= Self::Small.edge() as f32 { Self::Small } else { Self::Large }
    }

    fn other(self) -> Self {
        match self {
            Self::Small => Self::Large,
            Self::Large => Self::Small,
        }
    }
}

/// What a worker hands back for a requested `(id, tier)`.
pub enum Delivery {
    /// The thumbnail at the requested tier, and its average color.
    Thumb(Pixels, [u8; 3]),
    /// A low-resolution stand-in (EXIF thumbnail); the `Thumb` follows.
    Preview(Pixels, [u8; 3]),
    Failed,
}

/// Thread counts. Source reads are capped separately from decoding so a spinning disk
/// serves a few sequential-ish reads instead of many competing random ones.
#[derive(Clone, Copy, Debug)]
pub struct Threads {
    pub decode: usize,
    pub io: usize,
}

impl Threads {
    /// Decode: cores - 1 (2..=16). I/O: 2 if `root` is on a rotational disk, 8 on SSD/NVMe,
    /// 4 if unknown. `ARCHIVE_STUDIO_DECODE_THREADS` / `ARCHIVE_STUDIO_IO_THREADS` override.
    pub fn for_root(root: &Path) -> Self {
        let env = |name| std::env::var(name).ok().and_then(|v| v.parse::<usize>().ok()).filter(|&n| n > 0);
        let cores = thread::available_parallelism().map_or(4, |n| n.get());
        Self {
            decode: env("ARCHIVE_STUDIO_DECODE_THREADS").unwrap_or((cores.saturating_sub(1)).clamp(2, 16)),
            io: env("ARCHIVE_STUDIO_IO_THREADS").unwrap_or(match rotational(root) {
                Some(true) => 2,
                Some(false) => 8,
                None => 4,
            }),
        }
    }
}

/// Whether the block device holding `path` is rotational, from sysfs.
fn rotational(path: &Path) -> Option<bool> {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::fs::MetadataExt;
        let dev = std::fs::metadata(path).ok()?.dev();
        let major = ((dev >> 32) & 0xffff_f000) | ((dev >> 8) & 0x0fff);
        let minor = ((dev >> 12) & 0xffff_ff00) | (dev & 0xff);
        let base = format!("/sys/dev/block/{major}:{minor}");
        // A partition's queue settings live on its parent disk.
        [format!("{base}/queue/rotational"), format!("{base}/../queue/rotational")]
            .iter()
            .find_map(|p| std::fs::read_to_string(p).ok())
            .map(|v| v.trim() == "1")
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = path;
        None
    }
}

struct Job {
    id: u32,
    tier: Tier,
    path: PathBuf,
    kind: MediaKind,
}

/// A job moving through the pipeline stages.
struct Work {
    job: Job,
    fingerprint: Option<Fingerprint>,
    /// The source file, once an I/O thread has read it.
    bytes: Option<Vec<u8>>,
}

struct Pipeline {
    sched: Mutex<Sched<Work>>,
    /// CPU workers: lookups and decodes.
    cpu: Condvar,
    /// I/O threads: source reads.
    io: Condvar,
}

type OnReady = Arc<dyn Fn(u32, Tier, Delivery) + Send + Sync>;

pub struct ThumbCache {
    pipeline: Arc<Pipeline>,
    writer: Arc<Writer>,
    images: RefCell<Lru<(u32, Tier), Image>>,
    previews: RefCell<Lru<u32, Image>>,
    colors: RefCell<HashMap<u32, [u8; 3]>>,
    failed: RefCell<HashSet<u32>>,
    shut_down: Cell<bool>,
}

impl ThumbCache {
    /// `db` is the SQLite thumbnail store (`None`, or unusable: memory only).
    /// `on_ready` runs on a worker thread and must forward to the UI thread,
    /// where the result is handed to [`complete`](Self::complete).
    pub fn new(db: Option<&Path>, threads: Threads, on_ready: impl Fn(u32, Tier, Delivery) + Send + Sync + 'static) -> Self {
        let on_ready: OnReady = Arc::new(on_ready);
        let pipeline = Arc::new(Pipeline {
            sched: Mutex::new(Sched::new(QUEUE_CAP, threads.decode.max(4))),
            cpu: Condvar::new(),
            io: Condvar::new(),
        });
        let disk = db.and_then(DiskCache::open);
        let compressed: Compressed = Arc::new(Mutex::new(Lru::new(COMPRESSED_BUDGET)));
        let writer = Arc::new(Writer::spawn(disk.clone(), compressed.clone(), WRITE_QUEUE_CAP));
        let ctx = Ctx { pipeline: pipeline.clone(), writer: writer.clone(), on_ready };
        for i in 0..threads.decode.max(1) {
            let (ctx, loader) = (ctx.clone(), Loader::new(disk.as_ref(), compressed.clone()));
            let _ = thread::Builder::new().name(format!("thumb-cpu-{i}")).spawn(move || ctx.cpu_worker(loader));
        }
        for i in 0..threads.io.max(1) {
            let ctx = ctx.clone();
            let _ = thread::Builder::new().name(format!("thumb-io-{i}")).spawn(move || ctx.io_worker());
        }

        Self {
            pipeline,
            writer,
            images: RefCell::new(Lru::new(DECODED_BUDGET)),
            previews: RefCell::new(Lru::new(PREVIEW_BUDGET)),
            colors: RefCell::default(),
            failed: RefCell::default(),
            shut_down: Cell::new(false),
        }
    }

    /// The thumbnail at `tier`, else the other tier or the EXIF preview as a stand-in
    /// (`exact == false`) so zooming and cold loads don't show empty tiles.
    pub fn get(&self, id: u32, tier: Tier) -> Option<(Image, bool)> {
        let mut images = self.images.borrow_mut();
        if let Some(image) = images.get(&(id, tier)) {
            return Some((image, true));
        }
        images.get(&(id, tier.other())).or_else(|| self.previews.borrow_mut().get(&id)).map(|image| (image, false))
    }

    /// The average color of `id`'s thumbnail, once any version of it has been seen.
    pub fn color(&self, id: u32) -> Option<[u8; 3]> {
        self.colors.borrow().get(&id).copied()
    }

    /// Makes `wanted` (best first) the set of thumbnails to load: those not cached yet are
    /// queued in that order, and queued work for anything else is cancelled.
    /// `source` gives the file behind an id.
    pub fn schedule(&self, wanted: impl IntoIterator<Item = Key>, mut source: impl FnMut(u32) -> Option<(PathBuf, MediaKind)>) {
        let wanted: Vec<Key> = {
            let (mut images, failed) = (self.images.borrow_mut(), self.failed.borrow());
            wanted.into_iter().filter(|&(id, tier)| !failed.contains(&id) && !images.touch(&(id, tier))).collect()
        };
        self.pipeline.sched.lock().unwrap().schedule(wanted, |(id, tier)| {
            let (path, kind) = source(id)?;
            Some(Work { job: Job { id, tier, path, kind }, fingerprint: None, bytes: None })
        });
        self.pipeline.cpu.notify_all();
        self.pipeline.io.notify_all();
    }

    /// Takes a worker's result; returns whether the tile's appearance changed.
    pub fn complete(&self, id: u32, tier: Tier, delivery: Delivery) -> bool {
        match delivery {
            Delivery::Thumb(pixels, color) => {
                self.pipeline.sched.lock().unwrap().finish((id, tier));
                self.colors.borrow_mut().insert(id, color);
                self.previews.borrow_mut().remove(&id);
                let bytes = pixels.width() as usize * pixels.height() as usize * 3;
                self.images.borrow_mut().insert((id, tier), Image::from_rgb8(pixels), bytes);
                true
            }
            Delivery::Preview(pixels, color) => {
                self.colors.borrow_mut().entry(id).or_insert(color);
                let mut images = self.images.borrow_mut();
                if images.touch(&(id, Tier::Small)) || images.touch(&(id, Tier::Large)) {
                    return false;
                }
                let bytes = pixels.width() as usize * pixels.height() as usize * 3;
                self.previews.borrow_mut().insert(id, Image::from_rgb8(pixels), bytes);
                true
            }
            Delivery::Failed => {
                self.pipeline.sched.lock().unwrap().finish((id, tier));
                self.failed.borrow_mut().insert(id);
                false
            }
        }
    }

    /// Stops the workers and gives the writer a moment to store what it has queued.
    /// Called at app exit; also on drop.
    pub fn shutdown(&self) {
        if self.shut_down.replace(true) {
            return;
        }
        self.pipeline.sched.lock().unwrap().closed = true;
        self.pipeline.cpu.notify_all();
        self.pipeline.io.notify_all();
        self.writer.close(EXIT_FLUSH);
    }
}

impl Drop for ThumbCache {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// What every pipeline thread shares.
#[derive(Clone)]
struct Ctx {
    pipeline: Arc<Pipeline>,
    writer: Arc<Writer>,
    on_ready: OnReady,
}

impl Ctx {
    /// Blocks until `pick` yields a job; `None` once the pipeline is shut down.
    fn next(&self, cv: &Condvar, pick: impl Fn(&mut Sched<Work>) -> Option<(Key, Work)>) -> Option<(Key, Work)> {
        let mut sched = self.pipeline.sched.lock().unwrap();
        loop {
            if sched.closed {
                return None;
            }
            if let Some(next) = pick(&mut sched) {
                return Some(next);
            }
            sched = cv.wait(sched).unwrap();
        }
    }

    fn deliver(&self, job: &Job, image: &RgbImage, color: [u8; 3]) {
        (self.on_ready)(job.id, job.tier, Delivery::Thumb(pixels(image), color));
    }

    /// Delivers the requested tier of a fresh source decode, then hands both tiers to the writer.
    fn produce(&self, job: &Job, fingerprint: Option<Fingerprint>, large: Option<RgbImage>) {
        let Some(large) = large else {
            (self.on_ready)(job.id, job.tier, Delivery::Failed);
            return;
        };
        let small = decode::fit(&large, Tier::Small.edge());
        let color = decode::average_color(&small);
        self.deliver(job, if job.tier == Tier::Large { &large } else { &small }, color);
        if let Some(fingerprint) = fingerprint {
            self.writer.submit(Produced { fingerprint, large, small, color });
        }
    }

    /// Cache lookups, and decodes of source bytes the I/O threads have read.
    fn cpu_worker(self, mut loader: Loader) {
        while let Some((key, mut work)) = self.next(&self.pipeline.cpu, Sched::next_cpu) {
            let job = &work.job;
            if let Some(bytes) = work.bytes.take() {
                self.pipeline.io.notify_one(); // room for more read-ahead
                self.produce(job, work.fingerprint, decode::decode_bytes(&bytes, job.kind, Tier::Large.edge()));
                continue;
            }
            if job.kind != MediaKind::Image {
                // TODO: video frames / audio cover art.
                (self.on_ready)(job.id, job.tier, Delivery::Failed);
                continue;
            }
            work.fingerprint = Fingerprint::of(&job.path);
            let hit = work.fingerprint.and_then(|f| loader.cached(f, job.tier).or_else(|| self.writer.lookup(f, job.tier)));
            if let Some((image, color)) = hit {
                self.deliver(job, &image, color);
            } else if self.pipeline.sched.lock().unwrap().miss(key, work) {
                self.pipeline.io.notify_one();
            }
        }
    }

    /// Source reads, at most `Threads::io` at a time.
    fn io_worker(self) {
        while let Some((key, mut work)) = self.next(&self.pipeline.io, Sched::next_read) {
            match self.read_source(&work.job) {
                Ok(bytes) => {
                    work.bytes = Some(bytes);
                    if self.pipeline.sched.lock().unwrap().read_done(key, work) {
                        self.pipeline.cpu.notify_one();
                    }
                }
                Err(streamed) => self.produce(&work.job, work.fingerprint, streamed),
            }
        }
    }

    /// The whole file, after delivering its EXIF preview from the first bytes. TIFFs and
    /// huge files are instead decoded here while streaming (`Err`), as are unreadable ones.
    fn read_source(&self, job: &Job) -> Result<Vec<u8>, Option<RgbImage>> {
        let mut file = File::open(&job.path).map_err(|_| None)?;
        let len = file.metadata().map_or(0, |m| m.len());
        let stream = || decode::decode(&job.path, job.kind, Tier::Large.edge());
        if len > MAX_READ_BYTES {
            return Err(stream());
        }
        let mut bytes = Vec::with_capacity(len as usize + 1);
        (&mut file).take(HEAD_BYTES).read_to_end(&mut bytes).map_err(|_| None)?;
        if decode::is_tiff(&bytes) {
            return Err(stream());
        }
        if let Some(preview) = decode::preview_from_bytes(&bytes) {
            let color = decode::average_color(&preview);
            (self.on_ready)(job.id, job.tier, Delivery::Preview(pixels(&preview), color));
        }
        file.read_to_end(&mut bytes).map_err(|_| None)?;
        Ok(bytes)
    }
}

fn pixels(image: &RgbImage) -> Pixels {
    Pixels::clone_from_slice(image.as_raw(), image.width(), image.height())
}

/// Lets an acquisition backend seed the thumbnail cache for a scan it just saved,
/// from the full image it already has in memory (upright), so the review grid never
/// decodes the file. Call after `path` is fully written and closed: the cache key
/// includes its size and mtime. Returns whether the thumbnails were stored.
#[allow(dead_code)] // called by real capture backends
pub fn store_for_new_file(path: &Path, image: &DynamicImage) -> bool {
    let disk = cache::default_db_path().and_then(|db| DiskCache::open(&db));
    disk.is_some_and(|disk| store_in(&disk, path, image))
}

/// A small-tier thumbnail of the image at `path`, for a capture backend's strip of recent
/// captures: from the disk cache when it's there, else decoded (and not stored).
pub fn small_for_file(path: &Path) -> Option<RgbImage> {
    let disk = cache::default_db_path().and_then(|db| DiskCache::open(&db));
    if let (Some(fingerprint), Some(disk)) = (Fingerprint::of(path), &disk) {
        let mut loader = Loader::new(Some(disk), Arc::new(Mutex::new(Lru::new(1 << 20))));
        if let Some((image, _)) = loader.cached(fingerprint, Tier::Small) {
            return Some(image);
        }
    }
    decode::decode(path, MediaKind::Image, Tier::Small.edge())
}

fn store_in(disk: &DiskCache, path: &Path, image: &DynamicImage) -> bool {
    let (Some(fingerprint), Some(mut conn)) = (Fingerprint::of(path), disk.connect()) else {
        return false;
    };
    let large = decode::fit(&image.to_rgb8(), Tier::Large.edge());
    let small = decode::fit(&large, Tier::Small.edge());
    let color = decode::average_color(&small);
    let scratch: Compressed = Arc::new(Mutex::new(Lru::new(1 << 20)));
    writer::store(Some(&mut conn), &scratch, &[Produced { fingerprint, large, small, color }])
}

/// Where loads were served from; read by tests and the benchmark.
#[derive(Clone, Copy, Debug, Default)]
#[cfg_attr(not(test), allow(dead_code))]
struct LoadStats {
    memory_hits: u32,
    disk_hits: u32,
    decoded: u32,
}

/// One worker's view of the cache tiers below the UI: compressed LRU → disk.
struct Loader {
    disk: Option<DiskConn>,
    compressed: Compressed,
    /// Decoded by [`load`](Self::load), stored by [`persist`](Self::persist) (tests).
    #[cfg(test)]
    unsaved: Vec<Produced>,
    stats: LoadStats,
}

impl Loader {
    fn new(disk: Option<&DiskCache>, compressed: Compressed) -> Self {
        Self {
            disk: disk.and_then(DiskCache::connect),
            compressed,
            #[cfg(test)]
            unsaved: Vec::new(),
            stats: LoadStats::default(),
        }
    }

    /// The cached thumbnail and its color (computed if the row predates colors).
    fn cached(&mut self, fingerprint: Fingerprint, tier: Tier) -> Option<(RgbImage, [u8; 3])> {
        let key = fingerprint.key(tier);
        let in_memory = self.compressed.lock().unwrap().get(&key);
        if let Some((bytes, color)) = in_memory {
            self.stats.memory_hits += 1;
            return Some((cache::decode(&bytes)?, writer::unpack(color)));
        }
        let (bytes, color) = self.disk.as_ref()?.get(key)?;
        let image = cache::decode(&bytes)?;
        let color = color.map_or_else(|| decode::average_color(&image), writer::unpack);
        let bytes: Arc<[u8]> = bytes.into();
        let len = bytes.len();
        self.compressed.lock().unwrap().insert(key, (bytes, writer::pack(color)), len);
        self.stats.disk_hits += 1;
        Some((image, color))
    }

    /// The pipeline in one call, for tests: cache lookup, else a synchronous decode whose
    /// tiers [`persist`](Self::persist) stores.
    #[cfg(test)]
    fn load(&mut self, path: &Path, kind: MediaKind, tier: Tier) -> Option<RgbImage> {
        let fingerprint = Fingerprint::of(path);
        if let Some((image, _)) = fingerprint.and_then(|f| self.cached(f, tier)) {
            return Some(image);
        }
        let large = decode::decode(path, kind, Tier::Large.edge())?;
        let small = decode::fit(&large, Tier::Small.edge());
        self.stats.decoded += 1;
        let out = if tier == Tier::Large { large.clone() } else { small.clone() };
        if let Some(fingerprint) = fingerprint {
            let color = decode::average_color(&small);
            self.unsaved.push(Produced { fingerprint, large, small, color });
        }
        Some(out)
    }

    #[cfg(test)]
    fn persist(&mut self) {
        let items = std::mem::take(&mut self.unsaved);
        writer::store(self.disk.as_mut(), &self.compressed, &items);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant, SystemTime};

    fn loader(db: Option<&Path>) -> Loader {
        let disk = db.and_then(DiskCache::open);
        Loader::new(disk.as_ref(), Arc::new(Mutex::new(Lru::new(COMPRESSED_BUDGET))))
    }

    fn write_image(path: &Path, w: u32, h: u32) {
        image::RgbImage::from_pixel(w, h, image::Rgb([200, 100, 50])).save(path).unwrap();
    }

    fn longest(image: &RgbImage) -> u32 {
        image.width().max(image.height())
    }

    #[test]
    fn decodes_and_downscales_images() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["wide.jpg", "tall.png", "scan.tif"] {
            let (w, h) = if name.starts_with("tall") { (500, 1000) } else { (1000, 600) };
            let path = dir.path().join(name);
            write_image(&path, w, h);

            let thumb = decode::decode(&path, MediaKind::Image, Tier::Large.edge()).expect(name);
            assert_eq!(longest(&thumb), Tier::Large.edge(), "{name}");
            let expected_height = thumb.width() as f32 * h as f32 / w as f32;
            assert!((thumb.height() as f32 - expected_height).abs() <= 1.0, "{name} keeps aspect ratio");
        }
        assert!(decode::decode(&dir.path().join("missing.jpg"), MediaKind::Image, 384).is_none());
    }

    #[test]
    fn round_trips_through_disk_cache() {
        let dir = tempfile::tempdir().unwrap();
        let (db, src) = (dir.path().join("cache/thumbs.sqlite"), dir.path().join("a.png"));
        write_image(&src, 1000, 600);

        let mut first = loader(Some(&db));
        let decoded = first.load(&src, MediaKind::Image, Tier::Large).unwrap();
        first.persist();
        assert_eq!(first.stats.decoded, 1);
        assert_eq!(first.disk.as_ref().unwrap().len(), 2, "both tiers stored");

        let mut second = loader(Some(&db));
        let cached = second.load(&src, MediaKind::Image, Tier::Large).unwrap();
        assert_eq!(cached.dimensions(), decoded.dimensions());
        assert_eq!((second.stats.disk_hits, second.stats.decoded), (1, 0));

        second.load(&src, MediaKind::Image, Tier::Large).unwrap();
        assert_eq!(second.stats.memory_hits, 1, "second hit served from the compressed LRU");
    }

    #[test]
    fn modified_file_is_not_served_stale() {
        let dir = tempfile::tempdir().unwrap();
        let (db, src) = (dir.path().join("thumbs.sqlite"), dir.path().join("a.png"));
        write_image(&src, 1000, 600);
        let mut l = loader(Some(&db));
        l.load(&src, MediaKind::Image, Tier::Large).unwrap();
        l.persist();
        let before = Fingerprint::of(&src).unwrap().key(Tier::Large);

        write_image(&src, 600, 1000);
        let file = std::fs::File::options().write(true).open(&src).unwrap();
        file.set_modified(SystemTime::now() + Duration::from_secs(5)).unwrap();
        assert_ne!(Fingerprint::of(&src).unwrap().key(Tier::Large), before);

        let mut fresh = loader(Some(&db));
        let thumb = fresh.load(&src, MediaKind::Image, Tier::Large).unwrap();
        assert!(thumb.height() > thumb.width(), "new contents, not the cached landscape thumbnail");
        assert_eq!(fresh.stats.decoded, 1);
    }

    #[test]
    fn tiers_are_stored_separately() {
        let dir = tempfile::tempdir().unwrap();
        let (db, src) = (dir.path().join("thumbs.sqlite"), dir.path().join("a.jpg"));
        write_image(&src, 1200, 800);
        let f = Fingerprint::of(&src).unwrap();
        assert_ne!(f.key(Tier::Small), f.key(Tier::Large));

        let mut l = loader(Some(&db));
        assert_eq!(longest(&l.load(&src, MediaKind::Image, Tier::Small).unwrap()), 192);
        l.persist();
        let mut fresh = loader(Some(&db));
        assert_eq!(longest(&fresh.load(&src, MediaKind::Image, Tier::Large).unwrap()), 384);
        assert_eq!(longest(&fresh.load(&src, MediaKind::Image, Tier::Small).unwrap()), 192);
        assert_eq!((fresh.stats.disk_hits, fresh.stats.decoded), (2, 0));

        assert_eq!(Tier::for_tile(160.0), Tier::Small);
        assert_eq!(Tier::for_tile(160.0 * 2.0), Tier::Large);
    }

    #[test]
    fn lru_evicts_by_bytes() {
        let mut lru = Lru::new(1000);
        for i in 0..10 {
            lru.insert(i, i, 100);
        }
        assert_eq!(lru.bytes(), 1000);
        lru.get(&0);
        lru.insert(10, 10, 300);
        assert!(lru.bytes() <= 900);
        assert!(lru.contains(&0) && lru.contains(&10), "recently used and newest survive");
        assert!(!lru.contains(&1) && !lru.contains(&2) && !lru.contains(&3) && !lru.contains(&4));
        assert!(lru.contains(&9));

        lru.insert(11, 11, 5000);
        assert!(lru.contains(&11), "an oversized entry is still kept until the next insert");
    }

    #[test]
    fn corrupt_database_falls_back_to_no_cache() {
        let dir = tempfile::tempdir().unwrap();
        let (db, src) = (dir.path().join("thumbs.sqlite"), dir.path().join("a.png"));
        std::fs::write(&db, vec![0x5a; 64 * 1024]).unwrap();
        write_image(&src, 800, 600);

        assert!(DiskCache::open(&db).is_none());
        assert!(DiskCache::open(&dir.path().join("a.png/not-a-dir/x.sqlite")).is_none());
        let mut l = loader(Some(&db));
        assert!(l.disk.is_none());
        assert_eq!(longest(&l.load(&src, MediaKind::Image, Tier::Large).unwrap()), 384);
        l.persist();
        let _ = ThumbCache::new(Some(&db), Threads { decode: 1, io: 1 }, |_, _, _| {});
    }

    #[test]
    fn capture_hook_seeds_the_cache() {
        let dir = tempfile::tempdir().unwrap();
        let (db, src) = (dir.path().join("thumbs.sqlite"), dir.path().join("scan.png"));
        let scan = DynamicImage::ImageRgb8(image::RgbImage::from_pixel(2400, 3600, image::Rgb([10, 20, 30])));
        scan.save(&src).unwrap();
        assert!(store_in(&DiskCache::open(&db).unwrap(), &src, &scan));

        let mut l = loader(Some(&db));
        assert_eq!(l.load(&src, MediaKind::Image, Tier::Large).unwrap().dimensions(), (256, 384));
        assert_eq!(l.load(&src, MediaKind::Image, Tier::Small).unwrap().dimensions(), (128, 192));
        assert_eq!((l.stats.disk_hits, l.stats.decoded), (2, 0));
    }

    /// `cargo test --release -- --ignored --nocapture bench_cold_vs_warm`
    #[test]
    #[ignore]
    fn bench_cold_vs_warm() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("test_images");
        let mut files = Vec::new();
        let mut dirs = vec![root];
        while let Some(dir) = dirs.pop() {
            for entry in std::fs::read_dir(dir).unwrap().flatten() {
                let path = entry.path();
                let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
                if path.is_dir() {
                    dirs.push(path);
                } else if matches!(ext.as_str(), "jpg" | "jpeg" | "png" | "tif" | "tiff") {
                    files.push(path);
                }
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("thumbs.sqlite");
        let run = |l: &mut Loader, tier: Tier, persist: bool| {
            let start = Instant::now();
            for path in &files {
                l.load(path, MediaKind::Image, tier);
                if persist {
                    l.persist();
                }
            }
            start.elapsed()
        };
        let report = |label: &str, t: Duration| {
            println!("{label:<34} {:>8.1} ms total  {:>7.3} ms/file", t.as_secs_f64() * 1e3, t.as_secs_f64() * 1e3 / files.len() as f64);
        };
        println!("{} images", files.len());

        let mut cold = loader(Some(&db));
        report("cold: decode + encode + store", run(&mut cold, Tier::Large, true));
        let mut nocache = loader(None);
        report("no cache: decode only", run(&mut nocache, Tier::Large, false));
        let mut warm = loader(Some(&db));
        report("warm: disk hit (384)", run(&mut warm, Tier::Large, false));
        report("hot: compressed-memory hit (384)", run(&mut warm, Tier::Large, false));
        let mut small = loader(Some(&db));
        report("warm: disk hit (192)", run(&mut small, Tier::Small, false));
        println!("{:?} / {:?}", cold.stats, warm.stats);
        println!("db size {} KB", std::fs::metadata(&db).unwrap().len() / 1024);
    }

    fn produced(src: &Path, rgb: [u8; 3]) -> Produced {
        let large = image::RgbImage::from_fn(384, 256, |x, y| image::Rgb([rgb[0], rgb[1].wrapping_add((x / 8) as u8), rgb[2].wrapping_add((y / 8) as u8)]));
        let small = decode::fit(&large, Tier::Small.edge());
        let color = decode::average_color(&small);
        Produced { fingerprint: Fingerprint::of(src).unwrap(), large, small, color }
    }

    #[test]
    fn writer_batches_writes_and_drops_under_pressure() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("thumbs.sqlite");
        let disk = DiskCache::open(&db).unwrap();
        let srcs: Vec<PathBuf> = (0..60)
            .map(|i| {
                let p = dir.path().join(format!("{i}.jpg"));
                std::fs::write(&p, [i as u8; 8]).unwrap();
                p
            })
            .collect();
        let compressed: Compressed = Arc::new(Mutex::new(Lru::new(COMPRESSED_BUDGET)));

        // Unhurried: every write lands, with its color, and the memory tier is filled too.
        let w = Writer::spawn(Some(disk.clone()), compressed.clone(), 64);
        for src in &srcs[..20] {
            assert!(w.submit(produced(src, [90, 120, 30])));
        }
        assert!(w.flush(Duration::from_secs(20)));
        assert_eq!(w.counts(), (20, 0));
        let conn = disk.connect().unwrap();
        assert_eq!(conn.len(), 40, "both tiers of each file");
        let (_, color) = conn.get(Fingerprint::of(&srcs[3]).unwrap().key(Tier::Small)).unwrap();
        assert_eq!(color.map(writer::unpack), Some(produced(&srcs[3], [90, 120, 30]).color));
        assert!(compressed.lock().unwrap().contains(&Fingerprint::of(&srcs[0]).unwrap().key(Tier::Large)));
        assert!(w.close(Duration::from_secs(5)));
        assert!(!w.submit(produced(&srcs[0], [0; 3])), "closed: dropped");

        // Flooded: submit never blocks, overflow is dropped, the rest still lands.
        let w = Writer::spawn(Some(disk.clone()), compressed, 4);
        let items: Vec<_> = srcs[20..].iter().map(|s| produced(s, [10, 10, 10])).collect();
        let start = Instant::now();
        for item in items {
            w.submit(item);
        }
        assert!(start.elapsed() < Duration::from_millis(50), "submits took {:?}", start.elapsed());
        assert!(w.close(Duration::from_secs(20)));
        let (written, dropped) = w.counts();
        assert!(dropped > 0 && written >= 4, "written {written}, dropped {dropped}");
        assert_eq!(written + dropped, 40);
        assert_eq!(conn.len(), 40 + 2 * written as usize);
    }

    #[test]
    fn colors_persist_and_old_databases_migrate() {
        let dir = tempfile::tempdir().unwrap();
        let (db, src) = (dir.path().join("thumbs.sqlite"), dir.path().join("a.png"));
        image::RgbImage::from_pixel(1000, 600, image::Rgb([200, 100, 50])).save(&src).unwrap();
        let f = Fingerprint::of(&src).unwrap();

        // A cache written before colors existed, holding this file's large tier.
        let old_thumb = image::RgbImage::from_pixel(384, 230, image::Rgb([10, 20, 200]));
        {
            let conn = rusqlite::Connection::open(&db).unwrap();
            conn.execute_batch("CREATE TABLE thumbs (key INTEGER PRIMARY KEY, jpeg BLOB NOT NULL);").unwrap();
            conn.execute("INSERT INTO thumbs VALUES (?1, ?2)", rusqlite::params![f.key(Tier::Large), cache::encode(&old_thumb).unwrap()])
                .unwrap();
        }
        let mut l = loader(Some(&db));
        let (image, color) = l.cached(f, Tier::Large).expect("old rows still served");
        assert_eq!(image.dimensions(), (384, 230));
        assert!(color.iter().zip([10, 20, 200]).all(|(&a, b)| a.abs_diff(b) <= 3), "computed for a NULL color: {color:?}");
        assert_eq!(l.disk.as_ref().unwrap().get(f.key(Tier::Large)).unwrap().1, None);

        // New entries store their color, and it is read back as stored.
        assert_eq!(l.load(&src, MediaKind::Image, Tier::Small).map(|i| i.dimensions()), Some((192, 115)));
        l.persist();
        let mut fresh = loader(Some(&db));
        let stored = fresh.disk.as_ref().unwrap().get(f.key(Tier::Small)).unwrap().1.map(writer::unpack);
        assert_eq!(stored, Some([200, 100, 50]));
        assert_eq!(fresh.cached(f, Tier::Small).unwrap().1, [200, 100, 50]);
        assert!(DiskCache::open(&db).is_some(), "reopening a migrated database");
    }

    /// A JPEG whose EXIF segment carries a 160×107 thumbnail, like a camera's.
    fn jpeg_with_exif_thumb(path: &Path, rgb: [u8; 3]) {
        let jpeg = |w, h| {
            let mut out = Vec::new();
            let img = image::RgbImage::from_pixel(w, h, image::Rgb(rgb));
            image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 90).encode_image(&img).unwrap();
            out
        };
        let thumb = jpeg(160, 107);
        let mut exif = b"Exif\0\0II*\0".to_vec();
        exif.extend(8u32.to_le_bytes());
        exif.extend(0u16.to_le_bytes()); // IFD0: no entries
        exif.extend(14u32.to_le_bytes()); // IFD1 at 14
        exif.extend(2u16.to_le_bytes());
        exif.extend([0x01, 0x02, 4, 0, 1, 0, 0, 0]);
        exif.extend(44u32.to_le_bytes());
        exif.extend([0x02, 0x02, 4, 0, 1, 0, 0, 0]);
        exif.extend((thumb.len() as u32).to_le_bytes());
        exif.extend(0u32.to_le_bytes());
        exif.extend(&thumb);
        let full = jpeg(1600, 1067);
        let mut out = full[..2].to_vec();
        out.extend([0xff, 0xe1]);
        out.extend(((exif.len() + 2) as u16).to_be_bytes());
        out.extend(exif);
        out.extend(&full[2..]);
        std::fs::write(path, out).unwrap();
    }

    #[test]
    fn pipeline_sends_preview_then_thumbnail_with_color() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("thumbs.sqlite");
        let files = [dir.path().join("cam.jpg"), dir.path().join("scan.png"), dir.path().join("song.mp3")];
        jpeg_with_exif_thumb(&files[0], [40, 160, 90]);
        image::RgbImage::from_pixel(900, 600, image::Rgb([220, 200, 30])).save(&files[1]).unwrap();
        std::fs::write(&files[2], b"ID3").unwrap();

        let run = |expect_preview: bool| {
            let (tx, rx) = std::sync::mpsc::channel();
            let cache = ThumbCache::new(Some(&db), Threads { decode: 2, io: 1 }, move |id, tier, d| {
                let _ = tx.send((id, tier, d));
            });
            let kinds = [MediaKind::Image, MediaKind::Image, MediaKind::Audio];
            cache.schedule((0..3).map(|id| (id, Tier::Large)), |id| Some((files[id as usize].clone(), kinds[id as usize])));
            let mut events = Vec::new();
            while events.iter().filter(|e: &&(u32, &str)| e.1 != "preview").count() < 3 {
                let (id, tier, d) = rx.recv_timeout(Duration::from_secs(10)).unwrap();
                events.push((id, match d {
                    Delivery::Thumb(..) => "thumb",
                    Delivery::Preview(..) => "preview",
                    Delivery::Failed => "failed",
                }));
                cache.complete(id, tier, d);
            }
            let previews: Vec<_> = events.iter().filter(|e| e.1 == "preview").map(|e| e.0).collect();
            assert_eq!(previews, if expect_preview { vec![0] } else { vec![] }, "{events:?}");
            let position = |e| events.iter().position(|x| *x == e).unwrap();
            if expect_preview {
                assert!(position((0, "preview")) < position((0, "thumb")));
            }
            assert!(events.contains(&(1, "thumb")) && events.contains(&(2, "failed")));
            assert!(cache.get(0, Tier::Large).is_some_and(|(_, exact)| exact));
            let near = |c: [u8; 3], e: [u8; 3]| c.iter().zip(e).all(|(&a, b)| a.abs_diff(b) <= 4);
            assert!(near(cache.color(0).unwrap(), [40, 160, 90]) && near(cache.color(1).unwrap(), [220, 200, 30]));
            assert!(cache.color(2).is_none() && cache.get(2, Tier::Large).is_none());
            cache.schedule((0..3).map(|id| (id, Tier::Large)), |_| panic!("nothing left to load"));
            // drop: shutdown flushes the writer
        };
        run(true);
        run(false); // second session: served from the disk cache, no source reads
    }

    #[test]
    fn detects_thread_counts() {
        let t = Threads::for_root(Path::new("/"));
        assert!((2..=16).contains(&t.decode) && t.io >= 1);
        assert_eq!(rotational(Path::new("/proc/self")), None, "virtual filesystems are unknown");
    }
}

/// Scheduling benchmark: time to first screen and a simulated fast scroll, driving
/// [`ThumbCache::schedule`] the way the review grid does.
/// `BENCH_DIR=… BENCH_COLD_DISK=1 BENCH_SPEED=40 cargo test --release -- --ignored --nocapture bench_schedule`
#[cfg(test)]
mod sched_bench {
    use super::*;
    use std::os::fd::AsRawFd;
    use std::sync::mpsc;
    use std::time::Instant;

    const COLS: usize = 8;
    const ROWS: usize = 5;

    fn files() -> Vec<PathBuf> {
        let root = std::env::var_os("BENCH_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("test_images"));
        let mut files = Vec::new();
        let mut dirs = vec![root];
        while let Some(dir) = dirs.pop() {
            for entry in std::fs::read_dir(dir).unwrap().flatten() {
                let path = entry.path();
                let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
                if path.is_dir() {
                    dirs.push(path)
                } else if matches!(ext.as_str(), "jpg" | "png" | "tif") {
                    files.push(path)
                }
            }
        }
        files.sort_by_key(|p| p.file_name().unwrap().to_ascii_lowercase());
        files
    }

    /// Drops the files from the page cache, so reads hit the disk.
    fn evict(files: &[PathBuf]) {
        for f in files {
            if let Ok(file) = File::open(f) {
                // SAFETY: advisory call on an open descriptor.
                unsafe { libc::posix_fadvise(file.as_raw_fd(), 0, 0, libc::POSIX_FADV_DONTNEED) };
            }
        }
    }

    struct Harness<'a> {
        files: &'a [PathBuf],
        cache: ThumbCache,
        rx: mpsc::Receiver<(u32, Tier, Delivery)>,
        thumbs: HashSet<u32>,
        shown: HashSet<u32>,
        delivered: u32,
        previews: u32,
    }

    impl<'a> Harness<'a> {
        fn new(files: &'a [PathBuf], db: &Path) -> Self {
            let (tx, rx) = mpsc::channel();
            let threads = Threads::for_root(&files[0]);
            let cache = ThumbCache::new(Some(db), threads, move |id, tier, d| {
                let _ = tx.send((id, tier, d));
            });
            Self { files, cache, rx, thumbs: HashSet::new(), shown: HashSet::new(), delivered: 0, previews: 0 }
        }

        fn pump(&mut self, until: Instant) {
            while let Ok((id, tier, d)) = self.rx.recv_timeout(until.saturating_duration_since(Instant::now())) {
                match d {
                    Delivery::Thumb(..) => {
                        self.delivered += 1;
                        self.thumbs.insert(id);
                    }
                    Delivery::Preview(..) => self.previews += 1,
                    Delivery::Failed => {}
                }
                self.shown.insert(id);
                self.cache.complete(id, tier, d);
                if Instant::now() >= until {
                    break;
                }
            }
        }

        /// What `GridModel::reschedule` does for rows `first..first + ROWS`.
        fn view(&self, first: usize, down: bool, moving: bool) {
            let total = self.files.len().div_ceil(COLS);
            let rows = sched::window_rows(first, first + ROWS - 1, total, down, moving, ROWS * 2, ROWS / 2);
            let n = self.files.len();
            let ids = rows.iter().flat_map(|&r| (r * COLS..((r + 1) * COLS).min(n)).map(|i| (i as u32, Tier::Small)));
            self.cache.schedule(ids, |id| Some((self.files[id as usize].clone(), MediaKind::Image)));
        }

        /// Until every tile of the rows has its thumbnail (`full`) or anything at all.
        fn wait_rows(&mut self, rows: std::ops::Range<usize>, full: bool) -> Duration {
            let start = Instant::now();
            let need: Vec<u32> = (rows.start * COLS..(rows.end * COLS).min(self.files.len())).map(|i| i as u32).collect();
            while !need.iter().all(|i| if full { self.thumbs.contains(i) } else { self.shown.contains(i) }) {
                self.pump(Instant::now() + Duration::from_millis(1));
                assert!(start.elapsed() < Duration::from_secs(120), "timeout");
            }
            start.elapsed()
        }
    }

    #[test]
    #[ignore]
    fn bench_schedule() {
        let files = files();
        let cold_disk = std::env::var("BENCH_COLD_DISK").is_ok_and(|v| !v.is_empty());
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("thumbs.sqlite");
        println!("{} files, cold disk: {cold_disk}, {:?}", files.len(), Threads::for_root(&files[0]));
        let ms = |d: Duration| d.as_secs_f64() * 1e3;

        if cold_disk {
            evict(&files)
        }
        let mut h = Harness::new(&files, &db);
        let start = Instant::now();
        h.view(0, true, false);
        while h.shown.is_empty() {
            h.pump(Instant::now() + Duration::from_millis(1));
        }
        let first = start.elapsed();
        let full = h.wait_rows(0..ROWS, true) + first;
        println!("cold first screen: {:>8.1} ms (first tile {:.1} ms)", ms(full), ms(first));
        h.pump(Instant::now() + Duration::from_millis(500));
        drop(h); // flushes the writer

        if cold_disk {
            evict(&files)
        }
        let mut h = Harness::new(&files, &db);
        h.view(0, true, false);
        println!("warm first screen: {:>8.1} ms", ms(h.wait_rows(0..ROWS, true)));
        drop(h);

        let speed: f64 = std::env::var("BENCH_SPEED").ok().and_then(|s| s.parse().ok()).unwrap_or(40.0);
        let end = (files.len() / COLS).saturating_sub(ROWS).min(200);
        let dir2 = tempfile::tempdir().unwrap();
        if cold_disk {
            evict(&files)
        }
        let mut h = Harness::new(&files, &dir2.path().join("t.sqlite"));
        let start = Instant::now();
        let mut first_row = 0;
        let (mut frames, mut covered, mut shown, mut tiles_seen) = (0u32, 0u32, 0u32, 0u32);
        h.view(0, true, false);
        loop {
            let target = ((start.elapsed().as_secs_f64() * speed) as usize).min(end);
            if target != first_row {
                first_row = target;
                h.view(first_row, true, true);
            }
            h.pump(Instant::now() + Duration::from_millis(16));
            frames += 1;
            for id in first_row * COLS..((first_row + ROWS) * COLS).min(files.len()) {
                tiles_seen += 1;
                covered += h.thumbs.contains(&(id as u32)) as u32;
                shown += h.shown.contains(&(id as u32)) as u32;
            }
            if first_row == end {
                break;
            }
        }
        let scroll = start.elapsed();
        h.view(end, true, false); // the grid's settle timer
        let settle = h.wait_rows(end..end + ROWS, true);
        println!(
            "scroll {end} rows @ {speed} rows/s: {:.0} ms, {frames} frames, visible coverage {:.1}% (incl. previews {:.1}%), settle {:.1} ms, jobs done {} (+{} previews)",
            ms(scroll),
            100.0 * covered as f64 / tiles_seen as f64,
            100.0 * shown as f64 / tiles_seen as f64,
            ms(settle),
            h.delivered,
            h.previews
        );
    }
}
