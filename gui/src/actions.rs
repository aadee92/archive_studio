//! The item context menu: Rotate left / right, Add tag…, front and back (pair as the back of
//! the previous / next item of the view, swap, unpair), Find similar (Review ranked by how
//! much pictures look like it: [`review::find_similar`](crate::review::find_similar)), Show
//! in folder, Copy (the files to the clipboard, [`crate::clipboard`]), Properties, Delete, and for a face confirmed as someone (People's grid) Set as
//! cover ([`people::set_cover`](crate::people::set_cover)), and for faces in People what its
//! view does to them (Not Ann, Un-ignore, …: [`offer_face_action`]),
//! for any item a page shows (a Review tile, a face in People, a recent capture in Acquire,
//! the Details picture and its similar strip). Pages resolve the item under the pointer and
//! call [`nav::context_menu`](crate::nav::context_menu), which lands in [`show_menu`]; the
//! menu itself is a `ContextMenuArea` in `app.slint`, shown where the pointer was. Details'
//! keyboard shortcuts (Delete, T) go through [`act`] too.
//!
//! Right-clicked in a multi-selection (Review), the menu acts on all of its items
//! ([`show_menu_for`], [`act_on`]): rotating turns each in turn ([`ops::rotate_all`]), Add
//! tag tags them all in one transaction ([`ops::add_tag_all`]), Delete trashes them all
//! ([`ops::delete_all`]); each is one change, with one notice whose Undo undoes all of it.
//! Pairing, Show in folder and Properties are for one item and disabled then.
//!
//! Every action is an [`archive_core::ops`] call, run on a worker: rotating goes to the
//! pipeline's engine if it runs in this window (else straight to the core), deleting moves
//! to the library's trash, tagging takes what the user typed
//! ([`ops::add_tag`]), pairing goes through [`ops::pair`] / [`ops::swap_sides`] /
//! [`ops::unpair`]; showing in the file manager is [`desktop::show_in_folder`]. What changed
//! is reported with [`pipeline::changed`], which refreshes every view showing it, as for the
//! engine's own reports (Details' dog-ear and "Front & back" follow a pairing that way).
//! Each is recorded in the catalog's undo journal; the notice after it offers Undo
//! ([`crate::undo`], also Ctrl+Z).

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, SystemTime};

use slint::{ComponentHandle, ModelRc, SharedString, Timer, TimerMode, VecModel};

use crate::nav::Target;
use crate::pipeline::{self, Handle};
use crate::review::human_size;
use crate::{AppWindow, ItemMenu, PropRow};
use archive_core::catalog::{AssetId, Catalog, Source, TagId};
use archive_core::desktop;
use archive_core::media::{classify, MediaKind};
use archive_core::ops::{self, Properties};

/// Tags offered under the tag box.
const MATCHES: usize = 6;
/// How long a notice stays up; one with Undo a little longer.
const NOTICE_FOR: Duration = Duration::from_secs(5);
const UNDO_FOR: Duration = Duration::from_secs(10);

struct State {
    root: PathBuf,
    pipeline: Handle,
    /// The item the menu was opened for.
    target: Option<Target>,
    /// The items it acts on when it was opened on a multi-selection (empty otherwise).
    items: Vec<PathBuf>,
    /// Who the target's face is confirmed as, for "Set as cover" ([`offer_cover`]).
    cover: Option<(TagId, String)>,
    /// The items the tag dialog adds to.
    tagging: Vec<PathBuf>,
    /// Every tag in the catalog, for the tag dialog's matches.
    tags: Vec<String>,
    /// What the notice's button does: undo (`Some(true)`) or redo the last change.
    step: Option<bool>,
    notice_timer: Timer,
    /// Retires the reply for a properties dialog closed meanwhile.
    props_generation: u64,
}

thread_local! {
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
}

fn with_state<R>(f: impl FnOnce(&mut State) -> R) -> Option<R> {
    STATE.with_borrow_mut(|state| state.as_mut().map(f))
}

