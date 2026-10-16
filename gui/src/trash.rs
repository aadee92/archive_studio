//! The Trash page: what was deleted (the library's trash, [`archive_core::catalog::trash`]),
//! newest first, each with its thumbnail (drawn from the file where it lies in the trash),
//! the folder it was in, when it was deleted and its size. Select with click, Shift-click
//! (a range), Ctrl-click, Ctrl+A, Esc; then Restore (back where it was, [`ops::restore_all`],
//! with its back if that is in the trash too; undoable from the notice), or Delete
//! permanently ([`ops::purge`], confirmed: it can't be undone). Empty trash deletes
//! everything, or only what was deleted more than some days ago ([`ops::empty_trash`]),
//! confirmed with how many items that is.
//!
//! The list is read on a worker when the page is shown, and again when the trash's totals
//! change while it is ([`totals`], from the pipeline relay's counts: deletes, restores and
//! undo from anywhere). Thumbnails are loaded on a worker for the whole list
//! (small tier, `thumbs::small_for_files`), dropped if the list is read again meanwhile.
//! Every change runs on a worker; restored files are reported with [`pipeline::changed`],
//! so they come back to Review, and the totals are recounted.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, SystemTime};

use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};

use crate::actions::{self, local_time, notify};
use crate::pipeline::{self, Handle};
use crate::review::{grouped, human_size};
use crate::thumbs::{self, Pixels};
use crate::{nav, AppWindow, TrashBridge, TrashRow, TrashTile};
use archive_core::catalog::trash::{Trashed, TrashTotals};
use archive_core::catalog::AssetId;
use archive_core::ops;

/// Retires thumbnail loads for an older list.
static GENERATION: AtomicU64 = AtomicU64::new(0);

struct State {
    root: PathBuf,
    pipeline: Handle,
    items: Vec<Trashed>,
    thumbs: HashMap<PathBuf, slint::Image>,
    selected: HashSet<AssetId>,
    /// Where a Shift-click range starts (an index of `items`).
    anchor: Option<usize>,
    columns: usize,
    rows: Rc<VecModel<TrashRow>>,
    /// The totals as of the list shown (a different count means it is out of date).
    loaded: Option<TrashTotals>,
    /// A read is under way.
    loading: bool,
    /// Asked for confirmation: these assets for good (`None`: empty the trash).
    confirming: Option<Option<Vec<AssetId>>>,
}

thread_local! {
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
}

fn with_state<R>(f: impl FnOnce(&mut State) -> R) -> Option<R> {
    STATE.with_borrow_mut(|state| state.as_mut().map(f))
}

