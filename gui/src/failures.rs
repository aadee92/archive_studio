//! The Failures page: every pipeline stage that failed on the current file of an item
//! ([`Catalog::failures`](archive_core::catalog::Catalog::failures)), grouped by stage under
//! a heading with its count and "Retry all", each with the file, the error and when. Select
//! with click, Shift-click, Ctrl-click, Ctrl+A, Esc; Retry runs the stage again on the
//! selection ([`ops::retry`]: through the engine when it runs in this window, which then
//! starts on them at once; else in the catalog, for the pipeline to pick up when it next
//! looks). Double-click (or Open, Enter) shows the item on Details, stepping through the
//! failures; right-click gives the item menu; Show in folder.
//!
//! The sidebar shows the page while anything failed. The status line's "N failed" opens it
//! (Review's "Failed" chip still filters the grid). The list is read on a worker when the
//! page is shown and whenever the pipeline relay counts again while it is ([`recounted`]:
//! a stage failed, or a retry ran), keeping the selection; an unchanged list isn't redrawn.

use std::cell::RefCell;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::thread;
use std::time::{Duration, SystemTime};

use slint::{ComponentHandle, Model, ModelRc, VecModel};

use crate::actions::{self, local_time, notify};
use crate::nav::{self, Scope, Target};
use crate::pipeline::{self, Handle};
use crate::review::grouped;
use crate::{AppWindow, FailureRow, FailuresBridge};
use archive_core::catalog::runs::{Failure, Retry};
use archive_core::catalog::AssetId;
use archive_core::{desktop, ops};

/// A failure: the stage on the asset.
type Key = (AssetId, String);

fn key(f: &Failure) -> Key {
    (f.asset, f.stage.clone())
}

struct State {
    root: PathBuf,
    pipeline: Handle,
    items: Vec<Failure>,
    /// Row of the page -> index into `items` (`None` for a stage's heading).
    rows: Vec<Option<usize>>,
    model: Rc<VecModel<FailureRow>>,
    selected: HashSet<Key>,
    /// Where a Shift-click range starts (an index of `items`).
    anchor: Option<usize>,
    loading: bool,
}

thread_local! {
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
}

fn with_state<R>(f: impl FnOnce(&mut State) -> R) -> Option<R> {
    STATE.with_borrow_mut(|state| state.as_mut().map(f))
}