pub fn install(ui: &AppWindow, root: PathBuf, pipeline: Handle) {
    STATE.set(Some(State {
        root,
        pipeline,
        target: None,
        items: Vec::new(),
        cover: None,
        tagging: Vec::new(),
        tags: Vec::new(),
        step: None,
        notice_timer: Timer::default(),
        props_generation: 0,
    }));
    let menu = ui.global::<ItemMenu>();
    let weak = ui.as_weak();
    menu.on_action(move |action| {
        let (target, items) = with_state(|s| (s.target.clone(), s.items.clone())).unwrap_or_default();
        match (weak.upgrade(), target) {
            (Some(ui), Some(target)) => {
                log::debug!("context menu action {action:?} on {target:?}{}", if items.len() > 1 { format!(" and the {} items selected with it", items.len()) } else { String::new() });
                act_on(&ui, &action, target, items);
            }
            (_, None) => log::warn!("context menu action {action:?} with no item - ignored"),
            (None, _) => {}
        }
    });
    let weak = ui.as_weak();
    menu.on_tag_edited(move |text| {
        if let Some(ui) = weak.upgrade() {
            offer_tags(&ui, &text);
        }
    });
    let weak = ui.as_weak();
    menu.on_tag_submit(move |text| {
        log::debug!("tag dialog: submit {text:?}");
        if let Some(ui) = weak.upgrade() {
            add_tag(&ui, text.trim());
        }
    });
    let weak = ui.as_weak();
    menu.on_tag_cancel(move || {
        log::debug!("tag dialog: cancel (closing)");
        if let Some(ui) = weak.upgrade() {
            ui.global::<ItemMenu>().set_tag_open(false);
            with_state(|s| s.tagging.clear());
        }
    });
    let weak = ui.as_weak();
    menu.on_props_close(move || {
        log::debug!("properties dialog: close");
        if let Some(ui) = weak.upgrade() {
            ui.global::<ItemMenu>().set_props_open(false);
            with_state(|s| s.props_generation += 1);
        }
    });
    let weak = ui.as_weak();
    menu.on_notice_clicked(move || {
        let step = with_state(|s| s.step).flatten();
        log::debug!("notice action clicked ({})", if step == Some(false) { "redo" } else { "undo" });
        if let (Some(ui), Some(undo)) = (weak.upgrade(), step) {
            crate::undo::step(&ui, undo);
        }
    });
}

/// Shows the menu for `target` where the page saw the right-click (`ItemMenu.x` / `y`).
pub fn show_menu(ui: &AppWindow, target: Target) {
    show_menu_for(ui, target, Vec::new());
}

/// Shows the menu for `target`, acting on all of `items` (the multi-selection it was
/// right-clicked in, `target` among them) if there are more than one.
pub fn show_menu_for(ui: &AppWindow, target: Target, items: Vec<PathBuf>) {
    let many = items.len() > 1;
    let rotatable = if many { items.iter().any(|p| rotatable(p)) } else { rotatable(&target.path) };
    // Whether the view has an item before / after it; checked again when picked.
    let (at, len) = (target.scope.current, target.scope.len);
    let count = if many { items.len() } else { 1 };
    with_state(|s| {
        s.target = Some(target);
        s.items = if many { items } else { Vec::new() };
        s.cover = None;
    });
    let menu = ui.global::<ItemMenu>();
    menu.set_cover_for(SharedString::default());
    menu.set_face_action(SharedString::default());
    menu.set_count(count as i32);
    menu.set_can_rotate(rotatable);
    menu.set_can_pair_prev(at.is_some_and(|i| i > 0));
    menu.set_can_pair_next(at.is_some_and(|i| i + 1 < len));
    menu.set_serial(menu.get_serial() + 1);
}

/// Offers "Set as cover for `name`" in the menu just shown (right after [`show_menu`]): its
/// target's face is confirmed as `person`.
pub fn offer_cover(ui: &AppWindow, person: TagId, name: String) {
    ui.global::<ItemMenu>().set_cover_for(name.as_str().into());
    with_state(|s| s.cover = Some((person, name)));
}

/// Offers `title` (People's "Not Ann", "Un-ignore", …) in the menu just shown (right after
/// [`show_menu`]); picking it is [`people::face_menu_action`](crate::people::face_menu_action).
pub fn offer_face_action(ui: &AppWindow, title: String) {
    ui.global::<ItemMenu>().set_face_action(title.into());
}

/// An image whose file can be turned losslessly.
fn rotatable(path: &Path) -> bool {
    classify(path) == Some(MediaKind::Image) && archive_core::pipeline::rotate::can_rotate(path)
}

/// Turns the image at `path` `cw` degrees clockwise: by the engine if it runs in this
/// window (it reports the rotation itself), else by the core here. Blocks for as long as
/// the file takes to rewrite: call it on a worker.
pub fn rotate(root: &Path, pipeline: &Handle, path: &Path, cw: u16) -> Result<(), String> {
    let engine = pipeline.engine();
    log::debug!("rotate {} {cw}° by the {}", path.display(), if engine.is_some() { "engine" } else { "core (no engine here)" });
    let catalog = open(root)?;
    let asset = catalog.asset(path).map_err(|e| e.to_string())?;
    ops::rotate(&catalog, engine.as_deref(), asset, cw)?;
    if engine.is_none() {
        pipeline::changed(path, true);
    }
    Ok(())
}

fn open(root: &Path) -> Result<Catalog, String> {
    open_catalog(root)
}

