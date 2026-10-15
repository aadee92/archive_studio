//! People mode: the person tags with faces, "Unknown" first, and the face crops of the one
//! selected, where faces are selected and named.
//!
//! A worker thread reads the people and the selected person's faces from the catalog (faces
//! are put in similarity order, which is quadratic). Crops are small JPEGs in the catalog,
//! decoded on the UI thread only for the rows on screen and cached.
//!
//! Naming or ignoring faces doesn't reorder the grid: the faces stay where they are, masked
//! with the name, until the grid is reloaded (another person picked, the page shown again).
//! The catalog is written by a writer thread, which then refreshes the suggestions, so
//! faces like the ones just named are listed under that person on the next load.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use slint::{ComponentHandle, Image, Model, ModelNotify, ModelRc, ModelTracker, SharedString, VecModel};

use crate::nav::{self, Scope, Target};
use crate::thumbs::Pixels;
use crate::{AppWindow, FaceRow, FaceTile, PeopleBridge, PersonItem, ScrollRestore};
use archive_core::catalog::faces::{FaceFilter, FaceId, FaceOrder, DEFAULT_MIN_SIMILARITY};
use archive_core::catalog::{Catalog, TagId, TagKind};

/// `PersonItem.key` of the faces nobody has named.
const UNKNOWN: i32 = -1;
/// Decoded crops kept; ~75 KB each at the stored 160 px.
const CROP_CACHE: usize = 3000;
/// Where a typed name without a `|` goes.
const PEOPLE: &str = "People";
/// Names offered under the name box.
const MATCHES: usize = 6;
/// The writer waits this long for more changes before refreshing the suggestions.
const SETTLE: Duration = Duration::from_millis(500);

struct FaceRef {
    id: FaceId,
    path: PathBuf,
    detail: SharedString,
    suggested: bool,
    /// Named or ignored since the grid was loaded.
    mask: Option<Mask>,
}

struct Mask {
    label: SharedString,
    ignored: bool,
    /// The change that put it there, to take it off again if that fails.
    change: u64,
}

/// A person tag, for the name box.
struct Name {
    /// "Alice" for `People|Family|Alice`.
    display: String,
    path: String,
}

/// What a worker read for one selection.
struct Loaded {
    people: People,
    selected: i32,
    title: String,
    faces: Vec<FaceRef>,
}

/// The people list and every person tag.
struct People {
    items: Vec<PersonItem>,
    names: Vec<(String, String)>,
}

/// A change for the writer thread.
enum Change {
    Name { change: u64, faces: Vec<FaceId>, path: String },
    Ignore { change: u64, faces: Vec<FaceId> },
}

/// The face grid: `faces` cut into rows of `columns`.
struct FaceGrid {
    ui: slint::Weak<AppWindow>,
    root: PathBuf,
    /// The UI thread's connection, for crops.
    catalog: Option<Catalog>,
    faces: RefCell<Vec<FaceRef>>,
    columns: Cell<usize>,
    crops: RefCell<HashMap<FaceId, Image>>,
    /// Bumped per load; a worker's result for an older one is dropped.
    generation: Cell<u64>,
    notify: ModelNotify,
    /// Positions in `faces`.
    selection: RefCell<BTreeSet<usize>>,
    /// Where a shift-click range starts.
    anchor: Cell<Option<usize>>,
    /// What a drag adds its rubber band to.
    drag_base: RefCell<BTreeSet<usize>>,
    names: RefCell<Vec<Name>>,
    writer: mpsc::Sender<Change>,
    changes: Cell<u64>,
    /// A view to return to (Back) once the load in flight lands.
    restore: RefCell<Option<Snapshot>>,
}

/// The People view as it was left, for Back (see `nav`): whose faces, which were
/// selected, and the scroll position. Faces are kept by id, so this holds across a reload.
pub(crate) struct Snapshot {
    person: i32,
    selected: Vec<FaceId>,
    anchor: Option<FaceId>,
    /// Where a shift-click range starts.
    anchor_click: Option<FaceId>,
    /// How far the first row in view was scrolled past.
    offset: f32,
}

