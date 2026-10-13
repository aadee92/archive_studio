//! Review mode: indexes media under a root directory and shows it as a virtualized grid.
//!
//! Why it stays fast with hundreds of thousands of files:
//! - The index is append-only: one shared chunk (`Arc<[Entry]>`) per indexer batch, with
//!   sort keys precomputed on the indexer thread. Filtering and sorting produce a
//!   `Vec<u32>` of entry ids. No per-file UI objects ever exist.
//! - The grid is a `ListView` of rows backed by [`GridModel`], which slices the id list
//!   on demand. Slint only asks for rows in the viewport, so scrolling is O(visible).
//! - While indexing, each batch is filtered, sorted on its own and merged into the shown
//!   order (no re-sort), and the view is told exactly which rows changed or were added,
//!   so on-screen rows are updated in place and the scroll position holds.
//! - Re-filtering a large library runs on a worker thread against a snapshot of the chunk
//!   list; the UI thread only swaps in the result, and results for superseded queries
//!   are dropped. Filter input is debounced.
//! - Thumbnails are scheduled from the visible row range the view reports (plus a
//!   prefetch band in the scroll direction) at the size tier the tiles need, loaded
//!   off-thread from a persistent cache or decoded, and kept in a byte-budgeted LRU;
//!   see `thumbs/`. Finished thumbnails collect in a mailbox drained by one event-loop
//!   callback, which re-reads each affected row once. `row_data` only reads what has arrived.

use std::cell::{Cell, RefCell};
use std::cmp::Ordering;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{self, AtomicBool};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};
use std::{fs, thread};

use slint::{ComponentHandle, Model, ModelNotify, ModelRc, ModelTracker, SharedString, Timer, TimerMode, VecModel};

use crate::thumbs::{self, sched, Delivery, ThumbCache, Threads, Tier};
use crate::{AppWindow, MediaKind, MediaRow, MediaTile, ReviewBridge};

#[cfg_attr(test, derive(Clone))]
struct Entry {
    path: PathBuf,
    name: SharedString,
    name_lc: Box<str>,
    /// The first 16 bytes of `name_lc`, big-endian: compares like the names wherever they differ.
    name_prefix: u128,
    kind: MediaKind,
    size: u64,
    /// Modification time in ns since the epoch (0 if earlier or unknown).
    mtime: u64,
}

impl Entry {
    fn new(path: PathBuf, name: String, kind: MediaKind, size: u64, modified: SystemTime) -> Self {
        let name_lc: Box<str> = name.to_lowercase().into();
        let mut prefix = [0u8; 16];
        let n = name_lc.len().min(16);
        prefix[..n].copy_from_slice(&name_lc.as_bytes()[..n]);
        let mtime = modified.duration_since(SystemTime::UNIX_EPOCH).map_or(0, |d| d.as_nanos().min(u64::MAX as u128) as u64);
        Self { path, name: name.into(), name_lc, name_prefix: u128::from_be_bytes(prefix), kind, size, mtime }
    }
}

fn classify(path: &Path) -> Option<MediaKind> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    Some(match ext.as_str() {
        "jpg" | "jpeg" | "png" | "tif" | "tiff" | "webp" | "heic" | "bmp" | "dng" => MediaKind::Image,
        "mp4" | "mkv" | "mov" | "avi" | "m4v" | "mts" | "dv" | "mpg" | "mpeg" | "webm" => MediaKind::Video,
        "wav" | "flac" | "mp3" | "m4a" | "aac" | "ogg" => MediaKind::Audio,
        _ => return None,
    })
}

/// The indexed files, append-only, as shared chunks (one per indexer batch): a snapshot for
/// the filter thread is a clone of the chunk list. An entry's id is its position.
#[derive(Clone, Default)]
struct Index {
    chunks: Vec<Arc<[Entry]>>,
    /// First id of each chunk.
    starts: Vec<u32>,
    len: usize,
}

impl Index {
    fn push(&mut self, chunk: Arc<[Entry]>) {
        if !chunk.is_empty() {
            self.starts.push(self.len as u32);
            self.len += chunk.len();
            self.chunks.push(chunk);
        }
    }

    fn len(&self) -> usize {
        self.len
    }

    fn get(&self, id: u32) -> Option<&Entry> {
        let chunk = self.starts.partition_point(|&start| start <= id).checked_sub(1)?;
        self.chunks[chunk].get((id - self.starts[chunk]) as usize)
    }

    /// Entries `from..` with their ids.
    fn iter_from(&self, from: usize) -> impl Iterator<Item = (u32, &Entry)> {
        let first = self.starts.partition_point(|&start| start as usize <= from).saturating_sub(1);
        self.chunks[first..]
            .iter()
            .zip(&self.starts[first..])
            .flat_map(|(chunk, &start)| (start..).zip(chunk.iter()))
            .skip_while(move |&(id, _)| (id as usize) < from)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Sort {
    #[default]
    Name,
    /// Newest first.
    Date,
    /// Largest first.
    Size,
    /// Kind, then name.
    Type,
}

/// An entry with its sort key: `(Sort::key, id, entry)`.
type Keyed<'a> = (u128, u32, &'a Entry);

impl Sort {
    /// From the UI's sort combo index (0 name, 1 date modified, 2 size, 3 type).
    fn from_index(index: i32) -> Self {
        match index {
            1 => Self::Date,
            2 => Self::Size,
            3 => Self::Type,
            _ => Self::Name,
        }
    }

    /// Primary key, consistent with [`cmp`](Self::cmp): a smaller key always sorts first.
    fn key(self, e: &Entry) -> u128 {
        match self {
            Self::Name => e.name_prefix,
            Self::Date => (!e.mtime).into(),
            Self::Size => (!e.size).into(),
            Self::Type => ((e.kind as u128) << 126) | (e.name_prefix >> 2),
        }
    }