pub(crate) fn open_catalog(root: &Path) -> Result<Catalog, String> {
    Catalog::open(&Catalog::default_path(root), root).map_err(|e| format!("catalog: {e}"))
}

fn context() -> Option<(PathBuf, Handle)> {
    with_state(|s| (s.root.clone(), s.pipeline.clone()))
}

fn file_name(path: &Path) -> String {
    path.file_name().map_or_else(|| path.display().to_string(), |n| n.to_string_lossy().into_owned())
}

/// Does `action` (an `ItemMenu.action` name) to `target`; also how Details' keys reach them.
pub(crate) fn act(ui: &AppWindow, action: &str, target: Target) {
    act_on(ui, action, target, Vec::new());
}

/// Does `action` to all of `items` if there are more than one (a multi-selection, with
/// `target` the item right-clicked or focused), else to `target`; also how Review's keys
/// reach them.
pub(crate) fn act_on(ui: &AppWindow, action: &str, target: Target, items: Vec<PathBuf>) {
    if items.len() > 1 {
        match action {
            "rotate-left" => rotate_items(ui, items, 270),
            "rotate-right" => rotate_items(ui, items, 90),
            "delete" => delete(ui, items),
            "tag" => open_tag_dialog(ui, items),
            "copy" => copy_files(ui, items),
            other => log::warn!("context menu: {other:?} is for one item, not {} - ignored", items.len()),
        }
        return;
    }
    match action {
        "rotate-left" => rotate_item(ui, target.path, 270),
        "rotate-right" => rotate_item(ui, target.path, 90),
        "delete" => delete(ui, vec![target.path]),
        "tag" => open_tag_dialog(ui, vec![target.path]),
        "properties" => properties(ui, target.path),
        "copy" => copy_files(ui, vec![target.path]),
        "show-in-folder" => show_in_folder(ui, target.path),
        "find-similar" => crate::review::find_similar(ui, &target.path),
        "set-cover" => match (target.face, with_state(|s| s.cover.clone()).flatten()) {
            (Some(face), Some((person, name))) => crate::people::set_cover(ui, person, face, name),
            _ => log::warn!("context menu: set-cover on {target:?} with no named face - ignored"),
        },
        "face-action" => crate::people::face_menu_action(),
        "open-external" => open_externally(ui, target.path),
        "pair-prev" => pair_with_neighbour(ui, target, -1),
        "pair-next" => pair_with_neighbour(ui, target, 1),
        "swap-sides" => change_pair(ui, target.path, PairChange::Swap),
        "unpair" => change_pair(ui, target.path, PairChange::Unpair),
        other => log::warn!("context menu: unknown action {other:?}"),
    }
}

fn rotate_item(ui: &AppWindow, path: PathBuf, cw: u16) {
    let Some((root, pipeline)) = context() else { return };
    let weak = ui.as_weak();
    thread::spawn(move || {
        let result = rotate(&root, &pipeline, &path, cw);
        if result.is_ok() {
            log::info!("rotated {} {cw}°", path.display());
            pipeline.recount();
        }
        let _ = weak.upgrade_in_event_loop(move |ui| match result {
            Ok(()) => notify(&ui, format!("Rotated {} {cw}°", file_name(&path)), false, true),
            Err(e) => {
                log::warn!("rotate {}: {e}", path.display());
                notify(&ui, format!("Can't rotate {}: {e}", file_name(&path)), true, false);
            }
        });
    });
}

/// Turns each of `paths` that can be turned, one after the other on a worker; one notice
/// at the end.
fn rotate_items(ui: &AppWindow, paths: Vec<PathBuf>, cw: u16) {
    let Some((root, pipeline)) = context() else { return };
    let weak = ui.as_weak();
    log::debug!("rotate {} items {cw}°", paths.len());
    thread::spawn(move || {
        let (mut done, mut failed, mut first_error) = (0, 0, None);
        let mut fail = |path: &Path, e: String| {
            log::warn!("rotate {}: {e}", path.display());
            failed += 1;
            first_error.get_or_insert_with(|| format!("{}: {e}", file_name(path)));
        };
        // All in one go (one change to undo), those that can be turned.
        let engine = pipeline.engine();
        match open(&root) {
            Ok(catalog) => {
                let mut assets = Vec::new();
                for path in &paths {
                    match catalog.asset(path) {
                        Ok(asset) if rotatable(path) => assets.push(asset),
                        Ok(_) => fail(path, "can't be rotated losslessly".to_owned()),
                        Err(e) => fail(path, e.to_string()),
                    }
                }
                for (asset, result) in ops::rotate_all(&catalog, engine.as_deref(), &assets, cw) {
                    let path = catalog.asset_path(asset).unwrap_or_default();
                    match result {
                        Ok(()) => {
                            done += 1;
                            if engine.is_none() {
                                pipeline::changed(&path, true);
                            }
                        }
                        Err(e) => fail(&path, e),
                    }
                }
            }
            Err(e) => paths.iter().for_each(|p| fail(p, e.clone())),
        }
        log::info!("rotated {done} of {} items {cw}° ({failed} failed)", paths.len());
        pipeline.recount();
        let _ = weak.upgrade_in_event_loop(move |ui| match first_error {
            None => notify(&ui, format!("Rotated {} items", done), false, true),
            Some(e) => notify(&ui, format!("Rotated {done} of {} items; can't rotate {failed} ({e})", done + failed), done == 0, done > 0),
        });
    });
}

