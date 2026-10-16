//! Details page: one item enlarged, opened through [`nav::open`] (double-click in Review,
//! People or Acquire). The image is decoded off the UI thread no larger than the window shows
//! it ([`pictures`]); its faces and the rest are read from the catalog on another worker
//! meanwhile, a newer open retiring an older read. Until the picture is in, its thumbnail
//! (from the thumbnail cache, as in the filmstrip) stands in for it, the same shape, so the
//! sharp one replaces it in place.
//!
//! Faces are drawn as boxes over the picture (placed by the page in displayed-image
//! coordinates, so they follow the window), named when someone confirmed who they are. A
//! face opened from People is flashed once the picture is up.
//!
//! A two-sided photo (catalog pair, else `-front` / `-back` file names) shows a turned-down
//! corner that flips to the other side, and can show both sides next to each other, front
//! on the left, each with its own faces.
//!
//! Under the picture, the items most like it among those of the view it was opened from
//! (its [`Scope`]): `Catalog::similar` over their assets, on a worker, after the item itself
//! (the reference), under "Similar in Review: <the view's filter>". The strip is always
//! there, so the picture doesn't move when they come in. A click on one shows its picture
//! in place of the item's ([`peek`]) without leaving it: the header, the filmstrip, stepping
//! and Back stay with the reference, and the strip isn't searched again; the picture's
//! faces, info panel, actions and menu are the shown one's. A click on the reference shows
//! it again, as does stepping (the item stepped to is the new reference).
//!
//! Left / Right (Home / End) step through that scope in its order, and a filmstrip above
//! the picture shows the items around the shown one (small thumbnails from the cache, on a
//! worker); a click shows another. Stepping replaces the item in place, without history, so
//! Back returns to the view it was opened from (whose selection follows: see `nav::back`).
//! The pictures of the next items (three ahead in the direction of the last step, two
//! behind) are decoded ahead, several at once, so a step shows the new one from memory, in
//! the same frame; past those (holding an arrow key) it shows the thumbnail and then the
//! picture once decoded.
//!
//! The picture zooms and pans (the page does that: wheel, drag, double-click, + / - / 0 / 1).
//! Zoomed in past what the decoded picture resolves, the page asks for the part in view
//! ([`detail`]): the file is decoded at full resolution on a worker (once, kept while zoomed
//! in on it: one at a time, at most [`FULL_PIXELS`]), and that part of it, no larger than it
//! is shown, is drawn over the picture.
//!
//! Faces on the picture can be fixed up there, all through the core on a worker (one change
//! at a time): a click on a box opens a popup next to it to name or rename the face (the
//! people whose names match what is typed, as in People's name box: `matching_people`), say
//! it is not who it is named or suggested as (`Catalog::not_person`), ignore it, or delete the
//! box (`Catalog::delete_faces`); right-clicking a box offers the same as a menu. "Add face"
//! draws a box around a face the detector missed: `ops::add_face` stores it with its crop and
//! an embedding (with the engine's face models, else ones loaded here once), and its popup
//! opens. Each change is reported with `pipeline::changed` (Review's tags) and to People.
//!
//! The info panel beside the picture (toggled from the bottom bar, kept open across items)
//! shows the file (name, path to copy, date taken and modified, its own dimensions, size,
//! kind: `ops::properties`) and its tags by kind, People first, each with an × that takes
//! it off (`ops::remove_tag`; a person's faces on the picture go back to unknown), and a box
//! to add one with the catalog's matching tags under it (as the Add tag dialog,
//! `ops::add_tag`). It is read again whenever the item's catalog data changes
//! ([`data_changed`], from the pipeline relay), as is the other side of the photo, so a
//! pairing made from the item menu shows at once.
//!
//! Keys on the page (see `details.slint`) reach the same actions as its buttons and the item
//! menu: R / Shift+R (L) rotate, Enter accepts a proposed rotation, Delete and T go through
//! [`actions::act`] for the shown item, F flips, B / N toggle boxes and names, Space runs a
//! slideshow (the page's own timer stepping through the scope), F11 full screen. After the
//! shown item is deleted, the next item of the scope is shown ([`files_gone`]).
//!
//! For an image it also offers the pipeline's say on it: a rotation the orient stage
//! proposed for review (Accept / Keep: `ops::accept` / `ops::keep`), and turning it by hand
//! ([`actions::rotate`]), each by the engine if it runs here, else by the core directly, and
//! each undoable from the notice like the face changes. That can take a second (the file is
//! rewritten), so it runs on a worker; the turned file comes back through the pipeline
//! relay and is reloaded here (faces too: their regions are turned with the file).

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::Instant;

use image::RgbImage;
use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};

mod pictures;

use pictures::{stamp, Picture, Stamp};

use crate::actions;
use crate::nav::{self, Scope, Target};
use crate::pipeline::{self, Handle};
use crate::thumbs::{self, Pixels};
use crate::{AppWindow, DetailFace, DetailsBridge, FilmItem, InfoTag, PropRow, SimilarItem};
use archive_core::catalog::faces::{FaceId, FaceStatus, DEFAULT_MIN_SIMILARITY};
use archive_core::catalog::people::matching_people;
use archive_core::catalog::runs::RunStatus;
use archive_core::catalog::{AppliedTag, Catalog, Region, Source, TagId, TagKind, DEFAULT_MIN_CONFIDENCE};
use archive_core::faces::Faces;
use archive_core::ops;
use archive_core::media::{classify, MediaKind};
use archive_core::pipeline::ORIENT;

/// Longest edge decoded for the page, whatever the window: a picture shown larger than that is
/// sharpened by the full-resolution detail ([`detail`]).
const MAX_EDGE: u32 = 8192;
/// The other side, shown at most half as wide.
const OTHER_EDGE: u32 = 2048;
/// Most pixels decoded for zooming in (~300 MB of RGB): a larger file is decoded as large
/// as fits them.
const FULL_PIXELS: u64 = 100_000_000;
/// Items in the similar strip.
const SIMILAR: usize = 12;
/// The best of them whose pictures are decoded ahead, after the neighbours in the scope.
const SIMILAR_AHEAD: usize = 3;
/// Items in the filmstrip on either side of the shown one (more than fit a 4K screen).
const FILM: usize = 24;
/// The filmstrip's and the similar strip's tiles, in logical pixels (as on the page).
const FILM_TILE: f32 = 64.0;
/// Names offered under the face popup's name box.
const MATCHES: usize = 6;

/// The item shown.
#[derive(Default)]
struct Shown {
    target: Option<Target>,
    kind: Option<MediaKind>,
    /// Kind and size, before the dimensions are known.
    info: String,
    /// The face to flash once the picture is up; taken by the first load that shows it.
    flash: Option<FaceId>,
    /// Retires slower loads of earlier items (or of this one before it changed).
    generation: u64,
    /// The other side of the photo, once known.
    other: Option<PathBuf>,
    other_generation: u64,
    /// A similar item shown in place of the target's picture (clicked in the similar strip):
    /// the picture, faces, info and actions are its; the target stays the reference.
    peek: Option<PathBuf>,
    /// The items in the similar strip, after the reference.
    similar: Vec<PathBuf>,
    similar_generation: u64,
    /// How good the picture up is.
    up: Up,
    /// The file's own size, upright, once read.
    dims: Option<(u32, u32)>,
    /// When it was asked for, to log how long its pictures took.
    shown_at: Option<Instant>,
    /// The last step went forward: decode ahead in that direction.
    forward: bool,
    /// The filmstrip: the items in it, and their thumbnails as far as loaded.
    film: Rc<VecModel<FilmItem>>,
    film_paths: Vec<PathBuf>,
    film_thumbs: HashMap<PathBuf, slint::Image>,
    /// Retires info panel reads of earlier items (or before a change).
    info_generation: u64,
    other_side_generation: u64,
}

/// The picture on the page, worst first.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
enum Up {
    #[default]
    Nothing,
    Thumbnail,
    /// Decoded for a smaller window.
    Soft,
    Sharp,
}

impl Shown {
    /// The item whose picture is shown: the one peeked at, else the target.
    fn path(&self) -> Option<&Path> {
        self.peek.as_deref().or_else(|| self.target.as_ref().map(|t| t.path.as_path()))
    }

    /// Where the item shown is in the similar strip: 0 the reference, -1 not in it.
    fn similar_shown(&self) -> i32 {
        match &self.peek {
            None => 0,
            Some(peek) => self.similar.iter().position(|p| p == peek).map_or(-1, |i| i as i32 + 1),
        }
    }
}

struct Context {
    root: PathBuf,
    pipeline: Handle,
}

thread_local! {
    static SHOWN: RefCell<Shown> = RefCell::default();
    static CONTEXT: RefCell<Option<Context>> = const { RefCell::new(None) };
}

/// The latest load (`Shown::generation`), for its worker: a superseded one (stepped past)
/// doesn't read the catalog.
static LOADING: AtomicU64 = AtomicU64::new(0);
/// Retires filmstrip thumbnail workers of earlier items.
static FILM_GENERATION: AtomicU64 = AtomicU64::new(0);

/// The shown picture at full resolution, for zooming in past what it was decoded at
/// ([`detail`]): one at a time, decoded on first need, dropped when no longer zoomed in.
struct Full {
    path: PathBuf,
    stamp: Stamp,
    image: RgbImage,
}

static FULL: Mutex<Option<Full>> = Mutex::new(None);
/// Retires detail requests of an earlier view (or item).
static DETAIL: AtomicU64 = AtomicU64::new(0);

/// The view of a zoomed picture: the part of it in view (fractions), and the width the whole
/// picture is shown at, in physical pixels.
#[derive(Clone, Copy, Debug, PartialEq)]
struct View {
    left: f32,
    top: f32,
    right: f32,
    bottom: f32,
    width: f32,
}

/// What the engine is asked to do with the shown file.
#[derive(Clone, Copy, Debug)]
enum Action {
    Accept,
    Keep,
    Rotate(u16),
}

/// A face as read for the page.
#[derive(Clone)]
struct FaceInfo {
    id: FaceId,
    face: DetailFace,
    ignored: bool,
    /// Who it is, and (unnamed) who it is suggested as.
    person: Option<(TagId, String)>,
    suggestion: Option<(TagId, String)>,
}

/// A change to a face of the shown picture, made on a worker.
#[derive(Debug)]
enum FaceChange {
    /// Named as the typed name (`Catalog::name_faces`), or as this person.
    Name(FaceId, String),
    Confirm(FaceId, TagId),
    Not(FaceId),
    Ignore(FaceId),
    Delete(FaceId),
    /// A box drawn around a face the detector missed.
    Add(Region),
}

thread_local! {
    /// The faces drawn over the shown picture, in the order of `DetailsBridge.faces`.
    static FACES: RefCell<Vec<FaceInfo>> = const { RefCell::new(Vec::new()) };
    /// Every person (tag path), for the face popup's matches; read when it opens.
    static NAMES: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
    /// Every tag in the catalog, for the info panel's tag box; read with the panel.
    static TAGS: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
}