    /// Display order: key, then the full name where it matters, then id (oldest first).
    fn cmp(self, a: &Keyed, b: &Keyed) -> Ordering {
        a.0.cmp(&b.0)
            .then_with(|| match self {
                Self::Name | Self::Type => a.2.name_lc.cmp(&b.2.name_lc),
                Self::Date | Self::Size => Ordering::Equal,
            })
            .then(a.1.cmp(&b.1))
    }
}

#[derive(Clone, Default)]
struct Query {
    /// Already lowercased.
    text: String,
    /// 0 all, 1 images, 2 video, 3 audio
    kind: i32,
    sort: Sort,
}

impl Query {
    fn accepts(&self, e: &Entry) -> bool {
        let kind_ok = match self.kind {
            1 => e.kind == MediaKind::Image,
            2 => e.kind == MediaKind::Video,
            3 => e.kind == MediaKind::Audio,
            _ => true,
        };
        kind_ok && e.name_lc.contains(&self.text)
    }

    /// The entries it accepts, in display order.
    fn filter_sort<'a>(&self, entries: impl Iterator<Item = (u32, &'a Entry)>) -> Vec<Keyed<'a>> {
        let sort = self.sort;
        let mut keyed: Vec<Keyed> = entries.filter(|(_, e)| self.accepts(e)).map(|(id, e)| (sort.key(e), id, e)).collect();
        keyed.sort_unstable_by(|a, b| sort.cmp(a, b));
        keyed
    }
}

const HIDDEN: u32 = u32::MAX;
/// Rows prefetched ahead of the viewport in the scroll direction, in screens.
const PREFETCH_AHEAD_SCREENS: usize = 2;
/// The view counts as resting (visible tiles loaded top to bottom) after this long.
const SCROLL_SETTLE: Duration = Duration::from_millis(150);
/// Libraries this small are re-filtered on the UI thread (≈0.3 ms per 2k entries), larger
/// ones on the filter thread; likewise for indexer batches merged into the grid.
const SYNC_FILTER_LIMIT: usize = 20_000;
/// UI-thread time one drain of finished thumbnails may take; the rest waits for the next
/// turn of the event loop, so a burst never holds up a frame.
const DRAIN_BUDGET: Duration = Duration::from_millis(4);

/// A filter + sort of the whole index.
struct Filtered {
    generation: u64,
    /// Entries `0..upto` were considered.
    upto: usize,
    visible: Vec<u32>,
    keys: Vec<u128>,
    slot_of: Vec<u32>,
}

impl Filtered {
    fn compute(index: &Index, query: &Query, generation: u64) -> Self {
        let (keys, visible): (Vec<u128>, Vec<u32>) = query.filter_sort(index.iter_from(0)).into_iter().map(|(key, id, _)| (key, id)).unzip();
        let mut slot_of = vec![HIDDEN; index.len()];
        for (slot, &id) in visible.iter().enumerate() {
            slot_of[id as usize] = slot as u32;
        }
        Self { generation, upto: index.len(), visible, keys, slot_of }
    }
}

/// Re-filters on a background thread. Requests queued behind a newer one are skipped.
struct FilterWorker {
    tx: mpsc::Sender<(u64, Index, Query)>,
}

impl FilterWorker {
    /// `deliver` runs on the worker thread with each result.
    fn spawn(deliver: impl Fn(Filtered) + Send + 'static) -> Self {
        let (tx, rx) = mpsc::channel::<(u64, Index, Query)>();
        let _ = thread::Builder::new().name("review-filter".into()).spawn(move || {
            while let Ok(mut request) = rx.recv() {
                while let Ok(newer) = rx.try_recv() {
                    request = newer;
                }
                let (generation, index, query) = request;
                deliver(Filtered::compute(&index, &query, generation));
            }
        });
        Self { tx }
    }
}

/// Results from worker threads waiting for the UI thread. Only the push that finds the
/// mailbox idle asks for a drain, so a burst costs one event-loop callback, not one each.
struct Mailbox<T> {
    items: Mutex<Vec<T>>,
    /// A drain is scheduled.
    queued: AtomicBool,
}

impl<T> Mailbox<T> {
    fn new() -> Self {
        Self { items: Mutex::new(Vec::new()), queued: AtomicBool::new(false) }
    }

    /// Returns whether the caller must schedule a drain.
    fn push(&self, item: T) -> bool {
        self.items.lock().unwrap().push(item);
        !self.queued.swap(true, atomic::Ordering::AcqRel)
    }

    /// Everything queued, oldest first. Pushes from here on schedule a new drain.
    fn take(&self) -> Vec<T> {
        self.queued.store(false, atomic::Ordering::Release);
        std::mem::take(&mut *self.items.lock().unwrap())
    }

    /// Returns items a drain didn't get to, ahead of newer ones; and whether the caller
    /// must schedule a drain for them.
    fn put_back(&self, rest: Vec<T>) -> bool {
        let mut items = self.items.lock().unwrap();
        items.splice(0..0, rest);
        drop(items);
        !self.queued.swap(true, atomic::Ordering::AcqRel)
    }
}

/// A finished thumbnail job on its way to the UI thread.
type Ready = (u32, Tier, Delivery);

/// [`ModelNotify`], recording what it sends in tests.
#[derive(Default)]
struct Notify {
    inner: ModelNotify,
    #[cfg(test)]
    log: RefCell<Vec<Change>>,
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Change {
    Changed(usize),
    Added(usize, usize),
    Reset,
}

impl Notify {
    fn row_changed(&self, row: usize) {
        #[cfg(test)]
        self.log.borrow_mut().push(Change::Changed(row));
        self.inner.row_changed(row);
    }

    fn row_added(&self, index: usize, count: usize) {
        #[cfg(test)]
        self.log.borrow_mut().push(Change::Added(index, count));
        self.inner.row_added(index, count);
    }