pub fn install(ui: &AppWindow, root: PathBuf, pipeline: Handle) {
    let rows = Rc::new(VecModel::default());
    let bridge = ui.global::<TrashBridge>();
    bridge.set_rows(ModelRc::from(rows.clone()));
    STATE.set(Some(State {
        root,
        pipeline,
        items: Vec::new(),
        thumbs: HashMap::new(),
        selected: HashSet::new(),
        anchor: None,
        columns: 1,
        rows,
        loaded: None,
        loading: false,
        confirming: None,
    }));
    let weak = ui.as_weak();
    bridge.on_columns_changed(move |n| {
        let n = n.max(1) as usize;
        if with_state(|s| std::mem::replace(&mut s.columns, n) != n).unwrap_or(false) {
            if let Some(ui) = weak.upgrade() {
                show(&ui);
            }
        }
    });
    let weak = ui.as_weak();
    bridge.on_click(move |index, extend, toggle| {
        if let Some(ui) = weak.upgrade() {
            click(&ui, index, extend, toggle);
        }
    });
    let weak = ui.as_weak();
    bridge.on_select_all(move || {
        with_state(|s| {
            s.selected = s.items.iter().map(|t| t.asset).collect();
        });
        if let Some(ui) = weak.upgrade() {
            show(&ui);
        }
    });
    let weak = ui.as_weak();
    bridge.on_clear_selection(move || {
        with_state(|s| {
            s.selected.clear();
            s.anchor = None;
        });
        if let Some(ui) = weak.upgrade() {
            show(&ui);
        }
    });
    let weak = ui.as_weak();
    bridge.on_restore(move || {
        if let Some(ui) = weak.upgrade() {
            restore(&ui);
        }
    });
    let weak = ui.as_weak();
    bridge.on_purge(move || {
        if let Some(ui) = weak.upgrade() {
            let assets = with_state(|s| selected_assets(s)).unwrap_or_default();
            if !assets.is_empty() {
                ask(&ui, Some(assets));
            }
        }
    });
    let weak = ui.as_weak();
    bridge.on_empty(move || {
        if let Some(ui) = weak.upgrade() {
            ask(&ui, None);
        }
    });
    let weak = ui.as_weak();
    bridge.on_confirm_changed(move || {
        if let Some(ui) = weak.upgrade() {
            describe_confirm(&ui);
        }
    });
    let weak = ui.as_weak();
    bridge.on_confirm(move || {
        if let Some(ui) = weak.upgrade() {
            confirmed(&ui);
        }
    });
    let weak = ui.as_weak();
    bridge.on_cancel(move || {
        log::debug!("trash: confirmation cancelled");
        with_state(|s| s.confirming = None);
        if let Some(ui) = weak.upgrade() {
            close_confirm(&ui);
        }
    });
}

/// The page is shown: read the trash again.
pub fn entered(ui: &AppWindow) {
    load(ui);
}

/// The pipeline relay counted the trash: the sidebar follows, and the page re-reads the list
/// if it is shown and the totals moved (something was deleted or restored elsewhere).
pub fn totals(ui: &AppWindow, totals: TrashTotals) {
    let bridge = ui.global::<TrashBridge>();
    bridge.set_count(totals.items as i32);
    let stale = with_state(|s| !s.loading && s.loaded.is_some_and(|loaded| loaded != totals)).unwrap_or(false);
    if stale && ui.get_page() == nav::TRASH {
        log::debug!("trash: totals now {totals:?}; reading the list again");
        load(ui);
    } else if ui.get_page() != nav::TRASH {
        // Read again when shown.
        with_state(|s| s.loaded = None);
    }
}

/// Reads the trash on a worker, then shows it.
fn load(ui: &AppWindow) {
    let Some(root) = with_state(|s| {
        s.loading = true;
        s.root.clone()
    }) else {
        return;
    };
    ui.global::<TrashBridge>().set_loading(true);
    let weak = ui.as_weak();
    thread::spawn(move || {
        let result = actions::open_catalog(&root).and_then(|catalog| {
            let items = catalog.trashed().map_err(|e| e.to_string())?;
            let totals = catalog.trash_totals().map_err(|e| e.to_string())?;
            Ok((items, totals))
        });
        let _ = weak.upgrade_in_event_loop(move |ui| loaded(&ui, result));
    });
}

fn loaded(ui: &AppWindow, result: Result<(Vec<Trashed>, TrashTotals), String>) {
    let bridge = ui.global::<TrashBridge>();
    bridge.set_loading(false);
    let (items, totals) = match result {
        Ok(read) => read,
        Err(e) => {
            log::warn!("trash: can't read it: {e}");
            with_state(|s| s.loading = false);
            return notify(ui, format!("Can't read the trash: {e}"), true, false);
        }
    };
    log::debug!("trash: {} items, {} bytes", items.len(), totals.bytes);
    bridge.set_count(totals.items as i32);
    bridge.set_summary(summary(&items).into());
    let generation = GENERATION.fetch_add(1, Ordering::Relaxed) + 1;
    let missing = with_state(|s| {
        s.loading = false;
        s.loaded = Some(totals);
        let present: HashSet<AssetId> = items.iter().map(|t| t.asset).collect();
        s.selected.retain(|a| present.contains(a));
        if s.anchor.is_some_and(|i| i >= items.len()) {
            s.anchor = None;
        }
        s.thumbs.retain(|path, _| items.iter().any(|t| t.in_trash == *path));
        s.items = items;
        s.items.iter().map(|t| t.in_trash.clone()).filter(|p| !s.thumbs.contains_key(p)).collect::<Vec<_>>()
    })
    .unwrap_or_default();
    show(ui);
    if missing.is_empty() {
        return;
    }
    let weak = ui.as_weak();
    thread::spawn(move || {
        thumbs::small_for_files(&missing, |i, small| {
            if GENERATION.load(Ordering::Relaxed) != generation {
                return false;
            }
            let path = missing[i].clone();
            let pixels = small.map(|rgb| Pixels::clone_from_slice(rgb.as_raw(), rgb.width(), rgb.height()));
            let _ = weak.upgrade_in_event_loop(move |_| thumb(path, pixels));
            true
        });
    });
}

