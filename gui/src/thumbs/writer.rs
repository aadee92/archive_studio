//! Persists freshly decoded thumbnails off the decode path: one low-priority thread
//! JPEG-encodes both tiers, fills the compressed LRU and writes SQLite in batched
//! transactions. Decoders hand over with [`Writer::submit`], which never blocks: when the
//! bounded queue is full the write is dropped (the file is just decoded again next time).

use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use image::RgbImage;

use super::cache::{self, DiskCache, DiskConn, Fingerprint};
use super::{Compressed, Tier};

/// Rows per SQLite transaction (each file is two rows).
const BATCH: usize = 16;

/// Both tiers of one decoded file, and its average color.
pub struct Produced {
    pub fingerprint: Fingerprint,
    pub large: RgbImage,
    pub small: RgbImage,
    pub color: [u8; 3],
}

impl Produced {
    fn tier(&self, tier: Tier) -> &RgbImage {
        if tier == Tier::Large { &self.large } else { &self.small }
    }
}

pub fn pack(color: [u8; 3]) -> u32 {
    u32::from_be_bytes([0, color[0], color[1], color[2]])
}

pub fn unpack(color: u32) -> [u8; 3] {
    let [_, r, g, b] = color.to_be_bytes();
    [r, g, b]
}

/// Encodes `items` into the compressed LRU and, in one transaction, the disk cache.
/// Returns whether the disk write happened.
pub fn store(disk: Option<&mut DiskConn>, compressed: &Compressed, items: &[Produced]) -> bool {
    let mut rows: Vec<(i64, Arc<[u8]>, u32)> = Vec::with_capacity(items.len() * 2);
    for item in items {
        for tier in [Tier::Large, Tier::Small] {
            if let Some(jpeg) = cache::encode(item.tier(tier)) {
                rows.push((item.fingerprint.key(tier), jpeg.into(), pack(item.color)));
            }
        }
    }
    let mut lru = compressed.lock().unwrap();
    for (key, jpeg, color) in &rows {
        lru.insert(*key, (jpeg.clone(), *color), jpeg.len());
    }
    drop(lru);
    match disk {
        Some(disk) if !rows.is_empty() => disk.put(&rows.iter().map(|(k, j, c)| (*k, &j[..], *c)).collect::<Vec<_>>()),
        _ => false,
    }
}

#[derive(Default)]
struct State {
    queue: VecDeque<Produced>,
    closed: bool,
    /// A batch is being encoded / written outside the lock.
    busy: bool,
    finished: bool,
    written: u64,
    dropped: u64,
}

#[derive(Default)]
struct Shared {
    state: Mutex<State>,
    wake: Condvar,
    idle: Condvar,
}

pub struct Writer {
    shared: Arc<Shared>,
    cap: usize,
}

impl Writer {
    /// `cap`: files waiting to be written before new ones are dropped.
    pub fn spawn(disk: Option<DiskCache>, compressed: Compressed, cap: usize) -> Self {
        let shared = Arc::new(Shared::default());
        let worker = shared.clone();
        let spawned = thread::Builder::new().name("thumb-writer".into()).spawn(move || {
            lower_priority();
            let mut conn = disk.as_ref().and_then(DiskCache::connect);
            let mut state = worker.state.lock().unwrap();
            loop {
                if state.queue.is_empty() {
                    worker.idle.notify_all();
                    if state.closed {
                        break;
                    }
                    state = worker.wake.wait(state).unwrap();
                    continue;
                }
                let n = state.queue.len().min(BATCH);
                let batch: Vec<Produced> = state.queue.drain(..n).collect();
                state.busy = true;
                drop(state);
                store(conn.as_mut(), &compressed, &batch);
                state = worker.state.lock().unwrap();
                state.busy = false;
                state.written += batch.len() as u64;
            }
            state.finished = true;
            log::debug!("thumbnail writer stopped: {} written, {} dropped", state.written, state.dropped);
            worker.idle.notify_all();
        });
        if let Err(e) = &spawned {
            log::warn!("thumbnail writer: can't start a thread ({e}); thumbnails won't be cached on disk");
            shared.state.lock().unwrap().closed = true;
        }
        Self { shared, cap }
    }

    /// Queues `item` for storage; `false` (dropped) if the queue is full or closed.
    pub fn submit(&self, item: Produced) -> bool {
        let mut state = self.shared.state.lock().unwrap();
        if state.closed || state.queue.len() >= self.cap {
            state.dropped += 1;
            return false;
        }
        state.queue.push_back(item);
        drop(state);
        self.shared.wake.notify_one();
        true
    }

    /// A thumbnail still waiting to be written, so a re-request doesn't decode it again.
    pub fn lookup(&self, fingerprint: Fingerprint, tier: Tier) -> Option<(RgbImage, [u8; 3])> {
        let key = fingerprint.key(tier);
        let state = self.shared.state.lock().unwrap();
        let item = state.queue.iter().find(|p| p.fingerprint.key(tier) == key)?;
        Some((item.tier(tier).clone(), item.color))
    }

    /// Waits up to `timeout` for everything queued so far to be written.
    #[cfg(test)]
    pub fn flush(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let mut state = self.shared.state.lock().unwrap();
        while !state.finished && (state.busy || !state.queue.is_empty()) {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return false;
            }
            state = self.shared.idle.wait_timeout(state, left).unwrap().0;
        }
        true
    }

    /// Stops accepting writes, then gives the thread up to `timeout` to store what is queued.
    pub fn close(&self, timeout: Duration) -> bool {
        self.shared.state.lock().unwrap().closed = true;
        self.shared.wake.notify_all();
        let deadline = Instant::now() + timeout;
        let mut state = self.shared.state.lock().unwrap();
        while !state.finished {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return false;
            }
            state = self.shared.idle.wait_timeout(state, left).unwrap().0;
        }
        true
    }

    #[cfg(test)]
    pub fn counts(&self) -> (u64, u64) {
        let state = self.shared.state.lock().unwrap();
        (state.written, state.dropped)
    }
}

/// Encoding competes with decoding for cores; the decoders should win.
fn lower_priority() {
    #[cfg(target_os = "linux")]
    // SAFETY: plain syscalls on the calling thread; failure just leaves the priority as is.
    unsafe {
        let tid = libc::syscall(libc::SYS_gettid) as libc::id_t;
        libc::setpriority(libc::PRIO_PROCESS, tid, 10);
    }
}