/// A change to the item's front/back pairing.
#[derive(Clone, Debug)]
enum PairChange {
    /// The item is the back of this front.
    BackOf(PathBuf),
    Swap,
    Unpair,
}

/// The item becomes the back of the item `delta` items from it in its view (scanned right
/// before or after it).
fn pair_with_neighbour(ui: &AppWindow, target: Target, delta: isize) {
    let paths = target.scope.paths();
    // The menu's item may not be one of the view's (the other side of a photo on Details).
    let at = target.scope.current.filter(|&i| paths.get(i) == Some(&target.path));
    let Some(front) = at.and_then(|i| i.checked_add_signed(delta)).and_then(|i| paths.get(i)).cloned() else {
        log::debug!("pair with item {delta:+}: {:?} has no such neighbour in its view", target.path);
        notify(ui, format!("{} has no {} item in this view", file_name(&target.path), if delta < 0 { "previous" } else { "next" }), true, false);
        return;
    };
    change_pair(ui, target.path, PairChange::BackOf(front));
}

/// Makes `change` to the pairing of the item at `path` on a worker, and reports every file
/// whose other side changed.
fn change_pair(ui: &AppWindow, path: PathBuf, change: PairChange) {
    let Some((root, _)) = context() else { return };
    let weak = ui.as_weak();
    thread::spawn(move || {
        let result = open(&root).and_then(|mut catalog| {
            let asset = catalog.asset(&path).map_err(|e| e.to_string())?;
            let (changed, done) = match &change {
                PairChange::BackOf(front) => {
                    let front = catalog.asset(front).map_err(|e| e.to_string())?;
                    let changed = ops::pair(&mut catalog, front, asset).map_err(|e| e.to_string())?;
                    let name = catalog.asset_path(front).map(|p| file_name(&p)).unwrap_or_default();
                    (changed, format!("{} is the back of {name}", file_name(&path)))
                }
                PairChange::Swap => {
                    let (front, back) = ops::swap_sides(&mut catalog, asset).map_err(|e| e.to_string())?;
                    let name = |a| catalog.asset_path(a).map(|p| file_name(&p)).unwrap_or_default();
                    (vec![front, back], format!("{} is the front now, {} the back", name(front), name(back)))
                }
                PairChange::Unpair => match ops::unpair(&mut catalog, asset).map_err(|e| e.to_string())? {
                    Some(other) => (vec![asset, other], format!("{} has no other side now", file_name(&path))),
                    None => return Err("it isn't paired with another side".to_owned()),
                },
            };
            for a in changed {
                if let Ok(p) = catalog.asset_path(a) {
                    pipeline::changed(&p, false);
                }
            }
            Ok(done)
        });
        match &result {
            Ok(done) => log::info!("{change:?} on {}: {done}", path.display()),
            Err(e) => log::warn!("{change:?} on {}: {e}", path.display()),
        }
        let _ = weak.upgrade_in_event_loop(move |ui| match result {
            Ok(done) => notify(&ui, done, false, true),
            Err(e) => notify(&ui, format!("Can't change the sides of {}: {e}", file_name(&path)), true, false),
        });
    });
}

/// Shows the item in the file manager, selected if it can ([`desktop::show_in_folder`]
/// waits for the file manager's answer: on a worker).
fn show_in_folder(ui: &AppWindow, path: PathBuf) {
    let weak = ui.as_weak();
    thread::spawn(move || {
        let result = desktop::show_in_folder(&path);
        log::debug!("show in folder {}: {result:?}", path.display());
        if let Err(e) = result {
            let _ = weak.upgrade_in_event_loop(move |ui| notify(&ui, format!("Can't show {}: {e}", file_name(&path)), true, false));
        }
    });
}

