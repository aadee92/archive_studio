//! People mode: the person tags with faces, "Unknown" first, and the face crops of the one
//! selected, where faces are selected and named.
//!
//! A worker thread reads the people and the selected person's faces from the catalog (faces
//! are put in similarity order, which is quadratic). Crops are small JPEGs in the catalog,
//! decoded on the UI thread only for the rows on screen and cached.
//!
//! Where a tile is larger on screen than the stored crop (on a HiDPI screen), the crop is
//! cut again from the picture at the tile's size once the view rests, on workers (all the
//! faces of one picture from one decode, [`thumbs::decode_regions`]), and kept in memory.
//!
//! Naming or ignoring faces doesn't reorder the grid: the faces stay where they are, masked
//! with the name, until the grid is reloaded (another person picked, the page shown again).
//! The catalog is written by a writer thread (`Catalog::name_faces`, which also tags the
//! pictures `People|<Name>`), which then reports the pictures to `pipeline::changed` so
//! Review shows their new tags, and refreshes the suggestions, so faces like the ones just
//! named are listed under that person on the next load.
//!
//! The list shows each person's cover face (`Catalog::person_cover`, decoded small on the
//! worker that reads the list, and cached), and is sorted and narrowed on the UI thread
//! ([`arrange`]: by name, most faces or recently named; the order is kept for the session).
//! A person's menu (Rename…, Merge into…, Delete person…), or dragging one onto another,
//! opens `tags`' dialog for them; "Set as cover" on a face (here or on Details) is
//! [`set_cover`].
//!
//! A person's suggested faces come first; "Confirm" names the selected ones as them, "Not
//! Ann" rejects her for them (`Catalog::reject_person`: they stay unknown and aren't
//! suggested as her again), and "N rejected" lists the faces said not to be her, where a
//! rejection can be taken back (`Catalog::clear_rejections`). "Ignored", at the bottom of
//! the list, holds the ignored faces, to un-ignore (`Catalog::unignore_faces`) or name.
//! These are masked in place too.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use slint::{ComponentHandle, Image, Model, ModelNotify, ModelRc, ModelTracker, SharedString, VecModel};

use std::sync::Mutex;

use crate::nav::{self, Scope, Target};
use crate::thumbs::{self, cache::Lru, Pixels};
use crate::{actions, AppWindow, FaceRow, FaceTile, PeopleBridge, PersonItem, ScrollRestore};
use archive_core::catalog::faces::{crop_region, FaceFilter, FaceId, FaceOrder, PersonFaces, DEFAULT_MIN_SIMILARITY};
use archive_core::catalog::people::{arrange, display_name, matching_people, person_path, PeopleOrder};
use archive_core::catalog::{Catalog, Region, TagId, TagKind};
use archive_core::ops;

/// `PersonItem.key` of the faces nobody has named.
const UNKNOWN: i32 = -1;
/// `PersonItem.key` of the ignored faces (-2 is the page's "no person dragged").
const IGNORED: i32 = -3;
/// Decoded crops kept; ~75 KB each at the stored 160 px.
const CROP_CACHE: usize = 3000;
/// Names offered under the name box.
const MATCHES: usize = 6;
/// Crops cut afresh at the tiles' size, kept in memory (~170 KB each at 240 px).
const SHARP_BUDGET: usize = 64 << 20;
/// The view rests this long before sharper crops are cut for the faces in it.
const SHARP_SETTLE: Duration = Duration::from_millis(150);
/// The writer waits this long for more changes before refreshing the suggestions.
const SETTLE: Duration = Duration::from_millis(500);
/// Edge of the decoded cover faces (the list draws them at 26 px; twice that for HiDPI).
const AVATAR_EDGE: u32 = 52;
/// Decoded covers kept across list reads; ~8 KB each.
const AVATAR_CACHE: usize = 4000;

/// Covers decoded by the workers that read the list, by face, so a re-read only decodes
/// covers it hasn't seen.
static AVATARS: Mutex<Option<HashMap<FaceId, Pixels>>> = Mutex::new(None);

struct FaceRef {
    id: FaceId,
    path: PathBuf,
    /// Where the face is on its picture.
    region: Region,
    detail: SharedString,
    suggested: bool,
    /// Named or ignored since the grid was loaded.
    mask: Option<Mask>,
}

struct Mask {
    label: SharedString,
    /// Drawn light: ignored, rejected, back to unknown (not named).
    muted: bool,
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
    /// The faces said not to be that person, rather than theirs.
    rejected: bool,
    /// How many faces were said not to be that person.
    rejections: usize,
    /// "Ann" for a person (also on their rejected faces); "" otherwise.
    name: String,
    title: String,
    faces: Vec<FaceRef>,
}

/// The people list and every person tag.
struct People {
    /// Faces nobody has named or been suggested as.
    unknown: usize,
    ignored: usize,
    people: Vec<PersonFaces>,
    /// The covers' pixels.
    avatars: Vec<(FaceId, Pixels)>,
    names: Vec<(String, String)>,
}

/// A change for the writer thread.
/// `paths`: the pictures of `faces`, whose tags change with them.
enum Change {
    Name { change: u64, faces: Vec<FaceId>, paths: Vec<PathBuf>, path: String },
    Ignore { change: u64, faces: Vec<FaceId>, paths: Vec<PathBuf> },
    Unignore { change: u64, faces: Vec<FaceId>, paths: Vec<PathBuf> },
    /// The faces aren't `person` (no tags change).
    Reject { change: u64, faces: Vec<FaceId>, person: TagId },
    /// Take that back.
    Unreject { change: u64, faces: Vec<FaceId>, person: TagId },
}

/// What the grid shows: whose faces.
#[derive(Clone, Copy, Debug, PartialEq)]
enum View {
    Unknown,
    Ignored,
    /// Their suggested faces, then their confirmed ones.
    Person(TagId),
    /// The faces said not to be them.
    Rejected(TagId),
}

