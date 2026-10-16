//! Details' pictures: each decoded no larger than the window can show it (its longest edge in
//! physical pixels, not the file's), on a few threads at once, for the item shown and the
//! items next to it in the scope, so a step shows the next one from memory. Kept while they
//! are wanted, within [`BUDGET`].
//!
//! The UI says what it wants ([`want`]: the shown item first, then the likeliest next), takes
//! what is ready without waiting ([`get`]), and is told when a picture comes in (the `ready`
//! callback given to [`start`]).

use std::path::{Path, PathBuf};
use std::sync::{Condvar, Mutex, OnceLock};
use std::thread;
use std::time::{Instant, SystemTime};

use crate::thumbs::{self, Pixels};
use archive_core::media::MediaKind;

/// Most decoded bytes held: the shown picture and its neighbours (six 4K-screen pictures).
const BUDGET: usize = 320 << 20;
/// A picture decoded for a window at least this fraction of the size of the current one is
/// still sharp (no new decode after a small resize).
const SHARP_ENOUGH: f64 = 0.85;

/// When a file was decoded: a picture of an older version isn't used.
pub type Stamp = Option<(u64, SystemTime)>;

pub fn stamp(path: &Path) -> Stamp {
    let meta = std::fs::metadata(path).ok()?;
    Some((meta.len(), meta.modified().ok()?))
}

/// A picture as [`get`] has it.
pub enum Picture {
    /// Decoded for the window as it is.
    Sharp(Pixels),
    /// Decoded for a smaller window: better than a thumbnail until the sharp one is in.
    Soft(Pixels),
    /// The file can't be decoded.
    Failed,
}

struct Ready {
    path: PathBuf,
    stamp: Stamp,
    /// The edge it was decoded for.
    edge: u32,
    /// `None`: it failed.
    pixels: Option<Pixels>,
}

#[derive(Default)]
struct Pool {
    /// The longest edge wanted.
    edge: u32,
    /// To decode and keep, the shown item first.
    wanted: Vec<PathBuf>,
    pending: Vec<PathBuf>,
    ready: Vec<Ready>,
    /// No longer wanted, freed by a decode thread: freeing a picture takes a while (tens of
    /// milliseconds in a debug build), not to be spent on the UI thread.
    dropped: Vec<Ready>,
}

impl Pool {
    fn sharp(&self, edge: u32) -> bool {
        f64::from(edge) >= f64::from(self.edge) * SHARP_ENOUGH
    }

    /// The first wanted picture neither decoding nor ready (sharp, or failed).
    fn next(&self) -> Option<PathBuf> {
        let done = |p: &PathBuf| self.ready.iter().any(|r| r.path == *p && (r.pixels.is_none() || self.sharp(r.edge)));
        self.wanted.iter().find(|p| !self.pending.contains(p) && !done(p)).cloned()
    }

    /// Wants `paths` at `edge`, as many as the budget holds; drops the others.
    fn want(&mut self, edge: u32, mut paths: Vec<PathBuf>) {
        // About 4:3, three bytes a pixel.
        let each = (edge as usize).pow(2) * 9 / 4;
        paths.truncate((BUDGET / each.max(1)).max(2));
        self.drop_where(|r| !paths.contains(&r.path));
        self.edge = edge;
        self.wanted = paths;
    }

    /// Keeps a decoded picture (replacing an older one) if it is still wanted.
    fn put(&mut self, ready: Ready) -> bool {
        if !self.wanted.contains(&ready.path) {
            return false;
        }
        self.ready.retain(|r| r.path != ready.path);
        self.ready.push(ready);
        true
    }

    fn drop_where(&mut self, gone: impl Fn(&Ready) -> bool) {
        let (dropped, kept) = std::mem::take(&mut self.ready).into_iter().partition(gone);
        self.ready = kept;
        self.dropped.extend::<Vec<Ready>>(dropped);
    }

    fn get(&self, path: &Path, stamp: Stamp) -> Option<Picture> {
        let r = self.ready.iter().find(|r| r.path == path && stamp.is_some() && r.stamp == stamp)?;
        Some(match &r.pixels {
            None => Picture::Failed,
            Some(pixels) if self.sharp(r.edge) => Picture::Sharp(pixels.clone()),
            Some(pixels) => Picture::Soft(pixels.clone()),
        })
    }
}

static POOL: Mutex<Option<Pool>> = Mutex::new(None);
/// Signalled whenever there may be more to decode.
static WORK: Condvar = Condvar::new();
type OnReady = Box<dyn Fn(PathBuf) + Send + Sync>;
static ON_READY: OnceLock<OnReady> = OnceLock::new();

/// Starts the decode threads (once); `ready` is called on one of them with each picture
/// decoded (or failed) and kept.
pub fn start(ready: impl Fn(PathBuf) + Send + Sync + 'static) {
    if ON_READY.set(Box::new(ready)).is_err() {
        return;
    }
    *POOL.lock().unwrap() = Some(Pool::default());
    // The shown picture and the next two or three at once, leaving cores to the pipeline.
    let threads = thread::available_parallelism().map_or(2, |n| n.get() / 2).clamp(1, 4);
    for i in 0..threads {
        let _ = thread::Builder::new().name(format!("details-decode-{i}")).spawn(work);
    }
}