/// "1,204 items · 3.4 GB" (or nothing for an empty trash).
fn summary(items: &[Trashed]) -> String {
    match items.len() {
        0 => String::new(),
        n => {
            let bytes: u64 = items.iter().filter_map(|t| t.size).sum();
            format!("{} · {}", if n == 1 { "1 item".to_owned() } else { format!("{} items", grouped(n)) }, human_size(bytes))
        }
    }
}

/// A thumbnail came in (none: the file has none; not asked for again).
fn thumb(path: PathBuf, pixels: Option<Pixels>) {
    with_state(|s| {
        let image = pixels.map(slint::Image::from_rgb8).unwrap_or_default();
        let Some(i) = s.items.iter().position(|t| t.in_trash == path) else { return };
        s.thumbs.insert(path, image);
        let (row, column) = (i / s.columns, i % s.columns);
        if let Some(data) = s.rows.row_data(row) {
            let tiles = data.tiles.clone();
            if let Some(tile) = tiles.row_data(column) {
                tiles.set_row_data(column, tile_of(s, &s.items[tile.index as usize], tile.index as usize));
            }
        }
    });
}

fn tile_of(s: &State, item: &Trashed, index: usize) -> TrashTile {
    let name = item.original.file_name().map_or_else(String::new, |n| n.to_string_lossy().into_owned());
    let folder = item.original.parent().and_then(|p| p.strip_prefix(&s.root).ok()).map_or_else(String::new, |p| p.display().to_string());
    let when = local_time(SystemTime::UNIX_EPOCH + Duration::from_secs(item.trashed_at.max(0) as u64));
    let detail = match item.size {
        Some(size) => format!("Deleted {when} · {}", human_size(size)),
        None => format!("Deleted {when}"),
    };
    let thumb = s.thumbs.get(&item.in_trash);
    TrashTile {
        index: index as i32,
        thumb: thumb.cloned().unwrap_or_default(),
        thumb_ready: thumb.is_some(),
        name: name.into(),
        folder: folder.into(),
        detail: detail.into(),
        selected: s.selected.contains(&item.asset),
    }
}

/// Lays the list out in rows of the page's column count, with the selection.
fn show(ui: &AppWindow) {
    let selection = with_state(|s| {
        let rows: Vec<TrashRow> = s
            .items
            .chunks(s.columns)
            .enumerate()
            .map(|(r, chunk)| {
                let tiles: Vec<TrashTile> = chunk.iter().enumerate().map(|(c, item)| tile_of(s, item, r * s.columns + c)).collect();
                TrashRow { tiles: ModelRc::new(VecModel::from(tiles)) }
            })
            .collect();
        // In place where the row count holds, so the list keeps its scroll position.
        let old = s.rows.row_count();
        for (i, row) in rows.into_iter().enumerate() {
            if i < old {
                s.rows.set_row_data(i, row);
            } else {
                s.rows.push(row);
            }
        }
        let new = s.items.len().div_ceil(s.columns);
        while s.rows.row_count() > new {
            s.rows.remove(s.rows.row_count() - 1);
        }
        s.selected.len()
    })
    .unwrap_or(0);
    ui.global::<TrashBridge>().set_selection(selection as i32);
}