/// The face grid: `faces` cut into rows of `columns`.
struct FaceGrid {
    ui: slint::Weak<AppWindow>,
    root: PathBuf,
    /// The UI thread's connection, for crops: opened on a worker (opening can wait out a
    /// migration), so `None` until it lands.
    catalog: RefCell<Option<Catalog>>,
    faces: RefCell<Vec<FaceRef>>,
    columns: Cell<usize>,
    crops: RefCell<HashMap<FaceId, Image>>,
    /// Crops cut from the pictures at `sharp_px` (tile size × scale factor), for faces whose
    /// stored crop is smaller than that.
    sharp: RefCell<Lru<FaceId, Image>>,
    sharp_px: Cell<u32>,
    /// Faces drawn with a crop smaller than their tile, in the order drawn, to cut sharper
    /// once the view rests; and those being cut.
    blurry: RefCell<Vec<FaceId>>,
    cutting: RefCell<HashSet<FaceId>>,
    sharp_timer: slint::Timer,
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
    /// The people list as read (by path), before `arrange` sorts and filters it.
    people: RefCell<Vec<PersonFaces>>,
    unknown: Cell<usize>,
    ignored: Cell<usize>,
    /// The covers, as images.
    avatars: RefCell<HashMap<FaceId, Image>>,
    writer: mpsc::Sender<Change>,
    changes: Cell<u64>,
    /// A view to return to (Back) once the load in flight lands.
    restore: RefCell<Option<Snapshot>>,
    /// The next load lists the person's rejected faces rather than theirs.
    rejected: Cell<bool>,
    /// The faces the item menu was opened on (positions): the one right-clicked, or the
    /// selection it is in.
    menu_faces: RefCell<BTreeSet<usize>>,
}

/// The People view as it was left, for Back (see `nav`): whose faces, which were
/// selected, and the scroll position. Faces are kept by id, so this holds across a reload.
pub(crate) struct Snapshot {
    person: i32,
    /// Their rejected faces.
    rejected: bool,
    selected: Vec<FaceId>,
    anchor: Option<FaceId>,
    /// Where a shift-click range starts.
    anchor_click: Option<FaceId>,
    /// How far the first row in view was scrolled past.
    offset: f32,
}

impl std::fmt::Debug for Snapshot {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(
            f,
            "People {{ person {}{}, {} faces selected, scroll anchor face {:?} +{:.0}px, shift anchor {:?} }}",
            self.person,
            if self.rejected { " (rejected)" } else { "" },
            self.selected.len(),
            self.anchor.map(|id| id.0),
            self.offset,
            self.anchor_click.map(|id| id.0)
        )
    }
}

thread_local! {
    static GRID: RefCell<Option<Rc<FaceGrid>>> = const { RefCell::new(None) };
    /// Faces were changed elsewhere (on Details) while the page was hidden: Back re-reads
    /// the grid instead of keeping it as it was.
    static STALE: Cell<bool> = const { Cell::new(false) };
}

fn with_grid(f: impl FnOnce(&Rc<FaceGrid>)) {
    GRID.with_borrow(|grid| {
        if let Some(grid) = grid {
            f(grid)
        }
    });
}

/// The window's scale factor changed: the faces are drawn again, cut sharper where the
/// stored crops are now too small.
pub fn scale_changed() {
    with_grid(|g| g.notify.reset());
}

pub fn install(ui: &AppWindow, root: PathBuf) {
    let open = root.clone();
    let _ = thread::Builder::new().name("people-catalog".into()).spawn(move || {
        let catalog = Catalog::open(&Catalog::default_path(&open), &open)
            .inspect_err(|e| log::warn!("people: no catalog for face crops: {e}"))
            .ok();
        let _ = slint::invoke_from_event_loop(move || {
            with_grid(|g| {
                *g.catalog.borrow_mut() = catalog;
                // Rows drawn so far had no crops.
                g.notify.reset();
            })
        });
    });
    let grid = Rc::new(FaceGrid {
        ui: ui.as_weak(),
        root: root.clone(),
        catalog: RefCell::default(),
        faces: RefCell::default(),
        columns: Cell::new(1),
        crops: RefCell::default(),
        sharp: RefCell::new(Lru::new(SHARP_BUDGET)),
        sharp_px: Cell::new(0),
        blurry: RefCell::default(),
        cutting: RefCell::default(),
        sharp_timer: slint::Timer::default(),
        generation: Cell::new(0),
        notify: ModelNotify::default(),
        selection: RefCell::default(),
        anchor: Cell::new(None),
        drag_base: RefCell::default(),
        names: RefCell::default(),
        people: RefCell::default(),
        unknown: Cell::new(0),
        ignored: Cell::new(0),
        avatars: RefCell::default(),
        writer: spawn_writer(root, ui.as_weak()),
        changes: Cell::new(0),
        restore: RefCell::default(),
        rejected: Cell::new(false),
        menu_faces: RefCell::default(),
    });
    GRID.set(Some(grid.clone()));

    let bridge = ui.global::<PeopleBridge>();
    bridge.set_rows(ModelRc::from(grid.clone()));
    bridge.on_select(|key| {
        log::debug!("person list click: key {key} ({})", person_label(key));
        with_grid(|g| {
            g.rejected.set(false);
            g.load(Some(key), "person picked in the list")
        })
    });
    bridge.on_show_rejected(|on| {
        log::debug!("show the rejected faces: {on}");
        with_grid(|g| {
            g.rejected.set(on);
            g.load(None, if on { "rejected faces asked for" } else { "back from the rejected faces" })
        })
    });
    bridge.on_act(|action| {
        log::debug!("{action} the selected faces");
        with_grid(|g| {
            let selection = g.selection.borrow().clone();
            g.act(&action, selection)
        })
    });
    bridge.on_is_text(|text| {
        let mut chars = text.chars();
        // Not a control character or one of Slint's special keys (private use area).
        matches!((chars.next(), chars.next()), (Some(c), None) if !c.is_control() && !('\u{e000}'..='\u{f8ff}').contains(&c))
    });
    bridge.on_arrange(|| with_grid(|g| g.arrange()));
    bridge.on_person_action(|key, action| {
        log::debug!("person menu: {action} key {key} ({})", person_label(key));
        with_grid(|g| match (g.ui.upgrade(), g.person_path(key)) {
            (Some(ui), Some(path)) => crate::tags::edit(&ui, path, &action, None),
            _ => log::debug!("person menu: key {key} not in the list - ignored"),
        })
    });
    bridge.on_merge_drop(|from, index| {
        with_grid(|g| {
            let Some(ui) = g.ui.upgrade() else { return };
            let into = usize::try_from(index).ok().and_then(|i| ui.global::<PeopleBridge>().get_people().row_data(i));
            match (g.person_path(from), into) {
                (Some(path), Some(into)) if into.key >= 0 && into.key != from => {
                    log::debug!("person {path:?} dropped on {:?}: confirm the merge", into.path);
                    crate::tags::edit(&ui, path, "merge", Some(into.path.into()));
                }
                (path, into) => log::debug!("person {from} ({path:?}) dropped on row {index} ({:?}) - nothing to merge", into.map(|p| p.key)),
            }
        })
    });
    bridge.on_columns_changed(|n| {
        with_grid(|g| {
            // Also sent each time the page is shown; only a change re-lays the grid.
            let before = g.columns.replace(n.max(1) as usize);
            if before != n.max(1) as usize {
                log::debug!("face grid columns {before} -> {n}: model reset");
                g.notify.reset();
            }
        })
    });
    bridge.on_open(|row, col| {
        with_grid(|g| match (g.ui.upgrade(), g.position(row, col)) {
            (Some(ui), Some(i)) => {
                let target = g.target(i);
                log::debug!("face double-click at row {row} col {col}: face #{i} -> open");
                nav::open(&ui, target);
            }
            (_, None) => log::debug!("face double-click at row {row} col {col}: no face there ({} faces) - ignored", g.faces.borrow().len()),
            (None, _) => log::debug!("face double-click: window gone - ignored"),
        })
    });
    bridge.on_context_menu(|row, col| {
        with_grid(|g| match (g.ui.upgrade(), g.position(row, col)) {
            (Some(ui), Some(i)) => {
                let target = g.target(i);
                nav::context_menu(&ui, target);
                // A face confirmed as the person on screen (not one only suggested, or named
                // otherwise since the grid was loaded) can stand for them.
                let bridge = ui.global::<PeopleBridge>();
                let confirmed = g.faces.borrow().get(i).is_some_and(|f| !f.suggested && f.mask.is_none());
                if let (View::Person(person), true) = (g.view(), confirmed) {
                    actions::offer_cover(&ui, person, bridge.get_person_name().into());
                }
                // What the view does to faces, for the one right-clicked or the selection
                // it is in.
                let faces = if g.selection.borrow().contains(&i) { g.selection.borrow().clone() } else { BTreeSet::from([i]) };
                if let Some(title) = g.menu_title(&faces) {
                    actions::offer_face_action(&ui, title);
                }
                *g.menu_faces.borrow_mut() = faces;
            }
            _ => log::debug!("face right-click at row {row} col {col}: no face there - no menu"),
        })
    });
    bridge.on_click(|row, col, extend, toggle| {
        with_grid(|g| {
            let at = g.position(row, col);
            log::debug!("face click at row {row} col {col} -> {at:?} (shift {extend}, ctrl {toggle})");
            g.click(at, extend, toggle)
        })
    });
    bridge.on_drag_begin(|keep| {
        log::debug!("face drag begins (keep selection {keep})");
        with_grid(|g| {
            *g.drag_base.borrow_mut() = if keep { g.selection.borrow().clone() } else { BTreeSet::new() };
        })
    });
    bridge.on_drag_to(|r0, r1, c0, c1| {
        log::trace!("face drag rows {r0}..={r1} cols {c0}..={c1}");
        with_grid(|g| g.drag_to(r0, r1, c0, c1))
    });
    bridge.on_select_all(|| {
        log::debug!("select all faces");
        with_grid(|g| g.set_selection((0..g.faces.borrow().len()).collect()))
    });
    bridge.on_clear_selection(|| {
        log::debug!("clear face selection");
        with_grid(|g| g.set_selection(BTreeSet::new()))
    });
    bridge.on_name_edited(|text| {
        log::trace!("name box: {text:?}");
        with_grid(|g| g.offer_names(&text))
    });
    bridge.on_assign(|name| {
        log::debug!("assign {name:?} to the selected faces");
        with_grid(|g| g.assign(&name))
    });
    bridge.on_ignore(|| {
        log::debug!("ignore the selected faces");
        with_grid(|g| g.ignore())
    });
}