/// One face change at a time, in the order asked.
static FACE_WRITES: Mutex<()> = Mutex::new(());
/// Face models loaded here for "Add face" when no engine runs in this window (once; `None`
/// if they can't be).
static OWN_FACE_MODELS: OnceLock<Option<Arc<Faces>>> = OnceLock::new();

pub fn install(ui: &AppWindow, root: PathBuf, pipeline: Handle) {
    CONTEXT.set(Some(Context { root, pipeline }));
    let weak = ui.as_weak();
    pictures::start(move |path| {
        let _ = weak.upgrade_in_event_loop(move |ui| picture_ready(&ui, &path));
    });
    let details = ui.global::<DetailsBridge>();
    let weak = ui.as_weak();
    details.on_item_action(move |action| {
        let target = shown_target();
        log::debug!("details: {action:?} on the item shown");
        if let (Some(ui), Some(target)) = (weak.upgrade(), target) {
            actions::act(&ui, &action, target);
        }
    });
    let weak = ui.as_weak();
    details.on_accept(move || ask(&weak, Action::Accept));
    let weak = ui.as_weak();
    details.on_keep(move || ask(&weak, Action::Keep));
    let weak = ui.as_weak();
    details.on_rotate(move |cw| ask(&weak, Action::Rotate(cw.rem_euclid(360) as u16)));
    let weak = ui.as_weak();
    details.on_context_menu(move |side| {
        log::debug!("details: right-click on side {side}");
        let target = match side {
            0 => shown_target(),
            _ => SHOWN.with_borrow(|shown| Some(Target::new(shown.other.clone()?, shown.target.clone()?.scope))),
        };
        if let (Some(ui), Some(target)) = (weak.upgrade(), target) {
            nav::context_menu(&ui, target);
        }
    });
    let weak = ui.as_weak();
    details.on_flip(move || {
        log::debug!("details: flip to the other side");
        if let Some(ui) = weak.upgrade() {
            flip(&ui);
        }
    });
    let weak = ui.as_weak();
    details.on_side_by_side_changed(move || {
        if let Some(ui) = weak.upgrade() {
            if ui.global::<DetailsBridge>().get_side_by_side() {
                load_other(&ui);
            }
        }
    });
    let weak = ui.as_weak();
    details.on_show_similar(move |i| {
        log::debug!("details: click on similar #{i}");
        let path = SHOWN.with_borrow(|shown| match usize::try_from(i) {
            Ok(0) => Some(None),
            Ok(i) => shown.similar.get(i - 1).cloned().map(Some),
            Err(_) => None,
        });
        if let (Some(ui), Some(path)) = (weak.upgrade(), path) {
            peek(&ui, path);
        }
    });
    let weak = ui.as_weak();
    details.on_similar_menu(move |i| {
        if let (Some(ui), Some(target)) = (weak.upgrade(), similar_target(i)) {
            nav::context_menu(&ui, target);
        }
    });
    let weak = ui.as_weak();
    details.on_detail(move |left, top, right, bottom, width| {
        let view = (right > left && bottom > top && width > 0.0).then_some(View { left, top, right, bottom, width });
        if let Some(ui) = weak.upgrade() {
            detail(&ui, view);
        }
    });
    details.set_film(SHOWN.with_borrow(|shown| ModelRc::from(shown.film.clone())));
    let weak = ui.as_weak();
    details.on_step(move |delta| {
        if let Some(ui) = weak.upgrade() {
            step(&ui, delta);
        }
    });
    let weak = ui.as_weak();
    details.on_go_to(move |i| {
        log::debug!("details: go to #{i} of the scope");
        if let (Some(ui), Ok(i)) = (weak.upgrade(), usize::try_from(i)) {
            go_to(&ui, i);
        }
    });
    details.on_face_at(|x, y| face_at(x, y).map_or(-1, |i| i as i32));
    let weak = ui.as_weak();
    details.on_edit_face(move |i| {
        if let Some(ui) = weak.upgrade() {
            edit_face(&ui, i);
        }
    });
    let weak = ui.as_weak();
    details.on_edit_edited(move |text| {
        if let Some(ui) = weak.upgrade() {
            let offered: Vec<SharedString> =
                NAMES.with_borrow(|names| matching_people(names, &text, MATCHES)).into_iter().map(SharedString::from).collect();
            ui.global::<DetailsBridge>().set_edit_matches(ModelRc::new(VecModel::from(offered)));
        }
    });
    let weak = ui.as_weak();
    details.on_edit_assign(move |text| {
        if let Some(ui) = weak.upgrade() {
            assign(&ui, text.trim());
        }
    });
    let weak = ui.as_weak();
    details.on_edit_cancel(move || {
        if let Some(ui) = weak.upgrade() {
            edit_face(&ui, -1);
        }
    });
    let weak = ui.as_weak();
    details.on_face_action(move |i, action| {
        let Some(ui) = weak.upgrade() else { return };
        let Some(face) = face_info(i) else {
            log::debug!("details: face action {action:?} on face #{i}: no such face - ignored");
            return;
        };
        log::debug!("details: face action {action:?} on face #{i} (face {})", face.id.0);
        match action.as_str() {
            "name" => edit_face(&ui, i),
            "not" => change_face(&ui, FaceChange::Not(face.id)),
            "ignore" => change_face(&ui, FaceChange::Ignore(face.id)),
            "delete" => change_face(&ui, FaceChange::Delete(face.id)),
            "cover" => match face.person {
                Some((person, name)) => crate::people::set_cover(&ui, person, face.id, name),
                None => log::debug!("details: face {} isn't named: no cover - ignored", face.id.0),
            },
            other => log::warn!("details: unknown face action {other:?}"),
        }
    });
    let weak = ui.as_weak();
    details.on_face_menu(move |i| {
        let (Some(ui), Some(face)) = (weak.upgrade(), face_info(i)) else { return };
        log::debug!("details: right-click on face #{i} (face {})", face.id.0);
        let details = ui.global::<DetailsBridge>();
        let who = face.person.as_ref().or(face.suggestion.as_ref()).map(|(_, name)| name.as_str()).unwrap_or_default();
        details.set_menu_face(i);
        details.set_menu_person(who.into());
        details.set_menu_named(face.person.is_some());
        details.set_menu_ignored(face.ignored);
        details.set_face_menu_serial(details.get_face_menu_serial() + 1);
    });
    let weak = ui.as_weak();
    details.on_add_face(move |x, y, w, h| {
        log::debug!("details: face box drawn at {x:.3},{y:.3} {w:.3}x{h:.3}");
        if let Some(ui) = weak.upgrade() {
            change_face(&ui, FaceChange::Add(Region { x, y, w, h }));
        }
    });
    let weak = ui.as_weak();
    details.on_info_toggled(move || {
        if let Some(ui) = weak.upgrade() {
            let open = ui.global::<DetailsBridge>().get_info_open();
            log::debug!("details: info panel {}", if open { "opened" } else { "closed" });
            load_info(&ui);
        }
    });
    let weak = ui.as_weak();
    details.on_remove_tag(move |key| {
        if let Some(ui) = weak.upgrade() {
            change_tag(&ui, TagChange::Remove(key.into()));
        }
    });
    let weak = ui.as_weak();
    details.on_info_tag_edited(move |text| {
        if let Some(ui) = weak.upgrade() {
            let offered: Vec<SharedString> =
                TAGS.with_borrow(|tags| actions::matching_tags(tags, &text).into_iter().map(SharedString::from).collect());
            let details = ui.global::<DetailsBridge>();
            details.set_info_tag_matches(ModelRc::new(VecModel::from(offered)));
            details.set_info_tag_error(SharedString::default());
        }
    });
    let weak = ui.as_weak();
    details.on_info_tag_submit(move |text| {
        let text = text.trim();
        if let (Some(ui), false) = (weak.upgrade(), text.is_empty()) {
            change_tag(&ui, TagChange::Add(text.to_owned()));
        }
    });
    let weak = ui.as_weak();
    details.on_film_menu(move |i| {
        let target = SHOWN.with_borrow(|shown| {
            let (path, scope) = shown.target.as_ref()?.scope.at(usize::try_from(i).ok()?)?;
            Some(Target::new(path, scope))
        });
        if let (Some(ui), Some(target)) = (weak.upgrade(), target) {
            nav::context_menu(&ui, target);
        }
    });
}

/// Shows `target` on the Details page (the caller switches to it: see [`nav::open`]); it is
/// the reference of the similar strip, searched anew.
pub fn show(ui: &AppWindow, mut target: Target) {
    let started = Instant::now();
    locate(&mut target);
    log::debug!("details: show {target:?}");
    let path = target.path.clone();
    let details = ui.global::<DetailsBridge>();
    details.set_scope_label(target.scope.label.as_str().into());
    details.set_scope_count(target.scope.len as i32);
    details.set_scope_index(target.scope.current.map_or(-1, |i| i as i32));
    details.set_similar_caption(target.scope.similar_caption().into());
    let flash = target.face;
    SHOWN.with_borrow_mut(|shown| {
        shown.target = Some(target);
        shown.peek = None;
        shown.similar.clear();
    });
    details.set_has_item(true);
    details.set_name(file_name(&path));
    details.set_path(path.display().to_string().into());
    present(ui, started, flash);
    load_film(ui);
    load_similar(ui, true);
}

/// Shows `path`'s picture (a similar item of the strip) in place of the reference's, or the
/// reference's again (`None`), leaving the reference, the filmstrip and the strip as they are.
fn peek(ui: &AppWindow, path: Option<PathBuf>) {
    let Some(path) = SHOWN.with_borrow(|shown| {
        let reference = &shown.target.as_ref()?.path;
        let path = path.filter(|p| p != reference);
        (shown.peek != path).then_some(path)
    }) else {
        return;
    };
    log::debug!("details: peek at {:?} (reference {:?})", path, SHOWN.with_borrow(|s| s.target.as_ref().map(|t| t.path.clone())));
    SHOWN.with_borrow_mut(|shown| shown.peek = path);
    present(ui, Instant::now(), None);
    // Larger than a strip's thumbnail, until the picture is decoded.
    if let Some(path) = SHOWN.with_borrow(|shown| shown.path().filter(|_| shown.up < Up::Soft && shown.kind == Some(MediaKind::Image)).map(Path::to_path_buf)) {
        let weak = ui.as_weak();
        thread::spawn(move || load_stand_in(&weak, path));
    }
}

