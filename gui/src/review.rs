//! Review mode: indexes media under a root directory and shows it as a virtualized grid.
//!
//! Why it stays fast with hundreds of thousands of files:
//! - The index is append-only: one shared chunk (`Arc<[Entry]>`) per indexer batch, with
//!   sort keys precomputed on the indexer thread. Filtering and sorting produce a
//!   `Vec<u32>` of entry ids. No per-file UI objects ever exist.
//! - The grid is rows backed by [`GridModel`], which slices the id list on demand (laid out
//!   by [`Layout`]: month headers when sorted by date). The page is only handed the rows in
//!   the viewport ([`Window`]), so scrolling is O(visible).
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
//! - Each entry carries its catalog flags (faces, back side, review, failure, the user's
//!   tags) for the tile badges and the "Needs attention" filters, read in one bulk query
//!   per indexer batch or relay burst (`Catalog::flags_of`), never per tile; a file whose
//!   flags change is replaced like a changed one.
//!
//! Selection: a set of entry ids (no per-tile state; `row_data` asks the set), with a focus
//! (the keyboard cursor, also the item clicked last) and an anchor for Shift ranges. Click,
//! Shift-click (range from the anchor), Ctrl-click (toggle), rubber-band drag (Shift / Ctrl
//! add to the selection), Ctrl+A, Esc; the arrow keys, Page Up / Down, Home / End move the
//! focus and select it (Shift extends). It follows an item that is replaced (a new entry for
//! a changed file); items that leave the grid, whether deleted or filtered out, drop out of
//! it, so an action on the selection only ever touches what is shown. Back snapshots keep
//! it by path. Right-clicking an item of a multi-selection gives the menu for all of them.
//!
//! Tags: the left pane's tag tree counts each tag on the live entries (with the tags under
//! it: `TagCounter`, kept up as entries come and go) and filters by a picked tag's subtree
//! (`=People|Ann`); nodes collapse. Its right-click menu renames, merges or deletes a tag
//! ([`crate::tags`]); [`tag_moved`] then makes a filter on that tag follow.
//!
//! Search by description: in "Description" mode the search text is ranked against the
//! images' CLIP embeddings on a worker ([`semantic`]), as is "Find similar" from the item
//! menu ([`find_similar`]). The ranking (path hash -> rank) becomes part of the [`Query`]:
//! only ranked entries pass, and the "Relevance" sort orders by rank, so the kind, tag and
//! attention filters combine with it and merges and refilters work as for any query. A
//! ranking is kept for the description it was made for: changing the other filters (or
//! coming Back) doesn't search again. Each search has a generation; a result for an older
//! one is dropped, as are refilter results.

use std::cell::{Cell, RefCell};
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{self, AtomicBool};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};
use std::{fs, thread};

use slint::{ComponentHandle, Model, ModelNotify, ModelRc, ModelTracker, SharedString, Timer, TimerMode, VecModel};

use crate::thumbs::{self, sched, Delivery, ThumbCache, Threads, Tier};
use archive_core::catalog::query::{self, Expr};
use archive_core::catalog::flags::AssetFlags;
use archive_core::catalog::tags::{TagCounter, TagNode};
use archive_core::catalog::{AssetId, Catalog, TagKind};
use archive_core::dates;
use archive_core::media::{classify, MediaKind};

use crate::actions;
use crate::nav::{self, Scope, Target};
use crate::pipeline::Handle;
use crate::semantic::{self, Found, Searcher, Want};
use crate::{AppWindow, Attention, MediaRow, MediaTile, ReviewBridge, ScrollRestore, TagItem};

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
    /// The date it is sorted and grouped by ([`dates::shown`]): when it was taken if the
    /// catalog knows, else when it was modified; wall-clock seconds.
    date: i64,
    tags: Arc<[(String, Option<TagKind>)]>,
    /// What the catalog says about it at a glance (faces, back side, review, failure, the
    /// user's tags), read in bulk off the UI thread; for the tile's badges and the
    /// "Needs attention" filters.
    flags: AssetFlags,
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
            date: dates::local(modified),
            tags: tags.into(),
            flags: AssetFlags::NONE,
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
    /// Newest first, by [`Entry::date`]; shown in a month's group each ([`Layout`]).
    Date,
    /// Largest first.
    Size,
    /// Kind, then name.
    Type,
    /// Best match first, for a search by description ([`Query::ranking`]); name otherwise.
    Relevance,
}

/// An entry with its sort key: `(Sort::key, id, entry)`.
type Keyed<'a> = (u128, u32, &'a Entry);

impl Sort {
    /// From the UI's sort combo index (0 name, 1 date, 2 size, 3 type, 4 relevance).
    fn from_index(index: i32) -> Self {
        match index {
            1 => Self::Date,
            2 => Self::Size,
            3 => Self::Type,
            4 => Self::Relevance,
            _ => Self::Name,
        }
    }

    /// Primary key, consistent with [`cmp`](Self::cmp): a smaller key always sorts first.
    /// `rank`: the entry's place in a search by description (`u32::MAX` if none).
    fn key(self, e: &Entry, rank: u32) -> u128 {
        match self {
            Self::Name => e.name_prefix,
            Self::Date => date_key(e.date),
            Self::Size => (!e.size).into(),
            Self::Type => ((e.kind as u128) << 126) | (e.name_prefix >> 2),
            Self::Relevance => ((rank as u128) << 96) | (e.name_prefix >> 32),
        }
    }