thread_local! {
    static GRID: RefCell<Option<Rc<FaceGrid>>> = const { RefCell::new(None) };
}

fn with_grid(f: impl FnOnce(&Rc<FaceGrid>)) {
    GRID.with_borrow(|grid| {
        if let Some(grid) = grid {
            f(grid)
        }
    });
}

pub fn install(ui: &AppWindow, root: PathBuf) {
    let catalog = Catalog::open(&Catalog::default_path(&root), &root).ok();
    let grid = Rc::new(FaceGrid {
        ui: ui.as_weak(),
        root: root.clone(),
        catalog,
        faces: RefCell::default(),
        columns: Cell::new(1),
        crops: RefCell::default(),
        generation: Cell::new(0),
        notify: ModelNotify::default(),
        selection: RefCell::default(),
        anchor: Cell::new(None),
        drag_base: RefCell::default(),
        names: RefCell::default(),
        writer: spawn_writer(root, ui.as_weak()),
        changes: Cell::new(0),
        restore: RefCell::default(),
    });
    GRID.set(Some(grid.clone()));

    let bridge = ui.global::<PeopleBridge>();
    bridge.set_rows(ModelRc::from(grid.clone()));
    bridge.on_select(|key| with_grid(|g| g.load(Some(key))));
    bridge.on_columns_changed(|n| {
        with_grid(|g| {
            // Also sent each time the page is shown; only a change re-lays the grid.
            if g.columns.replace(n.max(1) as usize) != n.max(1) as usize {
                g.notify.reset();
            }
        })
    });
    bridge.on_open(|row, col| {
        with_grid(|g| {
            if let (Some(ui), Some(target)) = (g.ui.upgrade(), g.position(row, col).map(|i| g.target(i))) {
                nav::open(&ui, target);
            }
        })
    });
    bridge.on_context_menu(|row, col| {
        with_grid(|g| {
            if let (Some(ui), Some(target)) = (g.ui.upgrade(), g.position(row, col).map(|i| g.target(i))) {
                nav::context_menu(&ui, target);
            }
        })
    });
    bridge.on_click(|row, col, extend, toggle| with_grid(|g| g.click(g.position(row, col), extend, toggle)));
    bridge.on_drag_begin(|keep| {
        with_grid(|g| {
            *g.drag_base.borrow_mut() = if keep { g.selection.borrow().clone() } else { BTreeSet::new() };
        })
    });
    bridge.on_drag_to(|r0, r1, c0, c1| with_grid(|g| g.drag_to(r0, r1, c0, c1)));
    bridge.on_select_all(|| with_grid(|g| g.set_selection((0..g.faces.borrow().len()).collect())));
    bridge.on_clear_selection(|| with_grid(|g| g.set_selection(BTreeSet::new())));
    bridge.on_name_edited(|text| with_grid(|g| g.offer_names(&text)));
    bridge.on_assign(|name| with_grid(|g| g.assign(&name)));
    bridge.on_ignore(|| with_grid(|g| g.ignore()));
}

/// The page was picked in the sidebar: faces get named elsewhere (and found by scans), so
/// the catalog is re-read.
pub fn entered() {
    with_grid(|g| g.load(None));
}

pub(crate) fn snapshot() -> Option<Snapshot> {
    GRID.with_borrow(|grid| {
        let g = grid.as_ref()?;
        let ui = g.ui.upgrade()?;
        let bridge = ui.global::<PeopleBridge>();
        let faces = g.faces.borrow();
        let first = bridge.get_scroll_row().max(0) as usize * g.columns.get();
        Some(Snapshot {
            person: bridge.get_selected(),
            selected: g.selection.borrow().iter().filter_map(|&i| faces.get(i)).map(|f| f.id).collect(),
            anchor: faces.get(first).map(|f| f.id),
            anchor_click: g.anchor.get().and_then(|i| faces.get(i)).map(|f| f.id),
            offset: bridge.get_scroll_offset(),
        })
    })
}