/// Click (replace the selection), Shift-click (a range from the anchor), Ctrl-click
/// (toggle); a click beside the tiles clears it.
fn click(ui: &AppWindow, index: i32, extend: bool, toggle: bool) {
    with_state(|s| {
        let Some(i) = usize::try_from(index).ok().filter(|&i| i < s.items.len()) else {
            if !extend && !toggle {
                s.selected.clear();
                s.anchor = None;
            }
            return;
        };
        let asset = s.items[i].asset;
        match (extend, s.anchor) {
            (true, Some(anchor)) => {
                if !toggle {
                    s.selected.clear();
                }
                let (from, to) = (anchor.min(i), anchor.max(i));
                s.selected.extend(s.items[from..=to].iter().map(|t| t.asset));
            }
            _ if toggle => {
                if !s.selected.remove(&asset) {
                    s.selected.insert(asset);
                }
                s.anchor = Some(i);
            }
            _ => {
                s.selected = HashSet::from([asset]);
                s.anchor = Some(i);
            }
        }
    });
    show(ui);
}

/// The selection, in the list's order.
fn selected_assets(s: &State) -> Vec<AssetId> {
    s.items.iter().map(|t| t.asset).filter(|a| s.selected.contains(a)).collect()
}

fn file_name(path: &Path) -> String {
    path.file_name().map_or_else(|| path.display().to_string(), |n| n.to_string_lossy().into_owned())
}

/// Puts the selection back where it was (each with its back, if that is in the trash), on
/// a worker; the files come back to Review.
fn restore(ui: &AppWindow) {
    let Some((root, pipeline, assets)) = with_state(|s| (s.root.clone(), s.pipeline.clone(), selected_assets(s))) else { return };
    if assets.is_empty() {
        return;
    }
    log::debug!("trash: restore {} items", assets.len());
    let bridge = ui.global::<TrashBridge>();
    bridge.set_working(true);
    let weak = ui.as_weak();
    thread::spawn(move || {
        let result = actions::open_catalog(&root).map(|mut catalog| {
            let (mut restored, mut errors) = (Vec::new(), Vec::new());
            for (asset, result) in ops::restore_all(&mut catalog, &assets) {
                match result {
                    Ok(paths) => restored.extend(paths),
                    Err(e) => {
                        log::warn!("trash: restore asset {}: {e}", asset.0);
                        errors.push(e.to_string());
                    }
                }
            }
            (restored, errors)
        });
        if let Ok((paths, _)) = &result {
            for path in paths {
                pipeline::changed(path, false);
            }
            log::info!("trash: restored {} files", paths.len());
        }
        pipeline.recount();
        let _ = weak.upgrade_in_event_loop(move |ui| {
            ui.global::<TrashBridge>().set_working(false);
            match result {
                Ok((paths, errors)) => {
                    let what = match paths.as_slice() {
                        [path] => file_name(path),
                        paths => format!("{} items", grouped(paths.len())),
                    };
                    match errors.first() {
                        None => notify(&ui, format!("Restored {what}"), false, true),
                        Some(e) if paths.is_empty() => notify(&ui, format!("Can't restore: {e}"), true, false),
                        Some(e) => notify(&ui, format!("Restored {what}; {} not ({e})", errors.len()), true, true),
                    }
                }
                Err(e) => notify(&ui, format!("Can't restore: {e}"), true, false),
            }
            load(&ui);
        });
    });
}

/// Opens the confirmation for deleting `assets` for good (`None`: emptying the trash).
fn ask(ui: &AppWindow, assets: Option<Vec<AssetId>>) {
    let bridge = ui.global::<TrashBridge>();
    bridge.set_confirm_mode(if assets.is_some() { "purge" } else { "empty" }.into());
    bridge.set_confirm_error(SharedString::default());
    bridge.set_confirm_older(false);
    with_state(|s| s.confirming = Some(assets));
    describe_confirm(ui);
}