    fn reset(&self) {
        #[cfg(test)]
        self.log.borrow_mut().push(Change::Reset);
        self.inner.reset();
    }
}

struct GridModel {
    ui: slint::Weak<AppWindow>,
    entries: RefCell<Index>,
    /// Filtered + sorted entry ids, in display order.
    visible: RefCell<Vec<u32>>,
    /// `Sort::key` of each `visible` entry: merges compare these contiguous keys and only
    /// look up entries on a tie.
    keys: RefCell<Vec<u128>>,
    /// Entry id -> position in `visible`, or `HIDDEN`.
    slot_of: RefCell<Vec<u32>>,
    columns: Cell<usize>,
    tier: Cell<Tier>,
    /// First and last (inclusive) row in view.
    viewport: Cell<(usize, usize)>,
    scrolling_down: Cell<bool>,
    settle: Timer,
    query: RefCell<Query>,
    /// Of the latest refilter; older results are stale.
    generation: Cell<u64>,
    /// A refilter is running on the filter thread; `visible` still shows the old query.
    pending: Cell<bool>,
    filter: FilterWorker,
    sync_limit: usize,
    selected: Cell<Option<u32>>,
    indexing: Cell<bool>,
    thumbs: ThumbCache,
    ready: Arc<Mailbox<Ready>>,
    /// Tests: a drain left work for the next event-loop turn.
    #[cfg(test)]
    redrain_due: Cell<bool>,
    notify: Notify,
}

thread_local! {
    static GRID: RefCell<Option<Rc<GridModel>>> = const { RefCell::new(None) };
}

/// Runs `f` against the grid. UI thread only, which is where `upgrade_in_event_loop` runs.
fn with_grid(f: impl FnOnce(&GridModel)) {
    GRID.with_borrow(|grid| {
        if let Some(grid) = grid {
            f(grid)
        }
    });
}

pub fn install(ui: &AppWindow, root: PathBuf) {
    let db = thumbs::cache::default_db_path();
    let ready = Arc::new(Mailbox::new());
    let (mailbox, weak) = (ready.clone(), ui.as_weak());
    let thumbs = ThumbCache::new(db.as_deref(), Threads::for_root(&root), move |id, tier, delivery| {
        if mailbox.push((id, tier, delivery)) {
            let _ = weak.upgrade_in_event_loop(|_| with_grid(GridModel::drain_ready));
        }
    });
    let weak = ui.as_weak();
    let filter = FilterWorker::spawn(move |result| {
        let _ = weak.upgrade_in_event_loop(move |_| {
            with_grid(|g| {
                g.apply_filtered(result);
            })
        });
    });

    let grid = Rc::new(GridModel::new(ui.as_weak(), thumbs, ready, filter, SYNC_FILTER_LIMIT));
    GRID.set(Some(grid.clone()));

    let bridge = ui.global::<ReviewBridge>();
    bridge.set_rows(ModelRc::from(grid.clone()));
    bridge.set_root_path(root.display().to_string().into());
    bridge.on_columns_changed(|n| with_grid(|g| g.set_columns(n.max(1) as usize)));
    bridge.on_viewport_changed(|first, last| with_grid(|g| g.set_viewport(first, last)));
    bridge.on_select(|id| with_grid(|g| g.select(id as u32)));
    bridge.on_open(|id| with_grid(|g| g.open(id as u32)));

    let debounce = Timer::default();
    bridge.on_filter_changed(move |text, kind, sort| {
        let query = Query { text: text.to_lowercase(), kind, sort: Sort::from_index(sort) };
        debounce.start(TimerMode::SingleShot, Duration::from_millis(120), move || {
            with_grid(|g| g.set_query(query.clone()))
        });
    });

    grid.publish_status();
    spawn_indexer(root, ui.as_weak());
}

/// Stops thumbnail work at app exit, letting queued cache writes finish briefly.
pub fn shutdown() {
    with_grid(|g| g.thumbs.shutdown());
}

/// Walks `root` on a background thread, streaming batches to the UI every ~150 ms.
/// TODO: persist the index (e.g. SQLite with EXIF/duration columns) and watch the
/// tree with inotify instead of rescanning on every launch.
fn spawn_indexer(root: PathBuf, ui: slint::Weak<AppWindow>) {
    thread::spawn(move || {
        walk(root, |batch, done| {
            let _ = ui.upgrade_in_event_loop(move |_| with_grid(|g| g.append(batch, done)));
        })
    });
}

/// Calls `flush` with what was found every ~150 ms, and once more at the end (`done`).
fn walk(root: PathBuf, mut flush: impl FnMut(Arc<[Entry]>, bool)) {
    let mut batch = Vec::new();
    let mut last_flush = Instant::now();
    let mut dirs = vec![root];

    while let Some(dir) = dirs.pop() {
        let Ok(read) = fs::read_dir(&dir) else { continue };
        for item in read.flatten() {
            let Ok(file_type) = item.file_type() else { continue };
            if file_type.is_dir() {
                if !item.file_name().to_string_lossy().starts_with('.') {
                    dirs.push(item.path());
                }
                continue;
            }
            let path = item.path();
            let Some(kind) = classify(&path) else { continue };
            let meta = item.metadata().ok();
            let name = item.file_name().to_string_lossy().into_owned();
            let size = meta.as_ref().map_or(0, |m| m.len());
            let modified = meta.and_then(|m| m.modified().ok()).unwrap_or(SystemTime::UNIX_EPOCH);
            batch.push(Entry::new(path, name, kind, size, modified));
            if last_flush.elapsed() > Duration::from_millis(150) {
                flush(std::mem::take(&mut batch).into(), false);
                last_flush = Instant::now();
            }
        }
    }
    flush(batch.into(), true);
}

impl GridModel {
    fn new(ui: slint::Weak<AppWindow>, thumbs: ThumbCache, ready: Arc<Mailbox<Ready>>, filter: FilterWorker, sync_limit: usize) -> Self {
        Self {
            ui,
            entries: RefCell::default(),
            visible: RefCell::default(),
            keys: RefCell::default(),
            slot_of: RefCell::default(),
            columns: Cell::new(1),
            tier: Cell::new(Tier::Small),
            viewport: Cell::new((0, 0)),
            scrolling_down: Cell::new(true),
            settle: Timer::default(),
            query: RefCell::default(),
            generation: Cell::new(0),
            pending: Cell::new(false),
            filter,
            sync_limit,
            selected: Cell::new(None),
            indexing: Cell::new(true),
            thumbs,
            ready,
            #[cfg(test)]
            redrain_due: Cell::new(false),
            notify: Notify::default(),
        }
    }