/// Back to a view left earlier. The grid isn't re-read if it still shows that person
/// (the faces stay where they were, as do any just named); only the people list is.
pub(crate) fn restore(snapshot: Snapshot) {
    with_grid(|g| {
        let Some(ui) = g.ui.upgrade() else { return };
        let bridge = ui.global::<PeopleBridge>();
        if bridge.get_selected() == snapshot.person && !bridge.get_loading() {
            g.apply_restore(snapshot);
            g.load_people();
        } else {
            let person = snapshot.person;
            *g.restore.borrow_mut() = Some(snapshot);
            g.load(Some(person));
        }
    });
}

/// Re-reads the catalog if the page is on screen (e.g. the pipeline just grouped faces);
/// otherwise it is re-read when next shown anyway. While faces are selected or were just
/// named, only the people list is: the grid mustn't move under the user.
pub fn refresh_if_shown(ui: &AppWindow) {
    if ui.get_page() == nav::PEOPLE {
        with_grid(|g| {
            let working = !g.selection.borrow().is_empty() || g.faces.borrow().iter().any(|f| f.mask.is_some());
            if working {
                g.load_people();
            } else {
                g.load(None);
            }
        });
    }
}

impl FaceGrid {
    /// Reads the people and the faces of `key` (the current selection if `None`) on a worker.
    fn load(&self, key: Option<i32>) {
        let Some(ui) = self.ui.upgrade() else { return };
        let bridge = ui.global::<PeopleBridge>();
        let key = key.unwrap_or_else(|| bridge.get_selected());
        if key != bridge.get_selected() {
            // A different person: don't show the last one's faces under the new name.
            bridge.set_selected(key);
            self.faces.borrow_mut().clear();
            self.clear_selection();
            self.notify.reset();
            bridge.set_empty(true);
        }
        bridge.set_loading(true);

        let generation = self.generation.get() + 1;
        self.generation.set(generation);
        let (root, weak) = (self.root.clone(), self.ui.clone());
        thread::spawn(move || {
            let loaded = read(&root, key);
            let _ = weak.upgrade_in_event_loop(move |_| {
                with_grid(|g| {
                    if g.generation.get() == generation {
                        g.show(loaded)
                    }
                })
            });
        });
    }

    /// Re-reads only the people list (counts after naming faces), on a worker.
    fn load_people(&self) {
        let root = self.root.clone();
        let weak = self.ui.clone();
        thread::spawn(move || {
            let people = Catalog::open(&Catalog::default_path(&root), &root).map_err(|e| e.to_string()).and_then(|c| read_people(&c));
            if let Ok(people) = people {
                let _ = weak.upgrade_in_event_loop(move |_| with_grid(|g| g.show_people(people)));
            }
        });
    }

    fn show(&self, loaded: Result<Loaded, String>) {
        let Some(ui) = self.ui.upgrade() else { return };
        let bridge = ui.global::<PeopleBridge>();
        bridge.set_loading(false);
        let loaded = match loaded {
            Ok(loaded) => loaded,
            Err(e) => {
                bridge.set_status_text(format!("Catalog: {e}").into());
                return;
            }
        };
        self.show_people(loaded.people);
        bridge.set_selected(loaded.selected);
        bridge.set_person_name(if loaded.selected == UNKNOWN { SharedString::default() } else { loaded.title.as_str().into() });
        bridge.set_title(loaded.title.into());
        bridge.set_empty(loaded.faces.is_empty());
        *self.faces.borrow_mut() = loaded.faces;
        self.clear_selection();
        self.notify.reset();
        self.publish_status();
        let restore = self.restore.take();
        if let Some(snapshot) = restore.filter(|s| s.person == loaded.selected) {
            self.apply_restore(snapshot);
        }
    }

    /// Selects and scrolls as `snapshot` was, for the faces still there.
    fn apply_restore(&self, snapshot: Snapshot) {
        let Some(ui) = self.ui.upgrade() else { return };
        let position: HashMap<FaceId, usize> = self.faces.borrow().iter().enumerate().map(|(i, f)| (f.id, i)).collect();
        self.anchor.set(snapshot.anchor_click.and_then(|id| position.get(&id).copied()));
        self.set_selection(snapshot.selected.iter().filter_map(|id| position.get(id).copied()).collect());
        if let Some(&first) = snapshot.anchor.and_then(|id| position.get(&id)) {
            let bridge = ui.global::<PeopleBridge>();
            let serial = bridge.get_restore().serial + 1;
            let row = (first / self.columns.get()) as i32;
            bridge.set_restore(ScrollRestore { serial, row, offset: snapshot.offset });
        }
    }