pub fn install(ui: &AppWindow, root: PathBuf, pipeline: Handle) {
    let model = Rc::new(VecModel::default());
    let bridge = ui.global::<FailuresBridge>();
    bridge.set_rows(ModelRc::from(model.clone()));
    STATE.set(Some(State {
        root,
        pipeline,
        items: Vec::new(),
        rows: Vec::new(),
        model,
        selected: HashSet::new(),
        anchor: None,
        loading: false,
    }));
    let weak = ui.as_weak();
    bridge.on_click(move |row, extend, toggle| {
        if let Some(ui) = weak.upgrade() {
            click(&ui, row, extend, toggle);
        }
    });
    let weak = ui.as_weak();
    bridge.on_select_all(move || {
        with_state(|s| s.selected = s.items.iter().map(key).collect());
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
    bridge.on_retry(move || {
        let runs = with_state(|s| s.items.iter().map(key).filter(|k| s.selected.contains(k)).collect::<Vec<_>>()).unwrap_or_default();
        if let (Some(ui), false) = (weak.upgrade(), runs.is_empty()) {
            retry(&ui, Retry::Runs(runs));
        }
    });
    let weak = ui.as_weak();
    bridge.on_retry_stage(move |stage| {
        if let Some(ui) = weak.upgrade() {
            retry(&ui, Retry::Stage(stage.into()));
        }
    });
    let weak = ui.as_weak();
    bridge.on_open(move |row| {
        let item = with_state(|s| s.rows.get(row as usize).copied().flatten()).flatten();
        if let (Some(ui), Some(i)) = (weak.upgrade(), item) {
            open(&ui, i);
        }
    });
    let weak = ui.as_weak();
    bridge.on_open_selected(move || {
        if let (Some(ui), Some(i)) = (weak.upgrade(), selected_one()) {
            open(&ui, i);
        }
    });
    let weak = ui.as_weak();
    bridge.on_context_menu(move |row| {
        let item = with_state(|s| s.rows.get(row as usize).copied().flatten()).flatten();
        if let (Some(ui), Some(i)) = (weak.upgrade(), item) {
            if let Some(target) = target(i) {
                nav::context_menu(&ui, target);
            }
        }
    });
    let weak = ui.as_weak();
    bridge.on_show_in_folder(move || {
        let path = selected_one().and_then(|i| with_state(|s| s.items[i].path.clone()));
        if let (Some(ui), Some(path)) = (weak.upgrade(), path) {
            show_in_folder(&ui, path);
        }
    });
}

/// The page is shown: read the failures again.
pub fn entered(ui: &AppWindow) {
    load(ui);
}

/// The pipeline relay counted again (`failed` files have a failed stage): the sidebar
/// follows, and the page re-reads the list if it is shown.
pub fn recounted(ui: &AppWindow, failed: usize) {
    ui.global::<FailuresBridge>().set_count(failed as i32);
    if ui.get_page() == nav::FAILURES && !with_state(|s| s.loading).unwrap_or(true) {
        load(ui);
    }
}

fn load(ui: &AppWindow) {
    let Some(root) = with_state(|s| {
        s.loading = true;
        s.root.clone()
    }) else {
        return;
    };
    ui.global::<FailuresBridge>().set_loading(true);
    let weak = ui.as_weak();
    thread::spawn(move || {
        let result = actions::open_catalog(&root).and_then(|catalog| catalog.failures().map_err(|e| e.to_string()));
        let _ = weak.upgrade_in_event_loop(move |ui| loaded(&ui, result));
    });
}

fn loaded(ui: &AppWindow, result: Result<Vec<Failure>, String>) {
    let bridge = ui.global::<FailuresBridge>();
    bridge.set_loading(false);
    with_state(|s| s.loading = false);
    let items = match result {
        Ok(items) => items,
        Err(e) => {
            log::warn!("failures: can't read them: {e}");
            return notify(ui, format!("Can't read the failures: {e}"), true, false);
        }
    };
    let files: HashSet<AssetId> = items.iter().map(|f| f.asset).collect();
    bridge.set_count(files.len() as i32);
    let same = with_state(|s| s.items == items).unwrap_or(false);
    if same {
        return;
    }
    log::debug!("failures: {} runs on {} files", items.len(), files.len());
    with_state(|s| {
        let present: HashSet<Key> = items.iter().map(key).collect();
        s.selected.retain(|k| present.contains(k));
        s.anchor = None;
        s.items = items;
    });
    show(ui);
}

/// The rows: a heading per stage, then its failures.
fn show(ui: &AppWindow) {
    let selection = with_state(|s| {
        let mut rows = Vec::new();
        let mut index = Vec::new();
        for (i, f) in s.items.iter().enumerate() {
            if i == 0 || s.items[i - 1].stage != f.stage {
                let count = s.items[i..].iter().take_while(|g| g.stage == f.stage).count();
                rows.push(FailureRow { header: true, stage: f.stage.as_str().into(), count: count as i32, ..FailureRow::default() });
                index.push(None);
            }
            rows.push(row_of(s, f));
            index.push(Some(i));
        }
        // In place where the row count holds, so the list keeps its scroll position.
        let old = s.model.row_count();
        for (i, row) in rows.into_iter().enumerate() {
            if i < old {
                if s.model.row_data(i).as_ref() != Some(&row) {
                    s.model.set_row_data(i, row);
                }
            } else {
                s.model.push(row);
            }
        }
        while s.model.row_count() > index.len() {
            s.model.remove(s.model.row_count() - 1);
        }
        s.rows = index;
        s.selected.len()
    })
    .unwrap_or(0);
    ui.global::<FailuresBridge>().set_selection(selection as i32);
}

fn row_of(s: &State, f: &Failure) -> FailureRow {
    let name = f.path.file_name().map_or_else(String::new, |n| n.to_string_lossy().into_owned());
    let folder = f.path.parent().and_then(|p| p.strip_prefix(&s.root).ok()).map_or_else(String::new, |p| p.display().to_string());
    FailureRow {
        header: false,
        stage: f.stage.as_str().into(),
        count: 0,
        name: name.into(),
        folder: folder.into(),
        error: f.error.lines().next().unwrap_or_default().into(),
        time: local_time(SystemTime::UNIX_EPOCH + Duration::from_secs(f.at.max(0) as u64)).into(),
        selected: s.selected.contains(&key(f)),
    }
}

/// Click (replace the selection), Shift-click (a range from the anchor), Ctrl-click (toggle).
fn click(ui: &AppWindow, row: i32, extend: bool, toggle: bool) {
    with_state(|s| {
        let Some(i) = s.rows.get(row as usize).copied().flatten() else { return };
        let k = key(&s.items[i]);
        match (extend, s.anchor) {
            (true, Some(anchor)) => {
                if !toggle {
                    s.selected.clear();
                }
                let (from, to) = (anchor.min(i), anchor.max(i));
                let range: Vec<Key> = s.items[from..=to].iter().map(key).collect();
                s.selected.extend(range);
            }
            _ if toggle => {
                if !s.selected.remove(&k) {
                    s.selected.insert(k);
                }
                s.anchor = Some(i);
            }
            _ => {
                s.selected = HashSet::from([k]);
                s.anchor = Some(i);
            }
        }
    });
    show(ui);
}

/// The one failure selected, if exactly one is.
fn selected_one() -> Option<usize> {
    with_state(|s| {
        let mut chosen = s.items.iter().enumerate().filter(|(_, f)| s.selected.contains(&key(f)));
        match (chosen.next(), chosen.next()) {
            (Some((i, _)), None) => Some(i),
            _ => None,
        }
    })
    .flatten()
}

/// Failure `i` as an item of the page, which Details steps through (a file failed in two
/// stages is there twice).
fn target(i: usize) -> Option<Target> {
    with_state(|s| {
        let paths: Vec<PathBuf> = s.items.iter().map(|f| f.path.clone()).collect();
        let path = paths[i].clone();
        Target::new(path, Scope::of("Failures".into(), paths, Some(i)))
    })
}

fn open(ui: &AppWindow, i: usize) {
    if let Some(target) = target(i) {
        nav::open(ui, target);
    }
}

fn show_in_folder(ui: &AppWindow, path: PathBuf) {
    let weak = ui.as_weak();
    thread::spawn(move || {
        if let Err(e) = desktop::show_in_folder(&path) {
            let name = file_name(&path);
            let _ = weak.upgrade_in_event_loop(move |ui| notify(&ui, format!("Can't show {name}: {e}"), true, false));
        }
    });
}

fn file_name(path: &Path) -> String {
    path.file_name().map_or_else(|| path.display().to_string(), |n| n.to_string_lossy().into_owned())
}

/// Runs the failed stages `which` names again, on a worker: by the engine if it runs in
/// this window, else in the catalog. The files are reported changed (their badges in
/// Review follow) and the list is read again.
fn retry(ui: &AppWindow, which: Retry) {
    let Some((root, pipeline, paths)) = with_state(|s| {
        let paths: Vec<PathBuf> = s
            .items
            .iter()
            .filter(|f| match &which {
                Retry::All => true,
                Retry::Stage(stage) => f.stage == *stage,
                Retry::Runs(runs) => runs.contains(&key(f)),
            })
            .map(|f| f.path.clone())
            .collect();
        (s.root.clone(), s.pipeline.clone(), paths)
    }) else {
        return;
    };
    log::debug!("failures: retry {which:?}");
    ui.global::<FailuresBridge>().set_working(true);
    let weak = ui.as_weak();
    thread::spawn(move || {
        let engine = pipeline.engine();
        let result = actions::open_catalog(&root).and_then(|catalog| ops::retry(&catalog, engine.as_deref(), which));
        for path in &paths {
            pipeline::changed(path, false);
        }
        pipeline.recount();
        let _ = weak.upgrade_in_event_loop(move |ui| {
            ui.global::<FailuresBridge>().set_working(false);
            match result {
                // Counted as listed (a whole stage's retry also forgets runs on files since
                // changed or gone, which weren't shown).
                Ok(_) => {
                    let n = paths.len();
                    let what = if n == 1 { "1 failed stage".to_owned() } else { format!("{} failed stages", grouped(n)) };
                    let when = if engine.is_some() { "now" } else { "when the pipeline next runs" };
                    notify(&ui, format!("Retrying {what} {when}"), false, false);
                    with_state(|s| s.selected.clear());
                }
                Err(e) => notify(&ui, format!("Can't retry: {e}"), true, false),
            }
            load(&ui);
        });
    });
}