/// Puts the files on the clipboard for a file manager (or a text field) to paste
/// ([`crate::clipboard`]).
fn copy_files(ui: &AppWindow, paths: Vec<PathBuf>) {
    let weak = ui.as_weak();
    thread::spawn(move || {
        let result = crate::clipboard::copy_files(&paths);
        let text = match &result {
            Ok(()) if paths.len() == 1 => format!("Copied {}", file_name(&paths[0])),
            Ok(()) => format!("Copied {} items", paths.len()),
            Err(e) => {
                log::warn!("copy {} files: {e}", paths.len());
                format!("Can't copy: {e}")
            }
        };
        let _ = weak.upgrade_in_event_loop(move |ui| notify(&ui, text, result.is_err(), false));
    });
}

/// Opens the item in the desktop's default application for it.
fn open_externally(ui: &AppWindow, path: PathBuf) {
    let weak = ui.as_weak();
    thread::spawn(move || {
        if let Err(e) = desktop::open(&path) {
            let _ = weak.upgrade_in_event_loop(move |ui| notify(&ui, format!("Can't open {}: {e}", file_name(&path)), true, false));
        }
    });
}

/// Moves the items (and their backs, if they are fronts) to the library's trash; one notice
/// whose Undo restores all of them (one change in the journal).
fn delete(ui: &AppWindow, paths: Vec<PathBuf>) {
    let Some((root, pipeline)) = context() else { return };
    let weak = ui.as_weak();
    thread::spawn(move || {
        let result = open(&root).map(|mut catalog| {
            let (mut assets, mut errors) = (Vec::new(), Vec::new());
            for path in &paths {
                match catalog.asset(path) {
                    Ok(asset) => assets.push(asset),
                    Err(e) => errors.push(format!("{}: {e}", file_name(path))),
                }
            }
            let (mut trashed, mut files) = (Vec::new(), 0);
            for (asset, result) in ops::delete_all(&mut catalog, &assets) {
                match result {
                    Ok(items) => {
                        for t in &items {
                            pipeline::changed(&t.original, false);
                        }
                        if !items.is_empty() {
                            trashed.push(asset);
                            files += items.len();
                        }
                    }
                    Err(e) => errors.push(format!("{}: {e}", catalog.asset_path(asset).map(|p| file_name(&p)).unwrap_or_default())),
                }
            }
            (trashed, files, errors)
        });
        pipeline.recount();
        match &result {
            Ok((trashed, files, errors)) => {
                log::info!("deleted {} of {} items ({files} files to the trash){}", trashed.len(), paths.len(), if errors.is_empty() { String::new() } else { format!("; failed: {errors:?}") })
            }
            Err(e) => log::warn!("delete {} items: {e}", paths.len()),
        }
        let _ = weak.upgrade_in_event_loop(move |ui| {
            let (trashed, files, errors) = match result {
                Ok(done) => done,
                Err(e) => return notify(&ui, format!("Can't delete: {e}"), true, false),
            };
            let what = match (paths.as_slice(), trashed.len()) {
                ([path], _) if files > 1 => format!("{} and its back", file_name(path)),
                ([path], _) => file_name(path),
                (_, n) if files > n => format!("{n} items (and {} backs)", files - n),
                (_, n) => format!("{n} items"),
            };
            match errors.first() {
                None => notify(&ui, format!("Moved {what} to the trash"), false, true),
                Some(e) if trashed.is_empty() => notify(&ui, format!("Can't delete {e}"), true, false),
                Some(e) => notify(&ui, format!("Moved {what} to the trash; {} not ({e})", errors.len()), false, true),
            }
        });
    });
}

/// Shows `text` in the notice at the bottom of the window, with Undo if `undo` (the change
/// just made: the journal's latest, see [`crate::undo`]).
pub(crate) fn notify(ui: &AppWindow, text: String, error: bool, undo: bool) {
    notice(ui, text, error, undo.then_some(true));
}

/// Shows `text` in the notice, with a button that undoes (`Some(true)`) or redoes the last
/// change.
pub(crate) fn notice(ui: &AppWindow, text: String, error: bool, step: Option<bool>) {
    let label = match step {
        Some(true) => "Undo",
        Some(false) => "Redo",
        None => "",
    };
    log::debug!("notice{}: {text}{}", if error { " (error)" } else { "" }, if label.is_empty() { String::new() } else { format!(" [{label}]") });
    let menu = ui.global::<ItemMenu>();
    menu.set_notice(text.into());
    menu.set_notice_error(error);
    menu.set_notice_action(label.into());
    let weak = ui.as_weak();
    let stay = if step.is_none() { NOTICE_FOR } else { UNDO_FOR };
    with_state(|s| {
        s.step = step;
        s.notice_timer.start(TimerMode::SingleShot, stay, move || {
            if let Some(ui) = weak.upgrade() {
                hide_notice(&ui);
            }
        });
    });
}

pub(crate) fn hide_notice(ui: &AppWindow) {
    let menu = ui.global::<ItemMenu>();
    menu.set_notice(SharedString::default());
    menu.set_notice_action(SharedString::default());
    with_state(|s| {
        s.step = None;
        s.notice_timer.stop();
    });
}