    /// The face at position `i` as an item to open: its picture, pointing the face out.
    /// The scope is the pictures of the faces on screen, in grid order.
    fn target(&self, i: usize) -> Target {
        let faces = self.faces.borrow();
        let mut seen = HashSet::new();
        let paths: Vec<PathBuf> = faces.iter().filter(|f| seen.insert(&f.path)).map(|f| f.path.clone()).collect();
        let face = &faces[i];
        let current = paths.iter().position(|p| *p == face.path);
        let title = self.ui.upgrade().map(|ui| ui.global::<PeopleBridge>().get_title()).unwrap_or_default();
        Target { path: face.path.clone(), face: Some(face.id), scope: Scope::of(format!("People · {title}"), paths, current) }
    }

    fn show_people(&self, people: People) {
        let Some(ui) = self.ui.upgrade() else { return };
        ui.global::<PeopleBridge>().set_people(ModelRc::new(VecModel::from(people.items)));
        *self.names.borrow_mut() =
            people.names.into_iter().map(|(display, path)| Name { display, path }).collect();
    }

    fn publish_status(&self) {
        let Some(ui) = self.ui.upgrade() else { return };
        let bridge = ui.global::<PeopleBridge>();
        let (n, selected) = (self.faces.borrow().len(), self.selection.borrow().len());
        let mut text = if n == 1 { "1 face".to_owned() } else { format!("{n} faces") };
        if selected > 0 {
            text += &format!(" · {selected} selected");
        }
        bridge.set_selection(selected as i32);
        bridge.set_status_text(text.into());
    }

    /// The face at grid row `row`, column `col`.
    fn position(&self, row: i32, col: i32) -> Option<usize> {
        let columns = self.columns.get();
        if row < 0 || col < 0 || col as usize >= columns {
            return None;
        }
        let i = row as usize * columns + col as usize;
        (i < self.faces.borrow().len()).then_some(i)
    }

    /// A click on the face at `at` (`None`: between faces). `extend` (shift) selects the
    /// range from the last face clicked, `toggle` (ctrl) adds or removes one face.
    fn click(&self, at: Option<usize>, extend: bool, toggle: bool) {
        let Some(at) = at else {
            if !extend && !toggle {
                self.set_selection(BTreeSet::new());
            }
            return;
        };
        let mut selection = self.selection.borrow().clone();
        match self.anchor.get() {
            Some(anchor) if extend => {
                if !toggle {
                    selection.clear();
                }
                selection.extend(anchor.min(at)..=anchor.max(at));
            }
            _ if toggle || extend => {
                if !selection.remove(&at) {
                    selection.insert(at);
                }
                self.anchor.set(Some(at));
            }
            _ => {
                selection = BTreeSet::from([at]);
                self.anchor.set(Some(at));
            }
        }
        self.set_selection(selection);
    }

    /// The rubber band covers rows `r0..=r1`, columns `c0..=c1` (not clamped to the grid).
    fn drag_to(&self, r0: i32, r1: i32, c0: i32, c1: i32) {
        let (columns, n) = (self.columns.get(), self.faces.borrow().len());
        let mut selection = self.drag_base.borrow().clone();
        let (r1, c1) = (r1.min(self.row_count() as i32 - 1), c1.min(columns as i32 - 1));
        if r1 >= 0 && c1 >= 0 {
            for row in r0.max(0) as usize..=r1 as usize {
                selection.extend((c0.max(0) as usize..=c1 as usize).map(|c| row * columns + c).filter(|&i| i < n));
            }
        }
        self.set_selection(selection);
    }