/// `Alice (3)` for a person key, from the list on screen, for the log.
fn person_label(key: i32) -> String {
    let mut found = None;
    with_grid(|g| {
        if let Some(ui) = g.ui.upgrade() {
            let people = ui.global::<PeopleBridge>().get_people();
            found = people.iter().find(|p| p.key == key).map(|p| format!("{} ({})", p.name, p.count));
        }
    });
    found.unwrap_or_else(|| "not in the list".to_owned())
}

/// The user picks `face` (confirmed as `person`, called `name`) to stand for them in the
/// list: saved on a worker, then the list is re-read.
pub fn set_cover(ui: &AppWindow, person: TagId, face: FaceId, name: String) {
    let Some(root) = GRID.with_borrow(|g| g.as_ref().map(|g| g.root.clone())) else { return };
    log::debug!("set face {} as the cover for person {} ({name})", face.0, person.0);
    let weak = ui.as_weak();
    let _ = thread::Builder::new().name("people-cover".into()).spawn(move || {
        let result = actions::open_catalog(&root).and_then(|mut c| ops::set_person_cover(&mut c, person, Some(face)).map_err(|e| e.to_string()));
        let _ = weak.upgrade_in_event_loop(move |ui| match result {
            Ok(()) => {
                with_grid(|g| g.load_people());
                actions::notify(&ui, format!("Cover for {name} set"), false, true);
            }
            Err(e) => actions::notify(&ui, format!("Can't set the cover: {e}"), true, false),
        });
    });
}

/// The item menu's face action ([`actions::offer_face_action`]) was picked: done to the
/// faces it was opened on.
pub fn face_menu_action() {
    with_grid(|g| {
        let faces = g.menu_faces.take();
        match g.view_action() {
            Some(action) => {
                log::debug!("item menu: {action} {} faces", faces.len());
                g.act(action, faces)
            }
            None => log::debug!("item menu: no face action in {:?} - ignored", g.view()),
        }
    })
}

/// The page was picked in the sidebar: faces get named elsewhere (and found by scans), so
/// the catalog is re-read.
pub fn entered() {
    log::debug!("people page shown from the sidebar: re-reading the catalog");
    STALE.set(false);
    with_grid(|g| g.load(None, "page shown"));
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
            rejected: bridge.get_rejected_view(),
            selected: g.selection.borrow().iter().filter_map(|&i| faces.get(i)).map(|f| f.id).collect(),
            anchor: faces.get(first).map(|f| f.id),
            anchor_click: g.anchor.get().and_then(|i| faces.get(i)).map(|f| f.id),
            offset: bridge.get_scroll_offset(),
        })
    })
}

/// Faces were named, un-named, added or deleted elsewhere (on Details): re-read now if the
/// page is shown, else when it is next (Back included).
pub fn faces_changed(ui: &AppWindow) {
    if ui.get_page() == nav::PEOPLE {
        refresh_if_shown(ui);
    } else {
        log::debug!("faces changed elsewhere: the grid will be re-read when shown");
        STALE.set(true);
    }
}

