//! Review mode: indexes media under a root directory and shows it as a virtualized grid.
//!
//! Why it stays fast with tens of thousands of files:
//! - The index is a flat `Vec<Entry>`; filtering and sorting produce a `Vec<u32>` of
//!   entry ids. No per-file UI objects ever exist.
//! - The grid is a `ListView` of rows backed by [`GridModel`], which slices the id list
//!   on demand. Slint only asks for rows in the viewport, so scrolling is O(visible).
//! - Thumbnails are requested from `row_data` (only for visible rows), decoded
//!   off-thread and kept in a bounded LRU; see `thumbs.rs`.
//! - Filter input is debounced and matched against pre-lowercased names
//!   (a few ms per 100k entries). Indexing streams in batches so the grid fills progressively.

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, Instant, SystemTime};
use std::{fs, thread};

use slint::{ComponentHandle, Model, ModelNotify, ModelRc, ModelTracker, SharedString, Timer, TimerMode, VecModel};

use crate::thumbs::{Pixels, ThumbCache};
use crate::{AppWindow, MediaKind, MediaRow, MediaTile, ReviewBridge};

struct Entry {
    path: PathBuf,
    name: SharedString,
    name_lc: String,
    kind: MediaKind,
    size: u64,
    modified: SystemTime,
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

#[derive(Clone, Default)]
struct Query {
    /// Already lowercased.
    text: String,
    /// 0 all, 1 images, 2 video, 3 audio
    kind: i32,
    /// 0 name, 1 date modified, 2 size, 3 type
    sort: i32,
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
}

const HIDDEN: u32 = u32::MAX;

struct GridModel {
    ui: slint::Weak<AppWindow>,
    entries: RefCell<Vec<Entry>>,
    /// Filtered + sorted entry ids, in display order.
    visible: RefCell<Vec<u32>>,
    /// Entry id -> position in `visible`, or `HIDDEN`.
    slot_of: RefCell<Vec<u32>>,
    columns: Cell<usize>,
    query: RefCell<Query>,
    selected: Cell<Option<u32>>,
    indexing: Cell<bool>,
    thumbs: ThumbCache,
    notify: ModelNotify,
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
    let weak = ui.as_weak();
    let thumbs = ThumbCache::new(move |id, pixels| {
        let _ = weak.upgrade_in_event_loop(move |_| with_grid(|g| g.thumb_ready(id, pixels)));
    });

    let grid = Rc::new(GridModel {
        ui: ui.as_weak(),
        entries: RefCell::default(),
        visible: RefCell::default(),
        slot_of: RefCell::default(),
        columns: Cell::new(1),
        query: RefCell::default(),
        selected: Cell::new(None),
        indexing: Cell::new(true),
        thumbs,
        notify: ModelNotify::default(),
    });
    GRID.set(Some(grid.clone()));

    let bridge = ui.global::<ReviewBridge>();
    bridge.set_rows(ModelRc::from(grid.clone()));
    bridge.set_root_path(root.display().to_string().into());
    bridge.on_columns_changed(|n| with_grid(|g| g.set_columns(n.max(1) as usize)));
    bridge.on_select(|id| with_grid(|g| g.select(id as u32)));
    bridge.on_open(|id| with_grid(|g| g.open(id as u32)));

    let debounce = Timer::default();
    bridge.on_filter_changed(move |text, kind, sort| {
        let query = Query { text: text.to_lowercase(), kind, sort };
        debounce.start(TimerMode::SingleShot, Duration::from_millis(120), move || {
            with_grid(|g| g.set_query(query.clone()))
        });
    });