    fn set_selection(&self, selection: BTreeSet<usize>) {
        let before = self.selection.replace(selection);
        let columns = self.columns.get();
        let rows: BTreeSet<usize> =
            before.symmetric_difference(&self.selection.borrow()).map(|i| i / columns).collect();
        for row in rows {
            self.notify.row_changed(row);
        }
        self.publish_status();
    }

    fn clear_selection(&self) {
        self.selection.borrow_mut().clear();
        self.anchor.set(None);
    }

    /// Offers the person tags matching `text` under the name box, names starting with it
    /// first. A name shared by two tags is offered by path.
    fn offer_names(&self, text: &str) {
        let Some(ui) = self.ui.upgrade() else { return };
        let text = text.trim().to_lowercase();
        let names = self.names.borrow();
        let mut matches: Vec<(bool, &Name)> = if text.is_empty() {
            Vec::new()
        } else {
            names
                .iter()
                .filter_map(|n| {
                    let lower = n.display.to_lowercase();
                    lower.contains(&text).then(|| (!lower.starts_with(&text), n))
                })
                .collect()
        };
        matches.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.display.to_lowercase().cmp(&b.1.display.to_lowercase())));
        let offered: Vec<SharedString> = matches
            .iter()
            .take(MATCHES)
            .map(|(_, n)| {
                let shared = names.iter().filter(|o| o.display.eq_ignore_ascii_case(&n.display)).count() > 1;
                if shared { n.path.as_str().into() } else { n.display.as_str().into() }
            })
            .collect();
        ui.global::<PeopleBridge>().set_name_matches(ModelRc::new(VecModel::from(offered)));
    }

    /// The person tag `name` means, and its label: an existing person by name or path, or
    /// a new one under `People|`.
    fn resolve(&self, name: &str) -> Option<(String, String)> {
        let name = name.trim();
        if name.is_empty() || name.split('|').any(|s| s.trim().is_empty()) {
            return None;
        }
        let names = self.names.borrow();
        let known = names
            .iter()
            .find(|n| n.path.eq_ignore_ascii_case(name))
            .or_else(|| names.iter().find(|n| n.display.to_lowercase() == name.to_lowercase()));
        Some(match known {
            Some(n) => (n.path.clone(), n.display.clone()),
            None if name.contains('|') => (name.to_owned(), display_name(name).to_owned()),
            None => (format!("{PEOPLE}|{name}"), name.to_owned()),
        })
    }

    fn assign(&self, name: &str) {
        let Some((path, label)) = self.resolve(name) else { return };
        if !self.names.borrow().iter().any(|n| n.path.eq_ignore_ascii_case(&path)) {
            // So the next selection is offered this name before the writer reports back.
            self.names.borrow_mut().push(Name { display: label.clone(), path: path.clone() });
        }
        if let Some((change, faces)) = self.mask_selection(label.into(), false) {
            let _ = self.writer.send(Change::Name { change, faces, path });
        }
    }

    fn ignore(&self) {
        if let Some((change, faces)) = self.mask_selection("Ignored".into(), true) {
            let _ = self.writer.send(Change::Ignore { change, faces });
        }
    }

    /// Masks the selected faces with `label` and deselects them; returns the change number
    /// and the faces.
    fn mask_selection(&self, label: SharedString, ignored: bool) -> Option<(u64, Vec<FaceId>)> {
        let selection = std::mem::take(&mut *self.selection.borrow_mut());
        if selection.is_empty() {
            return None;
        }
        let change = self.changes.get() + 1;
        self.changes.set(change);
        let ids = {
            let mut faces = self.faces.borrow_mut();
            selection
                .iter()
                .map(|&i| {
                    faces[i].mask = Some(Mask { label: label.clone(), ignored, change });
                    faces[i].id
                })
                .collect()
        };
        let columns = self.columns.get();
        for row in selection.iter().map(|i| i / columns).collect::<BTreeSet<_>>() {
            self.notify.row_changed(row);
        }
        self.publish_status();
        Some((change, ids))
    }

    /// The writer applied `change`, or failed to: then its masks come off.
    fn changed(&self, change: u64, result: Result<People, String>) {
        match result {
            Ok(people) => self.show_people(people),
            Err(e) => {
                let columns = self.columns.get();
                let mut rows = BTreeSet::new();
                for (i, face) in self.faces.borrow_mut().iter_mut().enumerate() {
                    if face.mask.as_ref().is_some_and(|m| m.change == change) {
                        face.mask = None;
                        rows.insert(i / columns);
                    }
                }
                for row in rows {
                    self.notify.row_changed(row);
                }
                if let Some(ui) = self.ui.upgrade() {
                    ui.global::<PeopleBridge>().set_status_text(format!("Not saved: {e}").into());
                }
            }
        }
    }

    fn crop(&self, id: FaceId) -> Option<Image> {
        if let Some(image) = self.crops.borrow().get(&id) {
            return Some(image.clone());
        }
        let jpeg = self.catalog.as_ref()?.face_crop(id).ok()??;
        let rgb = image::load_from_memory_with_format(&jpeg, image::ImageFormat::Jpeg).ok()?.to_rgb8();
        let image = Image::from_rgb8(Pixels::clone_from_slice(rgb.as_raw(), rgb.width(), rgb.height()));
        let mut crops = self.crops.borrow_mut();
        if crops.len() >= CROP_CACHE {
            crops.clear();
        }
        crops.insert(id, image.clone());
        Some(image)
    }
}