/// Puts up the picture of the item shown ([`Shown::path`]: the reference, or the similar item
/// peeked at) and reads what goes with it: its faces, other side, info. Up at once if decoded
/// ahead, else a thumbnail of it until it is.
fn present(ui: &AppWindow, started: Instant, flash: Option<FaceId>) {
    let Some((path, similar_shown, target)) = SHOWN.with_borrow(|shown| {
        Some((shown.path()?.to_path_buf(), shown.similar_shown(), shown.target.clone()?))
    }) else {
        return;
    };
    let kind = classify(&path);
    let size = std::fs::metadata(&path).map_or(0, |m| m.len());
    let label = match kind {
        Some(MediaKind::Image) => "Image",
        Some(MediaKind::Video) => "Video",
        Some(MediaKind::Audio) => "Audio",
        None => "File",
    };
    let info = format!("{label} · {}", crate::review::human_size(size));
    let details = ui.global::<DetailsBridge>();
    SHOWN.with_borrow_mut(|shown| {
        shown.flash = flash;
        shown.kind = kind;
        shown.info = info.clone();
        shown.other = None;
        shown.other_generation += 1;
        shown.up = Up::Nothing;
        shown.dims = None;
        shown.shown_at = Some(started);
    });
    details.set_shown_name(file_name(&path));
    details.set_shown_path(path.display().to_string().into());
    details.set_similar_shown(similar_shown);
    details.set_info(info.as_str().into());
    details.set_image(slint::Image::default());
    details.set_image_ready(false);
    details.set_full_edge(0);
    detail(ui, None);
    details.set_faces(ModelRc::default());
    FACES.with_borrow_mut(Vec::clear);
    details.set_editing(-1);
    details.set_adding(false);
    details.set_flash_face(-1);
    details.set_review_cw(0);
    details.set_message(SharedString::default());
    details.set_working(false);
    details.set_other_side(SharedString::default());
    details.set_is_back(false);
    details.set_other_image(slint::Image::default());
    details.set_other_ready(false);
    details.set_other_faces(ModelRc::default());
    details.set_info_tag_error(SharedString::default());
    details.set_info_tag_matches(ModelRc::default());
    let image = kind == Some(MediaKind::Image);
    if image {
        want_pictures(ui, &target);
        if !show_picture(ui, &path) {
            // Review's (opened from there; the larger), else the filmstrip's or the similar
            // strip's; a larger one is loaded with the filmstrip's (or by `peek`).
            let thumb = crate::review::thumbnail(&path)
                .or_else(|| SHOWN.with_borrow(|shown| shown.film_thumbs.get(&path).cloned()))
                .or_else(|| similar_thumb(ui, similar_shown));
            if let Some(thumb) = thumb.filter(|t| t.size().width > 0) {
                set_picture(ui, thumb, Up::Thumbnail);
            }
        }
    }
    let up = SHOWN.with_borrow(|shown| shown.up);
    details.set_loading(image && up < Up::Sharp);
    log::debug!("details: show: {up:?} up in {:?}", started.elapsed());
    load(ui);
    load_info(ui);
}

/// The pictures to decode and keep: the one shown first, the reference (while a similar item
/// is shown), the neighbours in the scope, then the best similar items.
fn want_pictures(ui: &AppWindow, target: &Target) {
    let (shown, similar) = SHOWN.with_borrow(|shown| (shown.path().map(Path::to_path_buf), shown.similar.clone()));
    let mut paths: Vec<PathBuf> = shown.into_iter().collect();
    for path in [target.path.clone()].into_iter().chain(neighbours(target)).chain(similar.into_iter().take(SIMILAR_AHEAD)) {
        if !paths.contains(&path) && classify(&path) == Some(MediaKind::Image) {
            paths.push(path);
        }
    }
    pictures::want(screen_edge(ui), paths);
}

/// The thumbnail of item `i` of the similar strip, if it has one.
fn similar_thumb(ui: &AppWindow, i: i32) -> Option<slint::Image> {
    ui.global::<DetailsBridge>().get_similar().row_data(usize::try_from(i).ok()?).map(|item| item.thumb)
}

/// The item shown, as an item of the scope (the similar item peeked at, at its place there),
/// for its actions and menu.
fn shown_target() -> Option<Target> {
    let mut target = SHOWN.with_borrow(|shown| Some(Target::new(shown.path()?.to_path_buf(), shown.target.as_ref()?.scope.clone())))?;
    locate(&mut target);
    Some(target)
}

fn file_name(path: &Path) -> SharedString {
    path.file_name().map_or_else(Default::default, |n| n.to_string_lossy().as_ref().into())
}

/// Points the scope at `target` if it is one of its items but not the one it says (opened
/// from the similar strip); one that isn't (the back of a photo in it) keeps the position.
fn locate(target: &mut Target) {
    if target.scope.len < 2 {
        return;
    }
    let paths = target.scope.paths();
    if target.scope.current.and_then(|i| paths.get(i)) != Some(&target.path) {
        if let Some(i) = paths.iter().position(|p| *p == target.path) {
            target.scope.current = Some(i);
        }
    }
}

/// Shows the item `delta` items on in the scope, skipping files gone meanwhile; nothing at
/// either end (false).
fn step(ui: &AppWindow, delta: i32) -> bool {
    let Some(scope) = SHOWN.with_borrow(|shown| shown.target.as_ref().map(|t| t.scope.clone())) else { return false };
    let paths = scope.paths();
    let n = paths.len() as i64;
    let mut i = match scope.current {
        Some(i) => i as i64 + i64::from(delta),
        None if delta > 0 => 0,
        None => n - 1,
    };
    while (0..n).contains(&i) && !paths[i as usize].exists() {
        i += i64::from(delta.signum());
    }
    if !(0..n).contains(&i) {
        log::debug!("details: step {delta:+} from {:?}: at the end of {n} items", scope.current);
        return false;
    }
    go_to(ui, i as usize);
    true
}

/// Shows item `i` of the scope in place of the shown one (no history).
fn go_to(ui: &AppWindow, i: usize) {
    let Some((target, forward)) = SHOWN.with_borrow(|shown| {
        let scope = &shown.target.as_ref()?.scope;
        let (path, scope) = scope.at(i)?;
        let forward = scope.current >= shown.target.as_ref()?.scope.current;
        Some((Target::new(path, scope), forward))
    }) else {
        return;
    };
    SHOWN.with_borrow_mut(|shown| shown.forward = forward);
    show(ui, target);
}

/// The items of `target`'s scope to decode ahead, likeliest next first.
fn neighbours(target: &Target) -> Vec<PathBuf> {
    let Some(i) = target.scope.current.filter(|_| target.scope.len > 1) else { return Vec::new() };
    let paths = target.scope.paths();
    let forward = SHOWN.with_borrow(|shown| shown.forward);
    let at = |offset: i64| usize::try_from(i as i64 + offset).ok().and_then(|j| paths.get(j));
    let order: [i64; 5] = if forward { [1, 2, 3, -1, -2] } else { [-1, -2, -3, 1, 2] };
    order.into_iter().filter_map(at).filter(|p| classify(p) == Some(MediaKind::Image)).cloned().collect()
}

/// Fills the filmstrip with the items around the shown one, and loads the thumbnails it
/// doesn't have yet on a worker, nearest first.
fn load_film(ui: &AppWindow) {
    let Some((scope, model)) = SHOWN.with_borrow(|shown| Some((shown.target.as_ref()?.scope.clone(), shown.film.clone()))) else { return };
    let details = ui.global::<DetailsBridge>();
    if scope.len < 2 {
        let shown = SHOWN.with_borrow_mut(|shown| {
            shown.film_paths.clear();
            shown.film_thumbs.clear();
            shown.path().map(Path::to_path_buf)
        });
        model.set_vec(Vec::new());
        film_thumbs(ui, Vec::new(), shown);
        return;
    }
    let paths = scope.paths();
    let center = scope.current.unwrap_or(0).min(paths.len().saturating_sub(1));
    let start = center.saturating_sub(FILM);
    let window: Vec<PathBuf> = paths[start..(center + FILM + 1).min(paths.len())].to_vec();
    let missing: Vec<PathBuf> = SHOWN.with_borrow_mut(|shown| {
        let keep: HashSet<&PathBuf> = window.iter().collect();
        shown.film_thumbs.retain(|p, _| keep.contains(p));
        let items: Vec<FilmItem> = window
            .iter()
            .map(|p| FilmItem {
                thumb: shown.film_thumbs.get(p).cloned().unwrap_or_default(),
                name: p.file_name().map_or_else(Default::default, |n| n.to_string_lossy().as_ref().into()),
            })
            .collect();
        model.set_vec(items);
        let mut missing: Vec<(usize, PathBuf)> = (start..)
            .zip(&window)
            .filter(|(_, p)| !shown.film_thumbs.contains_key(*p))
            .map(|(j, p)| (j.abs_diff(center), p.clone()))
            .collect();
        missing.sort_by_key(|(distance, _)| *distance);
        shown.film_paths = window;
        missing.into_iter().map(|(_, p)| p).collect()
    });
    details.set_film_start(start as i32);
    log::trace!("details: filmstrip: {} thumbnails to load around #{center}", missing.len());
    let shown = SHOWN.with_borrow(|shown| shown.path().filter(|_| shown.up < Up::Soft).map(Path::to_path_buf));
    film_thumbs(ui, missing, shown);
}

/// Loads these thumbnails on a worker (in order), as large as the filmstrip's tiles are on
/// screen; first, if `stand_in` is given (the item shown, its picture not decoded yet), a
/// larger one of it to stand in for the picture: any the cache has, else decoded.
fn film_thumbs(ui: &AppWindow, missing: Vec<PathBuf>, stand_in: Option<PathBuf>) {
    let generation = FILM_GENERATION.fetch_add(1, Ordering::Relaxed) + 1;
    if missing.is_empty() && stand_in.is_none() {
        return;
    }
    let tier = thumbs::Tier::cover(FILM_TILE * ui.window().scale_factor());
    let weak = ui.as_weak();
    thread::spawn(move || {
        let pixels = |rgb: image::RgbImage| Pixels::clone_from_slice(rgb.as_raw(), rgb.width(), rgb.height());
        if let Some(path) = stand_in {
            load_stand_in(&weak, path);
        }
        thumbs::for_files(&missing, tier, thumbs::Accept::Sharp, |i, thumb| {
            if FILM_GENERATION.load(Ordering::Relaxed) != generation {
                return false;
            }
            let path = missing[i].clone();
            let thumb = thumb.map(pixels);
            let _ = weak.upgrade_in_event_loop(move |ui| film_thumb(&ui, path, thumb));
            true
        });
    });
}

/// Loads a thumbnail of `path` to stand in for its picture until that is decoded (any the
/// cache has, else decoded), on the calling worker.
fn load_stand_in(weak: &slint::Weak<AppWindow>, path: PathBuf) {
    let mut found = None;
    thumbs::for_files(&[&path], thumbs::Tier::STAND_IN, thumbs::Accept::Any, |_, thumb| {
        found = thumb;
        false
    });
    if let Some(rgb) = found {
        let thumb = Pixels::clone_from_slice(rgb.as_raw(), rgb.width(), rgb.height());
        let _ = weak.upgrade_in_event_loop(move |ui| stand_in_up(&ui, &path, thumb));
    }
}