/// Opens the Add tag dialog for `paths` (one item, or a multi-selection).
fn open_tag_dialog(ui: &AppWindow, paths: Vec<PathBuf>) {
    let Some((root, _)) = context() else { return };
    let shown = match paths.as_slice() {
        [path] => file_name(path),
        _ => format!("{} items", paths.len()),
    };
    log::debug!("tag dialog: open for {shown} ({:?}{})", paths.first(), if paths.len() > 1 { ", …" } else { "" });
    let menu = ui.global::<ItemMenu>();
    menu.set_tag_for(shown.into());
    menu.set_tag_matches(ModelRc::default());
    menu.set_tag_error(SharedString::default());
    menu.set_tag_working(false);
    menu.set_tag_open(true);
    with_state(|s| s.tagging = paths);
    let weak = ui.as_weak();
    thread::spawn(move || {
        let tags = open(&root).and_then(|c| c.all_tags().map_err(|e| e.to_string())).unwrap_or_else(|e| {
            log::warn!("tag dialog: reading the tags: {e}");
            Vec::new()
        });
        log::debug!("tag dialog: {} known tags", tags.len());
        let _ = weak.upgrade_in_event_loop(move |_| {
            with_state(|s| s.tags = tags);
        });
    });
}

/// The known tags matching `text`, those starting with it first.
pub(crate) fn matching_tags<'a>(tags: &'a [String], text: &str) -> Vec<&'a str> {
    let text = text.trim().to_lowercase();
    if text.is_empty() {
        return Vec::new();
    }
    let mut found: Vec<(bool, &str)> = tags
        .iter()
        .filter_map(|tag| {
            let lower = tag.to_lowercase();
            let starts = lower.starts_with(&text) || lower.split('|').any(|segment| segment.trim().starts_with(&text));
            (lower != text && lower.contains(&text)).then_some((!starts, tag.as_str()))
        })
        .collect();
    found.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.to_lowercase().cmp(&b.1.to_lowercase())));
    found.into_iter().take(MATCHES).map(|(_, tag)| tag).collect()
}

fn offer_tags(ui: &AppWindow, text: &str) {
    let offered: Vec<SharedString> =
        with_state(|s| matching_tags(&s.tags, text).into_iter().map(SharedString::from).collect()).unwrap_or_default();
    let menu = ui.global::<ItemMenu>();
    menu.set_tag_matches(ModelRc::new(VecModel::from(offered)));
    menu.set_tag_error(SharedString::default());
}

fn add_tag(ui: &AppWindow, text: &str) {
    let Some((paths, (root, _))) = with_state(|s| s.tagging.clone()).zip(context()) else { return };
    if paths.is_empty() {
        log::debug!("add tag {text:?}: no item being tagged - ignored");
        return;
    }
    if text.is_empty() {
        log::debug!("add tag: empty - ignored");
        return;
    }
    let menu = ui.global::<ItemMenu>();
    menu.set_tag_working(true);
    let (text, weak) = (text.to_owned(), ui.as_weak());
    thread::spawn(move || {
        let result = open(&root).and_then(|mut catalog| {
            let assets = paths.iter().map(|p| catalog.asset(p)).collect::<Result<Vec<_>, _>>().map_err(|e| e.to_string())?;
            match assets.as_slice() {
                [asset] => ops::add_tag(&mut catalog, *asset, &text),
                _ => ops::add_tag_all(&mut catalog, &assets, &text),
            }
            .map_err(|e| e.to_string())
        });
        match &result {
            Ok(_) => log::info!("tagged {} items ({:?}…): {text:?}", paths.len(), paths[0]),
            Err(e) => log::warn!("tag {} items ({:?}…) {text:?}: {e}", paths.len(), paths[0]),
        }
        if result.is_ok() {
            for path in &paths {
                pipeline::changed(path, false);
            }
        }
        let _ = weak.upgrade_in_event_loop(move |ui| {
            let menu = ui.global::<ItemMenu>();
            menu.set_tag_working(false);
            match result {
                Ok(_) => {
                    menu.set_tag_open(false);
                    with_state(|s| s.tagging.clear());
                    let (_, shown) = ops::parse_tag(&text);
                    let what = match paths.as_slice() {
                        [path] => file_name(path),
                        _ => format!("{} items", paths.len()),
                    };
                    notify(&ui, format!("Tagged {what}: {}", shown.replace('|', " / ")), false, true);
                }
                Err(e) => menu.set_tag_error(e.into()),
            }
        });
    });
}