    /// A batch from the indexer: merged into the shown order, notifying only the rows that
    /// changed (from the first insertion on) and the rows added at the end. `row_changed`
    /// re-reads a row only if the `ListView` has it instantiated (on screen) and is a no-op
    /// otherwise, so on-screen rows are updated in place; `reset` would drop and re-create
    /// them all. A batch too big to sort within a frame goes to the filter thread instead.
    fn append(&self, batch: Arc<[Entry]>, done: bool) {
        let from = self.entries.borrow().len();
        self.entries.borrow_mut().push(batch);
        self.indexing.set(!done);
        let added = self.entries.borrow().len() - from;
        if self.pending.get() {
            // The refilter in flight catches up on these entries when it lands.
        } else if added >= self.sync_limit.max(1) {
            // A huge batch (fast disk): sorting it would stall a frame, so re-filter off-thread.
            self.refilter();
        } else {
            let old_rows = self.row_count();
            if let Some(first) = self.merge_from(from) {
                let first_row = first / self.columns.get();
                let new_rows = self.row_count();
                for row in first_row..old_rows {
                    self.notify.row_changed(row);
                }
                if new_rows > old_rows {
                    self.notify.row_added(old_rows, new_rows - old_rows);
                }
                let (first_shown, last_shown) = self.viewport.get();
                if first_row <= last_shown + (last_shown - first_shown + 1) * PREFETCH_AHEAD_SCREENS {
                    self.reschedule();
                }
            }
        }
        self.publish_status();
    }

    /// Filters entries `from..` into `visible` in sort order (a merge: the batch is sorted
    /// on its own, existing ids are only moved) and updates `slot_of`.
    /// Returns the first slot whose entry changed, if any did.
    fn merge_from(&self, from: usize) -> Option<usize> {
        let index = self.entries.borrow();
        let query = self.query.borrow();
        let mut slot_of = self.slot_of.borrow_mut();
        slot_of.resize(index.len(), HIDDEN);
        let new = query.filter_sort(index.iter_from(from));
        let (sort, first_new) = (query.sort, new.first()?);
        let before = |key: u128, id: u32, item: &Keyed| match key.cmp(&item.0) {
            Ordering::Equal => sort.cmp(&(key, id, index.get(id).expect("shown ids are indexed")), item).is_lt(),
            order => order.is_lt(),
        };

        let (mut visible, mut keys) = (self.visible.borrow_mut(), self.keys.borrow_mut());
        let first = gallop(visible.len(), |i| before(keys[i], visible[i], first_new));
        // In place, from the back: each new id lands after the old ones that sort before it.
        let (mut read, mut write) = (visible.len(), visible.len() + new.len());
        visible.resize(write, 0);
        keys.resize(write, 0);
        for item in new.iter().rev() {
            let after = gallop(read - first, |i| !before(keys[read - 1 - i], visible[read - 1 - i], item));
            visible.copy_within(read - after..read, write - after);
            keys.copy_within(read - after..read, write - after);
            (read, write) = (read - after, write - after - 1);
            visible[write] = item.1;
            keys[write] = item.0;
        }
        debug_assert_eq!(read, write);
        for (slot, &id) in visible.iter().enumerate().skip(first) {
            slot_of[id as usize] = slot as u32;
        }
        Some(first)
    }

    fn set_query(&self, query: Query) {
        *self.query.borrow_mut() = query;
        self.refilter();
    }

    /// Re-filters everything: synchronously for small libraries, else on the filter thread
    /// (the grid keeps showing the previous result until it lands).
    fn refilter(&self) {
        let generation = self.generation.get() + 1;
        self.generation.set(generation);
        let query = self.query.borrow().clone();
        if self.entries.borrow().len() < self.sync_limit {
            let result = Filtered::compute(&self.entries.borrow(), &query, generation);
            self.apply_filtered(result);
        } else {
            self.pending.set(true);
            let _ = self.filter.tx.send((generation, self.entries.borrow().clone(), query));
        }
    }

    /// Shows a refilter result unless a newer one was requested; returns whether it did.
    fn apply_filtered(&self, result: Filtered) -> bool {
        if result.generation != self.generation.get() {
            return false;
        }
        self.pending.set(false);
        let behind = self.entries.borrow().len() - result.upto;
        *self.visible.borrow_mut() = result.visible;
        *self.keys.borrow_mut() = result.keys;
        *self.slot_of.borrow_mut() = result.slot_of;
        // Entries indexed since the snapshot: merged here, unless that's a big job itself.
        if behind >= self.sync_limit.max(1) {
            self.refilter();
        } else {
            self.merge_from(result.upto);
        }
        self.notify.reset();
        self.reschedule();
        self.publish_status();
        true
    }

    /// Also called when the tile size changes, to pick the thumbnail tier.
    fn set_columns(&self, columns: usize) {
        let tier = self.ui.upgrade().map_or(Tier::Large, |ui| {
            let tile = ui.global::<ReviewBridge>().get_tile_size();
            Tier::for_tile(tile * ui.window().scale_factor())
        });
        let columns_changed = self.columns.replace(columns) != columns;
        if self.tier.replace(tier) != tier || columns_changed {
            self.notify.reset();
            self.reschedule();
        }
    }

    /// The view scrolled or resized: rows `first..=last` are on screen.
    fn set_viewport(&self, first: i32, last: i32) {
        let first = first.max(0) as usize;
        let view = (first, (last.max(0) as usize).max(first));
        let previous = self.viewport.replace(view);
        if previous == view {
            return;
        }
        if first != previous.0 {
            self.scrolling_down.set(first > previous.0);
        }
        self.schedule_window(true);
        self.settle.start(TimerMode::SingleShot, SCROLL_SETTLE, || with_grid(|g| g.reschedule()));
    }