/// A thumbnail to stand in for the picture of `path` came in: up if that is still shown, its
/// picture not decoded yet, and nothing larger stands in for it already.
fn stand_in_up(ui: &AppWindow, path: &Path, pixels: Pixels) {
    let shown = SHOWN.with_borrow(|shown| shown.path() == Some(path) && shown.up < Up::Soft);
    let current = ui.global::<DetailsBridge>().get_image().size().width;
    if shown && pixels.width() > current {
        log::debug!("details: stand-in {}×{} up {:?} after the show", pixels.width(), pixels.height(), SHOWN.with_borrow(|shown| shown.shown_at.map(|t| t.elapsed())));
        set_picture(ui, slint::Image::from_rgb8(pixels), Up::Thumbnail);
    }
}

/// A filmstrip thumbnail came in (none: the file has none; not asked for again); it stands
/// in for the picture shown until that is decoded, if nothing does yet.
fn film_thumb(ui: &AppWindow, path: PathBuf, pixels: Option<Pixels>) {
    let thumb = pixels.map(slint::Image::from_rgb8).unwrap_or_default();
    let reference = SHOWN.with_borrow(|shown| shown.target.as_ref().is_some_and(|t| t.path == path));
    let placeholder = SHOWN.with_borrow_mut(|shown| {
        let placeholder = shown.path() == Some(path.as_path()) && shown.up == Up::Nothing && thumb.size().width > 0;
        if let Some(i) = shown.film_paths.iter().position(|p| *p == path) {
            if let Some(mut item) = shown.film.row_data(i) {
                item.thumb = thumb.clone();
                shown.film.set_row_data(i, item);
            }
        }
        if shown.film_paths.contains(&path) || placeholder {
            shown.film_thumbs.insert(path, thumb.clone());
        }
        placeholder
    });
    // The reference's tile in the similar strip, until the strip's own thumbnails are in.
    let similar = ui.global::<DetailsBridge>().get_similar();
    if let Some(mut first) = similar.row_data(0).filter(|f| reference && f.thumb.size().width == 0 && thumb.size().width > 0) {
        first.thumb = thumb.clone();
        similar.set_row_data(0, first);
    }
    if placeholder {
        log::debug!("details: thumbnail up {:?} after the show", SHOWN.with_borrow(|shown| shown.shown_at.map(|t| t.elapsed())));
        set_picture(ui, thumb, Up::Thumbnail);
    }
}

/// The window's scale factor changed: the strips' thumbnails again, at the new size.
pub fn scale_changed(ui: &AppWindow) {
    SHOWN.with_borrow_mut(|shown| shown.film_thumbs.clear());
    load_film(ui);
    load_similar(ui, false);
}

/// The longest edge of the window, in physical pixels: no picture on the page is shown larger.
fn screen_edge(ui: &AppWindow) -> u32 {
    let size = ui.window().size();
    // Not yet sized (the first open, before the window is shown): as a large screen.
    let edge = if size.width == 0 { 2560 } else { size.width.max(size.height) };
    edge.clamp(256, MAX_EDGE)
}

/// Shows the decoded picture of `path` if it is ready (and better than what is up); whether
/// one is up now.
fn show_picture(ui: &AppWindow, path: &Path) -> bool {
    let up = SHOWN.with_borrow(|shown| shown.up);
    match pictures::get(path) {
        Some(Picture::Sharp(pixels)) => set_picture(ui, slint::Image::from_rgb8(pixels), Up::Sharp),
        Some(Picture::Soft(pixels)) if up < Up::Soft => set_picture(ui, slint::Image::from_rgb8(pixels), Up::Soft),
        Some(Picture::Failed) => {
            let details = ui.global::<DetailsBridge>();
            details.set_loading(false);
            if up == Up::Nothing {
                details.set_image_ready(false);
            }
        }
        _ => {}
    }
    SHOWN.with_borrow(|shown| shown.up) > Up::Nothing
}

/// Puts `image` up as the shown item's picture.
fn set_picture(ui: &AppWindow, image: slint::Image, up: Up) {
    let details = ui.global::<DetailsBridge>();
    details.set_image(image);
    details.set_image_ready(true);
    SHOWN.with_borrow_mut(|shown| shown.up = up);
    if up == Up::Sharp {
        details.set_loading(false);
    }
    show_dims(ui);
}

/// The file's size in the info line, and what 100% zoom is, once both it and a decoded
/// picture are in (with only a thumbnail up, the page would ask for detail at once).
fn show_dims(ui: &AppWindow) {
    let details = ui.global::<DetailsBridge>();
    let Some((info, (w, h), up)) = SHOWN.with_borrow(|shown| Some((shown.info.clone(), shown.dims?, shown.up))) else { return };
    details.set_info(format!("{info} · {w} × {h}").into());
    details.set_full_edge(if up >= Up::Soft { w.max(h) as i32 } else { 0 });
}

/// A picture came in from [`pictures`]: up, if it is of the item shown and better.
fn picture_ready(ui: &AppWindow, path: &Path) {
    let Some((up, shown_at)) = SHOWN.with_borrow(|shown| (shown.path() == Some(path)).then_some((shown.up, shown.shown_at))) else { return };
    if up == Up::Sharp {
        return;
    }
    show_picture(ui, path);
    let now = SHOWN.with_borrow(|shown| shown.up);
    if now != up {
        log::debug!("details: {up:?} -> {now:?} {:?} after the show", shown_at.map(|t| t.elapsed()));
    }
}

/// Shows nothing (the item shown is gone and there is nowhere to go back to).
fn clear(ui: &AppWindow) {
    log::debug!("details: cleared (item gone, nowhere to go back to)");
    SHOWN.with_borrow_mut(|shown| {
        shown.target = None;
        shown.peek = None;
        shown.generation += 1;
        shown.other = None;
        shown.similar.clear();
        shown.similar_generation += 1;
    });
    let details = ui.global::<DetailsBridge>();
    details.set_has_item(false);
    details.set_name(SharedString::default());
    details.set_path(SharedString::default());
    details.set_info(SharedString::default());
    details.set_image_ready(false);
    details.set_image(slint::Image::default());
    details.set_faces(ModelRc::default());
    FACES.with_borrow_mut(Vec::clear);
    details.set_editing(-1);
    details.set_adding(false);
    details.set_other_side(SharedString::default());
    details.set_other_ready(false);
    details.set_shown_name(SharedString::default());
    details.set_shown_path(SharedString::default());
    details.set_similar(ModelRc::default());
    details.set_similar_searching(false);
}

/// The item shown, to show it again (Back): the reference, not a similar item peeked at;
/// without its face, which only flashes on arrival.
pub fn snapshot() -> Option<Target> {
    SHOWN.with_borrow(|shown| shown.target.clone()).map(|target| Target { face: None, ..target })
}

/// The other side of a two-sided photo by file name: `photo-0001-front.png` ↔
/// `photo-0001-back.png`, as the Epson V850 backend saves them. For files the catalog
/// hasn't paired (yet).
fn other_side(path: &Path) -> Option<(PathBuf, bool)> {
    let stem = path.file_stem()?.to_str()?;
    let (other, is_back) = if let Some(base) = stem.strip_suffix("-front") {
        (format!("{base}-back"), false)
    } else {
        (format!("{}-front", stem.strip_suffix("-back")?), true)
    };
    let mut other = path.with_file_name(other);
    if let Some(ext) = path.extension() {
        other.set_extension(ext);
    }
    other.exists().then_some((other, is_back))
}

/// The other side of the photo at `path` and whether `path` is the back: the catalog's
/// pairing, else the file names.
fn read_other_side(catalog: Option<&Catalog>, path: &Path) -> Option<(PathBuf, bool)> {
    // A file the catalog has goes by its pairing (which the user may have undone or changed).
    let Some((catalog, asset)) = catalog.and_then(|c| Some((c, c.find_asset(path).ok()??))) else { return other_side(path) };
    let paired = match catalog.back_of(asset).ok()? {
        Some(back) => (catalog.asset_path(back).ok()?, false),
        None => (catalog.asset_path(catalog.front_of(asset).ok()??).ok()?, true),
    };
    Some(paired).filter(|(p, _)| p.exists())
}

/// Shows the other side of the photo in place of this one (no history: Back still returns
/// to where the photo was opened from). Of a similar item peeked at, it is peeked at too.
fn flip(ui: &AppWindow) {
    let Some((target, peeking)) = SHOWN.with_borrow(|shown| Some((Target::new(shown.other.clone()?, shown.target.clone()?.scope), shown.peek.is_some()))) else {
        return;
    };
    if peeking {
        peek(ui, Some(target.path));
    } else {
        show(ui, target);
    }
}

/// The file at `path` changed on disk (e.g. it was rotated): reloads it if it is the one
/// shown (keeping the old picture up until the new one is decoded), the other side, or in
/// the similar strip (the reference too: its tile).
pub fn file_changed(ui: &AppWindow, path: &Path) {
    let (this, other, similar) = SHOWN.with_borrow_mut(|shown| {
        let this = shown.path() == Some(path) && shown.kind.is_some();
        if this {
            let size = std::fs::metadata(path).map_or(0, |m| m.len());
            if let Some((label, _)) = shown.info.split_once(" · ") {
                shown.info = format!("{label} · {}", crate::review::human_size(size));
            }
        }
        let similar = shown.similar.iter().any(|p| p == path) || shown.target.as_ref().is_some_and(|t| t.path == path);
        (this, shown.other.as_deref() == Some(path), similar)
    });
    if this || other || similar {
        log::debug!("details: {} changed (shown {this}, other side {other}, similar {similar})", path.display());
    }
    pictures::forget(path);
    if this {
        // The old picture stays up as a stand-in until the new one is decoded.
        SHOWN.with_borrow_mut(|shown| shown.up = shown.up.min(Up::Thumbnail));
        ui.global::<DetailsBridge>().set_full_edge(0);
        // The page asks again for the new file once its picture is up.
        detail(ui, None);
        load(ui);
    } else if other && ui.global::<DetailsBridge>().get_side_by_side() {
        load_other(ui);
    }
    if similar {
        load_similar(ui, false);
    }
}

