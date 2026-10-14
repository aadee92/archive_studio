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
//! - Files the pipeline adds, changes or loses are merged in, replaced or taken out
//!   ([`update_files`]) the same way, without re-walking.

use std::cell::{Cell, RefCell};
use std::cmp::Ordering;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{self, AtomicBool};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};
use std::{fs, thread};

use slint::{ComponentHandle, Model, ModelNotify, ModelRc, ModelTracker, SharedString, Timer, TimerMode, VecModel};

use crate::thumbs::{self, sched, Delivery, ThumbCache, Threads, Tier};
use archive_core::catalog::query::{self, Expr};
use archive_core::catalog::{Catalog, TagKind};
use archive_core::media::{classify, MediaKind};

use crate::{details, AppWindow, MediaRow, MediaTile, ReviewBridge};

#[cfg_attr(test, derive(Clone))]
pub(crate) struct Entry {
    path: PathBuf,
    /// [`path_hash`] of `path`, computed off the UI thread.
    path_hash: u64,
    name: SharedString,
    name_lc: Box<str>,
    /// The first 16 bytes of `name_lc`, big-endian: compares like the names wherever they differ.
    name_prefix: u128,
    kind: MediaKind,
    size: u64,
    /// Modification time in ns since the epoch (0 if earlier or unknown).
    mtime: u64,
    tags: Arc<[(String, Option<TagKind>)]>,
}

impl Entry {
    #[cfg(test)]
    fn new(path: PathBuf, name: String, kind: MediaKind, size: u64, modified: SystemTime) -> Self {
        Self::with_tags(path, name, kind, size, modified, Vec::new())
    }

    fn with_tags(
        path: PathBuf,
        name: String,
        kind: MediaKind,
        size: u64,
        modified: SystemTime,
        tags: Vec<(String, Option<TagKind>)>,
    ) -> Self {
        let name_lc: Box<str> = name.to_lowercase().into();
        let mut prefix = [0u8; 16];
        let n = name_lc.len().min(16);
        prefix[..n].copy_from_slice(&name_lc.as_bytes()[..n]);
        let mtime = modified.duration_since(SystemTime::UNIX_EPOCH).map_or(0, |d| d.as_nanos().min(u64::MAX as u128) as u64);
        Self {
            path_hash: path_hash(&path),
            path,
            name: name.into(),
            name_lc,
            name_prefix: u128::from_be_bytes(prefix),
            kind,
            size,
            mtime,
            tags: tags.into(),
        }
    }
}

/// Identifies a path in [`GridModel::by_path`]. 64 bits: a collision among a library's
/// paths is vanishingly unlikely, and would only make an update miss its entry.
fn path_hash(path: &Path) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    path.hash(&mut hasher);
    hasher.finish()
}

/// The indexed files, append-only, as shared chunks (one per indexer batch): a snapshot for
/// the filter thread is a clone of the chunk list. An entry's id is its position.
/// A file that changes gets a new entry (and so a new id, which also means a fresh
/// thumbnail); the old one, like that of a deleted file, is retired into `gone`.
#[derive(Clone, Default)]
struct Index {
    chunks: Vec<Arc<[Entry]>>,
    /// First id of each chunk.
    starts: Vec<u32>,
    len: usize,
    /// Retired ids, never shown again.
    gone: Arc<HashSet<u32>>,
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

    /// Entries still in the library.
    fn live(&self) -> usize {
        self.len - self.gone.len()
    }

    /// Entries `from..` with their ids, leaving out retired ones.
    fn live_from(&self, from: usize) -> impl Iterator<Item = (u32, &Entry)> {
        self.iter_from(from).filter(|(id, _)| !self.gone.contains(id))
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
    /// Already lowercased text string.
    text: String,
    /// Pre-parsed text query expression if it parsed as a valid tag query.
    text_expr: Option<Arc<Expr>>,
    /// Tag filter string.
    tag: String,
    /// Pre-parsed tag query expression.
    tag_expr: Option<Arc<Expr>>,
    /// 0 all, 1 images, 2 video, 3 audio
    kind: i32,
    sort: Sort,
}

impl Query {
    fn new(text: String, tag: String, kind: i32, sort: Sort) -> Self {
        let text_trimmed = text.trim();
        let text_expr = if text_trimmed.is_empty() {
            None
        } else {
            query::parse(text_trimmed).ok().map(Arc::new)
        };
        let tag_trimmed = tag.trim();
        let tag_expr = if tag_trimmed.is_empty() {
            None
        } else {
            query::parse(tag_trimmed).ok().map(Arc::new)
        };
        Self {
            text: text_trimmed.to_lowercase(),
            text_expr,
            tag: tag_trimmed.to_string(),
            tag_expr,
            kind,
            sort,
        }
    }