/// Wants pictures of `paths` (the shown item first, then the likeliest next ones) no larger
/// than `edge`; drops those of other items.
pub fn want(edge: u32, paths: Vec<PathBuf>) {
    if let Some(pool) = POOL.lock().unwrap().as_mut() {
        pool.want(edge, paths);
    }
    WORK.notify_all();
}

/// The picture of `path`, if decoded (doesn't wait).
pub fn get(path: &Path) -> Option<Picture> {
    let stamp = stamp(path);
    POOL.lock().unwrap().as_ref()?.get(path, stamp)
}

/// Drops the picture of `path` (the file changed); decoded again while wanted.
pub fn forget(path: &Path) {
    if let Some(pool) = POOL.lock().unwrap().as_mut() {
        pool.drop_where(|r| r.path == path);
    }
    WORK.notify_all();
}

fn work() {
    loop {
        let (path, edge) = {
            let mut pool = POOL.lock().unwrap();
            loop {
                if let Some(p) = pool.as_mut() {
                    if !p.dropped.is_empty() {
                        let dropped = std::mem::take(&mut p.dropped);
                        drop(pool);
                        drop(dropped);
                        pool = POOL.lock().unwrap();
                        continue;
                    }
                    if let Some(next) = p.next() {
                        p.pending.push(next.clone());
                        break (next, p.edge);
                    }
                }
                pool = WORK.wait(pool).unwrap();
            }
        };
        let started = Instant::now();
        let before = stamp(&path);
        let pixels = thumbs::decode(&path, MediaKind::Image, edge).map(|rgb| Pixels::clone_from_slice(rgb.as_raw(), rgb.width(), rgb.height()));
        log::debug!(
            "details: decoded {} for edge {edge} in {:?}: {}",
            path.display(),
            started.elapsed(),
            pixels.as_ref().map_or_else(|| "failed".to_owned(), |p| format!("{}x{}", p.width(), p.height()))
        );
        // Rewritten meanwhile (rotated): decoded again, as it isn't ready.
        let current = stamp(&path) == before;
        let kept = {
            let mut pool = POOL.lock().unwrap();
            let Some(pool) = pool.as_mut() else { continue };
            pool.pending.retain(|p| *p != path);
            current && pool.put(Ready { path: path.clone(), stamp: before, edge, pixels })
        };
        WORK.notify_all();
        if kept {
            if let Some(ready) = ON_READY.get() {
                ready(path);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ready(path: &str, edge: u32, ok: bool) -> Ready {
        Ready { path: path.into(), stamp: Some((1, SystemTime::UNIX_EPOCH)), edge, pixels: ok.then(|| Pixels::new(4, 3)) }
    }

    #[test]
    fn decodes_the_wanted_in_order_once_each() {
        let mut pool = Pool::default();
        pool.want(1600, vec!["a".into(), "b".into(), "c".into()]);
        assert_eq!(pool.next(), Some("a".into()));
        pool.pending.push("a".into());
        assert_eq!(pool.next(), Some("b".into()), "not twice at once");
        assert!(pool.put(ready("b", 1600, true)));
        assert!(pool.put(ready("c", 1600, false)));
        assert_eq!(pool.next(), None, "a failure isn't tried again");
        assert!(!pool.put(ready("z", 1600, true)), "not wanted");
    }

    #[test]
    fn a_larger_window_makes_pictures_soft_and_decodes_them_again() {
        let mut pool = Pool::default();
        let stamp = Some((1, SystemTime::UNIX_EPOCH));
        pool.want(1600, vec!["a".into(), "b".into()]);
        pool.put(ready("a", 1600, true));
        assert!(matches!(pool.get(Path::new("a"), stamp), Some(Picture::Sharp(_))));
        assert!(pool.get(Path::new("a"), Some((2, SystemTime::UNIX_EPOCH))).is_none(), "file changed since");
        pool.want(1700, vec!["a".into(), "b".into()]);
        assert!(matches!(pool.get(Path::new("a"), stamp), Some(Picture::Sharp(_))), "a small resize");
        pool.want(3840, vec!["b".into(), "a".into()]);
        assert!(matches!(pool.get(Path::new("a"), stamp), Some(Picture::Soft(_))));
        assert_eq!(pool.next(), Some("b".into()));
        pool.pending.push("b".into());
        assert_eq!(pool.next(), Some("a".into()), "decoded again, larger");
        pool.want(3840, vec!["b".into()]);
        assert!(pool.ready.is_empty(), "no longer wanted");
        assert_eq!(pool.dropped.len(), 1, "freed by a decode thread");
    }

    #[test]
    fn the_budget_bounds_how_many_are_kept() {
        let mut pool = Pool::default();
        let paths: Vec<PathBuf> = (0..10).map(|i| PathBuf::from(i.to_string())).collect();
        pool.want(1600, paths.clone());
        assert!(pool.wanted.len() == 10);
        pool.want(8000, paths);
        assert_eq!(pool.wanted.len(), 2, "the shown one and the next, at least");
    }
}