/// Files that left the library (e.g. to the trash): if it is the item shown, the page shows
/// the next item of its scope still there (the previous one at the end), else goes back to
/// where it was opened from (or shows nothing); a similar item shown gives way to the
/// reference; the other side and the similar strip drop it.
pub fn files_gone(ui: &AppWindow, gone: &[PathBuf]) {
    if gone.is_empty() {
        return;
    }
    let is_gone = |p: &Path| gone.iter().any(|g| g == p);
    let (this, peeked, other, similar) = SHOWN.with_borrow(|shown| {
        (
            shown.target.as_ref().is_some_and(|t| is_gone(&t.path)),
            shown.peek.as_deref().is_some_and(is_gone),
            shown.other.as_deref().is_some_and(is_gone),
            shown.similar.iter().any(|p| is_gone(p)),
        )
    });
    if this {
        log::debug!("details: the item shown left the library (page {})", ui.get_page());
        if ui.get_page() == nav::DETAILS && (step(ui, 1) || step(ui, -1)) {
            log::debug!("details: showing the next item of the scope instead");
        } else if ui.get_page() == nav::DETAILS && nav::can_go_back(ui) {
            nav::back(ui);
        } else {
            clear(ui);
        }
        return;
    }
    let details = ui.global::<DetailsBridge>();
    if other {
        SHOWN.with_borrow_mut(|shown| shown.other = None);
        details.set_other_side(SharedString::default());
        details.set_other_ready(false);
    }
    if similar {
        // The reference first, kept.
        let keep: Vec<bool> = SHOWN.with_borrow_mut(|shown| {
            let keep = std::iter::once(true).chain(shown.similar.iter().map(|p| !is_gone(p))).collect();
            shown.similar.retain(|p| !is_gone(p));
            keep
        });
        let items: Vec<SimilarItem> = details.get_similar().iter().zip(keep).filter(|(_, k)| *k).map(|(item, _)| item).collect();
        details.set_similar(ModelRc::new(VecModel::from(items)));
        details.set_similar_shown(SHOWN.with_borrow(Shown::similar_shown));
    }
    if peeked {
        log::debug!("details: the similar item shown left the library: back to the reference");
        peek(ui, None);
    }
}

fn root() -> Option<PathBuf> {
    CONTEXT.with_borrow(|c| c.as_ref().map(|c| c.root.clone()))
}

fn open_catalog(root: &Path) -> Option<Catalog> {
    Catalog::open(&Catalog::default_path(root), root).ok()
}

/// Decodes the shown item and reads its review state, faces and other side on a worker; a
/// newer load wins.
fn load(ui: &AppWindow) {
    let Some((path, kind, generation)) = SHOWN.with_borrow_mut(|shown| {
        shown.generation += 1;
        Some((shown.path()?.to_path_buf(), shown.kind?, shown.generation))
    }) else {
        return;
    };
    ui.global::<DetailsBridge>().set_is_image(kind == MediaKind::Image);

    let root = root();
    let weak = ui.as_weak();
    log::debug!("details: load #{generation} {}", path.display());
    LOADING.store(generation, Ordering::Relaxed);
    thread::spawn(move || {
        if LOADING.load(Ordering::Relaxed) != generation {
            return;
        }
        let started = Instant::now();
        let catalog = root.filter(|_| kind == MediaKind::Image).and_then(|root| open_catalog(&root));
        let review = catalog.as_ref().and_then(|c| orient_review(c, &path));
        let faces = catalog.as_ref().map(|c| read_faces(c, &path)).unwrap_or_default();
        let other = if kind == MediaKind::Image { read_other_side(catalog.as_ref(), &path) } else { None };
        drop(catalog);
        let dims = if kind == MediaKind::Image { thumbs::upright_dimensions(&path) } else { None };
        log::debug!("details: load #{generation} read in {:?}: {dims:?}, {} faces, review {review:?}, other side {other:?}", started.elapsed(), faces.len());
        let _ = weak.upgrade_in_event_loop(move |ui| {
            let Some(flash) = SHOWN.with_borrow_mut(|shown| {
                (shown.generation == generation).then(|| {
                    shown.dims = dims;
                    shown.flash.take()
                })
            }) else {
                log::debug!("details: load #{generation} superseded - dropped");
                return;
            };
            let details = ui.global::<DetailsBridge>();
            let (cw, detail) = review.unwrap_or_default();
            details.set_review_cw(cw as i32);
            details.set_review_detail(detail.into());
            show_dims(&ui);
            // Ignored faces (not a face, a stranger) aren't drawn, unless that's the one asked for.
            let flash_at = show_faces(&ui, faces, flash);
            if let Some(id) = flash {
                log::debug!("details: flash face {} -> box {flash_at:?}", id.0);
            }
            if let Some(i) = flash_at {
                details.set_flash_face(i as i32);
                details.set_flash_serial(details.get_flash_serial() + 1);
            }

            // Reloaded with this one too (after a rotation, say).
            set_other(&ui, other, true);
        });
    });
}

/// Shows `other` as the other side of the shown photo (and whether the shown one is the
/// back); decodes it again for "Front & back" if it is another file, or if `reload`.
fn set_other(ui: &AppWindow, other: Option<(PathBuf, bool)>, reload: bool) {
    let details = ui.global::<DetailsBridge>();
    let before = SHOWN.with_borrow_mut(|shown| std::mem::replace(&mut shown.other, other.as_ref().map(|(p, _)| p.clone())));
    details.set_other_side(other.as_ref().map_or_else(SharedString::default, |(p, _)| p.display().to_string().into()));
    details.set_is_back(other.as_ref().is_some_and(|(_, is_back)| *is_back));
    if other.is_none() {
        details.set_other_ready(false);
    } else if details.get_side_by_side() && (reload || before.as_ref() != other.as_ref().map(|(p, _)| p)) {
        details.set_other_ready(false);
        load_other(ui);
    }
}

/// The catalog data of these files changed (tags, faces, pairing; reported through the
/// pipeline relay): the info panel and the other side are read again if they concern the
/// item shown.
pub fn data_changed(ui: &AppWindow, paths: &[PathBuf]) {
    let concerns = SHOWN.with_borrow(|shown| {
        let shown_path = shown.path()?;
        paths.iter().any(|p| p == shown_path || shown.other.as_deref() == Some(p.as_path())).then(|| shown_path.to_path_buf())
    });
    let Some(path) = concerns else { return };
    log::debug!("details: catalog data of {} changed: reading its info and other side again", path.display());
    load_info(ui);
    reload_side(ui);
}

/// Reads the shown photo's other side again (its pairing changed), on a worker.
fn reload_side(ui: &AppWindow) {
    let Some((path, generation)) = SHOWN.with_borrow_mut(|shown| {
        shown.other_side_generation += 1;
        Some((shown.path()?.to_path_buf(), shown.other_side_generation)).filter(|_| shown.kind == Some(MediaKind::Image))
    }) else {
        return;
    };
    let (root, weak) = (root(), ui.as_weak());
    thread::spawn(move || {
        let catalog = root.and_then(|root| open_catalog(&root));
        let other = read_other_side(catalog.as_ref(), &path);
        let _ = weak.upgrade_in_event_loop(move |ui| {
            if SHOWN.with_borrow(|shown| shown.other_side_generation != generation || shown.path() != Some(path.as_path())) {
                return;
            }
            log::debug!("details: other side of {} is now {other:?}", path.display());
            set_other(&ui, other, false);
        });
    });
}

/// Reads the info panel's data for the shown item on a worker (if the panel is open), with
/// every tag in the catalog for its tag box.
fn load_info(ui: &AppWindow) {
    let details = ui.global::<DetailsBridge>();
    let Some((path, generation)) = SHOWN.with_borrow_mut(|shown| {
        shown.info_generation += 1;
        Some((shown.path()?.to_path_buf(), shown.info_generation))
    }) else {
        return;
    };
    if !details.get_info_open() {
        return;
    }
    let Some(root) = root() else { return };
    details.set_info_loading(true);
    let weak = ui.as_weak();
    thread::spawn(move || {
        let read = open_catalog(&root).ok_or_else(|| "can't open the catalog".to_owned()).and_then(|catalog| {
            let asset = catalog.asset(&path).map_err(|e| e.to_string())?;
            let p = ops::properties(&catalog, asset).map_err(|e| e.to_string())?;
            let tags = catalog.all_tags().unwrap_or_default();
            Ok((info_rows(&p), info_tags(&p.tags), tags))
        });
        if let Err(e) = &read {
            log::warn!("details: info for {}: {e}", path.display());
        }
        let _ = weak.upgrade_in_event_loop(move |ui| {
            if SHOWN.with_borrow(|shown| shown.info_generation != generation) {
                return;
            }
            let details = ui.global::<DetailsBridge>();
            details.set_info_loading(false);
            let (rows, tags) = match read {
                Ok((rows, tags, all)) => {
                    TAGS.set(all);
                    (rows, tags)
                }
                Err(e) => (vec![("Error".to_owned(), e)], Vec::new()),
            };
            let rows: Vec<PropRow> = rows.into_iter().map(|(label, value)| PropRow { label: label.into(), value: value.into(), heading: false }).collect();
            details.set_info_rows(ModelRc::new(VecModel::from(rows)));
            details.set_info_tags(ModelRc::new(VecModel::from(tags)));
        });
    });
}

/// The info panel's facts about the file: taken, modified, dimensions (the file's own),
/// size, kind; empty ones left out.
fn info_rows(p: &ops::Properties) -> Vec<(String, String)> {
    let kind = match p.kind {
        Some(MediaKind::Image) => "Image",
        Some(MediaKind::Video) => "Video",
        Some(MediaKind::Audio) => "Audio",
        None => "File",
    };
    let kind = match &p.format {
        Some(format) => format!("{kind} · {}", format.to_uppercase()),
        None => kind.to_owned(),
    };
    let status = if p.trashed { "In the trash" } else if p.missing { "Missing" } else { "" };
    [
        ("Status", status.to_owned()),
        ("Taken", p.exif.taken.clone().unwrap_or_default()),
        ("Modified", p.modified.map(actions::local_time).unwrap_or_default()),
        ("Dimensions", p.dimensions.map_or_else(String::new, |(w, h)| format!("{w} × {h}"))),
        ("Size", p.size.map(crate::review::human_size).unwrap_or_default()),
        ("Kind", kind),
    ]
    .into_iter()
    .filter(|(_, v)| !v.is_empty())
    .map(|(label, value)| (label.to_owned(), value))
    .collect()
}

/// The info panel's tags: grouped by kind, People first, each once (the user's, else the
/// most confident detector's, from the confidence searches use), under a heading per kind.
fn info_tags(tags: &[AppliedTag]) -> Vec<InfoTag> {
    const KINDS: [(TagKind, &str); 6] = [
        (TagKind::Person, "People"),
        (TagKind::Keyword, "Keywords"),
        (TagKind::Event, "Events"),
        (TagKind::Place, "Places"),
        (TagKind::Scene, "Scenes"),
        (TagKind::Object, "Objects"),
    ];
    let mut rows = Vec::new();
    for (kind, heading) in KINDS {
        let mut seen: Vec<(TagId, &AppliedTag)> = Vec::new();
        for tag in tags.iter().filter(|t| t.kind == kind && t.confidence.is_none_or(|c| c >= DEFAULT_MIN_CONFIDENCE)) {
            match seen.iter_mut().find(|(id, _)| *id == tag.tag) {
                Some((_, kept)) if kept.source == Source::User => {}
                Some((_, kept)) if tag.source == Source::User || tag.confidence > kept.confidence => *kept = tag,
                Some(_) => {}
                None => seen.push((tag.tag, tag)),
            }
        }
        if seen.is_empty() {
            continue;
        }
        rows.push(InfoTag { heading: true, label: heading.into(), key: SharedString::default(), note: SharedString::default() });
        for (_, tag) in seen {
            let shown = match kind {
                TagKind::Person => archive_core::catalog::people::display_name(&tag.path).to_owned(),
                _ => tag.path.replace('|', " / "),
            };
            let note = match (&tag.source, tag.confidence) {
                (Source::Detector(name), Some(c)) => format!("{} {:.0}%", name.split('/').next().unwrap_or(name), c * 100.0),
                _ => String::new(),
            };
            rows.push(InfoTag {
                heading: false,
                label: shown.into(),
                key: format!("{}:{}", kind.name(), tag.path).into(),
                note: note.into(),
            });
        }
    }
    rows
}