    /// Display order: key, then the full name where it matters, then id (oldest first).
    fn cmp(self, a: &Keyed, b: &Keyed) -> Ordering {
        a.0.cmp(&b.0)
            .then_with(|| match self {
                Self::Name | Self::Type | Self::Relevance => a.2.name_lc.cmp(&b.2.name_lc),
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
    /// The "Needs attention" filters: entries with all of `need`'s flags and none of
    /// `without`'s.
    need: AssetFlags,
    without: AssetFlags,
    sort: Sort,
    /// A search by description: [`path_hash`] -> rank (0 best). Only these entries pass.
    ranking: Option<Arc<HashMap<u64, u32>>>,
    /// What it looked for, for the Details scope ("“dog on a beach”", "like IMG_1.jpg").
    described: String,
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
            need: AssetFlags::NONE,
            without: AssetFlags::NONE,
            sort,
            ranking: None,
            described: String::new(),
        }
    }

    /// Narrowed to (and ranked by) a search by description.
    fn ranked(mut self, ranking: Option<Arc<HashMap<u64, u32>>>, described: String) -> Self {
        self.ranking = ranking;
        self.described = described;
        self
    }

    /// [`Sort::key`] of `e`.
    fn key(&self, e: &Entry) -> u128 {
        let rank = self.ranking.as_ref().and_then(|r| r.get(&e.path_hash).copied()).unwrap_or(u32::MAX);
        self.sort.key(e, rank)
    }

    /// Narrowed to what the "Needs attention" chips ask for.
    fn needing(mut self, attention: &Attention) -> Self {
        (self.need, self.without) = attention_flags(attention);
        self
    }

    fn accepts(&self, e: &Entry) -> bool {
        let kind_ok = match self.kind {
            1 => e.kind == MediaKind::Image,
            2 => e.kind == MediaKind::Video,
            3 => e.kind == MediaKind::Audio,
            _ => true,
        };
        if !kind_ok || !e.flags.matches(self.need, self.without) {
            return false;
        }
        if self.ranking.as_ref().is_some_and(|r| !r.contains_key(&e.path_hash)) {
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
        let mut keyed: Vec<Keyed> = entries.filter(|(_, e)| self.accepts(e)).map(|(id, e)| (self.key(e), id, e)).collect();
        keyed.sort_unstable_by(|a, b| sort.cmp(a, b));
        keyed
    }
}

/// The filter controls as the page set them: what [`GridModel::set_filters`] turns into a
/// [`Query`], after a search by description when one is asked for.
#[derive(Clone, Debug, Default, PartialEq)]
struct Filters {
    text: String,
    tag: String,
    kind: i32,
    sort: i32,
    attention: Attention,
    /// 0 name & tags, 1 description.
    mode: i32,
    /// "Find similar": the picture to look like, in description mode; else empty.
    similar: String,
}

impl Filters {
    /// The search by description this asks for, if any.
    fn want(&self) -> Option<Want> {
        if self.mode != 1 {
            return None;
        }
        if !self.similar.is_empty() {
            return Some(Want::Like(self.similar.clone().into()));
        }
        let text = self.text.trim();
        (!text.is_empty()).then(|| Want::Text(text.to_owned()))
    }

    /// The query, with the ranking of [`want`](Self::want) if there is one.
    fn query(&self, ranking: Option<Arc<HashMap<u64, u32>>>) -> Query {
        // In description mode the text describes; it isn't matched against names.
        let text = if self.mode == 1 { String::new() } else { self.text.clone() };
        let described = match self.want() {
            Some(Want::Text(text)) => format!("“{text}”"),
            Some(Want::Like(path)) => format!("like {}", file_name(&path)),
            None => String::new(),
        };
        Query::new(text, self.tag.clone(), self.kind, Sort::from_index(self.sort)).needing(&self.attention).ranked(ranking, described)
    }
}

fn file_name(path: &Path) -> String {
    path.file_name().map_or_else(|| path.display().to_string(), |n| n.to_string_lossy().into_owned())
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
/// Rows the page instantiates above and below those in view ([`Window`]), so a scroll by a
/// row or two shows rows that are there already.
const WINDOW_MARGIN: usize = 2;

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
    /// What `keys` are keys of.
    sort: Sort,
}

impl Filtered {
    fn compute(index: &Index, query: &Query, generation: u64) -> Self {
        let (keys, visible): (Vec<u128>, Vec<u32>) = query.filter_sort(index.live_from(0)).into_iter().map(|(key, id, _)| (key, id)).unzip();
        let mut slot_of = vec![HIDDEN; index.len()];
        for (slot, &id) in visible.iter().enumerate() {
            slot_of[id as usize] = slot as u32;
        }
        Self { generation, upto: index.len(), gone: index.gone.len(), visible, keys, slot_of, sort: query.sort }
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
            log::debug!("review filter thread started");
            while let Ok(mut request) = rx.recv() {
                while let Ok(newer) = rx.try_recv() {
                    log::debug!("refilter #{} skipped: #{} queued behind it", request.0, newer.0);
                    request = newer;
                }
                let (generation, index, query) = request;
                let started = Instant::now();
                let result = Filtered::compute(&index, &query, generation);
                log::debug!("refilter #{generation} on the filter thread: {} shown in {:?}", result.visible.len(), started.elapsed());
                deliver(result);
            }
            log::debug!("review filter thread stopped (channel closed)");
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

/// [`ModelNotify`], recording what it sends in tests; it passes what concerns the rows the
/// page has instantiated on to them ([`Window`]).
#[derive(Default)]
struct Notify {
    inner: ModelNotify,
    window: Window,
    #[cfg(test)]
    log: RefCell<Vec<Change>>,
}

/// The rows the page instantiates: those in view and [`WINDOW_MARGIN`] either side. The page
/// places them itself (a header row is lower than a row of tiles, and `ListView` can only
/// place rows of one height exactly), so the grid's rows reach it through this window
/// ([`WindowRows`]), which follows the viewport ([`GridModel::sync_window`]).
#[derive(Default)]
struct Window {
    /// The grid's rows `start..end` are the window's rows `0..`.
    range: Cell<(usize, usize)>,
    notify: ModelNotify,
    #[cfg(test)]
    log: RefCell<Vec<Change>>,
}

impl Window {
    fn row_changed(&self, row: usize) {
        #[cfg(test)]
        self.log.borrow_mut().push(Change::Changed(row));
        self.notify.row_changed(row);
    }

    fn row_added(&self, index: usize, count: usize) {
        #[cfg(test)]
        self.log.borrow_mut().push(Change::Added(index, count));
        self.notify.row_added(index, count);
    }

    fn row_removed(&self, index: usize, count: usize) {
        #[cfg(test)]
        self.log.borrow_mut().push(Change::Removed(index, count));
        self.notify.row_removed(index, count);
    }

    fn reset(&self) {
        #[cfg(test)]
        self.log.borrow_mut().push(Change::Reset);
        self.notify.reset();
    }
}

/// The page's view of the grid: the rows of its [`Window`].
struct WindowRows(Rc<GridModel>);

impl Model for WindowRows {
    type Data = MediaRow;

    fn row_count(&self) -> usize {
        let (start, end) = self.0.notify.window.range.get();
        end - start
    }

    fn row_data(&self, row: usize) -> Option<MediaRow> {
        let (start, end) = self.0.notify.window.range.get();
        (start + row < end).then(|| self.0.row_data(start + row)).flatten()
    }

    fn model_tracker(&self) -> &dyn ModelTracker {
        &self.0.notify.window.notify
    }
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
        let (start, end) = self.window.range.get();
        if (start..end).contains(&row) {
            self.window.row_changed(row - start);
        }
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
    /// What `keys` are keys of: the query's sort once its refilter landed.
    shown_sort: Cell<Sort>,
    /// `visible` in rows, with date headers when sorted by date; rebuilt ([`relayout`](Self::relayout))
    /// whenever `visible` or the column count changes.
    layout: RefCell<Layout>,
    columns: Cell<usize>,
    tier: Cell<Tier>,
    /// First and last (inclusive) row in view.
    viewport: Cell<(usize, usize)>,
    scrolling_down: Cell<bool>,
    settle: Timer,
    query: RefCell<Query>,
    /// The filter controls the query was (or is being) made from.
    filters: RefCell<Filters>,
    /// Searches by description, if this grid has a worker for them (not in tests).
    searcher: RefCell<Option<Searcher>>,
    /// Of the latest search by description; older results are stale.
    search_generation: Cell<u64>,
    /// A search by description is in flight: the grid still shows the previous result.
    searching: Cell<bool>,
    /// The last search by description's result, kept while that is what is asked for.
    ranking: RefCell<Option<(Want, Arc<HashMap<u64, u32>>)>>,
    /// Of the latest refilter; older results are stale.
    generation: Cell<u64>,
    /// A refilter is running on the filter thread; `visible` still shows the old query.
    pending: Cell<bool>,
    filter: FilterWorker,
    sync_limit: usize,
    /// Selected entry ids; only shown ones (see the module docs).
    selection: RefCell<HashSet<u32>>,
    /// The keyboard cursor: the entry clicked or moved to last.
    focus: Cell<Option<u32>>,
    /// Where a Shift range starts.
    anchor: Cell<Option<u32>>,
    /// Entries the rubber band added to the selection so far (not those selected before).
    band: RefCell<Vec<u32>>,
    indexing: Cell<bool>,
    /// Entry id of each path ([`path_hash`]), for [`update_files`](Self::update_files).
    by_path: RefCell<HashMap<u64, u32>>,
    /// File updates that arrived while indexing, applied once it is done (the indexer
    /// may find the same files).
    deferred: RefCell<Vec<FileUpdate>>,
    thumbs: ThumbCache,
    ready: Arc<Mailbox<Ready>>,
    /// How many live entries carry each tag or one under it: the tag tree.
    tag_counts: RefCell<TagCounter>,
    /// Tag tree nodes collapsed, by lowercase path.
    collapsed: RefCell<HashSet<String>>,
    /// The tag tree's lines as shown; updated in place.
    tag_model: Rc<VecModel<TagItem>>,
    /// A scroll position to return to (Back) once the refilter in flight lands: the first
    /// tile that was in view, how far its row was scrolled past, and a tile to keep in view
    /// (the item last shown on Details).
    restore: RefCell<Option<ScrollBack>>,
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

/// The grid's thumbnail of `path` if it has one in memory (the large one if it can), for
/// Details to show at once while it decodes the picture.
pub fn thumbnail(path: &Path) -> Option<slint::Image> {
    GRID.with_borrow(|grid| {
        let grid = grid.as_ref()?;
        let id = grid.id_of(path)?;
        grid.thumbs.get(id, Tier::Large).map(|(image, _exact)| image)
    })
}

pub fn install(ui: &AppWindow, root: PathBuf, pipeline: Handle) {
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
    let weak = ui.as_weak();
    *grid.searcher.borrow_mut() = Some(Searcher::spawn(root.clone(), pipeline, move |found| {
        let _ = weak.upgrade_in_event_loop(move |_| {
            with_grid(|g| {
                g.searched(found);
            })
        });
    }));
    GRID.set(Some(grid.clone()));

    let bridge = ui.global::<ReviewBridge>();
    bridge.set_rows(ModelRc::new(WindowRows(grid.clone())));
    bridge.set_root_path(root.display().to_string().into());
    bridge.on_columns_changed(|n| with_grid(|g| g.set_columns(n.max(1) as usize)));
    bridge.on_viewport_changed(|first, last| with_grid(|g| g.set_viewport(first, last)));
    // Geometry for the page, which places the rows itself (`Layout`); `_serial` is the
    // layout's, so bindings calling these re-read when it changes.
    bridge.on_row_y(|_serial, row, row_h, header_h| {
        GRID.with_borrow(|grid| grid.as_ref().map_or(0.0, |g| g.layout().row_y(row.max(0) as usize, row_h, header_h)))
    });
    bridge.on_row_at_y(|_serial, y, row_h, header_h| {
        GRID.with_borrow(|grid| grid.as_ref().map_or(-1, |g| g.layout().row_at_y(y, row_h, header_h).clamp(-1, i32::MAX as i64) as i32))
    });
    bridge.on_is_header(|_serial, row| GRID.with_borrow(|grid| grid.as_ref().is_some_and(|g| g.header_at(row).is_some())));
    bridge.on_click(|row, col, extend, toggle| {
        with_grid(|g| {
            if let Some(group) = g.header_at(row) {
                return g.select_group(group, toggle || extend);
            }
            let id = g.id_at(row, col);
            log::debug!("review click at row {row} col {col} -> entry {id:?} (shift {extend}, ctrl {toggle})");
            g.click(id, extend, toggle)
        })
    });
    bridge.on_drag_begin(|keep| {
        log::debug!("review drag begins (keep selection {keep})");
        with_grid(|g| g.drag_begin(keep))
    });
    bridge.on_drag_to(|r0, r1, c0, c1| {
        log::trace!("review drag rows {r0}..={r1} cols {c0}..={c1}");
        with_grid(|g| g.drag_to(r0, r1, c0, c1))
    });
    bridge.on_clear_selection(|| {
        log::debug!("review: clear selection");
        with_grid(|g| g.set_selection(HashSet::new()))
    });
    bridge.on_key(|key, shift, ctrl| {
        GRID.with_borrow(|grid| grid.as_ref().is_some_and(|g| g.key(&key.to_lowercase(), shift, ctrl)))
    });
    bridge.on_open(|row, col| {
        with_grid(|g| match g.id_at(row, col) {
            Some(id) => {
                log::debug!("review double-click at row {row} col {col}: entry {id} -> open");
                g.open(id)
            }
            None => log::debug!("review double-click at row {row} col {col}: no tile there - ignored"),
        })
    });
    bridge.on_context_menu(|row, col| {
        with_grid(|g| {
            let Some((ui, id)) = g.ui.upgrade().zip(g.id_at(row, col)) else {
                log::debug!("review right-click at row {row} col {col}: no tile there - no menu");
                return;
            };
            // Outside the selection: the menu is for that item, which is selected instead.
            if !g.selection.borrow().contains(&id) {
                g.select_only(id);
            }
            g.focus_on(id);
            let items = if g.selection.borrow().len() > 1 { g.selected_paths() } else { Vec::new() };
            if let Some(target) = g.target(id) {
                nav::context_menu_for(&ui, target, items);
            }
        })
    });

    let debounce = Timer::default();
    bridge.on_filter_changed(move |text, tag, kind, sort, attention, mode, similar| {
        log::debug!(
            "review filter changed: search {text:?} ({}), tags {tag:?}, kind {kind}, sort {sort}, needs attention [{}]{}",
            if mode == 1 { "description" } else { "name & tags" },
            attention_label(&attention),
            if similar.is_empty() { String::new() } else { format!(", like {similar:?}") }
        );
        let filters = Filters { text: text.into(), tag: tag.into(), kind, sort, attention, mode, similar: similar.into() };
        // A description is searched with a model: wait a little longer for the typing to stop.
        let wait = if mode == 1 { 350 } else { 120 };
        debounce.start(TimerMode::SingleShot, Duration::from_millis(wait), move || {
            with_grid(|g| g.set_filters(filters.clone()))
        });
    });

    bridge.set_available_tags(ModelRc::from(grid.tag_model.clone()));
    bridge.on_toggle_tag(|path| with_grid(|g| g.toggle_tag(&path)));

    grid.publish_status();
    grid.publish_tags();
    spawn_indexer(root, ui.as_weak());
}

/// The tag tree's collapsed nodes (lowercase paths), for the settings kept across runs.
pub fn collapsed_tags() -> Vec<String> {
    GRID.with_borrow(|grid| {
        let mut paths: Vec<String> = grid.as_ref().map(|g| g.collapsed.borrow().iter().cloned().collect()).unwrap_or_default();
        paths.sort_unstable();
        paths
    })
}

/// Sets up the view as it was left last time (`settings.rs`, at startup): the tag tree's
/// collapsed nodes, and the filters the bridge was given (media kind, sort, search mode).
pub fn restore_settings(ui: &AppWindow, collapsed: Vec<String>) {
    let bridge = ui.global::<ReviewBridge>();
    let filters = Filters {
        kind: bridge.get_kind_filter(),
        sort: bridge.get_sort_index(),
        mode: bridge.get_search_mode(),
        attention: bridge.get_attention(),
        ..Default::default()
    };
    log::debug!("review: restored kind {}, sort {}, search mode {}, {} tags collapsed", filters.kind, filters.sort, filters.mode, collapsed.len());
    with_grid(|g| {
        *g.collapsed.borrow_mut() = collapsed.into_iter().map(|p| p.to_ascii_lowercase()).collect();
        g.publish_tags();
        if filters != Filters::default() {
            g.set_filters(filters);
        }
    });
}

/// Shows Review with only the items that need `attention` (the status line's "3 rotations to
/// review", "2 failed"): search, tags and media type are cleared, the sort is kept. Coming
/// from another page, Back returns there.
pub fn show_attention(ui: &AppWindow, attention: Attention) {
    log::debug!("review: show what needs attention [{}]", attention_label(&attention));
    nav::go(ui, nav::REVIEW);
    let bridge = ui.global::<ReviewBridge>();
    bridge.set_search_text(SharedString::default());
    bridge.set_tag_text(SharedString::default());
    bridge.set_selected_tag(SharedString::default());
    bridge.set_kind_filter(0);
    bridge.set_attention(attention.clone());
    bridge.set_search_mode(0);
    bridge.set_similar_to(SharedString::default());
    if Sort::from_index(bridge.get_sort_index()) == Sort::Relevance {
        bridge.set_sort_index(0);
    }
    let filters = Filters { sort: bridge.get_sort_index(), attention, ..Default::default() };
    with_grid(|g| g.set_filters(filters));
}

/// "Find similar" from the item menu: shows Review with the pictures that look like the one
/// at `path` (by their CLIP embeddings), best first and it first, in description mode; the
/// other filters are cleared. Back returns to the view it was asked from, Review included.
pub fn find_similar(ui: &AppWindow, path: &Path) {
    log::debug!("review: find pictures like {}", path.display());
    if ui.get_page() == nav::REVIEW {
        nav::remember(ui);
    } else {
        nav::go(ui, nav::REVIEW);
    }
    let bridge = ui.global::<ReviewBridge>();
    bridge.set_search_text(SharedString::default());
    bridge.set_tag_text(SharedString::default());
    bridge.set_selected_tag(SharedString::default());
    bridge.set_kind_filter(0);
    bridge.set_attention(Attention::default());
    bridge.set_search_mode(1);
    bridge.set_similar_to(path.display().to_string().into());
    bridge.set_similar_name(file_name(path).into());
    bridge.set_sort_index(4);
    let filters = Filters { sort: 4, mode: 1, similar: path.display().to_string(), ..Default::default() };
    with_grid(|g| g.set_filters(filters));
}

/// Where [`GridModel::apply_restore`] scrolls: the first tile that was in view (`true`: the
/// date header over it was the first row in view), how far that row was scrolled past, and
/// a tile to keep in view.
type ScrollBack = (Option<PathBuf>, bool, f32, Option<PathBuf>);

/// The Review view as it was left, for Back (see `nav`): the filter controls, the selection
/// and the scroll position.
pub(crate) struct Snapshot {
    search: SharedString,
    tag: SharedString,
    selected_tag: SharedString,
    kind: i32,
    sort: i32,
    attention: Attention,
    /// Search mode (0 name & tags, 1 description) and "Find similar"'s picture.
    mode: i32,
    similar: SharedString,
    /// Entries are kept by path ([`path_hash`]): one replaced meanwhile (e.g. rotated) has
    /// a new id.
    selected: Vec<u64>,
    /// The keyboard cursor.
    focus: Option<PathBuf>,
    /// The first tile in view, and how far its row was scrolled past: restored by tile
    /// rather than pixels, so it holds if the column count changed meanwhile. `on_header`:
    /// the row in view was the date header over it.
    anchor: Option<PathBuf>,
    on_header: bool,
    offset: f32,
    /// The selection (and focus) was moved to the item last shown on Details ([`follow`](Self::follow)):
    /// scroll to it if it is out of view.
    followed: bool,
}

impl Snapshot {
    /// Selects `path` instead, the item last shown on Details (Left / Right there step
    /// through the view's items without adding to the history). Left as it was if that is
    /// the item opened (a multi-selection stays).
    pub(crate) fn follow(&mut self, path: PathBuf) {
        if self.focus.as_ref() != Some(&path) {
            self.selected = vec![path_hash(&path)];
            self.focus = Some(path);
            self.followed = true;
        }
    }
}

impl std::fmt::Debug for Snapshot {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(
            f,
            "Review {{ search {:?}{}{}, tags {:?} (picked {:?}), kind {}, sort {}, needs attention [{}], {} selected, focus {:?}{}, scroll anchor {:?} +{:.0}px }}",
            self.search.as_str(),
            if self.mode == 1 { " (description)" } else { "" },
            if self.similar.is_empty() { String::new() } else { format!(" like {:?}", self.similar.as_str()) },
            self.tag.as_str(),
            self.selected_tag.as_str(),
            self.kind,
            self.sort,
            attention_label(&self.attention),
            self.selected.len(),
            self.focus,
            if self.followed { " (followed from Details)" } else { "" },
            self.anchor,
            self.offset
        )
    }
}

pub(crate) fn snapshot() -> Option<Snapshot> {
    GRID.with_borrow(|grid| {
        let g = grid.as_ref()?;
        let ui = g.ui.upgrade()?;
        let bridge = ui.global::<ReviewBridge>();
        let path_of = |id: u32| g.entries.borrow().get(id).map(|e| e.path.clone());
        let row = bridge.get_scroll_row().max(0) as usize;
        let (first, on_header) = {
            let layout = g.layout();
            (layout.first_slot_from(row), matches!(layout.at(row), Some(RowAt::Header(_))))
        };
        let anchor = first.and_then(|first| g.visible.borrow().get(first).copied()).and_then(path_of);
        Some(Snapshot {
            search: bridge.get_search_text(),
            tag: bridge.get_tag_text(),
            selected_tag: bridge.get_selected_tag(),
            kind: bridge.get_kind_filter(),
            sort: bridge.get_sort_index(),
            attention: bridge.get_attention(),
            mode: bridge.get_search_mode(),
            similar: bridge.get_similar_to(),
            selected: g.selection.borrow().iter().filter_map(|&id| g.entries.borrow().get(id).map(|e| e.path_hash)).collect(),
            focus: g.focus.get().and_then(path_of),
            anchor,
            on_header,
            offset: bridge.get_scroll_offset(),
            followed: false,
        })
    })
}

pub(crate) fn restore(snapshot: Snapshot) {
    with_grid(|g| g.restore(snapshot));
}

/// Stops thumbnail work at app exit, letting queued cache writes finish briefly.
pub fn shutdown() {
    with_grid(|g| g.thumbs.shutdown());
}

/// Walks `root` on a background thread, streaming batches to the UI every ~150 ms. After
/// that, the pipeline's reports keep the grid current ([`update_files`]).
/// TODO: build the grid from the catalog instead of rescanning on every launch.
fn spawn_indexer(root: PathBuf, ui: slint::Weak<AppWindow>) {
    let _ = thread::Builder::new().name("review-indexer".into()).spawn(move || {
        let started = Instant::now();
        log::debug!("indexer: walking {}", root.display());
        let mut found = 0;
        walk(root, |batch, done| {
            found += batch.len();
            if done {
                log::info!("indexer: {found} media files in {:?}", started.elapsed());
            }
            if let Err(e) = ui.upgrade_in_event_loop(move |_| with_grid(|g| g.append(batch, done))) {
                log::debug!("indexer: batch not delivered: {e}");
            }
        })
    });
}

fn extract_file_tags(path: &Path, root: &Path, cat: Option<&Catalog>, asset: Option<AssetId>) -> Vec<(String, Option<TagKind>)> {
    let mut tags: Vec<(String, Option<TagKind>)> = Vec::new();

    // 1. Tags from catalog
    if let Some(applied) = cat.zip(asset).and_then(|(cat, asset)| cat.tags_of(asset).ok()) {
        for t in applied {
            if !tags.iter().any(|(p, _)| p.eq_ignore_ascii_case(&t.path)) {
                tags.push((t.path, Some(t.kind)));
            }
        }
    }

    // 2. Tags from XMP sidecar, but those the catalog wrote there itself: they may be stale
    // (renamed, taken off) until the pipeline syncs the sidecar again.
    let ours = cat.zip(asset).and_then(|(cat, asset)| cat.sidecar_tags(asset).ok()).unwrap_or_default();
    let sidecar = archive_core::catalog::xmp::sidecar_path(path);
    if sidecar.is_file() {
        if let Ok(content) = fs::read_to_string(&sidecar) {
            if let Ok(xmp_tags) = archive_core::catalog::xmp::extract_tags(&content) {
                for (path, kind) in xmp_tags {
                    if !tags.iter().any(|(p, _)| p.eq_ignore_ascii_case(&path)) && !ours.iter().any(|p| p.eq_ignore_ascii_case(&path)) {
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

/// The entry for the file at `path`, and its catalog asset if it has one. Its flags are left
/// for [`read_flags`], which reads a batch's at once.
fn entry_for(path: PathBuf, kind: MediaKind, meta: Option<fs::Metadata>, root: &Path, catalog: Option<&Catalog>) -> (Entry, Option<AssetId>) {
    let name = path.file_name().map_or_else(String::new, |n| n.to_string_lossy().into_owned());
    let size = meta.as_ref().map_or(0, |m| m.len());
    let modified = meta.and_then(|m| m.modified().ok()).unwrap_or(SystemTime::UNIX_EPOCH);
    let asset = catalog.and_then(|c| c.find_asset(&path).ok().flatten());
    let tags = extract_file_tags(&path, root, catalog, asset);
    (Entry::with_tags(path, name, kind, size, modified, tags), asset)
}

/// Fills in the flags of `entries` (whose catalog assets are `assets`, in the same order),
/// and the date they were taken where the catalog knows it, with one bulk read each for the
/// lot (`Catalog::flags_of`, `taken_of`). Blocking: indexer and relay threads.
fn read_flags<'a>(entries: impl Iterator<Item = &'a mut Entry>, assets: &[Option<AssetId>], catalog: Option<&Catalog>) {
    let Some(catalog) = catalog else { return };
    let ids: Vec<AssetId> = assets.iter().flatten().copied().collect();
    let read = catalog.flags_of(&ids).and_then(|flags| Ok((flags, catalog.taken_of(&ids)?)));
    let (flags, taken) = match read {
        Ok(read) => read,
        Err(e) => {
            log::warn!("review: reading the flags of {} files: {e}", ids.len());
            return;
        }
    };
    for (entry, asset) in entries.zip(assets) {
        entry.flags = asset.and_then(|a| flags.get(&a).copied()).unwrap_or_default();
        if let Some(&when) = asset.as_ref().and_then(|a| taken.get(a)) {
            entry.date = when;
        }
    }
}

/// A file the pipeline (or a capture) reported, as the grid should now show it.
pub(crate) enum FileUpdate {
    /// The file as it is now. `changed`: its pixels changed (e.g. rotated) even if its size,
    /// date and tags look the same.
    Present(Entry, bool),
    Gone(PathBuf),
}

impl FileUpdate {
    /// Reads the file at `path` (blocking: stat, catalog tags and flags, sidecar), or `None`
    /// if it isn't media the grid shows. For a worker thread; hand the result to
    /// [`update_files`].
    #[cfg(test)]
    pub(crate) fn read(path: &Path, root: &Path, catalog: Option<&Catalog>, changed: bool) -> Option<Self> {
        Self::read_all([(path.to_owned(), changed)], root, catalog).pop()
    }

    /// [`read`](Self::read) for each of `files` (path, whether its pixels changed), the
    /// flags of all of them read at once.
    pub(crate) fn read_all(files: impl IntoIterator<Item = (PathBuf, bool)>, root: &Path, catalog: Option<&Catalog>) -> Vec<Self> {
        let mut updates = Vec::new();
        let mut assets = Vec::new();
        for (path, changed) in files {
            let Some(kind) = classify(&path) else { continue };
            let Ok(rel) = path.strip_prefix(root) else { continue };
            if rel.components().any(|c| c.as_os_str().to_string_lossy().starts_with('.')) {
                continue;
            }
            match fs::metadata(&path) {
                Ok(meta) if meta.is_file() => {
                    let (entry, asset) = entry_for(path, kind, Some(meta), root, catalog);
                    updates.push(Self::Present(entry, changed));
                    assets.push(asset);
                }
                _ => updates.push(Self::Gone(path)),
            }
        }
        let present = updates.iter_mut().filter_map(|u| if let Self::Present(entry, _) = u { Some(entry) } else { None });
        read_flags(present, &assets, catalog);
        updates
    }

    pub(crate) fn path(&self) -> &Path {
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
    let (mut batch, mut assets) = (Vec::new(), Vec::new());
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
            let (entry, asset) = entry_for(path, kind, item.metadata().ok(), &root, catalog.as_ref());
            batch.push(entry);
            assets.push(asset);
            if last_flush.elapsed() > Duration::from_millis(150) {
                read_flags(batch.iter_mut(), &std::mem::take(&mut assets), catalog.as_ref());
                flush(std::mem::take(&mut batch).into(), false);
                last_flush = Instant::now();
            }
        }
    }
    read_flags(batch.iter_mut(), &assets, catalog.as_ref());
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
            shown_sort: Cell::new(Sort::default()),
            layout: RefCell::default(),
            columns: Cell::new(1),
            tier: Cell::new(Tier::Small),
            viewport: Cell::new((0, 0)),
            scrolling_down: Cell::new(true),
            settle: Timer::default(),
            query: RefCell::default(),
            filters: RefCell::default(),
            searcher: RefCell::default(),
            search_generation: Cell::new(0),
            searching: Cell::new(false),
            ranking: RefCell::default(),
            generation: Cell::new(0),
            pending: Cell::new(false),
            filter,
            sync_limit,
            selection: RefCell::default(),
            focus: Cell::new(None),
            anchor: Cell::new(None),
            band: RefCell::default(),
            indexing: Cell::new(true),
            by_path: RefCell::default(),
            deferred: RefCell::default(),
            thumbs,
            ready,
            tag_counts: RefCell::default(),
            collapsed: RefCell::default(),
            tag_model: Rc::default(),
            restore: RefCell::default(),
            #[cfg(test)]
            redrain_due: Cell::new(false),
            notify: Notify::default(),
        }
    }

    /// A batch from the indexer: merged into the shown order, notifying only the rows that
    /// changed (from the first insertion on) and the rows added at the end. `row_changed`
    /// re-reads a row only if the page has it instantiated (on screen) and is a no-op
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
            let mut counts = self.tag_counts.borrow_mut();
            for entry in batch.iter().filter(|e| !e.tags.is_empty()) {
                counts.add(entry.tags.iter().map(|(t, _)| t.as_str()));
                tags_changed = true;
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
            let old = self.layout().clone();
            if let Some(first) = self.merge_from(from) {
                self.relayout();
                let first_row = self.rows_changed(&old, first);
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
        // A tagged file was replaced or left: its tags no longer count (a replacement's are
        // counted as it is appended).
        let mut tags_changed = false;
        {
            let (index, by_path) = (self.entries.borrow(), self.by_path.borrow());
            let mut counts = self.tag_counts.borrow_mut();
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
                            if !changed && (old.size, old.mtime, old.date, &old.tags, old.flags) == (entry.size, entry.mtime, entry.date, &entry.tags, entry.flags) {
                                continue;
                            }
                            if !old.tags.is_empty() {
                                counts.remove(old.tags.iter().map(|(t, _)| t.as_str()));
                                tags_changed = true;
                            }
                            retired.push(id);
                            replaced.push((id, fresh.len()));
                        }
                        fresh.push(entry);
                    }
                    FileUpdate::Gone(_) => {
                        if let Some(old) = current.and_then(|id| index.get(id)).filter(|e| !e.tags.is_empty()) {
                            counts.remove(old.tags.iter().map(|(t, _)| t.as_str()));
                            tags_changed = true;
                        }
                        retired.extend(current)
                    }
                }
            }
        }
        // The selection, focus and anchor follow a replaced file to its new entry.
        let from = self.entries.borrow().len() as u32;
        {
            let mut selection = self.selection.borrow_mut();
            for &(old, i) in &replaced {
                let new = from + i as u32;
                if selection.remove(&old) {
                    selection.insert(new);
                }
                for cell in [&self.focus, &self.anchor] {
                    if cell.get() == Some(old) {
                        cell.set(Some(new));
                    }
                }
            }
        }
        self.retire(&retired);
        if !fresh.is_empty() {
            self.append(fresh.into(), true);
        }
        if !self.pending.get() {
            // A replaced file the filter no longer shows leaves the selection.
            self.prune_selection();
        }
        if tags_changed {
            self.publish_tags();
        }
        self.publish_status();
        self.publish_selection();
    }

    /// Retires entries: they leave the grid (the rows from the first one on are re-read) and
    /// their thumbnails are dropped.
    fn retire(&self, ids: &[u32]) {
        if ids.is_empty() {
            return;
        }
        Arc::make_mut(&mut self.entries.borrow_mut().gone).extend(ids);
        {
            let mut selection = self.selection.borrow_mut();
            for &id in ids {
                self.thumbs.invalidate(id);
                selection.remove(&id);
            }
        }
        if self.anchor.get().is_some_and(|id| ids.contains(&id)) {
            self.anchor.set(None);
        }
        // The focus moves on to the next item that stays, else the one before (deleting
        // with the keyboard).
        let next = self.focus.get().filter(|id| ids.contains(id)).map(|id| {
            self.focus.set(None);
            let (going, visible) = (ids.iter().collect::<HashSet<_>>(), self.visible.borrow());
            let at = self.slot(id)?;
            visible[at..].iter().chain(visible[..at].iter().rev()).find(|id| !going.contains(id)).copied()
        });
        if self.pending.get() {
            // The refilter in flight leaves them out when it lands.
            return;
        }
        let old = self.layout().clone();
        let Some(first) = self.hide(ids.iter().copied()) else { return };
        self.relayout();
        if let Some(Some(next)) = next {
            self.focus.set(Some(next));
            self.anchor.set(Some(next));
            if self.selection.borrow().is_empty() {
                self.selection.borrow_mut().insert(next);
            }
        }
        self.rows_changed(&old, first);
        self.reschedule();
    }

    /// The shown order changed from `slot` on (it was laid out as `old`): the view re-reads
    /// the rows from the first that may differ, and is told of rows added or removed at the
    /// end. Returns that first row.
    fn rows_changed(&self, old: &Layout, slot: usize) -> usize {
        let (old_rows, new_rows) = (old.row_count(), self.row_count());
        let first_row = old.changed_from(slot).min(self.layout().changed_from(slot));
        for row in first_row..old_rows.min(new_rows) {
            self.notify.row_changed(row);
        }
        if new_rows > old_rows {
            self.notify.row_added(old_rows, new_rows - old_rows);
        } else if new_rows < old_rows {
            self.notify.row_removed(new_rows, old_rows - new_rows);
        }
        self.sync_window(false);
        first_row
    }

    /// Lays `visible` out in rows anew (after it changed, or the column count did), and tells
    /// the page if that changed its geometry.
    fn relayout(&self) {
        let layout = Layout::new(&self.keys.borrow(), self.columns.get(), self.shown_sort.get() == Sort::Date);
        if *self.layout.borrow() == layout {
            return;
        }
        let rows = layout.row_count();
        *self.layout.borrow_mut() = layout;
        if let Some(ui) = self.ui.upgrade() {
            let bridge = ui.global::<ReviewBridge>();
            bridge.set_row_total(rows as i32);
            bridge.set_layout_serial(bridge.get_layout_serial().wrapping_add(1));
        }
    }

    /// The rows as laid out now. (Tests may change the column count without a relayout.)
    fn layout(&self) -> std::cell::Ref<'_, Layout> {
        if self.layout.borrow().columns != self.columns.get() {
            *self.layout.borrow_mut() = Layout::new(&self.keys.borrow(), self.columns.get(), self.shown_sort.get() == Sort::Date);
        }
        self.layout.borrow()
    }

    /// Moves the page's [`Window`] to the rows in view (`reset`: its rows all changed).
    fn sync_window(&self, reset: bool) {
        let (first, last) = self.viewport.get();
        let rows = self.row_count();
        let (a1, b1) = (first.saturating_sub(WINDOW_MARGIN).min(rows), (last + 1 + WINDOW_MARGIN).min(rows));
        let window = &self.notify.window;
        let (a0, b0) = window.range.get();
        if reset || b0 <= a1 || b1 <= a0 {
            window.range.set((a1, b1));
            window.reset();
            return;
        }
        // Overlapping: rows come and go at the edges, those in both stay instantiated.
        if a1 != a0 {
            window.range.set((a1, b0));
            if a1 > a0 {
                window.row_removed(0, a1 - a0);
            } else {
                window.row_added(0, a0 - a1);
            }
        }
        if b1 != b0 {
            window.range.set((a1, b1));
            if b1 > b0 {
                window.row_added(b0 - a1, b1 - b0);
            } else {
                window.row_removed(b1 - a1, b0 - b1);
            }
        }
    }

    /// The view's rows all changed (a new filter, column count or tile size).
    fn reset_view(&self) {
        self.notify.reset();
        self.sync_window(true);
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

    /// The filter controls changed (or were restored): re-filters, after searching by
    /// description if that asks for a search other than the last one (the grid shows what it
    /// showed until the result is in).
    fn set_filters(&self, filters: Filters) {
        *self.filters.borrow_mut() = filters.clone();
        let Some(want) = filters.want() else {
            // A search in flight is no longer wanted.
            self.search_generation.set(self.search_generation.get() + 1);
            self.searching.set(false);
            self.publish_note(String::new());
            self.set_query(filters.query(None));
            return;
        };
        let cached = self.ranking.borrow().as_ref().filter(|(had, _)| *had == want).map(|(_, ranking)| ranking.clone());
        if let Some(ranking) = cached {
            log::debug!("review: {want:?} ranked already ({} items)", ranking.len());
            self.search_generation.set(self.search_generation.get() + 1);
            self.searching.set(false);
            self.set_query(filters.query(Some(ranking)));
            return;
        }
        let generation = self.search_generation.get() + 1;
        self.search_generation.set(generation);
        log::debug!("review: description search #{generation} for {want:?}");
        self.searching.set(true);
        self.publish_note("Searching…".into());
        if let Some(searcher) = self.searcher.borrow().as_ref() {
            searcher.ask(generation, want);
        }
    }

    /// A search by description is done: shown (with the other filters) if it is still the
    /// latest one asked for; returns whether it was.
    fn searched(&self, found: Found) -> bool {
        if found.generation != self.search_generation.get() {
            log::debug!("description search #{} result dropped: #{} is newer", found.generation, self.search_generation.get());
            return false;
        }
        self.searching.set(false);
        self.publish_note(semantic::note(&found.result));
        let paths = found.result.map(|ranked| ranked.paths).unwrap_or_default();
        let mut ranking = HashMap::with_capacity(paths.len());
        for (rank, path) in paths.iter().enumerate() {
            ranking.entry(path_hash(path)).or_insert(rank as u32);
        }
        let ranking = Arc::new(ranking);
        *self.ranking.borrow_mut() = Some((found.want, ranking.clone()));
        let query = self.filters.borrow().query(Some(ranking));
        self.set_query(query);
        true
    }

    /// The line under the search box (searching, how much is described, why it can't).
    fn publish_note(&self, note: String) {
        if let Some(ui) = self.ui.upgrade() {
            ui.global::<ReviewBridge>().set_describe_note(note.into());
        }
    }

    /// Re-filters everything: synchronously for small libraries, else on the filter thread
    /// (the grid keeps showing the previous result until it lands).
    fn refilter(&self) {
        let generation = self.generation.get() + 1;
        self.generation.set(generation);
        let query = self.query.borrow().clone();
        log::debug!("refilter #{generation} of {} entries ({})", self.entries.borrow().len(), if self.entries.borrow().len() < self.sync_limit { "here" } else { "on the filter thread" });
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
            log::debug!("refilter #{} result dropped: #{} is newer", result.generation, self.generation.get());
            return false;
        }
        log::debug!("refilter #{} applied: {} shown (model reset)", result.generation, result.visible.len());
        self.pending.set(false);
        let behind = self.entries.borrow().len() - result.upto;
        *self.visible.borrow_mut() = result.visible;
        *self.keys.borrow_mut() = result.keys;
        *self.slot_of.borrow_mut() = result.slot_of;
        self.shown_sort.set(result.sort);
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
        // Items the new filter hides leave the selection.
        self.prune_selection();
        self.relayout();
        self.reset_view();
        self.reschedule();
        self.publish_status();
        self.publish_selection();
        self.apply_restore();
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
            self.relayout();
            self.reset_view();
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
        self.sync_window(false);
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
        let tier = self.tier.get();
        let (visible, entries, layout) = (self.visible.borrow(), self.entries.borrow(), self.layout());
        let ids = rows.iter().flat_map(|&row| visible.get(layout.slots(row)).unwrap_or_default());
        self.thumbs.schedule(ids.map(|&id| (id, tier)), |id| entries.get(id).map(|e| (e.path.clone(), e.kind)));
    }

    /// Selects `id` alone, focused, as the anchor of a Shift range.
    fn select_only(&self, id: u32) {
        self.focus_on(id);
        self.anchor.set(Some(id));
        self.set_selection(HashSet::from([id]));
    }

    /// Moves the keyboard cursor to `id`, redrawing both tiles.
    fn focus_on(&self, id: u32) {
        if let Some(old) = self.focus.replace(Some(id)) {
            self.refresh_entry(old);
        }
        self.refresh_entry(id);
    }

    /// Replaces the selection, redrawing the rows whose tiles changed.
    fn set_selection(&self, selection: HashSet<u32>) {
        let before = self.selection.replace(selection);
        let changed: Vec<u32> = before.symmetric_difference(&self.selection.borrow()).copied().collect();
        self.refresh_entries(changed);
        self.publish_selection();
    }

    /// Drops what isn't shown from the selection (a refilter hid it), and the focus and
    /// anchor if they aren't shown.
    fn prune_selection(&self) {
        let shown = |id: &u32| self.slot(*id).is_some();
        self.selection.borrow_mut().retain(shown);
        for cell in [&self.focus, &self.anchor] {
            if cell.get().is_some_and(|id| !shown(&id)) {
                cell.set(None);
            }
        }
    }

    /// A click on entry `id` (`None`: between tiles). `extend` (Shift) selects the range
    /// from the anchor, `toggle` (Ctrl) adds or removes one item (Ctrl+Shift adds the range).
    fn click(&self, id: Option<u32>, extend: bool, toggle: bool) {
        let Some(id) = id else {
            if !extend && !toggle {
                self.set_selection(HashSet::new());
            }
            return;
        };
        match self.anchor.get().and_then(|anchor| self.slot(anchor)).zip(self.slot(id)) {
            Some((from, to)) if extend => {
                let mut selection = if toggle { self.selection.borrow().clone() } else { HashSet::new() };
                selection.extend(self.visible.borrow()[from.min(to)..=from.max(to)].iter().copied());
                self.focus_on(id);
                self.set_selection(selection);
            }
            _ if toggle || extend => {
                let mut selection = self.selection.borrow().clone();
                if !selection.remove(&id) {
                    selection.insert(id);
                }
                self.focus_on(id);
                self.anchor.set(Some(id));
                self.set_selection(selection);
            }
            _ => self.select_only(id),
        }
    }

    /// A rubber band starts: `keep` adds to the selection instead of replacing it.
    fn drag_begin(&self, keep: bool) {
        self.band.borrow_mut().clear();
        if !keep {
            self.set_selection(HashSet::new());
        }
    }

    /// The rubber band covers rows `r0..=r1`, columns `c0..=c1` (not clamped to the grid):
    /// those tiles are selected, with what was selected when it started.
    fn drag_to(&self, r0: i32, r1: i32, c0: i32, c1: i32) {
        let columns = self.columns.get();
        let (r1, c1) = (r1.min(self.row_count() as i32 - 1), c1.min(columns as i32 - 1));
        let mut covered = HashSet::new();
        if r1 >= 0 && c1 >= 0 && r0 <= r1 && c0 <= c1 {
            let (visible, layout) = (self.visible.borrow(), self.layout());
            for row in r0.max(0) as usize..=r1 as usize {
                // Header rows have no tiles.
                let slots = layout.slots(row);
                covered.extend((c0.max(0) as usize..=c1 as usize).map(|c| slots.start + c).filter(|i| slots.contains(i)).map(|i| visible[i]));
            }
        }
        let mut changed = Vec::new();
        {
            let (mut selection, mut band) = (self.selection.borrow_mut(), self.band.borrow_mut());
            band.retain(|id| {
                let stays = covered.contains(id);
                if !stays {
                    selection.remove(id);
                    changed.push(*id);
                }
                stays
            });
            for id in covered {
                if selection.insert(id) {
                    band.push(id);
                    changed.push(id);
                }
            }
        }
        self.refresh_entries(changed);
        self.publish_selection();
    }

    /// A key on the grid (lowercase: `left`, `pageup`, `a`, …); returns whether it was used.
    fn key(&self, key: &str, shift: bool, ctrl: bool) -> bool {
        let len = self.visible.borrow().len();
        let (first, last) = self.viewport.get();
        let page = (last - first).max(1) as isize;
        let at = self.focus.get().and_then(|id| self.slot(id));
        let layout = self.layout().clone();
        let step = |delta: isize| match at {
            Some(at) => Some(at.saturating_add_signed(delta).min(len.saturating_sub(1))),
            // Nothing focused yet: the first tile in view.
            None => Some(layout.first_slot_from(first).unwrap_or(0).min(len.saturating_sub(1))),
        };
        // Within the column, across date headers; down from the row above a shorter last row
        // goes to its end. A page past the first or last row goes to the first or last item.
        let rows = |delta: isize, page: bool| match at {
            Some(at) => match layout.vertical(at, delta) {
                (_, false) if page => Some(if delta < 0 { 0 } else { len.saturating_sub(1) }),
                (to, _) => Some(to),
            },
            None => step(0),
        };
        let to = match key {
            "left" => step(-1),
            "right" => step(1),
            "up" => rows(-1, false),
            "down" => rows(1, false),
            "pageup" => rows(-page, true),
            "pagedown" => rows(page, true),
            "home" => Some(0),
            "end" => Some(len.saturating_sub(1)),
            "a" if ctrl => {
                log::debug!("review: select all ({len})");
                self.set_selection(self.visible.borrow().iter().copied().collect());
                return true;
            }
            "escape" if !self.selection.borrow().is_empty() => {
                log::debug!("review: Esc clears the selection");
                self.set_selection(HashSet::new());
                return true;
            }
            "enter" => {
                let Some(id) = self.focus.get().filter(|id| self.slot(*id).is_some()) else { return false };
                log::debug!("review: Enter opens entry {id}");
                self.open(id);
                return true;
            }
            "delete" if !ctrl => return self.act("delete"),
            "t" if !ctrl => return self.act("tag"),
            "c" if ctrl => return self.act("copy"),
            "r" if !ctrl => return self.act(if shift { "rotate-left" } else { "rotate-right" }),
            _ => return false,
        };
        let Some(slot) = to.filter(|_| len > 0) else { return false };
        let id = self.visible.borrow()[slot];
        log::trace!("review key {key} (shift {shift}): focus slot {slot}, entry {id}");
        match self.anchor.get().and_then(|a| self.slot(a)).filter(|_| shift) {
            Some(from) => {
                let range = self.visible.borrow()[from.min(slot)..=from.max(slot)].iter().copied().collect();
                self.focus_on(id);
                self.set_selection(range);
            }
            None => self.select_only(id),
        }
        self.reveal(slot);
        true
    }

    /// Does `action` (an item menu action) to the selection, from a key.
    fn act(&self, action: &str) -> bool {
        let Some(ui) = self.ui.upgrade() else { return false };
        let focus = self.focus.get().filter(|id| self.selection.borrow().contains(id));
        let Some(id) = focus.or_else(|| self.selection.borrow().iter().copied().min_by_key(|&id| self.slot(id))) else {
            return false;
        };
        let Some(target) = self.target(id) else { return false };
        let items = if self.selection.borrow().len() > 1 { self.selected_paths() } else { Vec::new() };
        log::debug!("review key: {action} on {} items", items.len().max(1));
        actions::act_on(&ui, action, target, items);
        true
    }

    /// The selected files, in the grid's order.
    fn selected_paths(&self) -> Vec<PathBuf> {
        let mut slots: Vec<(usize, u32)> = self.selection.borrow().iter().filter_map(|&id| Some((self.slot(id)?, id))).collect();
        slots.sort_unstable();
        let index = self.entries.borrow();
        slots.into_iter().filter_map(|(_, id)| index.get(id).map(|e| e.path.clone())).collect()
    }

    /// Scrolls just enough for the tile at `slot` to be in view.
    fn reveal(&self, slot: usize) {
        let Some(ui) = self.ui.upgrade() else { return };
        let bridge = ui.global::<ReviewBridge>();
        let serial = bridge.get_reveal().serial + 1;
        bridge.set_reveal(ScrollRestore { serial, row: self.layout().row_of(slot) as i32, offset: 0.0 });
    }

    /// Where entry `id` is shown, if it is.
    fn slot(&self, id: u32) -> Option<usize> {
        self.slot_of.borrow().get(id as usize).copied().filter(|&slot| slot != HIDDEN).map(|slot| slot as usize)
    }

    fn publish_selection(&self) {
        let Some(ui) = self.ui.upgrade() else { return };
        ui.global::<ReviewBridge>().set_selection_count(self.selection.borrow().len() as i32);
    }

    /// The entry shown at grid `row`, `column`.
    fn id_at(&self, row: i32, column: i32) -> Option<u32> {
        if row < 0 || column < 0 {
            return None;
        }
        let slots = self.layout().slots(row as usize);
        let slot = slots.start + column as usize;
        slots.contains(&slot).then(|| self.visible.borrow()[slot])
    }

    /// The date group whose header is at grid `row`, if one is.
    fn header_at(&self, row: i32) -> Option<usize> {
        match self.layout().at(usize::try_from(row).ok()?)? {
            RowAt::Header(g) => Some(g),
            RowAt::Tiles(_) => None,
        }
    }

    /// A click on a date header: selects the month's items (`add`: Ctrl, to the selection),
    /// the first of them focused, as the anchor of a Shift range.
    fn select_group(&self, g: usize, add: bool) {
        let slots = self.layout().group(g);
        let ids: Vec<u32> = self.visible.borrow()[slots].to_vec();
        log::debug!("review: header {g} picks its {} items (add {add})", ids.len());
        let mut selection = if add { self.selection.borrow().clone() } else { HashSet::new() };
        selection.extend(ids.iter().copied());
        if let Some(&first) = ids.first() {
            self.focus_on(first);
            self.anchor.set(Some(first));
        }
        self.set_selection(selection);
    }

    /// The live entry of the path with [`path_hash`] `hash`.
    fn id_of_hash(&self, hash: u64) -> Option<u32> {
        let id = *self.by_path.borrow().get(&hash)?;
        (!self.entries.borrow().gone.contains(&id)).then_some(id)
    }

    /// The live entry of `path`.
    fn id_of(&self, path: &Path) -> Option<u32> {
        let id = *self.by_path.borrow().get(&path_hash(path))?;
        let index = self.entries.borrow();
        (!index.gone.contains(&id) && index.get(id)?.path == path).then_some(id)
    }

    /// Shows `id` on the Details page.
    fn open(&self, id: u32) {
        let Some(ui) = self.ui.upgrade() else { return };
        let Some(target) = self.target(id) else { return };
        if !self.selection.borrow().contains(&id) || self.focus.get() != Some(id) {
            self.select_only(id);
        }
        nav::open(&ui, target);
    }

    /// Entry `id` as an item opened from here.
    fn target(&self, id: u32) -> Option<Target> {
        let path = self.entries.borrow().get(id)?.path.clone();
        Some(Target::new(path, self.scope(Some(id))))
    }

    /// What the grid shows now, as filtered and sorted: a snapshot of the index (shared
    /// chunks) and of the shown order, listed only when asked.
    fn scope(&self, current: Option<u32>) -> Scope {
        let (index, visible) = (self.entries.borrow().clone(), self.visible.borrow().clone());
        let current = current
            .and_then(|id| self.slot_of.borrow().get(id as usize).copied())
            .filter(|&slot| slot != HIDDEN)
            .map(|slot| slot as usize);
        let query = self.query.borrow();
        let mut label = "Review".to_owned();
        if !query.text.is_empty() {
            label += &format!(" · “{}”", query.text);
        }
        if !query.described.is_empty() {
            label += &format!(" · {}", query.described);
        }
        if !query.tag.is_empty() {
            label += &format!(" · tags {}", query.tag);
        }
        label += match query.kind {
            1 => " · Images",
            2 => " · Video",
            3 => " · Audio",
            _ => "",
        };
        let attention = attention_label(&attention_of(query.need, query.without));
        if !attention.is_empty() {
            label += &format!(" · {attention}");
        }
        Scope::new(label, visible.len(), current, move || {
            visible.iter().filter_map(|&id| index.get(id).map(|e| e.path.clone())).collect()
        })
    }

    /// Back to a view left earlier: its filters (re-filtering if they changed since), its
    /// selection and, once the filter result is in, its scroll position.
    fn restore(&self, s: Snapshot) {
        let Some(ui) = self.ui.upgrade() else { return };
        let bridge = ui.global::<ReviewBridge>();
        let filters = Filters {
            text: s.search.to_string(),
            tag: s.tag.to_string(),
            kind: s.kind,
            sort: s.sort,
            attention: s.attention.clone(),
            mode: s.mode,
            similar: s.similar.to_string(),
        };
        let refilter = *self.filters.borrow() != filters;
        log::debug!("review restore {s:?} (refilter {refilter})");
        bridge.set_search_text(s.search.clone());
        bridge.set_tag_text(s.tag.clone());
        bridge.set_selected_tag(s.selected_tag);
        bridge.set_kind_filter(s.kind);
        bridge.set_sort_index(s.sort);
        bridge.set_attention(s.attention.clone());
        bridge.set_search_mode(s.mode);
        bridge.set_similar_to(s.similar.clone());
        bridge.set_similar_name(file_name(Path::new(s.similar.as_str())).into());
        let reveal = s.focus.clone().filter(|_| s.followed);
        let focus = s.focus.and_then(|path| self.id_of(&path));
        if let Some(old) = self.focus.replace(focus) {
            self.refresh_entry(old);
        }
        self.anchor.set(focus);
        let ids = s.selected.iter().filter_map(|&hash| self.id_of_hash(hash)).collect();
        self.set_selection(ids);
        *self.restore.borrow_mut() = Some((s.anchor, s.on_header, s.offset, reveal));
        if refilter {
            self.set_filters(filters);
        }
        self.apply_restore();
    }

    /// Scrolls to the position [`restore`](Self::restore) asked for, unless a search or a
    /// refilter is still to land (then it does this).
    fn apply_restore(&self) {
        if self.pending.get() || self.searching.get() {
            return;
        }
        let Some((anchor, on_header, offset, reveal)) = self.restore.take() else { return };
        let layout = self.layout().clone();
        let row_of = |path: &PathBuf| {
            let id = self.id_of(path)?;
            let slot = self.slot_of.borrow().get(id as usize).copied().filter(|&slot| slot != HIDDEN)?;
            Some(layout.row_of(slot as usize))
        };
        // The header over the anchor, if that was in view and still heads its row.
        let header_over = |row: usize| match layout.at(row.wrapping_sub(1)) {
            Some(RowAt::Header(_)) if on_header => row - 1,
            _ => row,
        };
        let mut to = anchor.as_ref().and_then(row_of).map(|row| (header_over(row), offset));
        if to.is_none() {
            log::debug!("review restore: scroll anchor {anchor:?} not shown - scroll left as is");
        }
        // Details stepped away from the tile opened: keep the last one shown in view.
        if let Some(row) = reveal.as_ref().and_then(row_of) {
            let (first, last) = self.viewport.get();
            let screen = last - first + 1;
            let top = to.map_or(first, |(row, _)| row);
            // The bottom row may be cut off.
            if row < top || row >= top + screen.saturating_sub(1).max(1) {
                log::debug!("review restore: the item last shown on Details (row {row}) is out of view - centering it");
                to = Some((row.saturating_sub(screen / 2), 0.0));
            }
        }
        let Some((row, offset)) = to else { return };
        log::debug!("review restore: scroll to row {row} +{offset:.0}px");
        let Some(ui) = self.ui.upgrade() else { return };
        let bridge = ui.global::<ReviewBridge>();
        let serial = bridge.get_restore().serial + 1;
        bridge.set_restore(ScrollRestore { serial, row: row as i32, offset });
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

        let (slot_of, layout) = (self.slot_of.borrow(), self.layout());
        let mut rows: Vec<usize> =
            changed.iter().filter_map(|&id| slot_of.get(id as usize).filter(|&&slot| slot != HIDDEN)).map(|&slot| layout.row_of(slot as usize)).collect();
        drop((slot_of, layout));
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

    /// Re-delivers each row holding one of `ids` that is shown, once.
    fn refresh_entries(&self, ids: Vec<u32>) {
        let layout = self.layout().clone();
        let mut rows: Vec<usize> = ids.into_iter().filter_map(|id| self.slot(id)).map(|slot| layout.row_of(slot)).collect();
        rows.sort_unstable();
        rows.dedup();
        for row in rows {
            self.notify.row_changed(row);
        }
    }

    /// Re-delivers the row holding entry `id`, if that entry is currently shown.
    fn refresh_entry(&self, id: u32) {
        let slot = self.slot_of.borrow().get(id as usize).copied().unwrap_or(HIDDEN);
        if slot != HIDDEN {
            let row = self.layout().row_of(slot as usize);
            self.notify.row_changed(row);
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
            selected: self.selection.borrow().contains(&id),
            focused: self.focus.get() == Some(id),
            faces: entry.flags.contains(AssetFlags::FACES),
            unknown_faces: entry.flags.contains(AssetFlags::UNKNOWN_FACES),
            two_sided: entry.flags.intersects(AssetFlags::HAS_BACK | AssetFlags::IS_BACK),
            review: entry.flags.contains(AssetFlags::REVIEW),
            failed: entry.flags.contains(AssetFlags::FAILED),
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

    /// Shows the tag tree as counted now, without the lines under collapsed nodes; lines
    /// that didn't change aren't touched (it is redone for every indexer batch).
    fn publish_tags(&self) {
        let items = tag_items(&self.tag_counts.borrow().tree(), &self.collapsed.borrow());
        let model = &self.tag_model;
        if model.row_count() == items.len() {
            for (i, item) in items.into_iter().enumerate() {
                if model.row_data(i).as_ref() != Some(&item) {
                    model.set_row_data(i, item);
                }
            }
        } else {
            model.set_vec(items);
        }
    }

    /// Expands or collapses the tag tree's node at `path`.
    fn toggle_tag(&self, path: &str) {
        let key = path.to_ascii_lowercase();
        let collapsed = {
            let mut set = self.collapsed.borrow_mut();
            if !set.remove(&key) {
                set.insert(key);
                true
            } else {
                false
            }
        };
        log::debug!("review tag tree: {} {path:?}", if collapsed { "collapse" } else { "expand" });
        self.publish_tags();
    }
}

/// [`Sort::Date`]'s key of a date: newest first, so smaller for later dates.
fn date_key(date: i64) -> u128 {
    (!((date as u64) ^ (1 << 63))).into()
}

/// The date a [`date_key`] was made from.
fn key_date(key: u128) -> i64 {
    (!(key as u64) ^ (1 << 63)) as i64
}

/// What a grid row shows.
#[derive(Clone, Debug, PartialEq, Eq)]
enum RowAt {
    /// The header of a group (by its index).
    Header(usize),
    /// The tiles in these slots of the shown order.
    Tiles(std::ops::Range<usize>),
}

/// How the shown order is laid out in rows of `columns` tiles. Sorted by date, each month's
/// items are a group that starts on a row of its own under a header row; otherwise there is
/// one group without a header. Groups are found by binary search on the sort keys (months ·
/// log n), so the layout is simply rebuilt whenever the order changes; looking up a row's
/// slots, a slot's row or a row's position is a binary search over the groups. Selection
/// ranges, the keyboard, the rubber band, thumbnail scheduling, scroll positions and the
/// rows the view re-reads all go through it.
#[derive(Clone, Debug, Default, PartialEq)]
struct Layout {
    columns: usize,
    /// Slots shown.
    len: usize,
    /// Groups have a header row (sorted by date).
    headers: bool,
    /// Each group's first slot, ascending (the first is 0); none when nothing is shown.
    starts: Vec<usize>,
    /// Each group's first row (its header if it has one).
    rows: Vec<usize>,
    /// Each group's month, `(year, month)` (headers only).
    months: Vec<(i64, u32)>,
    total_rows: usize,
}

impl Layout {
    /// For `keys`, the shown order's sort keys; `by_date`: they are [`date_key`]s, newest first.
    fn new(keys: &[u128], columns: usize, by_date: bool) -> Self {
        let columns = columns.max(1);
        let mut layout = Self { columns, len: keys.len(), headers: by_date, ..Self::default() };
        let (mut start, mut row) = (0, 0);
        while start < keys.len() {
            let (end, month) = if by_date {
                let (year, month) = dates::month_of(key_date(keys[start]));
                // Newest first: the month's items are those up to its first second's key.
                let limit = date_key(dates::month_start(year, month));
                (start + keys[start..].partition_point(|&k| k <= limit).max(1), (year, month))
            } else {
                (keys.len(), (0, 0))
            };
            layout.starts.push(start);
            layout.rows.push(row);
            layout.months.push(month);
            row += usize::from(by_date) + (end - start).div_ceil(columns);
            start = end;
        }
        layout.total_rows = row;
        layout
    }

    fn row_count(&self) -> usize {
        self.total_rows
    }

    fn header_rows(&self) -> usize {
        usize::from(self.headers)
    }

    /// The group holding `slot` (the last one for slots past the end).
    fn group_of(&self, slot: usize) -> usize {
        self.starts.partition_point(|&s| s <= slot).saturating_sub(1)
    }

    /// The slots of group `g`.
    fn group(&self, g: usize) -> std::ops::Range<usize> {
        self.starts[g]..self.starts.get(g + 1).copied().unwrap_or(self.len)
    }

    /// The row showing `slot`.
    fn row_of(&self, slot: usize) -> usize {
        if self.starts.is_empty() {
            return slot / self.columns;
        }
        let g = self.group_of(slot);
        self.rows[g] + self.header_rows() + (slot - self.starts[g]) / self.columns
    }

    /// The first row that may show `slot` or a later one, or whose header may have changed
    /// with it: where re-reading starts after the order changed from `slot` on.
    fn changed_from(&self, slot: usize) -> usize {
        if self.headers && !self.starts.is_empty() {
            self.rows[self.group_of(slot)]
        } else {
            slot / self.columns
        }
    }

    fn at(&self, row: usize) -> Option<RowAt> {
        if row >= self.total_rows {
            return None;
        }
        let g = self.rows.partition_point(|&r| r <= row) - 1;
        if self.headers && row == self.rows[g] {
            return Some(RowAt::Header(g));
        }
        let group = self.group(g);
        let start = group.start + (row - self.rows[g] - self.header_rows()) * self.columns;
        Some(RowAt::Tiles(start..(start + self.columns).min(group.end)))
    }

    /// The slots of tile row `row` (empty for a header or past the end).
    fn slots(&self, row: usize) -> std::ops::Range<usize> {
        match self.at(row) {
            Some(RowAt::Tiles(slots)) => slots,
            _ => 0..0,
        }
    }

    /// The first slot shown at `row` or after it, if any.
    fn first_slot_from(&self, row: usize) -> Option<usize> {
        match self.at(row)? {
            RowAt::Header(g) => Some(self.starts[g]),
            RowAt::Tiles(slots) => Some(slots.start),
        }
    }

    /// The slot `rows` tile rows up (negative) or down from `slot`, in its column as far as
    /// the row reaches, headers skipped; and whether there were that many rows to go.
    fn vertical(&self, slot: usize, rows: isize) -> (usize, bool) {
        let from = self.row_of(slot);
        let column = slot - self.slots(from).start;
        let (mut row, mut best, mut left) = (from, from, rows.unsigned_abs());
        while left > 0 {
            match row.checked_add_signed(rows.signum()).filter(|&r| r < self.total_rows) {
                Some(next) => row = next,
                None => break,
            }
            if matches!(self.at(row), Some(RowAt::Tiles(_))) {
                best = row;
                left -= 1;
            }
        }
        let slots = self.slots(best);
        (slots.start + column.min(slots.len().saturating_sub(1)), left == 0)
    }

    /// Header rows before `row`.
    fn headers_before(&self, row: usize) -> usize {
        if self.headers { self.rows.partition_point(|&r| r < row) } else { 0 }
    }

    /// Where `row` starts, for rows `row_h` and headers `header_h` high (both as the page
    /// draws them).
    fn row_y(&self, row: usize, row_h: f32, header_h: f32) -> f32 {
        let headers = self.headers_before(row);
        (row - headers) as f32 * row_h + headers as f32 * header_h
    }

    /// The row at height `y` (-1 above the first; `row_count` and on past the last).
    fn row_at_y(&self, y: f32, row_h: f32, header_h: f32) -> i64 {
        if y < 0.0 || row_h <= 0.0 {
            return -1;
        }
        if !self.headers || self.starts.is_empty() {
            return (y / row_h) as i64;
        }
        let g = self.rows.partition_point(|&r| self.row_y(r, row_h, header_h) <= y).saturating_sub(1);
        let into = y - self.row_y(self.rows[g], row_h, header_h);
        if into < header_h {
            return self.rows[g] as i64;
        }
        (self.rows[g] + 1) as i64 + ((into - header_h) / row_h) as i64
    }

    /// A header's label and count ("October 2026", "42 items").
    fn header(&self, g: usize) -> (String, String) {
        const MONTHS: [&str; 12] =
            ["January", "February", "March", "April", "May", "June", "July", "August", "September", "October", "November", "December"];
        let (year, month) = self.months[g];
        let n = self.group(g).len();
        (format!("{} {year}", MONTHS[(month as usize).clamp(1, 12) - 1]), format!("{} {}", grouped(n), if n == 1 { "item" } else { "items" }))
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
        self.layout().row_count()
    }

    fn row_data(&self, row: usize) -> Option<MediaRow> {
        let layout = self.layout();
        match layout.at(row)? {
            RowAt::Header(g) => {
                let (label, detail) = layout.header(g);
                Some(MediaRow { row: row as i32, header: true, label: label.into(), detail: detail.into(), tiles: ModelRc::default() })
            }
            RowAt::Tiles(slots) => {
                drop(layout);
                let visible = self.visible.borrow();
                let ids = visible.get(slots)?;
                let entries = self.entries.borrow();
                let tiles: Vec<MediaTile> = ids.iter().filter_map(|&id| Some(self.tile(id, entries.get(id)?))).collect();
                Some(MediaRow { row: row as i32, tiles: ModelRc::new(VecModel::from(tiles)), ..MediaRow::default() })
            }
        }
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

/// The flags an entry needs (and must not have) for the "Needs attention" chips: each chip
/// narrows the view further.
fn attention_flags(a: &Attention) -> (AssetFlags, AssetFlags) {
    let mut need = AssetFlags::NONE;
    for (on, flag) in [(a.review, AssetFlags::REVIEW), (a.failed, AssetFlags::FAILED), (a.unknown_faces, AssetFlags::UNKNOWN_FACES), (a.has_back, AssetFlags::HAS_BACK)] {
        if on {
            need |= flag;
        }
    }
    (need, if a.untagged { AssetFlags::TAGGED } else { AssetFlags::NONE })
}

/// The chips that ask for these flags (the other way round).
fn attention_of(need: AssetFlags, without: AssetFlags) -> Attention {
    Attention {
        review: need.contains(AssetFlags::REVIEW),
        failed: need.contains(AssetFlags::FAILED),
        untagged: without.contains(AssetFlags::TAGGED),
        unknown_faces: need.contains(AssetFlags::UNKNOWN_FACES),
        has_back: need.contains(AssetFlags::HAS_BACK),
    }
}

/// The chips that are on, for the log and the Details scope ("Failed, Untagged").
fn attention_label(a: &Attention) -> String {
    let on = [(a.review, "Rotation to review"), (a.failed, "Failed"), (a.untagged, "Untagged"), (a.unknown_faces, "Unknown faces"), (a.has_back, "Has back")];
    on.iter().filter(|(on, _)| *on).map(|(_, name)| *name).collect::<Vec<_>>().join(", ")
}

/// The tag tree's lines, but those under a collapsed node.
fn tag_items(nodes: &[TagNode], collapsed: &HashSet<String>) -> Vec<TagItem> {
    let mut items = Vec::new();
    // Lines deeper than this are under a collapsed node.
    let mut hide_below: Option<usize> = None;
    for node in nodes {
        if hide_below.is_some_and(|depth| node.depth > depth) {
            continue;
        }
        let expanded = !collapsed.contains(&node.path.to_ascii_lowercase());
        hide_below = (node.has_children && !expanded).then_some(node.depth);
        items.push(TagItem {
            label: node.name.as_str().into(),
            path: node.path.as_str().into(),
            query: tag_query(&node.path).into(),
            depth: node.depth as i32,
            count: node.count.min(i32::MAX as usize) as i32,
            has_children: node.has_children,
            expanded,
        });
    }
    items
}

/// What picking the tag at `path` filters by: it and the tags under it (`=People|Ann`), the
/// path quoted if it has spaces or the like (so `People|Ann Lee` isn't two terms).
pub(crate) fn tag_query(path: &str) -> String {
    let plain = !path.chars().any(|c| c.is_whitespace() || matches!(c, '(' | ')' | ',' | ';' | '&' | '!' | '"'))
        && !path.starts_with(['-', '='])
        && !["or", "and", "not", "||", "&&"].iter().any(|w| path.eq_ignore_ascii_case(w));
    if plain || path.contains('"') { format!("={path}") } else { format!("=\"{path}\"") }
}

/// The tag path a tag tree pick filters by ([`tag_query`]'s), if `query` is one.
fn picked_path(query: &str) -> Option<&str> {
    let path = query.strip_prefix('=')?;
    Some(path.strip_prefix('"').and_then(|p| p.strip_suffix('"')).unwrap_or(path))
}

/// The tag at `from` was renamed or merged (into `to`) or deleted (`None`): a tag filter
/// picked from the tree for it, or for a tag under it, follows (or is cleared).
pub fn tag_moved(ui: &AppWindow, from: &str, to: Option<&str>) {
    let bridge = ui.global::<ReviewBridge>();
    let picked = bridge.get_tag_text();
    let Some(path) = picked_path(&picked) else { return };
    let (path_lc, from_lc) = (path.to_ascii_lowercase(), from.to_ascii_lowercase());
    if path_lc != from_lc && !path_lc.starts_with(&format!("{from_lc}|")) {
        return;
    }
    let query = to.map(|to| tag_query(&format!("{to}{}", &path[from.len().min(path.len())..]))).unwrap_or_default();
    log::debug!("review: the tag filter {picked:?} follows {from:?} -> {to:?}: {query:?}");
    let picked_from_tree = bridge.get_selected_tag() == picked;
    bridge.set_tag_text(query.clone().into());
    bridge.set_selected_tag(if picked_from_tree { query.clone().into() } else { SharedString::default() });
    with_grid(|g| {
        let mut filters = g.filters.borrow().clone();
        filters.tag = query;
        g.set_filters(filters);
    });
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

    /// The grid's rows as the view would show them: the ids of a row's tiles, or for a date
    /// header `[HEADER, month]`.
    fn rows(g: &GridModel) -> Vec<Vec<u32>> {
        let (layout, visible) = (g.layout(), g.visible.borrow());
        (0..layout.row_count())
            .map(|row| match layout.at(row).unwrap() {
                RowAt::Header(group) => vec![HEADER, layout.months[group].0 as u32 * 12 + layout.months[group].1],
                RowAt::Tiles(slots) => visible[slots].to_vec(),
            })
            .collect()
    }

    const HEADER: u32 = u32::MAX;

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
            Sort::Date => ids.sort_by(|a, b| e(b).date.cmp(&e(a).date)),
            Sort::Size => ids.sort_by(|a, b| e(b).size.cmp(&e(a).size)),
            Sort::Type => ids.sort_by(|a, b| (e(a).kind as i32, &e(a).name_lc).cmp(&(e(b).kind as i32, &e(b).name_lc))),
            Sort::Relevance => {
                let rank = |id: &u32| query.ranking.as_ref().and_then(|r| r.get(&e(id).path_hash).copied()).unwrap_or(u32::MAX);
                ids.sort_by(|a, b| (rank(a), &e(a).name_lc).cmp(&(rank(b), &e(b).name_lc)))
            }
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
        g.select_only(0);
        g.append(vec![b.clone()].into(), true);
        let index = g.entries.borrow();
        assert_eq!(index.len(), 3, "a replaced (it changed), b not duplicated");
        assert_eq!(index.gone.iter().copied().collect::<Vec<_>>(), vec![0]);
        let names: Vec<String> = g.visible.borrow().iter().map(|&id| index.get(id).unwrap().name.to_string()).collect();
        assert_eq!(names, ["a.png", "b.png"]);
        assert_eq!(*g.selection.borrow(), HashSet::from([2]), "the selection follows the file to its new entry");
        assert_eq!((g.focus.get(), g.anchor.get()), (Some(2), Some(2)));
        drop(index);

        g.update_files(vec![FileUpdate::Gone("/x/b.png".into()), FileUpdate::Gone("/x/unknown.png".into())]);
        assert_eq!(g.visible.borrow().len(), 1);
        assert_eq!(g.entries.borrow().live(), 1);
    }

    /// Click, Shift / Ctrl, rubber band and keys select by entry; the selection survives
    /// replacements, drops what is filtered out or deleted, and the focus moves on.
    #[test]
    fn multi_selection() {
        let g = grid(usize::MAX, 4, |_| {});
        let files: Vec<Entry> = (0..10)
            .map(|i| Entry::new(format!("/x/{i}.png").into(), format!("{i}.png"), MediaKind::Image, i, SystemTime::UNIX_EPOCH))
            .collect();
        g.append(files.clone().into(), true);
        let selected = |g: &GridModel| {
            let mut ids: Vec<u32> = g.selection.borrow().iter().copied().collect();
            ids.sort();
            ids
        };
        let tiles = |g: &GridModel| (0..g.row_count()).flat_map(|r| g.row_data(r).unwrap().tiles.iter().collect::<Vec<_>>()).collect::<Vec<_>>();

        g.click(Some(1), false, false);
        g.click(Some(6), true, false);
        assert_eq!(selected(&g), [1, 2, 3, 4, 5, 6], "shift: the range from the anchor");
        g.click(Some(3), false, true);
        assert_eq!(selected(&g), [1, 2, 4, 5, 6], "ctrl toggles");
        g.click(Some(8), true, true);
        assert_eq!(selected(&g), [1, 2, 3, 4, 5, 6, 7, 8], "ctrl+shift adds the range from the new anchor");
        let t = tiles(&g);
        assert!(t[8].selected && t[8].focused && !t[0].selected && !t[9].focused, "row_data reads the set");
        g.click(None, false, false);
        assert!(selected(&g).is_empty());

        // Rubber band over rows 0..=1, columns 1..=2, keeping 9.
        g.click(Some(9), false, false);
        g.drag_begin(true);
        g.drag_to(0, 1, 1, 2);
        assert_eq!(selected(&g), [1, 2, 5, 6, 9]);
        g.drag_to(0, 0, 1, 2);
        assert_eq!(selected(&g), [1, 2, 9], "shrinking the band gives back what it took");
        g.drag_begin(false);
        g.drag_to(2, 5, 0, 9);
        assert_eq!(selected(&g), [8, 9], "clamped to the grid");

        // Keys: 4 columns, ids = slots.
        g.click(Some(5), false, false);
        assert!(g.key("right", false, false) && g.focus.get() == Some(6));
        assert!(g.key("down", true, false));
        assert_eq!((g.focus.get(), selected(&g)), (Some(9), vec![6, 7, 8, 9]), "shift extends from the anchor");
        assert!(g.key("down", false, false) && g.focus.get() == Some(9), "no row below");
        assert!(g.key("up", false, false) && g.focus.get() == Some(5));
        assert!(g.key("home", false, false) && selected(&g) == [0]);
        assert!(g.key("up", false, false) && g.focus.get() == Some(0));
        assert!(g.key("end", true, false) && selected(&g).len() == 10);
        assert!(g.key("escape", false, false) && selected(&g).is_empty());
        assert!(!g.key("escape", false, false), "Esc with nothing selected is someone else's");
        assert!(g.key("a", false, true) && selected(&g).len() == 10);
        assert!(!g.key("x", false, false));

        // A changed file: the selection follows it to its new entry.
        g.update_files(vec![present(&Entry::new("/x/3.png".into(), "3.png".into(), MediaKind::Image, 99, SystemTime::UNIX_EPOCH), false)]);
        assert!(selected(&g).contains(&10) && !selected(&g).contains(&3));
        assert_eq!(g.selected_paths().len(), 10);
        assert_eq!(g.selected_paths()[3], PathBuf::from("/x/3.png"), "in the grid's order");

        // Filtered out: dropped.
        g.set_query(Query::new("1".into(), String::new(), 0, Sort::Name));
        assert_eq!(selected(&g), [1]);
        g.set_query(Query::default());
        assert_eq!(selected(&g), [1], "and not back when shown again");

        // Deleted: gone from the selection; the focus moves to the next item.
        g.click(Some(4), false, false);
        g.click(Some(5), false, true);
        g.update_files(vec![FileUpdate::Gone("/x/4.png".into()), FileUpdate::Gone("/x/5.png".into())]);
        assert_eq!((g.focus.get(), selected(&g)), (Some(6), vec![6]), "next item selected once nothing is");
    }

    /// An entry dated `date` (wall-clock seconds).
    fn dated(name: &str, date: i64) -> Entry {
        let mut e = Entry::new(format!("/x/{name}").into(), name.into(), MediaKind::Image, 1, SystemTime::UNIX_EPOCH);
        e.date = date;
        e
    }

    /// Like `replay`, for the page's window: rows it was told changed (or that are new) are
    /// read when it draws, the others must still show what they showed.
    fn replay_window(g: &GridModel, mirror: &mut Vec<Option<Vec<u32>>>) {
        let (start, end) = g.notify.window.range.get();
        let now = rows(g)[start..end].to_vec();
        for change in g.notify.window.log.take() {
            match change {
                Change::Changed(row) => mirror[row] = None,
                Change::Added(index, count) => drop(mirror.splice(index..index, std::iter::repeat_n(None, count))),
                Change::Removed(index, count) => drop(mirror.drain(index..index + count)),
                Change::Reset => *mirror = vec![None; end - start],
            }
        }
        assert_eq!(mirror.len(), end - start, "window rows");
        for (row, shown) in mirror.iter_mut().enumerate() {
            let shown = shown.get_or_insert_with(|| now[row].clone());
            assert_eq!(*shown, now[row], "window row {row} (grid row {}) not re-read", start + row);
        }
    }

    /// Sorted by date, each month gets a header row and starts a row of its own, kept right
    /// through incremental merges and removals, in the view and in the page's window.
    #[test]
    fn date_headers_group_months() {
        let mut rng = Rng(0x2545_f491);
        let month = 31 * 86_400;
        for columns in [1, 3, 7] {
            let g = grid(usize::MAX, columns, |_| {});
            g.viewport.set((3, 9));
            g.set_query(Query { sort: Sort::Date, ..Default::default() });
            let (mut mirror, mut window) = (Vec::new(), Vec::new());
            replay(&g, &mut mirror);
            replay_window(&g, &mut window);
            let mut n = 0;
            for _ in 0..40 {
                let batch: Vec<Entry> = (0..rng.below(30))
                    .map(|_| {
                        n += 1;
                        // A dozen months around 1970, some before it.
                        dated(&format!("{n}.jpg"), rng.below(12 * month) as i64 - 3 * month as i64)
                    })
                    .collect();
                g.append(batch.into(), false);
                replay(&g, &mut mirror);
                if rng.below(3) == 0 {
                    let gone: Vec<FileUpdate> = (0..3).map(|_| FileUpdate::Gone(format!("/x/{}.jpg", rng.below(n.max(1) as u64) + 1).into())).collect();
                    g.indexing.set(false);
                    g.update_files(gone);
                }
                if rng.below(4) == 0 {
                    let first = rng.below(g.row_count() as u64) as usize;
                    g.set_viewport(first as i32, first as i32 + 5);
                }

                // The layout is what a fresh one of the order would be; each month's items
                // come under its header, newest first, columns at a time.
                assert_eq!(*g.layout(), Layout::new(&g.keys.borrow(), columns, true));
                let (index, visible) = (g.entries.borrow(), g.visible.borrow().clone());
                let month_of = |id: u32| dates::month_of(index.get(id).unwrap().date);
                let mut expected = Vec::new();
                for group in visible.chunk_by(|&a, &b| month_of(a) == month_of(b)) {
                    let (year, m) = month_of(group[0]);
                    expected.push(vec![HEADER, year as u32 * 12 + m]);
                    expected.extend(group.chunks(columns).map(<[u32]>::to_vec));
                }
                drop(index);
                assert_eq!(rows(&g), expected);
                // Every slot's row holds it.
                let layout = g.layout().clone();
                for slot in 0..visible.len() {
                    assert!(layout.slots(layout.row_of(slot)).contains(&slot));
                }
                replay(&g, &mut mirror);
                replay_window(&g, &mut window);
            }
            // A refilter by name drops the headers.
            g.set_query(Query::default());
            assert!(!g.layout().headers && rows(&g).iter().all(|r| r.first() != Some(&HEADER)));
            replay_window(&g, &mut window);
        }
    }

    /// Keys, clicks, the rubber band and the page's geometry across date headers.
    #[test]
    fn date_headers_navigation_and_geometry() {
        let g = grid(usize::MAX, 3, |_| {});
        g.set_query(Query { sort: Sort::Date, ..Default::default() });
        let at = |y: i64, m: i64| dates::month_start(y, m as u32) + 86_400;
        // Newest first: 5 in March (ids 0..5), 2 in February (5, 6), 4 in January (7..11).
        let mut files: Vec<Entry> = (0..5).map(|i| dated(&format!("m{i}"), at(2026, 3) - i)).collect();
        files.extend((0..2).map(|i| dated(&format!("f{i}"), at(2026, 2) - i)));
        files.extend((0..4).map(|i| dated(&format!("j{i}"), at(2026, 1) - i)));
        g.append(files.into(), true);
        let m = |month: u32| vec![HEADER, 2026 * 12 + month];
        assert_eq!(rows(&g), [m(3), vec![0, 1, 2], vec![3, 4], m(2), vec![5, 6], m(1), vec![7, 8, 9], vec![10]]);
        assert_eq!(g.layout().header(0), ("March 2026".into(), "5 items".into()));
        assert_eq!(g.layout().header(1).1, "2 items");
        assert_eq!(g.id_at(0, 0), None, "a header has no tiles");
        assert_eq!((g.id_at(2, 1), g.id_at(2, 2)), (Some(4), None));
        assert_eq!(g.header_at(3), Some(1));
        let row = g.row_data(5).unwrap();
        assert!(row.header && row.label == "January 2026" && row.row == 5 && row.tiles.row_count() == 0);

        let selected = |g: &GridModel| {
            let mut ids: Vec<u32> = g.selection.borrow().iter().copied().collect();
            ids.sort();
            ids
        };
        // Up / Down keep the column across headers, as far as the row reaches.
        g.click(Some(2), false, false);
        assert!(g.key("down", false, false) && g.focus.get() == Some(4), "a shorter row: its end");
        assert!(g.key("down", false, false) && g.focus.get() == Some(6), "past February's header");
        assert!(g.key("down", false, false) && g.focus.get() == Some(8));
        assert!(g.key("down", false, false) && g.focus.get() == Some(10));
        assert!(g.key("down", false, false) && g.focus.get() == Some(10), "no row below");
        assert!(g.key("up", false, false) && g.focus.get() == Some(7));
        assert!(g.key("up", true, false) && g.focus.get() == Some(5));
        assert_eq!(selected(&g), [5, 6, 7], "Shift: the range in the shown order");
        assert!(g.key("left", false, false) && g.focus.get() == Some(4), "left / right run on through months");
        g.viewport.set((0, 2));
        assert!(g.key("pagedown", false, false) && g.focus.get() == Some(8), "two rows down: February's, then January's first");
        assert!(g.key("pagedown", false, false) && g.focus.get() == Some(10), "past the end: the last item");
        assert!(g.key("pageup", false, false) && g.focus.get() == Some(5));
        assert!(g.key("pageup", false, false) && g.focus.get() == Some(0), "past the start: the first item");

        // A header click picks its month; Ctrl adds.
        g.select_group(1, false);
        assert_eq!((selected(&g), g.focus.get()), (vec![5, 6], Some(5)));
        g.select_group(2, true);
        assert_eq!(selected(&g), [5, 6, 7, 8, 9, 10]);
        // The rubber band skips headers.
        g.drag_begin(false);
        g.drag_to(2, 4, 0, 0);
        assert_eq!(selected(&g), [3, 5]);

        // Geometry: rows 100 high, headers 40.
        let layout = g.layout().clone();
        let tops: Vec<f32> = (0..=layout.row_count()).map(|r| layout.row_y(r, 100.0, 40.0)).collect();
        assert_eq!(tops, [0.0, 40.0, 140.0, 240.0, 280.0, 380.0, 420.0, 520.0, 620.0]);
        for (row, &top) in tops.iter().enumerate() {
            assert_eq!(layout.row_at_y(top, 100.0, 40.0), row as i64);
            if row > 0 {
                assert_eq!(layout.row_at_y(top - 0.5, 100.0, 40.0), row as i64 - 1);
            }
        }
        assert_eq!(layout.row_at_y(-1.0, 100.0, 40.0), -1);
        assert_eq!(layout.first_slot_from(3), Some(5));
        // Without headers, rows are all one height.
        let plain = Layout::new(&g.keys.borrow(), 3, false);
        assert_eq!((plain.row_count(), plain.row_y(2, 100.0, 40.0), plain.row_at_y(250.0, 100.0, 40.0)), (4, 200.0, 2));
    }

    /// Naming someone in People (or adding a tag) reaches the grid as a file update: the
    /// tag list follows, shown nicely, and filtering by it finds the file.
    #[test]
    fn tag_updates_refresh_the_tag_list_and_filter() {
        let g = grid(usize::MAX, 4, |_| {});
        let at = SystemTime::UNIX_EPOCH;
        let a = Entry::new("/x/a.png".into(), "a.png".into(), MediaKind::Image, 10, at);
        let b = Entry::new("/x/b.png".into(), "b.png".into(), MediaKind::Image, 20, at);
        g.append(vec![a, b].into(), true);
        assert!(g.tag_counts.borrow().is_empty());

        let named = Entry::with_tags("/x/a.png".into(), "a.png".into(), MediaKind::Image, 10, at, vec![
            ("People|Ann Lee".to_owned(), Some(TagKind::Person)),
        ]);
        let anna = Entry::with_tags("/x/b.png".into(), "b.png".into(), MediaKind::Image, 20, at, vec![
            ("People|Ann Leeds".to_owned(), Some(TagKind::Person)),
        ]);
        g.update_files(vec![present(&named, false), present(&anna, false)]);
        let items = tag_items(&g.tag_counts.borrow().tree(), &HashSet::new());
        let lines: Vec<(&str, i32, i32, bool)> = items.iter().map(|t| (t.label.as_str(), t.depth, t.count, t.has_children)).collect();
        assert_eq!(lines, [("People", 0, 2, true), ("Ann Lee", 1, 1, false), ("Ann Leeds", 1, 1, false)]);
        assert_eq!((items[0].query.as_str(), items[1].query.as_str()), ("=People", "=\"People|Ann Lee\""));
        // Collapsed, the people are hidden.
        let collapsed = tag_items(&g.tag_counts.borrow().tree(), &HashSet::from(["people".to_owned()]));
        assert_eq!((collapsed.len(), collapsed[0].expanded), (1, false));

        // A pick filters by exactly that tag (not Ann Leeds), a parent by its subtree.
        let shown = |query: &str| {
            g.set_query(Query::new(String::new(), query.to_owned(), 0, Sort::Name));
            let index = g.entries.borrow();
            g.visible.borrow().iter().map(|&id| index.get(id).unwrap().name.to_string()).collect::<Vec<_>>()
        };
        assert_eq!(shown(&items[1].query), ["a.png"]);
        assert_eq!(shown(&items[0].query), ["a.png", "b.png"]);

        // Un-named again: the tag leaves the tree, its parent counts one.
        let unnamed = Entry::new("/x/a.png".into(), "a.png".into(), MediaKind::Image, 10, at);
        g.update_files(vec![present(&unnamed, false)]);
        let tree: Vec<(String, usize)> = g.tag_counts.borrow().tree().into_iter().map(|n| (n.path, n.count)).collect();
        assert_eq!(tree, [("People".to_owned(), 1), ("People|Ann Leeds".to_owned(), 1)]);
        g.update_files(vec![FileUpdate::Gone("/x/b.png".into())]);
        assert!(g.tag_counts.borrow().is_empty());
    }

    #[test]
    fn tag_queries_and_picks() {
        assert_eq!(tag_query("Dog"), "=Dog");
        assert_eq!(tag_query("or"), "=\"or\"");
        assert_eq!(tag_query("-x"), "=\"-x\"");
        for path in ["Dog", "People|Ann Lee", "a(b)"] {
            assert_eq!(picked_path(&tag_query(path)), Some(path));
            assert!(query::parse(&tag_query(path)).is_ok());
        }
        assert_eq!(picked_path("dog"), None);
    }

    fn flagged(name: &str, flags: AssetFlags) -> Entry {
        let mut e = Entry::new(PathBuf::from(format!("/x/{name}")), name.into(), MediaKind::Image, 10, SystemTime::UNIX_EPOCH);
        e.flags = flags;
        e
    }

    /// The "Needs attention" chips narrow the view by the entries' flags, each one further,
    /// and a file whose flags change is re-filtered (in place, never a reset).
    #[test]
    fn attention_filters_follow_flag_updates() {
        let g = grid(usize::MAX, 4, |_| {});
        let failed = flagged("a.png", AssetFlags::FAILED);
        let faces = flagged("b.png", AssetFlags::REVIEW | AssetFlags::FACES | AssetFlags::UNKNOWN_FACES);
        let tagged = flagged("c.png", AssetFlags::TAGGED | AssetFlags::HAS_BACK);
        g.append(vec![failed.clone(), faces.clone(), tagged.clone()].into(), true);
        let shown = |g: &GridModel| -> Vec<String> {
            let index = g.entries.borrow();
            g.visible.borrow().iter().map(|&id| index.get(id).unwrap().name.to_string()).collect()
        };
        let filter = |attention: Attention| {
            g.set_query(Query::new(String::new(), String::new(), 0, Sort::Name).needing(&attention));
            shown(&g)
        };
        assert_eq!(filter(Attention { failed: true, ..Default::default() }), ["a.png"]);
        assert_eq!(filter(Attention { untagged: true, ..Default::default() }), ["a.png", "b.png"]);
        assert_eq!(filter(Attention { untagged: true, review: true, ..Default::default() }), ["b.png"], "chips narrow together");
        assert_eq!(filter(Attention { unknown_faces: true, ..Default::default() }), ["b.png"]);
        assert_eq!(filter(Attention { has_back: true, ..Default::default() }), ["c.png"]);
        assert_eq!(filter(Attention::default()).len(), 3);

        let id = g.visible.borrow()[1];
        let tile = g.tile(id, g.entries.borrow().get(id).unwrap());
        assert!(tile.faces && tile.unknown_faces && tile.review && !tile.failed && !tile.two_sided, "b.png's badges");

        // Fixed: it leaves the "Failed" view; failing again brings it back.
        assert_eq!(filter(Attention { failed: true, ..Default::default() }), ["a.png"]);
        g.select_only(g.visible.borrow()[0]);
        g.notify.log.take();
        g.update_files(vec![present(&flagged("a.png", AssetFlags::NONE), false)]);
        assert!(shown(&g).is_empty());
        assert!(g.selection.borrow().is_empty(), "what the filter hides leaves the selection");
        g.update_files(vec![present(&flagged("b.png", AssetFlags::FAILED | AssetFlags::FACES), false)]);
        assert_eq!(shown(&g), ["b.png"]);
        assert!(!g.notify.log.borrow().contains(&Change::Reset), "updates never reset the view");
        let label = g.scope(None).label;
        assert_eq!(label, "Review · Failed");
    }

    /// A search by description ranks the view and combines with the other filters; changing
    /// those doesn't search again, a stale result is dropped, and files indexed later merge
    /// in at their rank.
    #[test]
    fn description_search_ranks_and_combines_with_filters() {
        use archive_core::catalog::semantic::Coverage;
        use semantic::Ranked;
        let g = grid(usize::MAX, 4, |_| {});
        let file = |name: &str, kind| Entry::new(PathBuf::from(format!("/x/{name}")), name.into(), kind, 10, SystemTime::UNIX_EPOCH);
        let entries = vec![file("a.jpg", MediaKind::Image), file("b.jpg", MediaKind::Image), file("c.mp4", MediaKind::Video), file("d.jpg", MediaKind::Image)];
        g.append(entries.into(), true);
        let shown = |g: &GridModel| -> Vec<String> {
            let index = g.entries.borrow();
            g.visible.borrow().iter().map(|&id| index.get(id).unwrap().name.to_string()).collect()
        };
        let dog = Filters { text: " dog ".into(), sort: 4, mode: 1, ..Default::default() };
        let found = |generation, paths: &[&str]| Found {
            generation,
            want: Want::Text("dog".into()),
            result: Ok(Ranked { paths: paths.iter().map(|p| PathBuf::from(format!("/x/{p}"))).collect(), coverage: Coverage { described: 4, images: 4 } }),
        };

        g.set_filters(dog.clone());
        assert!(g.searching.get());
        assert_eq!(shown(&g).len(), 4, "the view stays until the result is in");
        let asked = g.search_generation.get();
        g.set_filters(Filters { text: "cat".into(), ..dog.clone() });
        assert!(!g.searched(found(asked, &["a.jpg"])), "superseded");
        g.set_filters(dog.clone());
        assert!(g.searched(found(g.search_generation.get(), &["d.jpg", "c.mp4", "a.jpg", "e.jpg"])));
        assert_eq!(shown(&g), ["d.jpg", "c.mp4", "a.jpg"]);
        assert_eq!(g.scope(None).label, "Review · “dog”");

        // Other filters: no new search.
        let asked = g.search_generation.get();
        g.set_filters(Filters { kind: 1, ..dog.clone() });
        assert!(!g.searching.get());
        assert_eq!(shown(&g), ["d.jpg", "a.jpg"]);
        g.set_filters(Filters { sort: 0, ..dog.clone() });
        assert_eq!(shown(&g), ["a.jpg", "c.mp4", "d.jpg"]);
        g.set_filters(dog.clone());
        assert!(g.search_generation.get() > asked && !g.searching.get());

        // Indexed later: in at its rank.
        g.append(vec![file("e.jpg", MediaKind::Image), file("f.jpg", MediaKind::Image)].into(), true);
        assert_eq!(shown(&g), ["d.jpg", "c.mp4", "a.jpg", "e.jpg"]);

        // Back to names: everything, and a search still in flight is dropped.
        g.set_filters(Filters { text: "cat".into(), ..dog.clone() });
        let asked = g.search_generation.get();
        g.set_filters(Filters { text: "dog".into(), ..Default::default() });
        assert!(!g.searched(found(asked, &["a.jpg"])));
        assert!(shown(&g).is_empty(), "names with “dog”: none");
        g.set_filters(Filters::default());
        assert_eq!(shown(&g).len(), 6);
    }

    /// Files read for the grid carry the catalog's flags, from a walk and from the relay.
    #[test]
    fn entries_carry_catalog_flags() {
        use archive_core::catalog::runs::{Run, RunStatus};
        let dir = tempfile::tempdir().unwrap();
        let root = std::path::absolute(dir.path()).unwrap();
        for name in ["ok.png", "bad.png", "photo-front.png", "photo-back.png"] {
            std::fs::write(root.join(name), b"x").unwrap();
        }
        let catalog = Catalog::open(&Catalog::default_path(&root), &root).unwrap();
        for name in ["ok.png", "photo-front.png", "photo-back.png"] {
            catalog.asset(&root.join(name)).unwrap();
        }
        let (bad, _) = catalog.record_file(&root.join("bad.png"), MediaKind::Image, 1, 2).unwrap();
        let run = Run { stage: "faces", version: "v", input: "1:2", status: RunStatus::Failed, value: None, detail: "" };
        catalog.set_run(bad, &run).unwrap();

        let mut walked = HashMap::new();
        walk(root.clone(), |batch, _| walked.extend(batch.iter().map(|e| (e.name.to_string(), e.flags))));
        assert_eq!(walked["ok.png"], AssetFlags::NONE);
        assert_eq!(walked["bad.png"], AssetFlags::FAILED);
        assert_eq!(walked["photo-front.png"], AssetFlags::HAS_BACK);
        assert_eq!(walked["photo-back.png"], AssetFlags::IS_BACK);

        let files = ["ok.png", "bad.png", "gone.png", ".hidden.png", "photo-back.png"].map(|n| (root.join(n), false));
        let read = FileUpdate::read_all(files, &root, Some(&catalog));
        let flags: Vec<Option<AssetFlags>> = read.iter().map(|u| if let FileUpdate::Present(e, _) = u { Some(e.flags) } else { None }).collect();
        assert_eq!(flags, [Some(AssetFlags::NONE), Some(AssetFlags::FAILED), None, Some(AssetFlags::IS_BACK)]);
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