    grid.publish_status();
    spawn_indexer(root, ui.as_weak());
}

/// Walks `root` on a background thread, streaming batches to the UI every ~150 ms.
/// TODO: persist the index (e.g. SQLite with EXIF/duration columns) and watch the
/// tree with inotify instead of rescanning on every launch.
fn spawn_indexer(root: PathBuf, ui: slint::Weak<AppWindow>) {
    thread::spawn(move || {
        let flush = |batch: Vec<Entry>, done: bool| {
            let _ = ui.upgrade_in_event_loop(move |_| with_grid(|g| g.append(batch, done)));
        };
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
                batch.push(Entry {
                    name_lc: name.to_lowercase(),
                    name: name.into(),
                    kind,
                    size: meta.as_ref().map_or(0, |m| m.len()),
                    modified: meta.and_then(|m| m.modified().ok()).unwrap_or(SystemTime::UNIX_EPOCH),
                    path,
                });
                if last_flush.elapsed() > Duration::from_millis(150) {
                    flush(std::mem::take(&mut batch), false);
                    last_flush = Instant::now();
                }
            }
        }
        flush(batch, true);
    });
}

impl GridModel {
    fn append(&self, batch: Vec<Entry>, done: bool) {
        self.entries.borrow_mut().extend(batch);
        self.indexing.set(!done);
        // TODO: while indexing in the default sort, splice new ids in with
        // `row_added` instead of a full reset to keep the scroll position stable.
        self.refilter();
    }

    fn set_query(&self, query: Query) {
        *self.query.borrow_mut() = query;
        self.refilter();
    }

    fn set_columns(&self, columns: usize) {
        if self.columns.replace(columns) != columns {
            self.notify.reset();
        }
    }

    fn refilter(&self) {
        {
            let entries = self.entries.borrow();
            let query = self.query.borrow();
            let e = |id: &u32| &entries[*id as usize];

            let mut ids: Vec<u32> = (0..entries.len() as u32).filter(|id| query.accepts(e(id))).collect();
            match query.sort {
                1 => ids.sort_by(|a, b| e(b).modified.cmp(&e(a).modified)),
                2 => ids.sort_by(|a, b| e(b).size.cmp(&e(a).size)),
                3 => ids.sort_by(|a, b| {
                    (e(a).kind as i32, &e(a).name_lc).cmp(&(e(b).kind as i32, &e(b).name_lc))
                }),
                _ => ids.sort_by(|a, b| e(a).name_lc.cmp(&e(b).name_lc)),
            }

            let mut slot_of = self.slot_of.borrow_mut();
            slot_of.clear();
            slot_of.resize(entries.len(), HIDDEN);
            for (slot, &id) in ids.iter().enumerate() {
                slot_of[id as usize] = slot as u32;
            }
            *self.visible.borrow_mut() = ids;
        }
        self.notify.reset();
        self.publish_status();
    }

    fn select(&self, id: u32) {
        let previous = self.selected.replace(Some(id));
        for id in previous.into_iter().chain([id]) {
            self.refresh_entry(id);
        }
    }

    fn open(&self, id: u32) {
        // TODO: in-app viewer (zoom, side-by-side compare, metadata editing).
        if let Some(entry) = self.entries.borrow().get(id as usize) {
            let _ = std::process::Command::new("xdg-open").arg(&entry.path).spawn();
        }
    }

    fn thumb_ready(&self, id: u32, pixels: Option<Pixels>) {
        let loaded = pixels.is_some();
        self.thumbs.complete(id, pixels);
        if loaded {
            self.refresh_entry(id);
        }
    }

    /// Re-delivers the row holding entry `id`, if that entry is currently shown.
    fn refresh_entry(&self, id: u32) {
        let slot = self.slot_of.borrow().get(id as usize).copied().unwrap_or(HIDDEN);
        if slot != HIDDEN {
            self.notify.row_changed(slot as usize / self.columns.get());
        }
    }

    fn tile(&self, id: u32, entry: &Entry) -> MediaTile {
        let thumb = self.thumbs.get(id);
        if thumb.is_none() {
            self.thumbs.request(id, &entry.path, entry.kind);
        }
        MediaTile {
            index: id as i32,
            name: entry.name.clone(),
            kind: entry.kind,
            detail: human_size(entry.size).into(),
            thumb_ready: thumb.is_some(),
            thumb: thumb.unwrap_or_default(),
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
        let tiles: Vec<MediaTile> = ids.iter().map(|&id| self.tile(id, &entries[id as usize])).collect();
        Some(MediaRow { tiles: ModelRc::new(VecModel::from(tiles)) })
    }

    fn model_tracker(&self) -> &dyn ModelTracker {
        &self.notify
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