    fn accepts(&self, e: &Entry) -> bool {
        let kind_ok = match self.kind {
            1 => e.kind == MediaKind::Image,
            2 => e.kind == MediaKind::Video,
            3 => e.kind == MediaKind::Audio,
            _ => true,
        };
        if !kind_ok {
            return false;
        }

        // Text search: matches filename OR matches tag query if typed in search bar
        if !self.text.is_empty() {
            let name_match = e.name_lc.contains(&self.text);
            let tag_match = self.text_expr.as_ref().map_or(false, |expr| expr.matches(&e.tags));
            if !name_match && !tag_match {
                return false;
            }
        }

        // Dedicated tag filter
        if !self.tag.is_empty() {
            if let Some(expr) = &self.tag_expr {
                if !expr.matches(&e.tags) {
                    return false;
                }
            } else {
                let tag_lc = self.tag.to_ascii_lowercase();
                if !e.tags.iter().any(|(t, _)| t.to_ascii_lowercase().contains(&tag_lc)) {
                    return false;
                }
            }
        }
        true
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
    /// How many entries were retired at the time; later ones are still shown.
    gone: usize,
    visible: Vec<u32>,
    keys: Vec<u128>,
    slot_of: Vec<u32>,
}

impl Filtered {
    fn compute(index: &Index, query: &Query, generation: u64) -> Self {
        let (keys, visible): (Vec<u128>, Vec<u32>) = query.filter_sort(index.live_from(0)).into_iter().map(|(key, id, _)| (key, id)).unzip();
        let mut slot_of = vec![HIDDEN; index.len()];
        for (slot, &id) in visible.iter().enumerate() {
            slot_of[id as usize] = slot as u32;
        }
        Self { generation, upto: index.len(), gone: index.gone.len(), visible, keys, slot_of }
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
    Removed(usize, usize),
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

    fn row_removed(&self, index: usize, count: usize) {
        #[cfg(test)]
        self.log.borrow_mut().push(Change::Removed(index, count));
        self.inner.row_removed(index, count);
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
    /// Entry id of each path ([`path_hash`]), for [`update_files`](Self::update_files).
    by_path: RefCell<HashMap<u64, u32>>,
    /// File updates that arrived while indexing, applied once it is done (the indexer
    /// may find the same files).
    deferred: RefCell<Vec<FileUpdate>>,
    thumbs: ThumbCache,
    ready: Arc<Mailbox<Ready>>,
    available_tags: RefCell<BTreeSet<String>>,
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
    bridge.on_filter_changed(move |text, tag, kind, sort| {
        let query = Query::new(text.to_string(), tag.to_string(), kind, Sort::from_index(sort));
        debounce.start(TimerMode::SingleShot, Duration::from_millis(120), move || {
            with_grid(|g| g.set_query(query.clone()))
        });
    });

    grid.publish_status();
    grid.publish_tags();
    spawn_indexer(root, ui.as_weak());
}

/// Stops thumbnail work at app exit, letting queued cache writes finish briefly.
pub fn shutdown() {
    with_grid(|g| g.thumbs.shutdown());
}

/// Walks `root` on a background thread, streaming batches to the UI every ~150 ms. After
/// that, the pipeline's reports keep the grid current ([`update_files`]).
/// TODO: build the grid from the catalog instead of rescanning on every launch.
fn spawn_indexer(root: PathBuf, ui: slint::Weak<AppWindow>) {
    thread::spawn(move || {
        walk(root, |batch, done| {
            let _ = ui.upgrade_in_event_loop(move |_| with_grid(|g| g.append(batch, done)));
        })
    });
}

fn extract_file_tags(path: &Path, root: &Path, cat: Option<&Catalog>) -> Vec<(String, Option<TagKind>)> {
    let mut tags: Vec<(String, Option<TagKind>)> = Vec::new();

    // 1. Tags from catalog
    if let Some(cat) = cat {
        if let Ok(Some(asset_id)) = cat.find_asset(path) {
            if let Ok(applied) = cat.tags_of(asset_id) {
                for t in applied {
                    if !tags.iter().any(|(p, _)| p.eq_ignore_ascii_case(&t.path)) {
                        tags.push((t.path, Some(t.kind)));
                    }
                }
            }
        }
    }

    // 2. Tags from XMP sidecar
    let sidecar = archive_core::catalog::xmp::sidecar_path(path);
    if sidecar.is_file() {
        if let Ok(content) = fs::read_to_string(&sidecar) {
            if let Ok(xmp_tags) = archive_core::catalog::xmp::extract_tags(&content) {
                for (path, kind) in xmp_tags {
                    if !tags.iter().any(|(p, _)| p.eq_ignore_ascii_case(&path)) {
                        tags.push((path, Some(kind)));
                    }
                }
            }
        }
    }

    // 3. Folder-based tags (relative to root)
    if let Ok(rel) = path.strip_prefix(root) {
        if let Some(parent) = rel.parent() {
            for comp in parent.components() {
                let name = comp.as_os_str().to_string_lossy();
                if !name.is_empty() && !name.starts_with('.') {
                    let tag_name = name.into_owned();
                    if !tags.iter().any(|(p, _)| p.eq_ignore_ascii_case(&tag_name)) {
                        tags.push((tag_name, Some(TagKind::Keyword)));
                    }
                }
            }
        }
    }

    tags
}

fn entry_for(path: PathBuf, kind: MediaKind, meta: Option<fs::Metadata>, root: &Path, catalog: Option<&Catalog>) -> Entry {
    let name = path.file_name().map_or_else(String::new, |n| n.to_string_lossy().into_owned());
    let size = meta.as_ref().map_or(0, |m| m.len());
    let modified = meta.and_then(|m| m.modified().ok()).unwrap_or(SystemTime::UNIX_EPOCH);
    let tags = extract_file_tags(&path, root, catalog);
    Entry::with_tags(path, name, kind, size, modified, tags)
}

/// A file the pipeline (or a capture) reported, as the grid should now show it.
pub(crate) enum FileUpdate {
    /// The file as it is now. `changed`: its pixels changed (e.g. rotated) even if its size,
    /// date and tags look the same.
    Present(Entry, bool),
    Gone(PathBuf),
}

impl FileUpdate {
    /// Reads the file at `path` (blocking: stat, catalog tags, sidecar), or `None` if it
    /// isn't media the grid shows. For a worker thread; hand the result to [`update_files`].
    pub(crate) fn read(path: &Path, root: &Path, catalog: Option<&Catalog>, changed: bool) -> Option<Self> {
        let kind = classify(path)?;
        let hidden = path.strip_prefix(root).ok()?.components().any(|c| c.as_os_str().to_string_lossy().starts_with('.'));
        if hidden {
            return None;
        }
        Some(match fs::metadata(path) {
            Ok(meta) if meta.is_file() => Self::Present(entry_for(path.to_owned(), kind, Some(meta), root, catalog), changed),
            _ => Self::Gone(path.to_owned()),
        })
    }

    fn path(&self) -> &Path {
        match self {
            Self::Present(entry, _) => &entry.path,
            Self::Gone(path) => path,
        }
    }
}

/// Applies files the pipeline added, changed or saw go: new ones are merged into the grid in
/// sort order, changed ones replaced, gone ones taken out, without re-walking the library.
pub(crate) fn update_files(updates: Vec<FileUpdate>) {
    with_grid(|g| g.update_files(updates));
}

/// Calls `flush` with what was found every ~150 ms, and once more at the end (`done`).
fn walk(root: PathBuf, mut flush: impl FnMut(Arc<[Entry]>, bool)) {
    let catalog = Catalog::open(&Catalog::default_path(&root), &root).ok();
    let mut batch = Vec::new();
    let mut last_flush = Instant::now();
    let mut dirs = vec![root.clone()];

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
            batch.push(entry_for(path, kind, item.metadata().ok(), &root, catalog.as_ref()));
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
            by_path: RefCell::default(),
            deferred: RefCell::default(),
            thumbs,
            ready,
            available_tags: RefCell::default(),
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
        self.entries.borrow_mut().push(batch.clone());
        {
            let mut by_path = self.by_path.borrow_mut();
            by_path.extend(batch.iter().enumerate().map(|(i, e)| (e.path_hash, (from + i) as u32)));
        }
        let finished = self.indexing.get() && done;
        self.indexing.set(!done);
        let mut tags_changed = false;
        {
            let mut known = self.available_tags.borrow_mut();
            for entry in batch.iter() {
                for (t, _) in entry.tags.iter() {
                    if known.insert(t.clone()) {
                        tags_changed = true;
                    }
                }
            }
        }
        if tags_changed {
            self.publish_tags();
        }
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
        if finished {
            let deferred = self.deferred.take();
            if !deferred.is_empty() {
                self.update_files(deferred);
            }
        }
    }

    /// See [`update_files`]. A changed file gets a new entry (and id, so its thumbnail is made
    /// afresh) and the old one is retired; a file that looks the same is left alone.
    fn update_files(&self, updates: Vec<FileUpdate>) {
        if self.indexing.get() {
            self.deferred.borrow_mut().extend(updates);
            return;
        }
        let (mut retired, mut fresh, mut replaced) = (Vec::new(), Vec::new(), Vec::new());
        {
            let (index, by_path) = (self.entries.borrow(), self.by_path.borrow());
            let mut seen = HashSet::new();
            // Newest first: only the last report of a path counts.
            for update in updates.into_iter().rev() {
                let hash = path_hash(update.path());
                if !seen.insert(hash) {
                    continue;
                }
                let current = by_path
                    .get(&hash)
                    .copied()
                    .filter(|id| !index.gone.contains(id) && index.get(*id).is_some_and(|e| e.path == update.path()));
                match update {
                    FileUpdate::Present(entry, changed) => {
                        if let Some(id) = current {
                            let old = index.get(id).expect("mapped ids are indexed");
                            if !changed && (old.size, old.mtime, &old.tags) == (entry.size, entry.mtime, &entry.tags) {
                                continue;
                            }
                            retired.push(id);
                            replaced.push((id, fresh.len()));
                        }
                        fresh.push(entry);
                    }
                    FileUpdate::Gone(_) => retired.extend(current),
                }
            }
        }
        self.retire(&retired);
        if fresh.is_empty() {
            self.publish_status();
            return;
        }
        let from = self.entries.borrow().len() as u32;
        if let Some(&(_, i)) = replaced.iter().find(|(old, _)| self.selected.get() == Some(*old)) {
            self.selected.set(Some(from + i as u32));
        }
        self.append(fresh.into(), true);
    }

    /// Retires entries: they leave the grid (the rows from the first one on are re-read) and
    /// their thumbnails are dropped.
    fn retire(&self, ids: &[u32]) {
        if ids.is_empty() {
            return;
        }
        Arc::make_mut(&mut self.entries.borrow_mut().gone).extend(ids);
        for &id in ids {
            self.thumbs.invalidate(id);
        }
        if self.pending.get() {
            // The refilter in flight leaves them out when it lands.
            return;
        }
        let old_rows = self.row_count();
        let Some(first) = self.hide(ids.iter().copied()) else { return };
        let (first_row, new_rows) = (first / self.columns.get(), self.row_count());
        for row in first_row..new_rows {
            self.notify.row_changed(row);
        }
        if old_rows > new_rows {
            self.notify.row_removed(new_rows, old_rows - new_rows);
        }
        self.reschedule();
    }

    /// Takes `ids` out of the shown order; returns the first slot that changed, if any.
    fn hide(&self, ids: impl Iterator<Item = u32>) -> Option<usize> {
        let mut slot_of = self.slot_of.borrow_mut();
        let mut first = usize::MAX;
        for id in ids {
            if let Some(slot) = slot_of.get_mut(id as usize).filter(|slot| **slot != HIDDEN) {
                first = first.min(*slot as usize);
                *slot = HIDDEN;
            }
        }
        if first == usize::MAX {
            return None;
        }
        let (mut visible, mut keys) = (self.visible.borrow_mut(), self.keys.borrow_mut());
        let mut write = first;
        for read in first..visible.len() {
            let id = visible[read];
            if slot_of[id as usize] != HIDDEN {
                (visible[write], keys[write]) = (id, keys[read]);
                slot_of[id as usize] = write as u32;
                write += 1;
            }
        }
        visible.truncate(write);
        keys.truncate(write);
        Some(first)
    }

    /// Filters entries `from..` into `visible` in sort order (a merge: the batch is sorted
    /// on its own, existing ids are only moved) and updates `slot_of`.
    /// Returns the first slot whose entry changed, if any did.
    fn merge_from(&self, from: usize) -> Option<usize> {
        let index = self.entries.borrow();
        let query = self.query.borrow();
        let mut slot_of = self.slot_of.borrow_mut();
        slot_of.resize(index.len(), HIDDEN);
        let new = query.filter_sort(index.live_from(from));
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
        // Entries retired since the snapshot.
        if self.entries.borrow().gone.len() != result.gone {
            let gone = self.entries.borrow().gone.clone();
            self.hide(gone.iter().copied());
        }
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

    /// Shows `id` on the Details page.
    fn open(&self, id: u32) {
        let Some(ui) = self.ui.upgrade() else { return };
        let Some((path, name, kind, size)) =
            self.entries.borrow().get(id).map(|e| (e.path.clone(), e.name.clone(), e.kind, e.size))
        else {
            return;
        };
        self.select(id);
        details::show(&ui, path, name, kind, size);
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
            kind: ui_kind(entry.kind),
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
        let total = self.entries.borrow().live();
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

    fn publish_tags(&self) {
        let Some(ui) = self.ui.upgrade() else { return };
        let tags: Vec<SharedString> = self.available_tags.borrow().iter().map(|s| s.as_str().into()).collect();
        ui.global::<ReviewBridge>().set_available_tags(ModelRc::new(VecModel::from(tags)));
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

fn ui_kind(kind: MediaKind) -> crate::MediaKind {
    match kind {
        MediaKind::Image => crate::MediaKind::Image,
        MediaKind::Video => crate::MediaKind::Video,
        MediaKind::Audio => crate::MediaKind::Audio,
    }
}

pub fn human_size(bytes: u64) -> String {
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
pub(crate) fn grouped(n: usize) -> String {
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
                Change::Removed(index, count) => {
                    assert!(index + count <= mirror.len(), "{change:?}");
                    mirror.drain(index..index + count);
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
                g.set_query(Query { text: text.into(), kind, sort, ..Default::default() });
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

    fn present(entry: &Entry, changed: bool) -> FileUpdate {
        FileUpdate::Present(entry.clone(), changed)
    }

    /// Files the pipeline reports are merged in, replaced or taken out, and the grid ends up
    /// as a fresh walk of the same files would show it, without ever being reset.
    #[test]
    fn file_updates_match_a_fresh_index() {
        let mut rng = Rng(11);
        for sort in [Sort::Name, Sort::Date, Sort::Size, Sort::Type] {
            let g = grid(usize::MAX, 3, |_| {});
            g.set_query(Query { sort, ..Default::default() });
            let mut mirror = Vec::new();
            replay(&g, &mut mirror);
            // Distinct paths: the grid's notion of a file.
            let mut files: Vec<Entry> = random_entries(&mut rng, 300)
                .into_iter()
                .enumerate()
                .map(|(i, e)| Entry::new(PathBuf::from(format!("/x/{i}/{}", e.name)), e.name.to_string(), e.kind, e.size, SystemTime::UNIX_EPOCH))
                .collect();
            g.append(files[..200].to_vec().into(), true);
            replay(&g, &mut mirror);

            for round in 0..20 {
                let mut updates = Vec::new();
                for _ in 0..rng.below(30) {
                    let i = rng.below(files.len() as u64) as usize;
                    match rng.below(4) {
                        // New, or unchanged (a no-op).
                        0 | 1 => updates.push(present(&files[i], false)),
                        // Changed: a new size and date.
                        2 => {
                            let e = &files[i];
                            let modified = SystemTime::UNIX_EPOCH + Duration::from_secs(rng.below(9));
                            files[i] = Entry::new(e.path.clone(), e.name.to_string(), e.kind, rng.below(9) * 1000, modified);
                            updates.push(present(&files[i], false));
                        }
                        _ => updates.push(FileUpdate::Gone(files[i].path.clone())),
                    }
                }
                g.update_files(updates);
                assert!(!g.notify.log.borrow().contains(&Change::Reset), "updates never reset the view");
                replay(&g, &mut mirror);

                let index = g.entries.borrow();
                let shown: Vec<(PathBuf, u64, u64)> =
                    g.visible.borrow().iter().map(|&id| index.get(id).map(|e| (e.path.clone(), e.size, e.mtime)).unwrap()).collect();
                let live: Vec<&Entry> = index.live_from(0).map(|(_, e)| e).collect();
                assert_eq!(live.len(), shown.len(), "{sort:?} round {round}: everything live is shown");
                let mut paths: Vec<_> = shown.iter().map(|s| &s.0).collect();
                paths.sort();
                paths.dedup();
                assert_eq!(paths.len(), shown.len(), "a path is shown once");
                for (path, size, mtime) in &shown {
                    let file = files.iter().find(|f| &f.path == path).unwrap();
                    assert_eq!((file.size, file.mtime), (*size, *mtime), "the latest version of {path:?}");
                }
                assert_eq!(*g.visible.borrow(), Filtered::compute(&index, &g.query.borrow(), 0).visible, "in sort order");
            }
        }
    }

    #[test]
    fn file_updates_wait_for_indexing_and_keep_the_selection() {
        let g = grid(usize::MAX, 4, |_| {});
        let a = Entry::new("/x/a.png".into(), "a.png".into(), MediaKind::Image, 10, SystemTime::UNIX_EPOCH);
        let b = Entry::new("/x/b.png".into(), "b.png".into(), MediaKind::Image, 20, SystemTime::UNIX_EPOCH);
        g.append(vec![a.clone()].into(), false);
        // Reported while the indexer is still walking (which finds b itself).
        g.update_files(vec![present(&b, false), present(&a, true)]);
        assert_eq!(g.entries.borrow().len(), 1, "deferred");
        g.select(0);
        g.append(vec![b.clone()].into(), true);
        let index = g.entries.borrow();
        assert_eq!(index.len(), 3, "a replaced (it changed), b not duplicated");
        assert_eq!(index.gone.iter().copied().collect::<Vec<_>>(), vec![0]);
        let names: Vec<String> = g.visible.borrow().iter().map(|&id| index.get(id).unwrap().name.to_string()).collect();
        assert_eq!(names, ["a.png", "b.png"]);
        assert_eq!(g.selected.get(), Some(2), "the selection follows the file to its new entry");
        drop(index);

        g.update_files(vec![FileUpdate::Gone("/x/b.png".into()), FileUpdate::Gone("/x/unknown.png".into())]);
        assert_eq!(g.visible.borrow().len(), 1);
        assert_eq!(g.entries.borrow().live(), 1);
    }

    #[test]
    fn retired_entries_stay_out_of_background_refilters() {
        let (tx, rx) = mpsc::channel();
        let g = grid(10, 2, move |result| {
            let _ = tx.send(result);
        });
        let files: Vec<Entry> = (0..20)
            .map(|i| Entry::new(format!("/x/{i:02}.png").into(), format!("{i:02}.png"), MediaKind::Image, i, SystemTime::UNIX_EPOCH))
            .collect();
        g.append(files.clone().into(), true);
        while g.pending.get() {
            g.apply_filtered(rx.recv_timeout(Duration::from_secs(10)).unwrap());
        }
        g.set_query(Query { sort: Sort::Size, ..Default::default() });
        assert!(g.pending.get());
        // Retired while the refilter runs on its snapshot.
        g.update_files(vec![FileUpdate::Gone("/x/03.png".into())]);
        while g.pending.get() {
            g.apply_filtered(rx.recv_timeout(Duration::from_secs(10)).unwrap());
        }
        assert_eq!(g.visible.borrow().len(), 19);
        assert!(!g.visible.borrow().contains(&3));
        assert_eq!(g.slot_of.borrow()[3], HIDDEN);
        let index = g.entries.borrow();
        assert_eq!(*g.visible.borrow(), Filtered::compute(&index, &g.query.borrow(), 0).visible);
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

    #[test]
    fn query_filters_by_tag() {
        let modified = SystemTime::UNIX_EPOCH;
        let e1 = Entry::with_tags(
            PathBuf::from("/lib/beach.jpg"),
            "beach.jpg".to_string(),
            MediaKind::Image,
            1000,
            modified,
            vec![
                ("Scenes|Beach".to_string(), Some(TagKind::Scene)),
                ("People|Family|Alice".to_string(), Some(TagKind::Person)),
            ],
        );
        let e2 = Entry::with_tags(
            PathBuf::from("/lib/lake.jpg"),
            "lake.jpg".to_string(),
            MediaKind::Image,
            1200,
            modified,
            vec![
                ("Scenes|Lake".to_string(), Some(TagKind::Scene)),
                ("Objects|Dog".to_string(), Some(TagKind::Object)),
            ],
        );
        let e3 = Entry::with_tags(
            PathBuf::from("/lib/wedding.jpg"),
            "wedding.jpg".to_string(),
            MediaKind::Image,
            1500,
            modified,
            vec![("1999_wedding".to_string(), Some(TagKind::Keyword))],
        );

        // Filter by tag "beach"
        let q = Query::new("".into(), "beach".into(), 0, Sort::Name);
        assert!(q.accepts(&e1));
        assert!(!q.accepts(&e2));
        assert!(!q.accepts(&e3));

        // Filter by tag "dog"
        let q = Query::new("".into(), "dog".into(), 0, Sort::Name);
        assert!(!q.accepts(&e1));
        assert!(q.accepts(&e2));
        assert!(!q.accepts(&e3));

        // Boolean tag query "beach OR dog"
        let q = Query::new("".into(), "beach OR dog".into(), 0, Sort::Name);
        assert!(q.accepts(&e1));
        assert!(q.accepts(&e2));
        assert!(!q.accepts(&e3));

        // Negative tag query "-dog"
        let q = Query::new("".into(), "-dog".into(), 0, Sort::Name);
        assert!(q.accepts(&e1));
        assert!(!q.accepts(&e2));
        assert!(q.accepts(&e3));

        // Combined name and tag query
        let q = Query::new("beach".into(), "alice".into(), 0, Sort::Name);
        assert!(q.accepts(&e1));

        let q = Query::new("other".into(), "alice".into(), 0, Sort::Name);
        assert!(!q.accepts(&e1));

        // Folder tag "wedding"
        let q = Query::new("".into(), "wedding".into(), 0, Sort::Name);
        assert!(!q.accepts(&e1));
        assert!(!q.accepts(&e2));
        assert!(q.accepts(&e3));

        // Complex tag queries with AND and parentheses
        let q = Query::new("".into(), "(beach and alice)".into(), 0, Sort::Name);
        assert!(q.accepts(&e1));
        assert!(!q.accepts(&e2));

        let q = Query::new("".into(), "beach and alice".into(), 0, Sort::Name);
        assert!(q.accepts(&e1));
        assert!(!q.accepts(&e2));

        let q = Query::new("".into(), "(beach and dog)".into(), 0, Sort::Name);
        assert!(!q.accepts(&e1));
        assert!(!q.accepts(&e2));

        let q = Query::new("".into(), "(beach OR lake) and dog".into(), 0, Sort::Name);
        assert!(!q.accepts(&e1));
        assert!(q.accepts(&e2));

        // Complex query entered directly in the search bar
        let q = Query::new("(beach and alice)".into(), "".into(), 0, Sort::Name);
        assert!(q.accepts(&e1));
        assert!(!q.accepts(&e2));

        let q = Query::new("beach and not dog".into(), "".into(), 0, Sort::Name);
        assert!(q.accepts(&e1));
        assert!(!q.accepts(&e2));
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
                times.push(run(Query { text: text.into(), kind, sort: Sort::from_index(sort), ..Default::default() }));
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