/// A change to the shown item's tags from the info panel.
#[derive(Debug)]
enum TagChange {
    /// What was typed in the tag box (`ops::add_tag`).
    Add(String),
    /// The tag `kind:path` (`ops::remove_tag`).
    Remove(String),
}

/// Makes `change` on a worker, then reads the panel again and reports the item changed; a
/// person taken off also redraws its faces (theirs are unknown now) and tells People.
fn change_tag(ui: &AppWindow, change: TagChange) {
    let Some(path) = SHOWN.with_borrow(|shown| shown.path().map(Path::to_path_buf)) else { return };
    let Some(root) = root() else { return };
    let details = ui.global::<DetailsBridge>();
    details.set_info_tag_working(true);
    details.set_info_tag_error(SharedString::default());
    let weak = ui.as_weak();
    thread::spawn(move || {
        let mut catalog = open_catalog(&root);
        let result = match catalog.as_mut() {
            None => Err("can't open the catalog".to_owned()),
            Some(catalog) => catalog.asset(&path).map_err(|e| e.to_string()).and_then(|asset| match &change {
                TagChange::Add(text) => ops::add_tag(catalog, asset, text).map(|_| ()).map_err(|e| e.to_string()),
                TagChange::Remove(key) => match ops::remove_tag(catalog, asset, key) {
                    Ok(true) => Ok(()),
                    Ok(false) => Err("it isn't on this item any more".to_owned()),
                    Err(e) => Err(e.to_string()),
                },
            }),
        };
        let person = matches!(&change, TagChange::Remove(key) if ops::parse_tag(key).0 == TagKind::Person);
        let faces = (person && result.is_ok()).then(|| catalog.as_ref().map(|c| read_faces(c, &path)).unwrap_or_default());
        match &result {
            Ok(()) => log::info!("details: {change:?} on {} done", path.display()),
            Err(e) => log::warn!("details: {change:?} on {}: {e}", path.display()),
        }
        if result.is_ok() {
            pipeline::changed(&path, false);
        }
        let _ = weak.upgrade_in_event_loop(move |ui| {
            let details = ui.global::<DetailsBridge>();
            details.set_info_tag_working(false);
            if faces.is_some() {
                crate::people::faces_changed(&ui);
            }
            if SHOWN.with_borrow(|s| s.path() != Some(path.as_path())) {
                return;
            }
            match result {
                Ok(()) => {
                    if let TagChange::Add(_) = change {
                        details.set_info_tag_serial(details.get_info_tag_serial() + 1);
                        details.set_info_tag_matches(ModelRc::default());
                    }
                    if let Some(faces) = faces.filter(|_| details.get_editing() < 0) {
                        show_faces(&ui, faces, None);
                    }
                    load_info(&ui);
                }
                Err(e) => details.set_info_tag_error(e.into()),
            }
        });
    });
}

/// The page's zoomed `view` of the shown picture, settled (`None`: no longer zoomed in past
/// it): draws the part in view from the file at full resolution, decoded on a worker the first
/// time (the latest view wins). With `None`, drops that and the decoded file.
fn detail(ui: &AppWindow, view: Option<View>) {
    let generation = DETAIL.fetch_add(1, Ordering::Relaxed) + 1;
    let path = SHOWN.with_borrow(|shown| shown.path().filter(|_| shown.kind == Some(MediaKind::Image)).map(Path::to_path_buf));
    let (Some(view), Some(path)) = (view, path) else {
        let details = ui.global::<DetailsBridge>();
        if details.get_tile_w() > 0.0 {
            log::debug!("details: zoomed out - detail dropped");
        }
        details.set_tile(slint::Image::default());
        details.set_tile_w(0.0);
        // Not here: a decode in flight holds the lock.
        thread::spawn(move || {
            let mut full = FULL.lock().unwrap();
            if DETAIL.load(Ordering::Relaxed) == generation {
                *full = None;
            }
        });
        return;
    };
    log::trace!("details: detail #{generation} {view:?}");
    let weak = ui.as_weak();
    thread::spawn(move || {
        // One decode at a time; the requests queued behind it find it done.
        let mut full = FULL.lock().unwrap();
        if DETAIL.load(Ordering::Relaxed) != generation {
            return;
        }
        let stamp = stamp(&path);
        if !full.as_ref().is_some_and(|f| f.path == path && stamp.is_some() && f.stamp == stamp) {
            // The one held goes first: only one at a time.
            *full = None;
            let started = std::time::Instant::now();
            let Some(image) = thumbs::decode_full(&path, MediaKind::Image, FULL_PIXELS) else {
                log::warn!("details: {} could not be decoded at full resolution", path.display());
                return;
            };
            log::debug!("details: {} decoded at full resolution ({}x{}) in {:?}", path.display(), image.width(), image.height(), started.elapsed());
            *full = Some(Full { path: path.clone(), stamp, image });
        }
        if DETAIL.load(Ordering::Relaxed) != generation {
            return;
        }
        let Some(f) = full.as_ref() else { return };
        let (tile, [x, y, w, h]) = region(&f.image, view);
        drop(full);
        let pixels = Pixels::clone_from_slice(tile.as_raw(), tile.width(), tile.height());
        let _ = weak.upgrade_in_event_loop(move |ui| {
            if DETAIL.load(Ordering::Relaxed) != generation || SHOWN.with_borrow(|shown| shown.path() != Some(path.as_path())) {
                return;
            }
            let details = ui.global::<DetailsBridge>();
            details.set_tile(slint::Image::from_rgb8(pixels));
            details.set_tile_x(x);
            details.set_tile_y(y);
            details.set_tile_w(w);
            details.set_tile_h(h);
        });
    });
}

/// The part of `full` to draw for `view`: what is in view and a quarter of that around it
/// (for a short pan), no larger than it is shown; and where it is, in fractions of `full`.
fn region(full: &RgbImage, view: View) -> (RgbImage, [f32; 4]) {
    let (w, h) = full.dimensions();
    let span = |from: f32, to: f32, size: u32| {
        let margin = (to - from) / 4.0;
        let start = (((from - margin).clamp(0.0, 1.0) * size as f32).floor() as u32).min(size - 1);
        let end = (((to + margin).clamp(0.0, 1.0) * size as f32).ceil() as u32).clamp(start + 1, size);
        (start, end - start)
    };
    let (x, cw) = span(view.left, view.right, w);
    let (y, ch) = span(view.top, view.bottom, h);
    let crop = image::imageops::crop_imm(full, x, y, cw, ch).to_image();
    let edge = (cw.max(ch) as f32 * view.width / w as f32).ceil() as u32;
    let tile = thumbs::fit(&crop, edge.max(1));
    (tile, [x as f32 / w as f32, y as f32 / h as f32, cw as f32 / w as f32, ch as f32 / h as f32])
}

/// Decodes the other side of the photo and reads its faces, for the side-by-side view.
fn load_other(ui: &AppWindow) {
    let Some((path, generation)) = SHOWN.with_borrow_mut(|shown| {
        shown.other_generation += 1;
        Some((shown.other.clone()?, shown.other_generation))
    }) else {
        return;
    };
    let (root, weak) = (root(), ui.as_weak());
    thread::spawn(move || {
        let faces = root.and_then(|root| open_catalog(&root)).map(|c| read_faces(&c, &path)).unwrap_or_default();
        let pixels =
            thumbs::decode(&path, MediaKind::Image, thumbs::Bound::Fit(OTHER_EDGE)).map(|rgb| Pixels::clone_from_slice(rgb.as_raw(), rgb.width(), rgb.height()));
        let _ = weak.upgrade_in_event_loop(move |ui| {
            if SHOWN.with_borrow(|shown| shown.other_generation != generation) {
                return;
            }
            let details = ui.global::<DetailsBridge>();
            let faces: Vec<DetailFace> = faces.into_iter().filter(|f| !f.ignored).map(|f| f.face).collect();
            details.set_other_faces(ModelRc::new(VecModel::from(faces)));
            match pixels {
                Some(pixels) => {
                    details.set_other_image(slint::Image::from_rgb8(pixels));
                    details.set_other_ready(true);
                }
                None => details.set_other_ready(false),
            }
        });
    });
}

/// Finds the items of the reference's scope most like it, on a worker, and fills the strip
/// after the reference. `fresh` (another reference): the strip shows only the reference and
/// "Looking for similar…" meanwhile; else (thumbnails again, a file in it changed) it stays
/// as it is until they are in.
fn load_similar(ui: &AppWindow, fresh: bool) {
    let Some((path, scope, generation)) = SHOWN.with_borrow_mut(|shown| {
        shown.similar_generation += 1;
        let target = shown.target.as_ref()?;
        Some((target.path.clone(), target.scope.clone(), shown.similar_generation))
    }) else {
        return;
    };
    let Some(root) = root() else { return };
    let details = ui.global::<DetailsBridge>();
    let image = classify(&path) == Some(MediaKind::Image);
    if fresh {
        let thumb = crate::review::thumbnail(&path).or_else(|| SHOWN.with_borrow(|shown| shown.film_thumbs.get(&path).cloned()));
        let reference = SimilarItem { thumb: thumb.unwrap_or_default(), name: file_name(&path), score: 100 };
        details.set_similar(ModelRc::new(VecModel::from(vec![reference])));
        details.set_similar_shown(0);
        details.set_similar_searching(image);
    }
    // Listed here (once per scope): a scope may only be read on the UI thread.
    let candidates = scope.paths();
    let tier = thumbs::Tier::cover(FILM_TILE * ui.window().scale_factor());
    let weak = ui.as_weak();
    thread::spawn(move || {
        let found = if image { open_catalog(&root).map(|c| similar(&c, &path, &candidates)).unwrap_or_default() } else { Vec::new() };
        log::debug!("details: {} similar among {} in {:?}", found.len(), candidates.len(), scope.label);
        let found: Vec<(PathBuf, f32)> = std::iter::once((path, 1.0)).chain(found).collect();
        let paths: Vec<&PathBuf> = found.iter().map(|(path, _)| path).collect();
        let mut thumbs = vec![None; paths.len()];
        thumbs::for_files(&paths, tier, thumbs::Accept::Sharp, |i, thumb| {
            thumbs[i] = thumb.map(|rgb| Pixels::clone_from_slice(rgb.as_raw(), rgb.width(), rgb.height()));
            true
        });
        let items: Vec<(PathBuf, f32, Option<Pixels>)> = found.into_iter().zip(thumbs).map(|((path, score), thumb)| (path, score, thumb)).collect();
        let _ = weak.upgrade_in_event_loop(move |ui| {
            let Some((target, shown_at)) = SHOWN.with_borrow_mut(|shown| {
                (shown.similar_generation == generation).then(|| {
                    shown.similar = items.iter().skip(1).map(|(p, _, _)| p.clone()).collect();
                    (shown.target.clone(), shown.similar_shown())
                })
            }) else {
                return;
            };
            let model: Vec<SimilarItem> = items
                .into_iter()
                .map(|(path, score, thumb)| SimilarItem {
                    thumb: thumb.map(slint::Image::from_rgb8).unwrap_or_default(),
                    name: file_name(&path),
                    score: (score * 100.0).round() as i32,
                })
                .collect();
            let details = ui.global::<DetailsBridge>();
            details.set_similar(ModelRc::new(VecModel::from(model)));
            details.set_similar_shown(shown_at);
            details.set_similar_searching(false);
            // Their pictures ahead too, the best few, once the neighbours are.
            if let Some(target) = target {
                want_pictures(&ui, &target);
            }
        });
    });
}