fn properties(ui: &AppWindow, path: PathBuf) {
    let Some((root, _)) = context() else { return };
    let generation = with_state(|s| {
        s.props_generation += 1;
        s.props_generation
    })
    .unwrap_or_default();
    log::debug!("properties dialog: open #{generation} for {}", path.display());
    let menu = ui.global::<ItemMenu>();
    menu.set_props_title(file_name(&path).into());
    menu.set_props(ModelRc::default());
    menu.set_props_loading(true);
    menu.set_props_open(true);
    let weak = ui.as_weak();
    thread::spawn(move || {
        let rows = open(&root).and_then(|catalog| {
            let asset = catalog.asset(&path).map_err(|e| e.to_string())?;
            let p = ops::properties(&catalog, asset).map_err(|e| e.to_string())?;
            let side = |a: Option<AssetId>| a.and_then(|a| catalog.asset_path(a).ok());
            Ok(property_rows(&p, &root, side(p.front), side(p.back)))
        });
        let _ = weak.upgrade_in_event_loop(move |ui| {
            if with_state(|s| s.props_generation) != Some(generation) {
                log::debug!("properties #{generation}: dialog closed or reopened meanwhile - dropped");
                return;
            }
            if let Err(e) = &rows {
                log::warn!("properties: {e}");
            }
            let menu = ui.global::<ItemMenu>();
            menu.set_props_loading(false);
            let rows = rows.unwrap_or_else(|e| vec![row("Error", e)]);
            let rows: Vec<PropRow> =
                rows.into_iter().map(|(label, value, heading)| PropRow { label: label.into(), value: value.into(), heading }).collect();
            menu.set_props(ModelRc::new(VecModel::from(rows)));
        });
    });
}

type Row = (String, String, bool);

fn row(label: &str, value: impl Into<String>) -> Row {
    (label.to_owned(), value.into(), false)
}

fn heading(label: &str) -> Row {
    (label.to_owned(), String::new(), true)
}

/// The Properties dialog's lines, in sections; empty fields are left out.
fn property_rows(p: &Properties, root: &Path, front: Option<PathBuf>, back: Option<PathBuf>) -> Vec<Row> {
    let shown = |path: &Path| path.strip_prefix(root).unwrap_or(path).display().to_string();
    let mut rows = vec![heading("File"), row("Path", p.path.display().to_string())];
    if p.trashed {
        rows.push(row("Status", "In the trash"));
    } else if p.missing {
        rows.push(row("Status", "Missing"));
    }
    if let Some(size) = p.size {
        rows.push(row("Size", format!("{} ({size} bytes)", human_size(size))));
    }
    if let Some(format) = &p.format {
        rows.push(row("Format", format.to_uppercase()));
    }
    if let Some((w, h)) = p.dimensions {
        rows.push(row("Dimensions", format!("{w} × {h}")));
    }
    if let Some(t) = p.modified {
        rows.push(row("Modified", local_time(t)));
    }
    if let Some(t) = p.created {
        rows.push(row("Created", local_time(t)));
    }

    let e = &p.exif;
    let camera = [e.make.as_deref(), e.model.as_deref()].into_iter().flatten().collect::<Vec<_>>().join(" ");
    let exif: Vec<Row> = [
        ("Camera", camera),
        ("Taken", e.taken.clone().unwrap_or_default()),
        ("Digitized", e.digitized.clone().unwrap_or_default()),
        ("Edited", e.modified.clone().unwrap_or_default()),
        ("Software", e.software.clone().unwrap_or_default()),
        ("Exposure", e.exposure_time.map(exposure).unwrap_or_default()),
        ("Aperture", e.f_number.map_or_else(String::new, |f| format!("f/{f:.1}"))),
        ("ISO", e.iso.map_or_else(String::new, |i| i.to_string())),
        ("Focal length", e.focal_length.map_or_else(String::new, |f| format!("{f:.0} mm"))),
        ("Orientation", e.orientation.filter(|&o| o != 1).map_or_else(String::new, |o| format!("EXIF {o}"))),
    ]
    .into_iter()
    .filter(|(_, v)| !v.is_empty())
    .map(|(label, value)| row(label, value))
    .collect();
    if !exif.is_empty() {
        rows.push(heading("EXIF"));
        rows.extend(exif);
    }

    if !p.tags.is_empty() {
        rows.push(heading("Tags"));
        for tag in &p.tags {
            let by = match (&tag.source, tag.confidence) {
                (Source::User, _) => String::new(),
                (Source::Detector(name), Some(c)) => format!("  ({name}, {:.0}%)", c * 100.0),
                (Source::Detector(name), None) => format!("  ({name})"),
            };
            let kind = tag.kind.name();
            let kind = kind[..1].to_uppercase() + &kind[1..];
            rows.push(row(&kind, format!("{}{by}", tag.path.replace('|', " / "))));
        }
    }

    if !p.faces.is_empty() {
        rows.push(heading("Faces"));
        let named: Vec<&str> = p.faces.iter().filter_map(|f| f.person.as_ref().map(|(_, n)| n.as_str())).collect();
        let ignored = p.faces.iter().filter(|f| f.status == archive_core::catalog::faces::FaceStatus::Ignored).count();
        let unknown = p.faces.len() - named.len() - ignored;
        rows.push(row("Found", p.faces.len().to_string()));
        if !named.is_empty() {
            rows.push(row("Named", named.join(", ")));
        }
        if unknown > 0 {
            rows.push(row("Unknown", unknown.to_string()));
        }
        if ignored > 0 {
            rows.push(row("Ignored", ignored.to_string()));
        }
    }

    if front.is_some() || back.is_some() {
        rows.push(heading("Two-sided photo"));
        if let Some(back) = back {
            rows.push(row("Back", shown(&back)));
        }
        if let Some(front) = front {
            rows.push(row("Front", shown(&front)));
        }
    }

    let r = &p.rotation;
    if !r.detail.is_empty() || r.proposed_cw.is_some() {
        rows.push(heading("Rotation"));
        if let Some(cw) = r.proposed_cw {
            rows.push(row("Proposed", format!("{cw}° clockwise, waiting for you")));
        }
        if !r.detail.is_empty() {
            rows.push(row(if r.by_user { "By you" } else { "Orient" }, r.detail.clone()));
        }
    }

    if !p.stages.is_empty() {
        rows.push(heading("Pipeline"));
        for s in &p.stages {
            let status = format!("{:?}", s.status).to_lowercase();
            let mut value = if s.detail.is_empty() { status } else { format!("{status} · {}", s.detail) };
            if !s.current {
                value.push_str(" (on an older version of the file)");
            }
            rows.push(row(&s.stage, value));
        }
    }
    rows
}