/// Back to a view left earlier. The grid isn't re-read if it still shows that person
/// (the faces stay where they were, as do any just named), unless faces were changed
/// elsewhere meanwhile; only the people list is.
pub(crate) fn restore(snapshot: Snapshot) {
    let stale = STALE.replace(false);
    with_grid(|g| {
        let Some(ui) = g.ui.upgrade() else { return };
        let bridge = ui.global::<PeopleBridge>();
        if bridge.get_selected() == snapshot.person && bridge.get_rejected_view() == snapshot.rejected && !bridge.get_loading() && !stale {
            log::debug!("restore {snapshot:?}: same person on screen, applying now and re-reading the list");
            g.apply_restore(snapshot);
            g.load_people();
        } else {
            log::debug!(
                "restore {snapshot:?}: on screen person {} (loading {}, faces changed elsewhere {stale}), reloading first",
                bridge.get_selected(),
                bridge.get_loading()
            );
            let person = snapshot.person;
            g.rejected.set(snapshot.rejected);
            *g.restore.borrow_mut() = Some(snapshot);
            g.load(Some(person), "back to this person");
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
            log::debug!("pipeline grouped faces; page shown, working {working}: re-reading {}", if working { "the list" } else { "everything" });
            if working {
                g.load_people();
            } else {
                g.load(None, "pipeline grouped faces");
            }
        });
    }
}

impl FaceGrid {
    /// Reads the people and the faces of `key` (the current selection if `None`) on a worker.
    fn load(&self, key: Option<i32>, why: &str) {
        let Some(ui) = self.ui.upgrade() else { return };
        let bridge = ui.global::<PeopleBridge>();
        let key = key.unwrap_or_else(|| bridge.get_selected());
        // Only a person has rejected faces.
        let rejected = self.rejected.get() && key >= 0;
        let generation = self.generation.get() + 1;
        log::debug!(
            "load #{generation} person {key}{} ({why}); on screen {}, was loading {}",
            if rejected { " rejected" } else { "" },
            bridge.get_selected(),
            bridge.get_loading()
        );
        if key != bridge.get_selected() || rejected != bridge.get_rejected_view() {
            // A different person: don't show the last one's faces under the new name.
            log::debug!("load #{generation}: another person or view, clearing the grid (model reset)");
            bridge.set_selected(key);
            bridge.set_rejected_view(rejected);
            self.faces.borrow_mut().clear();
            self.clear_selection();
            self.notify.reset();
            bridge.set_empty(true);
        }
        bridge.set_loading(true);

        self.generation.set(generation);
        let (root, weak) = (self.root.clone(), self.ui.clone());
        let spawned = thread::Builder::new().name("people-load".into()).spawn(move || {
            let started = Instant::now();
            let loaded = read(&root, key, rejected);
            match &loaded {
                Ok(l) => log::debug!(
                    "load #{generation} read: {} people, person {} {:?}, {} faces in {:?}",
                    l.people.people.len(),
                    l.selected,
                    l.title,
                    l.faces.len(),
                    started.elapsed()
                ),
                Err(e) => log::warn!("load #{generation} failed after {:?}: {e}", started.elapsed()),
            }
            let posted = weak.upgrade_in_event_loop(move |_| {
                with_grid(|g| {
                    if g.generation.get() == generation {
                        g.show(loaded)
                    } else {
                        log::debug!("load #{generation} result dropped: load #{} superseded it", g.generation.get());
                    }
                })
            });
            if let Err(e) = posted {
                log::debug!("load #{generation} result not delivered: {e}");
            }
        });
        if let Err(e) = spawned {
            log::error!("load #{generation}: can't start a thread: {e}");
        }
    }