/// The items among `candidates` from the same scene or event as `path`, best first.
fn similar(catalog: &Catalog, path: &Path, candidates: &[PathBuf]) -> Vec<(PathBuf, f32)> {
    let Some(asset) = catalog.find_asset(path).ok().flatten() else { return Vec::new() };
    let ids: Vec<_> = candidates.iter().filter_map(|p| catalog.find_asset(p).ok().flatten()).collect();
    let found = catalog.similar(asset, &ids, SIMILAR).unwrap_or_default();
    found
        .into_iter()
        .filter_map(|(id, score)| Some((catalog.asset_path(id).ok()?, score)))
        .filter(|(p, _)| p.exists())
        .collect()
}

/// Item `i` of the similar strip (0 the reference), with the reference's scope, for its menu.
fn similar_target(i: i32) -> Option<Target> {
    let mut target = SHOWN.with_borrow(|shown| {
        let target = shown.target.as_ref()?;
        let path = match usize::try_from(i).ok()? {
            0 => target.path.clone(),
            i => shown.similar.get(i - 1)?.clone(),
        };
        Some(Target::new(path, target.scope.clone()))
    })?;
    locate(&mut target);
    Some(target)
}

/// The view the shown item was opened from, as it was then.
#[allow(dead_code)] // Kept for other features drawing on the scope.
pub fn scope() -> Option<Scope> {
    SHOWN.with_borrow(|shown| shown.target.as_ref().map(|t| t.scope.clone()))
}

/// The rotation orient left for the user to decide on `path`, if any.
fn orient_review(catalog: &Catalog, path: &Path) -> Option<(u16, String)> {
    let asset = catalog.find_asset(path).ok()??;
    match catalog.run_of(asset, ORIENT).ok()?? {
        (RunStatus::Review, Some(cw), detail) if cw % 360 != 0 => Some(((cw % 360) as u16, detail)),
        _ => None,
    }
}

/// The faces found in `path`, left to right, with the names of the people confirmed.
fn read_faces(catalog: &Catalog, path: &Path) -> Vec<FaceInfo> {
    let Some(asset) = catalog.find_asset(path).ok().flatten() else { return Vec::new() };
    let Ok(faces) = catalog.face_boxes(asset) else { return Vec::new() };
    faces
        .into_iter()
        .map(|face| {
            let name = face.person.as_ref().map(|(_, name)| SharedString::from(name.as_str())).unwrap_or_default();
            let suggestion = face.suggestion.map(|(tag, name, _)| (tag, name));
            let shown = suggestion.as_ref().map(|(_, name)| SharedString::from(name.as_str())).unwrap_or_default();
            let r = face.region;
            FaceInfo {
                id: face.face,
                face: DetailFace { x: r.x, y: r.y, w: r.w, h: r.h, name, suggestion: shown },
                ignored: face.status == FaceStatus::Ignored,
                person: face.person,
                suggestion,
            }
        })
        .collect()
}

/// Draws `faces` over the picture (but ignored ones, unless it is `keep`), closing a face's
/// popup; returns where `keep` is among those drawn.
fn show_faces(ui: &AppWindow, faces: Vec<FaceInfo>, keep: Option<FaceId>) -> Option<usize> {
    let faces: Vec<FaceInfo> = faces.into_iter().filter(|f| !f.ignored || Some(f.id) == keep).collect();
    let at = keep.and_then(|id| faces.iter().position(|f| f.id == id));
    let details = ui.global::<DetailsBridge>();
    details.set_editing(-1);
    details.set_faces(ModelRc::new(VecModel::from(faces.iter().map(|f| f.face.clone()).collect::<Vec<_>>())));
    FACES.set(faces);
    at
}

/// Drawn face `i`.
fn face_info(i: i32) -> Option<FaceInfo> {
    FACES.with_borrow(|faces| faces.get(usize::try_from(i).ok()?).cloned())
}

/// The drawn face at (`x`, `y`) (fractions of the picture): the smallest box holding it.
/// Called from the page's bindings: never waits for the list.
fn face_at(x: f32, y: f32) -> Option<usize> {
    let faces = FACES.try_with(|f| f.try_borrow().ok().map(|f| f.iter().map(|f| f.face.clone()).collect::<Vec<_>>())).ok()??;
    faces
        .iter()
        .enumerate()
        .filter(|(_, f)| (f.x..=f.x + f.w).contains(&x) && (f.y..=f.y + f.h).contains(&y))
        .min_by(|a, b| (a.1.w * a.1.h).total_cmp(&(b.1.w * b.1.h)))
        .map(|(i, _)| i)
}

/// Opens the popup for drawn face `i` (closes it for -1), starting with the name it has or
/// is suggested as; reads the people's names for its matches on a worker.
fn edit_face(ui: &AppWindow, i: i32) {
    let details = ui.global::<DetailsBridge>();
    let Some(face) = face_info(i) else {
        if details.get_editing() >= 0 {
            log::debug!("details: face popup closed");
        }
        details.set_editing(-1);
        return;
    };
    let person = face.person.as_ref().map(|(_, n)| n.clone()).unwrap_or_default();
    let suggestion = face.suggestion.as_ref().map(|(_, n)| n.clone()).unwrap_or_default();
    log::debug!("details: face popup for face #{i} (face {}, person {person:?}, suggested {suggestion:?})", face.id.0);
    details.set_edit_person(person.as_str().into());
    details.set_edit_suggestion(suggestion.as_str().into());
    details.set_edit_text(if person.is_empty() { suggestion } else { person }.into());
    details.set_edit_matches(ModelRc::default());
    details.set_editing(i);
    details.set_edit_serial(details.get_edit_serial() + 1);
    let Some(root) = root() else { return };
    let weak = ui.as_weak();
    thread::spawn(move || {
        let names = open_catalog(&root).and_then(|c| c.leaf_tags(TagKind::Person).ok()).unwrap_or_default();
        let names: Vec<String> = names.into_iter().map(|(_, path)| path).collect();
        let _ = weak.upgrade_in_event_loop(move |_| NAMES.set(names));
    });
}

/// The popup's name, for the face it is open for: the person it was suggested as if that is
/// the name (so a name shared by two people means that one), else whoever the name means.
fn assign(ui: &AppWindow, text: &str) {
    let details = ui.global::<DetailsBridge>();
    let Some(face) = face_info(details.get_editing()) else { return };
    if text.is_empty() {
        log::debug!("details: assign an empty name - ignored");
        return;
    }
    if face.person.as_ref().is_some_and(|(_, name)| name == text) {
        log::debug!("details: face {} is {text:?} already", face.id.0);
        edit_face(ui, -1);
        return;
    }
    let change = match face.suggestion {
        Some((tag, name)) if name == text => FaceChange::Confirm(face.id, tag),
        _ => FaceChange::Name(face.id, text.to_owned()),
    };
    change_face(ui, change);
}

/// Makes `change` to the shown picture's faces on a worker, then draws its faces anew (with
/// the popup open on a face just added), reports the picture changed and refreshes the
/// suggestions. A failure is shown in the notice.
fn change_face(ui: &AppWindow, change: FaceChange) {
    let Some(path) = SHOWN.with_borrow(|shown| shown.path().map(Path::to_path_buf)) else { return };
    let Some((root, pipeline)) = CONTEXT.with_borrow(|c| c.as_ref().map(|c| (c.root.clone(), c.pipeline.clone()))) else { return };
    let details = ui.global::<DetailsBridge>();
    details.set_editing(-1);
    details.set_face_working(true);
    let engine = pipeline.engine();
    let weak = ui.as_weak();
    thread::spawn(move || {
        let _one_at_a_time = FACE_WRITES.lock().unwrap_or_else(|e| e.into_inner());
        let started = std::time::Instant::now();
        let mut catalog = open_catalog(&root);
        let result = match catalog.as_mut() {
            Some(catalog) => apply(catalog, &path, &change, engine.as_deref()),
            None => Err("can't open the catalog".to_owned()),
        };
        let faces = catalog.as_ref().map(|c| read_faces(c, &path)).unwrap_or_default();
        // What was saved, as the journal has it, for the notice (with Undo).
        let saved = result.is_ok().then(|| catalog.as_ref()?.next_undo().ok().flatten().map(|(c, _)| c.description)).flatten();
        match &result {
            Ok(face) => log::info!("details: {change:?} on {} done in {:?} (face {})", path.display(), started.elapsed(), face.0),
            Err(e) => log::warn!("details: {change:?} on {}: {e}", path.display()),
        }
        if result.is_ok() {
            pipeline::changed(&path, false);
        }
        let added = matches!(change, FaceChange::Add(_));
        let reply = result.clone();
        let shown = path.clone();
        let _ = weak.upgrade_in_event_loop(move |ui| {
            ui.global::<DetailsBridge>().set_face_working(false);
            if reply.is_ok() {
                crate::people::faces_changed(&ui);
            }
            if let Some(saved) = saved {
                actions::notify(&ui, saved, false, true);
            }
            if SHOWN.with_borrow(|s| s.path() != Some(shown.as_path())) {
                return;
            }
            match reply {
                Ok(face) => {
                    show_faces(&ui, faces, None);
                    if added {
                        if let Some(i) = FACES.with_borrow(|f| f.iter().position(|f| f.id == face)) {
                            edit_face(&ui, i as i32);
                        }
                    }
                }
                Err(e) => actions::notify(&ui, format!("Can't change the face: {e}"), true, false),
            }
        });
        // Faces like the ones just named are suggested as them (as People does).
        if let (Ok(_), Some(catalog)) = (&result, catalog.as_mut()) {
            match catalog.refresh_suggestions(DEFAULT_MIN_SIMILARITY) {
                Ok(n) => log::debug!("details: suggestions refreshed: {n} faces suggested"),
                Err(e) => log::warn!("details: suggestions: {e}"),
            }
        }
    });
}