/// `1/250 s`, `2 s`.
fn exposure(seconds: f64) -> String {
    if seconds > 0.0 && seconds < 1.0 {
        format!("1/{:.0} s", 1.0 / seconds)
    } else {
        format!("{seconds} s")
    }
}

/// `2024-05-12 14:30`, in local time.
pub(crate) fn local_time(t: SystemTime) -> String {
    let Ok(since) = t.duration_since(SystemTime::UNIX_EPOCH) else { return String::new() };
    let secs = since.as_secs() as libc::time_t;
    // SAFETY: `localtime_r` only writes the `tm` we pass it.
    let tm = unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&secs, &mut tm).is_null() {
            return String::new();
        }
        tm
    };
    format!("{:04}-{:02}-{:02} {:02}:{:02}", tm.tm_year + 1900, tm.tm_mon + 1, tm.tm_mday, tm.tm_hour, tm.tm_min)
}

#[cfg(test)]
mod tests {
    use super::*;
    use archive_core::catalog::TagKind;

    #[test]
    fn tag_matches() {
        let tags: Vec<String> = ["People", "People|Alice", "People|Bob", "Places|Alicante", "Holidays|Malice"].map(String::from).into();
        assert_eq!(matching_tags(&tags, "ali"), ["People|Alice", "Places|Alicante", "Holidays|Malice"]);
        assert_eq!(matching_tags(&tags, "people"), ["People|Alice", "People|Bob"], "the typed tag itself isn't offered");
        assert!(matching_tags(&tags, "  ").is_empty());
    }

    #[test]
    fn properties_read_as_rows() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let front = root.join("photo-0001-front.png");
        image::RgbImage::new(30, 20).save(&front).unwrap();
        image::RgbImage::new(30, 20).save(root.join("photo-0001-back.png")).unwrap();
        let catalog = Catalog::open(&Catalog::default_path(&root), &root).unwrap();
        let asset = catalog.asset(&front).unwrap();
        catalog.asset(&root.join("photo-0001-back.png")).unwrap();
        catalog.add_tag_path(asset, "Places|Home", TagKind::Place).unwrap();
        let p = ops::properties(&catalog, asset).unwrap();
        let back = p.back.and_then(|b| catalog.asset_path(b).ok());
        assert!(back.is_some(), "paired by name");
        let rows = property_rows(&p, &root, None, back);
        let find = |label: &str| rows.iter().find(|r| r.0 == label && !r.2).map(|r| r.1.as_str());
        assert_eq!(find("Dimensions"), Some("30 × 20"));
        assert_eq!(find("Format"), Some("PNG"));
        assert_eq!(find("Place"), Some("Places / Home"));
        assert_eq!(find("Back"), Some("photo-0001-back.png"));
        assert!(find("Modified").is_some_and(|t| t.len() == 16));
        assert_eq!(exposure(0.004), "1/250 s");
    }
}
