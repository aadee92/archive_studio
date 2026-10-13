//! Thumbnail pipeline for the review grid.
//!
//! ```text
//!  UI thread                                worker threads
//!  GridModel::row_data ──request──▶ bounded LIFO queue ──▶ decode()
//!         ▲                                                   │
//!         └── ThumbCache::complete (LRU of slint::Image) ◀────┘ via event loop
//! ```
//!
//! - LIFO: the most recently requested tiles (what is on screen now) decode first.
//!   When fast scrolling overflows the queue, the oldest requests (rows already
//!   scrolled past) are dropped and simply re-requested if they come back into view.
//! - Pixels cross threads as `SharedPixelBuffer`; the `slint::Image` (not `Send`)
//!   is created on the UI thread.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;

use slint::{Image, Rgba8Pixel, SharedPixelBuffer};

use crate::MediaKind;

pub type Pixels = SharedPixelBuffer<Rgba8Pixel>;

/// Longest edge of a decoded thumbnail; covers the largest tile size on HiDPI.
#[allow(dead_code)] // used by decode() once implemented
const THUMB_EDGE: u32 = 384;
const QUEUE_CAP: usize = 512;
const CACHE_CAP: usize = 3000;

struct Job {
    id: u32,
    path: PathBuf,
    kind: MediaKind,
}

#[derive(Default)]
struct Queue {
    jobs: Mutex<VecDeque<Job>>,
    ready: Condvar,
}

pub struct ThumbCache {
    queue: Arc<Queue>,
    /// id -> (image, last-used tick)
    images: RefCell<HashMap<u32, (Image, u64)>>,
    tick: Cell<u64>,
    pending: RefCell<HashSet<u32>>,
    failed: RefCell<HashSet<u32>>,
}

impl ThumbCache {
    /// `on_ready` runs on a worker thread and must forward to the UI thread,
    /// where the result is handed to [`complete`](Self::complete).
    pub fn new(on_ready: impl Fn(u32, Option<Pixels>) + Send + Sync + 'static) -> Self {
        let queue = Arc::new(Queue::default());
        let on_ready = Arc::new(on_ready);
        let workers = thread::available_parallelism().map_or(4, |n| n.get()).clamp(2, 8);
        for _ in 0..workers {
            let (queue, on_ready) = (queue.clone(), on_ready.clone());
            thread::spawn(move || loop {
                let job = {
                    let mut jobs = queue.jobs.lock().unwrap();
                    loop {
                        if let Some(job) = jobs.pop_front() {
                            break job;
                        }
                        jobs = queue.ready.wait(jobs).unwrap();
                    }
                };
                on_ready(job.id, decode(&job.path, job.kind));
            });
        }

        Self {
            queue,
            images: RefCell::default(),
            tick: Cell::new(0),
            pending: RefCell::default(),
            failed: RefCell::default(),
        }
    }

    pub fn get(&self, id: u32) -> Option<Image> {
        let tick = self.bump();
        let mut images = self.images.borrow_mut();
        let (image, used) = images.get_mut(&id)?;
        *used = tick;
        Some(image.clone())
    }

    pub fn request(&self, id: u32, path: &Path, kind: MediaKind) {
        if self.failed.borrow().contains(&id) || !self.pending.borrow_mut().insert(id) {
            return;
        }
        let mut jobs = self.queue.jobs.lock().unwrap();
        jobs.push_front(Job { id, path: path.to_owned(), kind });
        if jobs.len() > QUEUE_CAP {
            if let Some(stale) = jobs.pop_back() {
                self.pending.borrow_mut().remove(&stale.id);
            }
        }
        drop(jobs);
        self.queue.ready.notify_one();
    }

    pub fn complete(&self, id: u32, pixels: Option<Pixels>) {
        self.pending.borrow_mut().remove(&id);
        let Some(pixels) = pixels else {
            self.failed.borrow_mut().insert(id);
            return;
        };
        let tick = self.bump();
        let mut images = self.images.borrow_mut();
        images.insert(id, (Image::from_rgba8(pixels), tick));

        // Evict the least recently used 10% in one pass rather than one-by-one.
        if images.len() > CACHE_CAP {
            let mut by_age: Vec<(u64, u32)> = images.iter().map(|(&id, (_, used))| (*used, id)).collect();
            let n = CACHE_CAP / 10;
            by_age.select_nth_unstable(n);
            for (_, id) in &by_age[..n] {
                images.remove(id);
            }
        }
    }

    fn bump(&self) -> u64 {
        self.tick.set(self.tick.get() + 1);
        self.tick.get()
    }
}

/// Produces a thumbnail no larger than [`THUMB_EDGE`] on its longest side.
///
/// To implement:
/// 1. Persistent cache first: `$XDG_CACHE_HOME/archive-studio/thumbs/<hash(path, mtime, size)>.webp`,
///    so a large library pays the decode cost once.
/// 2. Images: JPEG with DCT-domain downscaling (libjpeg-turbo / zune-jpeg scale 1/8) so
///    600 dpi scans are never fully decoded; TIFF/PNG via the `image` crate; honour EXIF orientation.
/// 3. Video: one frame ~10% in via ffmpeg (`-ss … -frames:v 1 -vf scale=…`) or libav bindings.
/// 4. Audio: embedded cover art, else `None` (the grid shows a placeholder glyph).
fn decode(_path: &Path, _kind: MediaKind) -> Option<Pixels> {
    None
}