impl Model for FaceGrid {
    type Data = FaceRow;

    fn row_count(&self) -> usize {
        self.faces.borrow().len().div_ceil(self.columns.get())
    }

    fn row_data(&self, row: usize) -> Option<FaceRow> {
        let columns = self.columns.get();
        let faces = self.faces.borrow();
        let selection = self.selection.borrow();
        let start = row * columns;
        let slice = faces.get(start..faces.len().min(start + columns))?;
        let tiles: Vec<FaceTile> = slice
            .iter()
            .enumerate()
            .map(|(i, face)| {
                let thumb = self.crop(face.id);
                FaceTile {
                    thumb_ready: thumb.is_some(),
                    thumb: thumb.unwrap_or_default(),
                    detail: face.detail.clone(),
                    suggested: face.suggested,
                    selected: selection.contains(&(start + i)),
                    assigned: face.mask.as_ref().map(|m| m.label.clone()).unwrap_or_default(),
                    ignored: face.mask.as_ref().is_some_and(|m| m.ignored),
                }
            })
            .collect();
        Some(FaceRow { tiles: ModelRc::new(VecModel::from(tiles)) })
    }

    fn model_tracker(&self) -> &dyn ModelTracker {
        &self.notify
    }
}

/// Writes the user's changes to the catalog, in order, reporting each to the UI with the
/// people list as it now is; once changes stop coming, refreshes the suggestions.
fn spawn_writer(root: PathBuf, ui: slint::Weak<AppWindow>) -> mpsc::Sender<Change> {
    let (send, receive) = mpsc::channel::<Change>();
    thread::spawn(move || {
        let mut catalog = None;
        let mut pending_refresh = false;
        loop {
            let change = if pending_refresh {
                match receive.recv_timeout(SETTLE) {
                    Ok(change) => change,
                    Err(mpsc::RecvTimeoutError::Timeout) => {
                        pending_refresh = false;
                        if let Some(c) = catalog.as_mut() {
                            refresh(c, &ui);
                        }
                        continue;
                    }
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                }
            } else {
                match receive.recv() {
                    Ok(change) => change,
                    Err(_) => break,
                }
            };
            if catalog.is_none() {
                catalog = Catalog::open(&Catalog::default_path(&root), &root).ok();
            }
            let (id, result) = match catalog.as_mut() {
                Some(c) => apply(c, change),
                None => (change_id(&change), Err("can't open the catalog".to_owned())),
            };
            pending_refresh |= result.is_ok();
            let _ = ui.upgrade_in_event_loop(move |_| with_grid(|g| g.changed(id, result)));
        }
    });
    send
}

fn change_id(change: &Change) -> u64 {
    match change {
        Change::Name { change, .. } | Change::Ignore { change, .. } => *change,
    }
}