    /// Hands the thumbnail pipeline the tiles to load, best first: on screen top to
    /// bottom, then the prefetch band ahead, then a little behind. Everything else
    /// still queued is cancelled.
    fn reschedule(&self) {
        self.schedule_window(false);
    }

    /// `moving`: the view is scrolling, so visible rows go leading edge first.
    fn schedule_window(&self, moving: bool) {
        let (first, last) = self.viewport.get();
        let screen = last - first + 1;
        let rows = sched::window_rows(
            first,
            last,
            self.row_count(),
            self.scrolling_down.get(),
            moving,
            screen * PREFETCH_AHEAD_SCREENS,
            (screen / 2).max(1),
        );
        let (columns, tier) = (self.columns.get(), self.tier.get());
        let (visible, entries) = (self.visible.borrow(), self.entries.borrow());
        let ids = rows.iter().flat_map(|&row| visible.get(row * columns..((row + 1) * columns).min(visible.len())).unwrap_or_default());
        self.thumbs.schedule(ids.map(|&id| (id, tier)), |id| entries.get(id).map(|e| (e.path.clone(), e.kind)));
    }

    fn select(&self, id: u32) {
        let previous = self.selected.replace(Some(id));
        for id in previous.into_iter().chain([id]) {
            self.refresh_entry(id);
        }
    }

    fn open(&self, id: u32) {
        // TODO: in-app viewer (zoom, side-by-side compare, metadata editing).
        if let Some(entry) = self.entries.borrow().get(id) {
            let _ = std::process::Command::new("xdg-open").arg(&entry.path).spawn();
        }
    }

    /// Applies the thumbnails that arrived since the last drain (in arrival order, within
    /// [`DRAIN_BUDGET`]), then re-delivers each affected row once.
    fn drain_ready(&self) {
        let start = Instant::now();
        let mut items = self.ready.take().into_iter();
        let mut changed = Vec::new();
        for (id, tier, delivery) in items.by_ref() {
            if self.thumbs.complete(id, tier, delivery) {
                changed.push(id);
            }
            if start.elapsed() >= DRAIN_BUDGET {
                break;
            }
        }
        let rest: Vec<Ready> = items.collect();
        if !rest.is_empty() && self.ready.put_back(rest) {
            self.drain_later();
        }

        let (slot_of, columns) = (self.slot_of.borrow(), self.columns.get());
        let mut rows: Vec<usize> =
            changed.iter().filter_map(|&id| slot_of.get(id as usize).filter(|&&slot| slot != HIDDEN)).map(|&slot| slot as usize / columns).collect();
        drop(slot_of);
        rows.sort_unstable();
        rows.dedup();
        for row in rows {
            self.notify.row_changed(row);
        }
    }

    /// Drains again on the next event-loop turn (timers run after the frame is drawn).
    fn drain_later(&self) {
        #[cfg(not(test))]
        Timer::single_shot(Duration::ZERO, || with_grid(GridModel::drain_ready));
        #[cfg(test)]
        self.redrain_due.set(true);
    }

    /// Re-delivers the row holding entry `id`, if that entry is currently shown.
    fn refresh_entry(&self, id: u32) {
        let slot = self.slot_of.borrow().get(id as usize).copied().unwrap_or(HIDDEN);
        if slot != HIDDEN {
            self.notify.row_changed(slot as usize / self.columns.get());
        }
    }

    fn tile(&self, id: u32, entry: &Entry) -> MediaTile {
        let thumb = self.thumbs.get(id, self.tier.get()).map(|(image, _exact)| image);
        let color = self.thumbs.color(id).map_or(slint::Color::default(), |[r, g, b]| slint::Color::from_rgb_u8(r, g, b));
        MediaTile {
            index: id as i32,
            name: entry.name.clone(),
            kind: entry.kind,
            detail: human_size(entry.size).into(),
            thumb_ready: thumb.is_some(),
            thumb: thumb.unwrap_or_default(),
            color,
            selected: self.selected.get() == Some(id),
        }
    }

    fn publish_status(&self) {
        let Some(ui) = self.ui.upgrade() else { return };
        let shown = self.visible.borrow().len();
        let total = self.entries.borrow().len();
        let mut status = if shown == total {
            format!("{} items", grouped(total))
        } else {
            format!("{} of {} items", grouped(shown), grouped(total))
        };
        if self.indexing.get() {
            status.push_str(" · indexing…");
        }
        let bridge = ui.global::<ReviewBridge>();
        bridge.set_status_text(status.into());
        bridge.set_indexing(self.indexing.get());
        bridge.set_empty(shown == 0);
    }
}

/// How many of the positions `0..len` satisfy `before` (true, then false), by exponential
/// search: O(log answer), so merging k sorted items into n takes O(k log(n/k)) comparisons.
fn gallop(len: usize, before: impl Fn(usize) -> bool) -> usize {
    let (mut lo, mut step) = (0, 1);
    while lo + step <= len && before(lo + step - 1) {
        lo += step;
        step *= 2;
    }
    let (mut lo, mut hi) = (lo, (lo + step - 1).min(len));
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        if before(mid) {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    lo
}

impl Model for GridModel {
    type Data = MediaRow;

    fn row_count(&self) -> usize {
        self.visible.borrow().len().div_ceil(self.columns.get())
    }

    fn row_data(&self, row: usize) -> Option<MediaRow> {
        let columns = self.columns.get();
        let visible = self.visible.borrow();
        let start = row * columns;
        let ids = visible.get(start..(start + columns).min(visible.len()))?;
        let entries = self.entries.borrow();
        let tiles: Vec<MediaTile> = ids.iter().filter_map(|&id| Some(self.tile(id, entries.get(id)?))).collect();
        Some(MediaRow { tiles: ModelRc::new(VecModel::from(tiles)) })
    }

    fn model_tracker(&self) -> &dyn ModelTracker {
        &self.notify.inner
    }
}

fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 { format!("{bytes} B") } else { format!("{value:.1} {}", UNITS[unit]) }
}

/// 12345 -> "12,345"
fn grouped(n: usize) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::thumbs::Pixels;
    use std::sync::atomic::AtomicUsize;