/// Makes `change` to the faces of the picture at `path`; returns the face it was made to.
fn apply(catalog: &mut Catalog, path: &Path, change: &FaceChange, engine: Option<&archive_core::pipeline::Engine>) -> Result<FaceId, String> {
    let err = |e: archive_core::catalog::Error| e.to_string();
    match change {
        FaceChange::Name(face, name) => ops::name_faces(catalog, &[*face], name).map(|_| *face).map_err(err),
        FaceChange::Confirm(face, person) => ops::confirm_faces(catalog, &[*face], *person).map(|()| *face).map_err(err),
        FaceChange::Not(face) => ops::not_person(catalog, &[*face]).map(|()| *face).map_err(err),
        FaceChange::Ignore(face) => ops::ignore_faces(catalog, &[*face]).map(|()| *face).map_err(err),
        FaceChange::Delete(face) => ops::delete_faces(catalog, &[*face]).map(|()| *face).map_err(err),
        FaceChange::Add(region) => {
            let asset = catalog.asset(path).map_err(err)?;
            let models = engine.and_then(|e| e.face_models()).or_else(own_face_models);
            ops::add_face(catalog, asset, *region, models.as_deref())
        }
    }
}

/// Face models for "Add face" without an engine here: loaded on first use (on a worker).
fn own_face_models() -> Option<Arc<Faces>> {
    OWN_FACE_MODELS
        .get_or_init(|| {
            ops::face_models().map(Arc::new).map_err(|e| log::warn!("details: {e}; faces added without an embedding")).ok()
        })
        .clone()
}

/// Asks for `action` on the shown file, on a worker (the engine replies once the file is
/// rewritten). A failure is shown under the picture.
fn ask(weak: &slint::Weak<AppWindow>, action: Action) {
    let Some(ui) = weak.upgrade() else { return };
    let Some(path) = SHOWN.with_borrow(|shown| shown.path().map(Path::to_path_buf)) else { return };
    let Some((root, pipeline)) = CONTEXT.with_borrow(|c| c.as_ref().map(|c| (c.root.clone(), c.pipeline.clone()))) else { return };
    let engine = pipeline.engine();
    log::debug!("details: {action:?} on {} (engine here: {})", path.display(), engine.is_some());
    let details = ui.global::<DetailsBridge>();
    details.set_working(true);
    details.set_message(SharedString::default());
    let weak = weak.clone();
    thread::spawn(move || {
        let result = match action {
            Action::Rotate(cw) => actions::rotate(&root, &pipeline, &path, cw),
            Action::Accept | Action::Keep => actions::open_catalog(&root).and_then(|catalog| {
                let asset = catalog.asset(&path).map_err(|e| e.to_string())?;
                match action {
                    Action::Accept => ops::accept(&catalog, engine.as_deref(), asset).map(|_| ()),
                    _ => ops::keep(&catalog, engine.as_deref(), asset),
                }
            }),
        };
        match &result {
            Ok(()) => log::info!("details: {action:?} {} done", path.display()),
            Err(e) => log::warn!("details: {action:?} {}: {e}", path.display()),
        }
        if result.is_ok() {
            // "N rotations to review" and Review's badge (a rotation is reported, a kept
            // file isn't); without an engine, the turned picture itself.
            pipeline::changed(&path, engine.is_none() && matches!(action, Action::Accept));
        }
        let _ = weak.upgrade_in_event_loop(move |ui| {
            if SHOWN.with_borrow(|shown| shown.path() != Some(path.as_path())) {
                return;
            }
            let details = ui.global::<DetailsBridge>();
            details.set_working(false);
            match result {
                // The picture itself is reloaded when the rotated file is reported.
                Ok(()) => {
                    details.set_review_cw(0);
                    let what = match action {
                        Action::Rotate(cw) => format!("Rotated {cw}°"),
                        Action::Accept => "Rotation accepted".to_owned(),
                        Action::Keep => "Kept as it is".to_owned(),
                    };
                    actions::notify(&ui, what, false, true);
                }
                Err(e) => {
                    details.set_message(e.into());
                }
            }
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locate_points_the_scope_at_the_item() {
        let paths: Vec<PathBuf> = ["a", "b", "c"].map(PathBuf::from).to_vec();
        let mut target = Target::new("c".into(), Scope::of("Review".into(), paths.clone(), Some(0)));
        locate(&mut target);
        assert_eq!(target.scope.current, Some(2));
        // Not one of them (the back of a photo in it): stays where it was.
        let mut target = Target::new("c-back".into(), Scope::of("Review".into(), paths, Some(1)));
        locate(&mut target);
        assert_eq!(target.scope.current, Some(1));
    }

    #[test]
    fn a_similar_item_peeked_at_is_shown_in_place_of_the_reference() {
        let paths: Vec<PathBuf> = ["a", "b", "c"].map(PathBuf::from).to_vec();
        let mut shown = Shown { target: Some(Target::new("a".into(), Scope::of("Review".into(), paths, Some(0)))), ..Default::default() };
        shown.similar = vec!["c".into(), "b".into()];
        assert_eq!((shown.path(), shown.similar_shown()), (Some(Path::new("a")), 0));
        shown.peek = Some("b".into());
        assert_eq!((shown.path(), shown.similar_shown()), (Some(Path::new("b")), 2));
        shown.peek = Some("b-back".into());
        assert_eq!(shown.similar_shown(), -1, "the other side of one isn't in the strip");
    }

    #[test]
    fn region_is_the_view_with_a_margin_no_larger_than_shown() {
        let full = RgbImage::from_fn(1000, 500, |x, _| image::Rgb([(x / 4) as u8, 0, 0]));
        // The middle half across, the top half down, shown 2000 px wide (2× the file).
        let view = View { left: 0.25, top: 0.0, right: 0.75, bottom: 0.5, width: 2000.0 };
        let (tile, rect) = region(&full, view);
        // A quarter of the view around it, within the image: 125 px each side, 62.5 down.
        assert_eq!(tile.dimensions(), (750, 313), "full resolution: shown larger than the file");
        assert_eq!(rect, [0.125, 0.0, 0.75, 0.626]);
        assert_eq!(tile.get_pixel(0, 0)[0], (125 / 4) as u8);
        // Shown at 500 px wide (half the file): scaled down to that.
        let (tile, _) = region(&full, View { width: 500.0, ..view });
        assert_eq!(tile.dimensions(), (375, 157));
        // A sliver at the edge still makes a tile.
        let (tile, rect) = region(&full, View { left: 1.0, top: 1.0, right: 1.0, bottom: 1.0, width: 1000.0 });
        assert_eq!(tile.dimensions(), (1, 1));
        assert_eq!(rect, [0.999, 0.998, 0.001, 0.002]);
    }

    #[test]
    fn other_side_by_file_name() {
        let dir = tempfile::tempdir().unwrap();
        let front = dir.path().join("photo-0001-front.png");
        let back = dir.path().join("photo-0001-back.png");
        std::fs::write(&front, b"").unwrap();
        assert_eq!(other_side(&front), None, "no back yet");
        std::fs::write(&back, b"").unwrap();
        assert_eq!(other_side(&front), Some((back.clone(), false)));
        assert_eq!(other_side(&back), Some((front.clone(), true)));
        assert_eq!(read_other_side(None, &back), Some((front, true)));
    }

    #[test]
    fn the_catalog_pairing_wins_over_the_names() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let [front, back, other] = ["photo-0001-front.png", "photo-0001-back.png", "x.png"].map(|n| root.join(n));
        for path in [&front, &back, &other] {
            image::RgbImage::new(4, 4).save(path).unwrap();
        }
        let mut catalog = Catalog::open(&Catalog::default_path(&root), &root).unwrap();
        let [f, b, x] = [&front, &back, &other].map(|p| catalog.asset(p).unwrap());
        assert_eq!(read_other_side(Some(&catalog), &front), Some((back.clone(), false)));
        ops::unpair(&mut catalog, f).unwrap();
        assert_eq!(read_other_side(Some(&catalog), &front), None, "unpaired by hand, whatever the names");
        ops::pair(&mut catalog, x, b).unwrap();
        assert_eq!(read_other_side(Some(&catalog), &back), Some((other.clone(), true)));
        ops::swap_sides(&mut catalog, b).unwrap();
        assert_eq!(read_other_side(Some(&catalog), &back), Some((other, false)));
    }

    #[test]
    fn info_lists_tags_by_kind_people_first() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let path = root.join("a.png");
        image::RgbImage::new(30, 20).save(&path).unwrap();
        let mut catalog = Catalog::open(&Catalog::default_path(&root), &root).unwrap();
        let a = catalog.asset(&path).unwrap();
        ops::add_tag(&mut catalog, a, "Holidays|1998").unwrap();
        ops::add_tag(&mut catalog, a, "People|Alice").unwrap();
        let beach = catalog.tag("Scenes|Beach", TagKind::Scene).unwrap();
        let dog = catalog.tag("Objects|Dog", TagKind::Object).unwrap();
        let found = |tag, confidence| archive_core::catalog::Detection { tag, confidence, region: None };
        catalog.set_detections(a, "clip/tinyclip-8m", &[found(beach, 0.8), found(dog, 0.4)]).unwrap();
        catalog.add_tag(a, beach, None).unwrap();

        let p = ops::properties(&catalog, a).unwrap();
        let tags: Vec<(bool, String, String, String)> =
            info_tags(&p.tags).into_iter().map(|t| (t.heading, t.label.into(), t.key.into(), t.note.into())).collect();
        let row = |heading, label: &str, key: &str, note: &str| (heading, label.to_owned(), key.to_owned(), note.to_owned());
        assert_eq!(
            tags,
            [
                row(true, "People", "", ""),
                row(false, "Alice", "person:People|Alice", ""),
                row(true, "Keywords", "", ""),
                row(false, "Holidays / 1998", "keyword:Holidays|1998", ""),
                // Once, as the user's; the unlikely dog not at all.
                row(true, "Scenes", "", ""),
                row(false, "Scenes / Beach", "scene:Scenes|Beach", ""),
            ]
        );
        let rows = info_rows(&p);
        assert!(rows.contains(&("Dimensions".to_owned(), "30 × 20".to_owned())));
        assert!(rows.contains(&("Kind".to_owned(), "Image · PNG".to_owned())));
        assert!(!rows.iter().any(|(label, _)| label == "Status"));
    }
}