fn apply(catalog: &mut Catalog, change: Change) -> (u64, Result<People, String>) {
    let id = change_id(&change);
    let result = match change {
        Change::Name { faces, path, .. } => {
            catalog.tag(&path, TagKind::Person).and_then(|tag| catalog.confirm_faces(&faces, tag))
        }
        Change::Ignore { faces, .. } => catalog.ignore_faces(&faces),
    };
    (id, result.map_err(|e| e.to_string()).and_then(|()| read_people(catalog)))
}

/// Suggestions from the faces confirmed so far; the counts change with them.
fn refresh(catalog: &mut Catalog, ui: &slint::Weak<AppWindow>) {
    if let Err(e) = catalog.refresh_suggestions(DEFAULT_MIN_SIMILARITY) {
        eprintln!("people: suggestions: {e}");
        return;
    }
    if let Ok(people) = read_people(catalog) {
        let _ = ui.upgrade_in_event_loop(move |_| with_grid(|g| g.show_people(people)));
    }
}

/// The last level of a tag path: "People|Alice" -> "Alice".
fn display_name(path: &str) -> &str {
    path.rsplit('|').next().unwrap_or(path).trim()
}

fn file_name(path: &Path) -> SharedString {
    path.file_name().map_or_else(Default::default, |n| n.to_string_lossy().as_ref().into())
}

/// The people list: Unknown, then every person with faces; and every person tag.
fn read_people(catalog: &Catalog) -> Result<People, String> {
    let err = |e: archive_core::catalog::Error| e.to_string();
    let unknown = catalog.faces(FaceFilter::Unknown, FaceOrder::Asset).map_err(err)?;
    let mut items = vec![PersonItem { key: UNKNOWN, name: "Unknown".into(), count: unknown.len() as i32 }];
    items.extend(catalog.people_with_faces().map_err(err)?.iter().map(|p| PersonItem {
        key: p.tag.0 as i32,
        name: display_name(&p.path).into(),
        count: (p.confirmed + p.suggested) as i32,
    }));
    let names = catalog
        .leaf_tags(TagKind::Person)
        .map_err(err)?
        .into_iter()
        .map(|(_, path)| (display_name(&path).to_owned(), path))
        .collect();
    Ok(People { items, names })
}

/// Worker thread: the people list, and the faces of `key` (Unknown if that person is gone).
fn read(root: &Path, key: i32) -> Result<Loaded, String> {
    let catalog = Catalog::open(&Catalog::default_path(root), root).map_err(|e| e.to_string())?;
    let err = |e: archive_core::catalog::Error| e.to_string();

    let people = read_people(&catalog)?;
    let person = catalog.people_with_faces().map_err(err)?.into_iter().find(|p| p.tag.0 as i32 == key);
    let (selected, title, faces) = match &person {
        // The faces only suggested as this person first, highlighted, then the confirmed
        // ones; each in similarity order.
        Some(p) => {
            let tag = TagId(p.tag.0);
            let mut faces = catalog.faces(FaceFilter::Suggested(tag), FaceOrder::Similarity).map_err(err)?;
            faces.extend(catalog.faces(FaceFilter::Confirmed(tag), FaceOrder::Similarity).map_err(err)?);
            (key, display_name(&p.path).to_owned(), faces)
        }
        // Similar faces side by side, so whoever names them can take them in runs.
        None => (UNKNOWN, "Unknown".to_owned(), catalog.faces(FaceFilter::Unknown, FaceOrder::Similarity).map_err(err)?),
    };

    let mut paths = HashMap::new();
    let faces = faces
        .into_iter()
        .filter_map(|face| {
            let path = match paths.get(&face.asset) {
                Some(path) => path,
                None => paths.entry(face.asset).or_insert(catalog.asset_path(face.asset).ok()?),
            }
            .clone();
            let suggested = person.is_some() && face.suggestion.is_some();
            let detail = match face.suggestion {
                Some((_, similarity)) if suggested => format!("Suggested · {:.0}%", similarity * 100.0).into(),
                _ => file_name(&path),
            };
            Some(FaceRef { id: face.id, path, detail, suggested, mask: None })
        })
        .collect();
    Ok(Loaded { people, selected, title, faces })
}