    /// Re-reads only the people list (counts after naming faces), on a worker.
    fn load_people(&self) {
        let root = self.root.clone();
        let weak = self.ui.clone();
        log::debug!("re-reading the people list only");
        let _ = thread::Builder::new().name("people-list".into()).spawn(move || {
            let started = Instant::now();
            let people = Catalog::open(&Catalog::default_path(&root), &root).map_err(|e| e.to_string()).and_then(|c| read_people(&c));
            match people {
                Ok(people) => {
                    log::debug!("people list read: {} people in {:?}", people.people.len(), started.elapsed());
                    let _ = weak.upgrade_in_event_loop(move |_| with_grid(|g| g.show_people(people)));
                }
                Err(e) => log::warn!("people list: {e}"),
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
                log::warn!("people load failed, grid left as it was: {e}");
                bridge.set_status_text(format!("Catalog: {e}").into());
                return;
            }
        };
        log::debug!(
            "show person {} {:?}: {} people, {} faces (model reset, selection cleared); restore pending: {}",
            loaded.selected,
            loaded.title,
            loaded.people.people.len(),
            loaded.faces.len(),
            self.restore.borrow().as_ref().map_or_else(|| "none".to_owned(), |s| format!("{s:?}"))
        );
        self.show_people(loaded.people);
        bridge.set_selected(loaded.selected);
        bridge.set_rejected_view(loaded.rejected);
        self.rejected.set(loaded.rejected);
        bridge.set_rejected_count(loaded.rejections as i32);
        bridge.set_person_name(loaded.name.into());
        bridge.set_title(loaded.title.into());
        bridge.set_empty(loaded.faces.is_empty());
        *self.faces.borrow_mut() = loaded.faces;
        self.clear_selection();
        self.notify.reset();
        self.publish_status();
        match self.restore.take() {
            Some(snapshot) if snapshot.person == loaded.selected && snapshot.rejected == loaded.rejected => self.apply_restore(snapshot),
            Some(snapshot) => log::debug!("restore {snapshot:?} dropped: person {} was loaded instead", loaded.selected),
            None => {}
        }
    }

    /// Selects and scrolls as `snapshot` was, for the faces still there.
    fn apply_restore(&self, snapshot: Snapshot) {
        let Some(ui) = self.ui.upgrade() else { return };
        let position: HashMap<FaceId, usize> = self.faces.borrow().iter().enumerate().map(|(i, f)| (f.id, i)).collect();
        self.anchor.set(snapshot.anchor_click.and_then(|id| position.get(&id).copied()));
        let selection: BTreeSet<usize> = snapshot.selected.iter().filter_map(|id| position.get(id).copied()).collect();
        let selected = selection.len();
        self.set_selection(selection);
        match snapshot.anchor.and_then(|id| position.get(&id)) {
            Some(&first) => {
                let bridge = ui.global::<PeopleBridge>();
                let serial = bridge.get_restore().serial + 1;
                let row = (first / self.columns.get()) as i32;
                log::debug!(
                    "restore applied: {selected} of {} faces selected, scroll to row {row} +{:.0}px (serial {serial}, {} rows)",
                    snapshot.selected.len(),
                    snapshot.offset,
                    self.row_count()
                );
                bridge.set_restore(ScrollRestore { serial, row, offset: snapshot.offset });
            }
            None => log::debug!(
                "restore applied: {selected} of {} faces selected; scroll anchor {:?} not in the grid ({} faces), scroll left as is",
                snapshot.selected.len(),
                snapshot.anchor.map(|id| id.0),
                position.len()
            ),
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
        log::debug!(
            "people list read: {} people with faces, {} unknown faces, {} ignored, {} covers, {} person tags",
            people.people.len(),
            people.unknown,
            people.ignored,
            people.avatars.len(),
            people.names.len()
        );
        {
            let mut avatars = self.avatars.borrow_mut();
            if avatars.len() >= AVATAR_CACHE {
                avatars.clear();
            }
            for (face, pixels) in people.avatars {
                avatars.entry(face).or_insert_with(|| Image::from_rgb8(pixels));
            }
        }
        *self.people.borrow_mut() = people.people;
        self.unknown.set(people.unknown);
        self.ignored.set(people.ignored);
        *self.names.borrow_mut() =
            people.names.into_iter().map(|(display, path)| Name { display, path }).collect();
        self.arrange();
    }

    /// Shows the people list as read, sorted and filtered as the page says: Unknown first,
    /// then the people the filter matches in the order picked ([`arrange`]), Ignored last.
    fn arrange(&self) {
        let Some(ui) = self.ui.upgrade() else { return };
        let bridge = ui.global::<PeopleBridge>();
        let order = PeopleOrder::ALL.get(bridge.get_sort().max(0) as usize).copied().unwrap_or_default();
        let filter = bridge.get_filter();
        let people = arrange(self.people.borrow().clone(), order, &filter);
        let avatars = self.avatars.borrow();
        let mut items = vec![PersonItem {
            key: UNKNOWN,
            name: "Unknown".into(),
            count: self.unknown.get() as i32,
            initial: "?".into(),
            ..Default::default()
        }];
        items.extend(people.iter().map(|p| {
            let name = display_name(&p.path);
            let avatar = p.cover.and_then(|f| avatars.get(&f)).cloned();
            PersonItem {
                key: p.tag.0 as i32,
                name: name.into(),
                path: p.path.as_str().into(),
                count: (p.confirmed + p.suggested) as i32,
                has_avatar: avatar.is_some(),
                avatar: avatar.unwrap_or_default(),
                initial: name.chars().next().map(|c| c.to_uppercase().collect::<String>()).unwrap_or_default().into(),
            }
        }));
        items.push(PersonItem { key: IGNORED, name: "Ignored".into(), count: self.ignored.get() as i32, initial: "–".into(), ..Default::default() });
        log::debug!(
            "people list model replaced ({}, filter {filter:?}): {} rows [{}]",
            order.name(),
            items.len(),
            items.iter().take(12).map(|p| format!("{}:{}({})", p.key, p.name, p.count)).collect::<Vec<_>>().join(", ")
        );
        bridge.set_people(ModelRc::new(VecModel::from(items)));
    }

    /// The tag path of the person with list key `key`.
    fn person_path(&self, key: i32) -> Option<String> {
        self.people.borrow().iter().find(|p| p.tag.0 as i32 == key).map(|p| p.path.clone())
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
        bridge.set_selected_suggestions(self.suggestions_in(&self.selection.borrow()).len() as i32);
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

    /// Offers the person tags matching `text` under the name box
    /// ([`matching_people`]: names starting with it first; a name shared by two people by path).
    fn offer_names(&self, text: &str) {
        let Some(ui) = self.ui.upgrade() else { return };
        let paths: Vec<String> = self.names.borrow().iter().map(|n| n.path.clone()).collect();
        let offered: Vec<SharedString> = matching_people(&paths, text, MATCHES).into_iter().map(SharedString::from).collect();
        ui.global::<PeopleBridge>().set_name_matches(ModelRc::new(VecModel::from(offered)));
    }

    /// The person tag `name` means, and its label: an existing person by path or name (the
    /// one picked from the matches), or a new one (`Catalog::person` decides for good when
    /// the writer names the faces).
    fn resolve(&self, name: &str) -> Option<(String, String)> {
        let name = name.trim();
        let path = person_path(name).ok()?;
        let names = self.names.borrow();
        let known = names
            .iter()
            .find(|n| n.path.eq_ignore_ascii_case(name) || n.path.eq_ignore_ascii_case(&path))
            .or_else(|| names.iter().find(|n| n.display.to_lowercase() == name.to_lowercase()));
        Some(match known {
            Some(n) => (n.path.clone(), n.display.clone()),
            None => (path.clone(), display_name(&path).to_owned()),
        })
    }

    fn assign(&self, name: &str) {
        let Some((path, label)) = self.resolve(name) else {
            log::debug!("assign {name:?}: not a person name - ignored");
            return;
        };
        if !self.names.borrow().iter().any(|n| n.path.eq_ignore_ascii_case(&path)) {
            // So the next selection is offered this name before the writer reports back.
            self.names.borrow_mut().push(Name { display: label.clone(), path: path.clone() });
        }
        let selection = self.selection.borrow().clone();
        self.name(selection, path, label);
    }

    /// Names the faces at `positions` as the person tag `path`, called `label`.
    fn name(&self, positions: BTreeSet<usize>, path: String, label: String) {
        if let Some((change, faces, paths)) = self.mask(&positions, label.into(), false) {
            log::debug!("change {change}: name {} faces {:?} as {path:?} ({} pictures)", faces.len(), ids(&faces), paths.len());
            self.send(Change::Name { change, faces, paths, path });
        }
    }

    fn ignore(&self) {
        let selection = self.selection.borrow().clone();
        if let Some((change, faces, paths)) = self.mask(&selection, "Ignored".into(), true) {
            log::debug!("change {change}: ignore {} faces {:?}", faces.len(), ids(&faces));
            self.send(Change::Ignore { change, faces, paths });
        }
    }

    fn send(&self, change: Change) {
        let id = change_id(&change);
        if self.writer.send(change).is_err() {
            log::error!("change {id}: the people writer thread is gone; not saved");
        }
    }

    /// What the grid shows.
    fn view(&self) -> View {
        let Some(ui) = self.ui.upgrade() else { return View::Unknown };
        let bridge = ui.global::<PeopleBridge>();
        match bridge.get_selected() {
            IGNORED => View::Ignored,
            key if key >= 0 && bridge.get_rejected_view() => View::Rejected(TagId(key as i64)),
            key if key >= 0 => View::Person(TagId(key as i64)),
            _ => View::Unknown,
        }
    }

    /// The view's action on faces ([`act`](Self::act)): what its button, Backspace and the
    /// item menu do.
    fn view_action(&self) -> Option<&'static str> {
        match self.view() {
            View::Unknown => None,
            View::Ignored => Some("unignore"),
            View::Person(_) => Some("reject"),
            View::Rejected(_) => Some("unreject"),
        }
    }

    /// The view's action as an item menu entry for the faces at `positions`, if it applies
    /// to any of them.
    fn menu_title(&self, positions: &BTreeSet<usize>) -> Option<String> {
        let name = self.ui.upgrade()?.global::<PeopleBridge>().get_person_name();
        match self.view() {
            View::Unknown => None,
            View::Person(_) if self.suggestions_in(positions).is_empty() => None,
            View::Person(_) => Some(format!("Not {name}")),
            View::Ignored => Some(if positions.len() > 1 { format!("Un-ignore {} faces", positions.len()) } else { "Un-ignore".to_owned() }),
            View::Rejected(_) => Some(format!("Suggest as {name} again")),
        }
    }

    /// The faces at `positions` still suggested as the person on screen (not named, ignored
    /// or rejected since the grid was loaded).
    fn suggestions_in(&self, positions: &BTreeSet<usize>) -> BTreeSet<usize> {
        let faces = self.faces.borrow();
        positions.iter().copied().filter(|&i| faces.get(i).is_some_and(|f| f.suggested && f.mask.is_none())).collect()
    }

    /// Does `action` to the faces at `positions`, as the view allows: "confirm" (the
    /// suggested ones as the person on screen), "reject" (the suggested ones aren't them),
    /// "unignore" (Ignored), "unreject" (a person's rejected faces may be suggested as them
    /// again).
    fn act(&self, action: &str, positions: BTreeSet<usize>) {
        let Some(ui) = self.ui.upgrade() else { return };
        let name: String = ui.global::<PeopleBridge>().get_person_name().into();
        match (action, self.view()) {
            ("confirm", View::Person(person)) => match self.person_path(person.0 as i32) {
                Some(path) => self.name(self.suggestions_in(&positions), path, name),
                None => log::debug!("confirm: person {} not in the list - ignored", person.0),
            },
            ("reject", View::Person(person)) => {
                let suggested = self.suggestions_in(&positions);
                if let Some((change, faces, _)) = self.mask(&suggested, format!("Not {name}").into(), true) {
                    log::debug!("change {change}: {} faces {:?} aren't person {}", faces.len(), ids(&faces), person.0);
                    let bridge = ui.global::<PeopleBridge>();
                    bridge.set_rejected_count(bridge.get_rejected_count() + faces.len() as i32);
                    self.send(Change::Reject { change, faces, person });
                }
            }
            ("unignore", View::Ignored) => {
                if let Some((change, faces, paths)) = self.mask(&positions, "Unknown".into(), true) {
                    log::debug!("change {change}: un-ignore {} faces {:?}", faces.len(), ids(&faces));
                    self.send(Change::Unignore { change, faces, paths });
                }
            }
            ("unreject", View::Rejected(person)) => {
                if let Some((change, faces, _)) = self.mask(&positions, format!("Can be {name}").into(), true) {
                    log::debug!("change {change}: {} faces {:?} may be person {} again", faces.len(), ids(&faces), person.0);
                    self.send(Change::Unreject { change, faces, person });
                }
            }
            (action, view) => log::debug!("{action}: not in {view:?} - ignored"),
        }
    }

    /// Masks the faces at `positions` with `label` and deselects them; returns the change
    /// number, the faces and their pictures.
    fn mask(&self, positions: &BTreeSet<usize>, label: SharedString, muted: bool) -> Option<(u64, Vec<FaceId>, Vec<PathBuf>)> {
        if positions.is_empty() {
            log::debug!("{label}: no faces to do it to - ignored");
            return None;
        }
        self.selection.borrow_mut().retain(|i| !positions.contains(i));
        let change = self.changes.get() + 1;
        self.changes.set(change);
        let (ids, mut paths): (Vec<FaceId>, Vec<PathBuf>) = {
            let mut faces = self.faces.borrow_mut();
            let n = faces.len();
            positions
                .iter()
                .filter(|&&i| i < n)
                .map(|&i| {
                    faces[i].mask = Some(Mask { label: label.clone(), muted, change });
                    (faces[i].id, faces[i].path.clone())
                })
                .unzip()
        };
        paths.sort();
        paths.dedup();
        let columns = self.columns.get();
        for row in positions.iter().map(|i| i / columns).collect::<BTreeSet<_>>() {
            self.notify.row_changed(row);
        }
        self.publish_status();
        Some((change, ids, paths))
    }

    /// The writer applied `change`, or failed to: then its masks come off.
    fn changed(&self, change: u64, result: Result<People, String>) {
        match result {
            Ok(people) => {
                log::debug!("change {change} saved");
                self.show_people(people)
            }
            Err(e) => {
                log::warn!("change {change} not saved, unmasking its faces: {e}");
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
        let jpeg = self.catalog.borrow().as_ref()?.face_crop(id).ok()??;
        let rgb = image::load_from_memory_with_format(&jpeg, image::ImageFormat::Jpeg).ok()?.to_rgb8();
        let image = Image::from_rgb8(Pixels::clone_from_slice(rgb.as_raw(), rgb.width(), rgb.height()));
        let mut crops = self.crops.borrow_mut();
        if crops.len() >= CROP_CACHE {
            crops.clear();
        }
        crops.insert(id, image.clone());
        Some(image)
    }

    /// The tiles' size on screen in physical pixels; crops cut for another size are dropped.
    fn tile_px(&self) -> u32 {
        let px = self.ui.upgrade().map_or(0, |ui| (ui.global::<PeopleBridge>().get_tile_size() * ui.window().scale_factor()).ceil() as u32);
        if self.sharp_px.replace(px) != px {
            *self.sharp.borrow_mut() = Lru::new(SHARP_BUDGET);
        }
        px
    }

    /// The face's crop to draw `px` wide: cut sharper from its picture if that was done,
    /// else the stored one (and if that is smaller, the face is cut once the view rests).
    fn thumb(&self, face: &FaceRef, px: u32) -> Option<Image> {
        if let Some(image) = self.sharp.borrow_mut().get(&face.id) {
            return Some(image);
        }
        let image = self.crop(face.id)?;
        let (mut blurry, cutting) = (self.blurry.borrow_mut(), self.cutting.borrow());
        if image.size().width < px && !cutting.contains(&face.id) && !blurry.contains(&face.id) {
            blurry.push(face.id);
            self.sharp_timer.start(slint::TimerMode::SingleShot, SHARP_SETTLE, || with_grid(|g| g.sharpen()));
        }
        Some(image)
    }

    /// Cuts the faces drawn blurry since the last time from their pictures at the tiles'
    /// size, on workers, a picture at a time, in the order drawn.
    fn sharpen(&self) {
        let want = std::mem::take(&mut *self.blurry.borrow_mut());
        let px = self.sharp_px.get();
        let faces = self.faces.borrow();
        let mut pictures: Vec<(PathBuf, Vec<(FaceId, Region)>)> = Vec::new();
        for id in want {
            let Some(face) = faces.iter().find(|f| f.id == id) else { continue };
            self.cutting.borrow_mut().insert(id);
            match pictures.iter_mut().find(|(path, _)| *path == face.path) {
                Some((_, faces)) => faces.push((id, face.region)),
                None => pictures.push((face.path.clone(), vec![(id, face.region)])),
            }
        }
        if pictures.is_empty() {
            return;
        }
        log::debug!("people: cutting {} faces of {} pictures at {px} px", pictures.iter().map(|p| p.1.len()).sum::<usize>(), pictures.len());
        let weak = self.ui.clone();
        thread::spawn(move || {
            let started = Instant::now();
            let next = std::sync::atomic::AtomicUsize::new(0);
            let workers = thread::available_parallelism().map_or(2, |n| n.get().saturating_sub(1).clamp(1, 8)).min(pictures.len());
            thread::scope(|scope| {
                for _ in 0..workers {
                    scope.spawn(|| {
                        while let Some((path, faces)) = pictures.get(next.fetch_add(1, std::sync::atomic::Ordering::Relaxed)) {
                            let cut = cut_faces(path, faces, px);
                            let _ = weak.upgrade_in_event_loop(move |_| with_grid(|g| g.sharpened(cut, px)));
                        }
                    });
                }
            });
            log::debug!("people: faces cut in {:?}", started.elapsed());
        });
    }

    /// Faces cut at `px` came in (`None`: the picture couldn't be read; the stored crop stays).
    fn sharpened(&self, cut: Vec<(FaceId, Option<Pixels>)>, px: u32) {
        let mut ids = Vec::with_capacity(cut.len());
        for (id, pixels) in cut {
            self.cutting.borrow_mut().remove(&id);
            if px != self.sharp_px.get() {
                continue;
            }
            // A failed one keeps its stored crop, without being asked for again.
            let Some(image) = pixels.map(Image::from_rgb8).or_else(|| self.crop(id)) else { continue };
            let size = image.size();
            self.sharp.borrow_mut().insert(id, image, size.width as usize * size.height as usize * 3);
            ids.push(id);
        }
        let columns = self.columns.get();
        let rows: BTreeSet<usize> = self.faces.borrow().iter().enumerate().filter(|(_, f)| ids.contains(&f.id)).map(|(i, _)| i / columns).collect();
        for row in rows {
            self.notify.row_changed(row);
        }
    }
}

/// The faces on the picture at `path`, cut as their stored crops are, `px` wide.
fn cut_faces(path: &Path, faces: &[(FaceId, Region)], px: u32) -> Vec<(FaceId, Option<Pixels>)> {
    let regions: Option<Vec<[f32; 4]>> = thumbs::upright_dimensions(path).map(|dims| {
        faces
            .iter()
            .map(|(_, region)| {
                let r = crop_region(*region, dims);
                [r.x, r.y, r.w, r.h]
            })
            .collect()
    });
    let cut = regions.and_then(|regions| thumbs::decode_regions(path, &regions, px));
    if cut.is_none() {
        log::debug!("people: can't cut faces from {}", path.display());
    }
    let mut cut = cut.unwrap_or_default().into_iter().map(|rgb| Pixels::clone_from_slice(rgb.as_raw(), rgb.width(), rgb.height()));
    faces.iter().map(|(id, _)| (*id, cut.next())).collect()
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
        let px = self.tile_px();
        let tiles: Vec<FaceTile> = slice
            .iter()
            .enumerate()
            .map(|(i, face)| {
                let thumb = self.thumb(face, px);
                FaceTile {
                    thumb_ready: thumb.is_some(),
                    thumb: thumb.unwrap_or_default(),
                    detail: face.detail.clone(),
                    suggested: face.suggested,
                    selected: selection.contains(&(start + i)),
                    assigned: face.mask.as_ref().map(|m| m.label.clone()).unwrap_or_default(),
                    ignored: face.mask.as_ref().is_some_and(|m| m.muted),
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
    let spawned = thread::Builder::new().name("people-writer".into()).spawn(move || {
        log::debug!("people writer started");
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
                catalog = match Catalog::open(&Catalog::default_path(&root), &root) {
                    Ok(c) => Some(c),
                    Err(e) => {
                        log::warn!("people writer: can't open the catalog: {e}");
                        None
                    }
                };
            }
            let started = Instant::now();
            let (id, result) = match catalog.as_mut() {
                Some(c) => apply(c, change),
                None => (change_id(&change), Err("can't open the catalog".to_owned())),
            };
            match &result {
                Ok(people) => log::debug!("change {id} written in {:?}; {} people now", started.elapsed(), people.people.len()),
                Err(e) => log::warn!("change {id} failed: {e}"),
            }
            pending_refresh |= result.is_ok();
            // What was saved, as the journal has it, for the notice (with Undo).
            let saved = result.is_ok().then(|| catalog.as_ref()?.next_undo().ok().flatten().map(|(c, _)| c.description)).flatten();
            if let Err(e) = ui.upgrade_in_event_loop(move |ui| {
                with_grid(|g| g.changed(id, result));
                if let Some(saved) = saved {
                    actions::notify(&ui, saved, false, true);
                }
            }) {
                log::debug!("change {id}: result not delivered: {e}");
            }
        }
        log::debug!("people writer stopped (channel closed)");
    });
    if let Err(e) = spawned {
        log::error!("people writer: can't start a thread: {e}");
    }
    send
}

/// Face ids for the log (the first few).
fn ids(faces: &[FaceId]) -> Vec<i64> {
    faces.iter().take(20).map(|f| f.0).collect()
}

fn change_id(change: &Change) -> u64 {
    match change {
        Change::Name { change, .. }
        | Change::Ignore { change, .. }
        | Change::Unignore { change, .. }
        | Change::Reject { change, .. }
        | Change::Unreject { change, .. } => *change,
    }
}

/// Applies `change`; once saved, its pictures are reported changed (their person tags).
fn apply(catalog: &mut Catalog, change: Change) -> (u64, Result<People, String>) {
    let id = change_id(&change);
    let (result, paths) = match change {
        Change::Name { faces, path, paths, .. } => (ops::name_faces(catalog, &faces, &path).map(|_| ()), paths),
        Change::Ignore { faces, paths, .. } => (ops::ignore_faces(catalog, &faces), paths),
        Change::Unignore { faces, paths, .. } => (ops::unignore_faces(catalog, &faces), paths),
        Change::Reject { faces, person, .. } => (ops::reject_person(catalog, &faces, person), Vec::new()),
        Change::Unreject { faces, person, .. } => (ops::clear_rejections(catalog, &faces, person), Vec::new()),
    };
    if result.is_ok() {
        for path in &paths {
            crate::pipeline::changed(path, false);
        }
    }
    (id, result.map_err(|e| e.to_string()).and_then(|()| read_people(catalog)))
}

/// Suggestions from the faces confirmed so far; the counts change with them.
fn refresh(catalog: &mut Catalog, ui: &slint::Weak<AppWindow>) {
    let started = Instant::now();
    match catalog.refresh_suggestions(DEFAULT_MIN_SIMILARITY) {
        Ok(n) => log::debug!("suggestions refreshed in {:?}: {n:?}", started.elapsed()),
        Err(e) => {
            log::warn!("people: suggestions: {e}");
            return;
        }
    }
    match read_people(catalog) {
        Ok(people) => {
            let _ = ui.upgrade_in_event_loop(move |_| with_grid(|g| g.show_people(people)));
        }
        Err(e) => log::warn!("people list after suggestions: {e}"),
    }
}

fn file_name(path: &Path) -> SharedString {
    path.file_name().map_or_else(Default::default, |n| n.to_string_lossy().as_ref().into())
}

/// The people list: how many faces are unknown and ignored, every person with faces and
/// their covers' pixels; and every person tag.
fn read_people(catalog: &Catalog) -> Result<People, String> {
    let err = |e: archive_core::catalog::Error| e.to_string();
    let unknown = catalog.count_faces(FaceFilter::Unknown).map_err(err)?;
    let ignored = catalog.count_faces(FaceFilter::Ignored).map_err(err)?;
    let people = catalog.people_with_faces().map_err(err)?;
    let avatars = people.iter().filter_map(|p| p.cover).filter_map(|face| Some((face, avatar(catalog, face)?))).collect();
    let names = catalog
        .leaf_tags(TagKind::Person)
        .map_err(err)?
        .into_iter()
        .map(|(_, path)| (display_name(&path).to_owned(), path))
        .collect();
    Ok(People { unknown, ignored, people, avatars, names })
}

/// The face's crop, small, for the people list: decoded once, then from [`AVATARS`].
fn avatar(catalog: &Catalog, face: FaceId) -> Option<Pixels> {
    if let Some(pixels) = AVATARS.lock().ok()?.as_ref().and_then(|cache| cache.get(&face)) {
        return Some(pixels.clone());
    }
    let jpeg = catalog.face_crop(face).ok()??;
    let rgb = image::load_from_memory_with_format(&jpeg, image::ImageFormat::Jpeg)
        .inspect_err(|e| log::debug!("people: cover face {}: {e}", face.0))
        .ok()?
        .to_rgb8();
    let small = image::imageops::thumbnail(&rgb, AVATAR_EDGE.min(rgb.width()), AVATAR_EDGE.min(rgb.height()));
    let pixels = Pixels::clone_from_slice(small.as_raw(), small.width(), small.height());
    let mut cache = AVATARS.lock().ok()?;
    let cache = cache.get_or_insert_with(HashMap::new);
    if cache.len() >= AVATAR_CACHE {
        cache.clear();
    }
    cache.insert(face, pixels.clone());
    Some(pixels)
}

/// Worker thread: the people list, and the faces of `key` (Unknown if that person is gone;
/// `rejected`: the faces said not to be them).
fn read(root: &Path, key: i32, rejected: bool) -> Result<Loaded, String> {
    let catalog = Catalog::open(&Catalog::default_path(root), root).map_err(|e| e.to_string())?;
    let err = |e: archive_core::catalog::Error| e.to_string();

    let people = read_people(&catalog)?;
    let person = people.people.iter().find(|p| p.tag.0 as i32 == key).map(|p| (p.tag, display_name(&p.path).to_owned()));
    let mut loaded = Loaded { people, selected: key, rejected: false, rejections: 0, name: String::new(), title: String::new(), faces: Vec::new() };
    let faces = match person {
        Some((tag, name)) => {
            loaded.rejections = catalog.count_faces(FaceFilter::Rejected(tag)).map_err(err)?;
            loaded.rejected = rejected;
            if rejected {
                loaded.title = format!("Not {name}");
                loaded.name = name;
                catalog.faces(FaceFilter::Rejected(tag), FaceOrder::Similarity).map_err(err)?
            } else {
                // The faces only suggested as this person first, highlighted, then the
                // confirmed ones; each in similarity order.
                loaded.title = name.clone();
                loaded.name = name;
                let mut faces = catalog.faces(FaceFilter::Suggested(tag), FaceOrder::Similarity).map_err(err)?;
                faces.extend(catalog.faces(FaceFilter::Confirmed(tag), FaceOrder::Similarity).map_err(err)?);
                faces
            }
        }
        None if key == IGNORED => {
            loaded.title = "Ignored".to_owned();
            catalog.faces(FaceFilter::Ignored, FaceOrder::Similarity).map_err(err)?
        }
        // Similar faces side by side, so whoever names them can take them in runs.
        None => {
            loaded.selected = UNKNOWN;
            loaded.title = "Unknown".to_owned();
            catalog.faces(FaceFilter::Unknown, FaceOrder::Similarity).map_err(err)?
        }
    };
    let suggestions = loaded.selected >= 0 && !loaded.rejected;

    let mut paths = HashMap::new();
    let faces = faces
        .into_iter()
        .filter_map(|face| {
            let path = match paths.get(&face.asset) {
                Some(path) => path,
                None => paths.entry(face.asset).or_insert(catalog.asset_path(face.asset).ok()?),
            }
            .clone();
            let suggested = suggestions && face.suggestion.is_some();
            let detail = match face.suggestion {
                Some((_, similarity)) if suggested => format!("Suggested · {:.0}%", similarity * 100.0).into(),
                _ => file_name(&path),
            };
            Some(FaceRef { id: face.id, path, region: face.region, detail, suggested, mask: None })
        })
        .collect();
    Ok(Loaded { faces, ..loaded })
}