    fn grid(sync_limit: usize, columns: usize, deliver: impl Fn(Filtered) + Send + 'static) -> GridModel {
        let thumbs = ThumbCache::new(None, Threads { decode: 1, io: 1 }, |_, _, _| {});
        let g = GridModel::new(Default::default(), thumbs, Arc::new(Mailbox::new()), FilterWorker::spawn(deliver), sync_limit);
        g.columns.set(columns);
        g.viewport.set((0, 4));
        g
    }

    /// xorshift64*
    struct Rng(u64);

    impl Rng {
        fn below(&mut self, n: u64) -> u64 {
            self.0 ^= self.0 >> 12;
            self.0 ^= self.0 << 25;
            self.0 ^= self.0 >> 27;
            self.0.wrapping_mul(0x2545_f491_4f6c_dd1d) % n.max(1)
        }
    }

    /// Names, sizes and dates with many ties and shared 16-byte prefixes.
    fn random_entries(rng: &mut Rng, n: usize) -> Vec<Entry> {
        (0..n)
            .map(|_| {
                let mut name = if rng.below(2) == 0 { "IMG_20230512_143".to_string() } else { String::new() };
                for _ in 0..rng.below(5) {
                    name.push(b"aAb_1."[rng.below(6) as usize] as char);
                }
                let kind = [MediaKind::Image, MediaKind::Video, MediaKind::Audio][rng.below(3) as usize];
                let modified = SystemTime::UNIX_EPOCH + Duration::from_secs(rng.below(6));
                Entry::new(PathBuf::from(format!("/x/{name}")), name, kind, rng.below(6) * 1000, modified)
            })
            .collect()
    }

    fn rows(g: &GridModel) -> Vec<Vec<u32>> {
        g.visible.borrow().chunks(g.columns.get()).map(<[u32]>::to_vec).collect()
    }

    /// Applies the logged notifications to `mirror` the way a view would (re-reading rows
    /// it is told about) and checks it ends up showing what the model holds.
    fn replay(g: &GridModel, mirror: &mut Vec<Vec<u32>>) {
        let now = rows(g);
        for change in g.notify.log.take() {
            match change {
                Change::Changed(row) => {
                    assert!(row < mirror.len(), "{change:?} past the {} rows the view knows", mirror.len());
                    mirror[row] = now[row].clone();
                }
                Change::Added(index, count) => {
                    assert!(index <= mirror.len(), "{change:?}");
                    mirror.splice(index..index, now[index..index + count].iter().cloned());
                }
                Change::Reset => *mirror = now.clone(),
            }
        }
        assert_eq!(mirror.len(), g.row_count());
        assert_eq!(*mirror, now, "view out of sync with the model");
    }

    /// The order before sort keys existed: a stable sort of ascending ids.
    fn reference_order(index: &Index, query: &Query) -> Vec<u32> {
        let mut ids: Vec<u32> = index.iter_from(0).filter(|(_, e)| query.accepts(e)).map(|(id, _)| id).collect();
        let e = |id: &u32| index.get(*id).unwrap();
        match query.sort {
            Sort::Name => ids.sort_by(|a, b| e(a).name_lc.cmp(&e(b).name_lc)),
            Sort::Date => ids.sort_by(|a, b| e(b).mtime.cmp(&e(a).mtime)),
            Sort::Size => ids.sort_by(|a, b| e(b).size.cmp(&e(a).size)),
            Sort::Type => ids.sort_by(|a, b| (e(a).kind as i32, &e(a).name_lc).cmp(&(e(b).kind as i32, &e(b).name_lc))),
        }
        ids
    }

    #[test]
    fn incremental_append_matches_full_refilter() {
        let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
        for sort in [Sort::Name, Sort::Date, Sort::Size, Sort::Type] {
            for (text, kind) in [("", 0), ("a", 0), ("", 1), ("img_20230512_143b", 0)] {
                let columns = 1 + rng.below(7) as usize;
                let g = grid(usize::MAX, columns, |_| {});
                g.set_query(Query { text: text.into(), kind, sort });
                let mut mirror = Vec::new();
                replay(&g, &mut mirror);
                let mut appended = 0;
                while appended < 1500 {
                    let max = if rng.below(4) == 0 { 400 } else { 40 };
                    let n = rng.below(max) as usize;
                    g.append(random_entries(&mut rng, n).into(), false);
                    appended += n;

                    let (index, query) = (g.entries.borrow(), g.query.borrow());
                    let full = Filtered::compute(&index, &query, 0);
                    assert_eq!(*g.visible.borrow(), full.visible, "{sort:?} {text:?} {kind} after {appended}");
                    assert_eq!(*g.slot_of.borrow(), full.slot_of);
                    assert_eq!(*g.keys.borrow(), full.keys);
                    assert_eq!(full.visible, reference_order(&index, &query), "sort keys order like the names/values");
                    drop((index, query));
                    assert!(!g.notify.log.borrow().contains(&Change::Reset), "appends never reset the view");
                    replay(&g, &mut mirror);
                }
            }
        }
    }