/// What the confirmation will delete: the assets, with their backs in the trash too.
fn doomed(s: &State, older: Option<u64>) -> Vec<&Trashed> {
    let now = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64);
    match &s.confirming {
        Some(Some(assets)) => {
            let backs: Vec<AssetId> = s.items.iter().filter(|t| assets.contains(&t.asset)).filter_map(|t| t.back).collect();
            s.items.iter().filter(|t| assets.contains(&t.asset) || backs.contains(&t.asset)).collect()
        }
        Some(None) => s.items.iter().filter(|t| older.is_none_or(|days| t.trashed_at <= now - days as i64 * 86_400)).collect(),
        None => Vec::new(),
    }
}

fn older_days(ui: &AppWindow) -> Option<u64> {
    let bridge = ui.global::<TrashBridge>();
    (bridge.get_confirm_mode() == "empty" && bridge.get_confirm_older()).then(|| bridge.get_confirm_days().max(1) as u64)
}

/// The confirmation's message: how many items and how much.
fn describe_confirm(ui: &AppWindow) {
    let older = older_days(ui);
    let Some((n, bytes)) = with_state(|s| {
        let doomed = doomed(s, older);
        (doomed.len(), doomed.iter().filter_map(|t| t.size).sum::<u64>())
    }) else {
        return;
    };
    let what = if n == 1 { "1 item".to_owned() } else { format!("{} items", grouped(n)) };
    let message = match (n, older) {
        (0, Some(days)) => format!("Nothing in the trash was deleted more than {days} days ago."),
        (0, None) => "The trash is empty.".to_owned(),
        _ => format!("{what} ({}) will be deleted for good. This can't be undone.", human_size(bytes)),
    };
    ui.global::<TrashBridge>().set_confirm_message(message.into());
}

fn close_confirm(ui: &AppWindow) {
    let bridge = ui.global::<TrashBridge>();
    bridge.set_confirm_mode(SharedString::default());
    bridge.set_confirm_error(SharedString::default());
}

/// Deletes for good what the confirmation said, on a worker.
fn confirmed(ui: &AppWindow) {
    let older = older_days(ui);
    let Some((root, pipeline, Some(which))) = with_state(|s| (s.root.clone(), s.pipeline.clone(), s.confirming.clone())) else { return };
    log::debug!("trash: delete for good: {}", match (&which, older) {
        (Some(assets), _) => format!("{} items", assets.len()),
        (None, Some(days)) => format!("everything deleted more than {days} days ago"),
        (None, None) => "everything".to_owned(),
    });
    let bridge = ui.global::<TrashBridge>();
    bridge.set_working(true);
    let weak = ui.as_weak();
    thread::spawn(move || {
        let result = actions::open_catalog(&root).and_then(|mut catalog| {
            match &which {
                Some(assets) => ops::purge(&mut catalog, assets),
                None => ops::empty_trash(&mut catalog, Duration::from_secs(older.unwrap_or(0) * 86_400)),
            }
            .map_err(|e| e.to_string())
        });
        pipeline.recount();
        let _ = weak.upgrade_in_event_loop(move |ui| {
            let bridge = ui.global::<TrashBridge>();
            bridge.set_working(false);
            match result {
                Ok(n) => {
                    log::info!("trash: deleted {n} items for good");
                    with_state(|s| s.confirming = None);
                    close_confirm(&ui);
                    let what = if n == 1 { "1 item".to_owned() } else { format!("{} items", grouped(n)) };
                    notify(&ui, format!("Deleted {what} for good"), false, false);
                }
                Err(e) => {
                    log::warn!("trash: delete for good: {e}");
                    bridge.set_confirm_error(format!("Can't delete: {e}").into());
                }
            }
            load(&ui);
        });
    });
}