    #[test]
    fn batched_delivery_coalesces_and_dedupes_rows() {
        let g = grid(usize::MAX, 8, |_| {});
        g.append(random_entries(&mut Rng(7), 2000).into(), true);
        g.notify.log.take();

        let scheduled = AtomicUsize::new(0);
        thread::scope(|s| {
            for t in 0..4 {
                let (ready, scheduled) = (g.ready.clone(), &scheduled);
                s.spawn(move || {
                    for id in (t..2000).step_by(4) {
                        if id % 10 == 0 && ready.push((id, Tier::Small, Delivery::Preview(Pixels::new(2, 2), [9, 9, 9]))) {
                            scheduled.fetch_add(1, atomic::Ordering::Relaxed);
                        }
                        if ready.push((id, Tier::Small, Delivery::Thumb(Pixels::new(4, 4), [1, 2, 3]))) {
                            scheduled.fetch_add(1, atomic::Ordering::Relaxed);
                        }
                    }
                });
            }
        });
        assert_eq!(scheduled.load(atomic::Ordering::Relaxed), 1, "one event-loop callback for the whole burst");

        let mut drains = 0;
        let mut notified = Vec::new();
        loop {
            g.drain_ready();
            drains += 1;
            let rows: Vec<usize> = g.notify.log.take().into_iter().map(|c| match c {
                Change::Changed(row) => row,
                other => panic!("{other:?}"),
            }).collect();
            assert!(rows.windows(2).all(|w| w[0] < w[1]), "each row notified once per drain");
            notified.extend(rows);
            if !g.redrain_due.replace(false) {
                break;
            }
        }
        notified.sort_unstable();
        notified.dedup();
        assert_eq!(notified, (0..250).collect::<Vec<_>>(), "every row with a new thumbnail");
        assert!(drains >= 1, "more than one only if a drain ran past its time budget");
        assert!((0..2000).all(|id| g.thumbs.get(id, Tier::Small).is_some_and(|(_, exact)| exact)));

        assert!(g.ready.push((0, Tier::Small, Delivery::Failed)), "an idle mailbox asks for a drain again");
        g.drain_ready();
        assert!(g.notify.log.take().is_empty());
    }

    #[test]
    fn stale_background_results_are_discarded() {
        let (tx, rx) = mpsc::channel();
        let g = grid(500, 4, move |result| {
            let _ = tx.send(result);
        });
        let mut rng = Rng(42);
        g.append(random_entries(&mut rng, 100).into(), false);
        assert!(!g.pending.get(), "small batches are merged on the UI thread");
        g.append(random_entries(&mut rng, 900).into(), false);
        assert!(g.pending.get(), "a big batch is sorted on the filter thread");
        assert!(g.apply_filtered(rx.recv_timeout(Duration::from_secs(10)).unwrap()));
        assert_eq!(g.visible.borrow().len(), 1000);

        let old = Query { text: "a".into(), ..Query::default() };
        let new = Query { kind: 1, sort: Sort::Date, ..Query::default() };
        g.set_query(old.clone());
        let stale_generation = g.generation.get();
        g.set_query(new.clone());
        assert!(g.pending.get());
        let shown = g.visible.borrow().clone();
        g.notify.log.take();

        let stale = Filtered::compute(&g.entries.borrow(), &old, stale_generation);
        assert!(!g.apply_filtered(stale), "superseded");
        assert_eq!(*g.visible.borrow(), shown);
        assert!(g.pending.get());

        // Indexing goes on while the filter thread works; the grid waits for its result.
        g.append(random_entries(&mut rng, 300).into(), true);
        assert!(g.notify.log.take().is_empty());
        assert_eq!(*g.visible.borrow(), shown);

        loop {
            let result = rx.recv_timeout(Duration::from_secs(10)).expect("filter result");
            let current = result.generation == g.generation.get();
            assert_eq!(g.apply_filtered(result), current);
            if current {
                break;
            }
        }
        assert!(!g.pending.get());
        assert_eq!(g.notify.log.take(), vec![Change::Reset]);
        let index = g.entries.borrow();
        assert_eq!(index.len(), 1300);
        assert_eq!(*g.visible.borrow(), reference_order(&index, &new), "includes entries indexed meanwhile");
        assert_eq!(*g.slot_of.borrow(), Filtered::compute(&index, &new, 0).slot_of);
    }

    #[test]
    fn index_chunks_and_gallop() {
        let mut index = Index::default();
        let mut rng = Rng(3);
        for n in [3, 0, 1, 5] {
            index.push(random_entries(&mut rng, n).into());
        }
        assert_eq!((index.len(), index.chunks.len()), (9, 3));
        assert!(index.get(8).is_some() && index.get(9).is_none());
        for from in 0..=10 {
            let ids: Vec<u32> = index.iter_from(from).map(|(id, _)| id).collect();
            assert_eq!(ids, (from.min(9) as u32..9).collect::<Vec<_>>());
            assert!(index.iter_from(from).all(|(id, e)| std::ptr::eq(e, index.get(id).unwrap())));
        }
        let ids: Vec<u32> = (0..100).collect();
        for cut in 0..=100 {
            assert_eq!(gallop(ids.len(), |i| ids[i] < cut), cut as usize);
        }
    }
}

/// UI-thread cost benchmarks for indexing, refiltering and thumbnail delivery.
/// `BENCH_DIR=… cargo test --release -- --ignored --nocapture --test-threads 1 bench4_`
#[cfg(test)]
mod bench4 {
    use super::*;

    fn grid(deliver: impl Fn(Filtered) + Send + 'static) -> GridModel {
        let thumbs = ThumbCache::new(None, Threads { decode: 1, io: 1 }, |_, _, _| {});
        let g = GridModel::new(Default::default(), thumbs, Arc::new(Mailbox::new()), FilterWorker::spawn(deliver), SYNC_FILTER_LIMIT);
        g.columns.set(8);
        g.viewport.set((0, 4));
        g
    }

    fn root() -> PathBuf {
        std::env::var_os("BENCH_DIR").map(PathBuf::from).unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("test_images"))
    }

    fn ms(d: Duration) -> f64 {
        d.as_secs_f64() * 1e3
    }

    /// The indexer's batches: as it flushes them (`every == 0`), or re-cut to `every` entries.
    fn batches(every: usize) -> mpsc::Receiver<(Arc<[Entry]>, bool)> {
        let (tx, rx) = mpsc::channel();
        let root = root();
        thread::spawn(move || {
            if every == 0 {
                walk(root, |batch, done| tx.send((batch, done)).unwrap());
                return;
            }
            let mut all = Vec::new();
            walk(root, |batch, _| all.push(batch));
            let mut entries: Vec<Entry> = all.into_iter().flat_map(|c| c.to_vec()).collect();
            while !entries.is_empty() {
                let rest = entries.split_off(every.min(entries.len()));
                let done = rest.is_empty();
                tx.send((std::mem::replace(&mut entries, rest).into(), done)).unwrap();
            }
        });
        rx
    }

    #[test]
    #[ignore]
    fn bench4_index() {
        for (sort, every) in [(Sort::Name, 0), (Sort::Name, 2000), (Sort::Date, 2000), (Sort::Name, 19000)] {
            let (tx, rx) = mpsc::channel();
            let g = grid(move |r| tx.send(r).unwrap());
            g.query.borrow_mut().sort = sort;
            let mut times = Vec::new();
            let mut notified = 0;
            fn timed(g: &GridModel, times: &mut Vec<Duration>, notified: &mut usize, f: impl FnOnce()) {
                let start = Instant::now();
                f();
                times.push(start.elapsed());
                *notified += g.notify.log.take().len();
            }
            for (batch, done) in batches(every) {
                // Background results land as their own event-loop callbacks.
                while let Ok(result) = rx.try_recv() {
                    timed(&g, &mut times, &mut notified, || { let _ = g.apply_filtered(result); });
                }
                timed(&g, &mut times, &mut notified, || g.append(batch, done));
            }
            while g.pending.get() {
                let result = rx.recv().unwrap();
                timed(&g, &mut times, &mut notified, || { let _ = g.apply_filtered(result); });
            }
            assert_eq!(g.visible.borrow().len(), g.entries.borrow().len());
            let total: Duration = times.iter().sum();
            let mut sorted = times.clone();
            sorted.sort();
            println!(
                "index sort={sort:?} batch={}: {} entries, {} UI callbacks, UI total {:.1} ms, per callback median {:.2} ms, p90 {:.2} ms, max {:.2} ms, {notified} notifications",
                if every == 0 { "150ms".to_string() } else { every.to_string() },
                g.entries.borrow().len(),
                times.len(),
                ms(total),
                ms(sorted[sorted.len() / 2]),
                ms(sorted[sorted.len() * 9 / 10]),
                ms(*sorted.last().unwrap())
            );
        }
    }

    #[test]
    #[ignore]
    fn bench4_filter() {
        let (tx, rx) = mpsc::channel();
        let g = grid(move |r| tx.send(r).unwrap());
        for (batch, done) in batches(0) {
            g.append(batch, done);
        }
        while g.pending.get() {
            g.apply_filtered(rx.recv().unwrap());
        }
        let n = g.entries.borrow().len();
        let queries = [("img_1", 0, 0), ("", 1, 0), ("", 0, 1), ("", 0, 2), ("", 0, 3), ("", 0, 0), ("scan", 0, 0)];
        // Completes a query: (latency until the grid shows it, UI-thread time spent).
        let run = |q: Query| {
            let start = Instant::now();
            g.set_query(q);
            let mut blocked = start.elapsed();
            while g.pending.get() {
                let result = rx.recv().unwrap();
                let apply = Instant::now();
                g.apply_filtered(result);
                blocked += apply.elapsed();
            }
            (start.elapsed(), blocked)
        };
        for (text, kind, sort) in queries {
            let mut times = Vec::new();
            for _ in 0..3 {
                run(Query::default());
                times.push(run(Query { text: text.into(), kind, sort: Sort::from_index(sort) }));
            }
            times.sort();
            let (latency, blocked) = times[1];
            println!(
                "filter n={n} text={text:?} kind={kind} sort={sort}: latency {:.2} ms, UI blocked {:.2} ms ({} shown)",
                ms(latency),
                ms(blocked),
                g.visible.borrow().len()
            );
        }
    }

    #[test]
    #[ignore]
    fn bench4_deliver() {
        type Job = Box<dyn FnOnce(&GridModel) + Send>;
        for _ in 0..3 {
            let (tx, rx) = mpsc::channel();
            let g = grid(move |r| tx.send(r).unwrap());
            for (batch, done) in batches(0) {
                g.append(batch, done);
            }
            while g.pending.get() {
                g.apply_filtered(rx.recv().unwrap());
            }
            let n = g.entries.borrow().len().min(2000) as u32;
            let (tx, rx) = mpsc::channel::<Job>();
            let producers: Vec<_> = (0..8u32)
                .map(|t| {
                    let (tx, ready) = (tx.clone(), g.ready.clone());
                    thread::spawn(move || {
                        for id in (t..n).step_by(8) {
                            let px = crate::thumbs::Pixels::new(192, 128);
                            if ready.push((id, Tier::Small, Delivery::Thumb(px, [1, 2, 3]))) {
                                let _ = tx.send(Box::new(|g: &GridModel| g.drain_ready()));
                            }
                        }
                    })
                })
                .collect();
            drop(tx);
            let (mut callbacks, mut ui, mut rows) = (0, Duration::ZERO, 0);
            let mut run = |job: Job| {
                let start = Instant::now();
                job(&g);
                // What the ListView's repeater does for an on-screen row_changed: re-read the row.
                for change in g.notify.log.take() {
                    rows += 1;
                    if let Change::Changed(row) = change {
                        if row < 5 {
                            let _ = g.row_data(row);
                        }
                    }
                }
                ui += start.elapsed();
                callbacks += 1;
            };
            // BENCH_PACE_MS: the event loop only gets to callbacks every so often (busy drawing).
            let pace = std::env::var("BENCH_PACE_MS").ok().and_then(|v| v.parse().ok()).map(Duration::from_millis);
            for job in rx {
                if let Some(pace) = pace {
                    thread::sleep(pace);
                }
                run(job);
                while g.redrain_due.replace(false) {
                    run(Box::new(|g: &GridModel| g.drain_ready()));
                }
            }
            for p in producers {
                p.join().unwrap();
            }
            println!("deliver {n} thumbs: {callbacks} event-loop callbacks, {rows} row notifications, UI time {:.2} ms", ms(ui));
        }
    }
}
