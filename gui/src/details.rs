//! Details page: one item enlarged, opened through [`nav::open`] (double-click in Review,
//! People or Acquire). The image is decoded off the UI thread, together with its faces from
//! the catalog; a newer open retires an older decode.
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
//! (its [`Scope`]): `Catalog::similar` over their assets, on a worker. Opening one keeps
//! that scope, so the strip stays "similar in your filter".
//!
//! Left / Right (Home / End) step through that scope in its order, and a filmstrip above
//! the picture shows the items around the shown one (small thumbnails from the cache, on a
//! worker); a click shows another. Stepping replaces the item in place, without history, so
//! Back returns to the view it was opened from (whose selection follows: see `nav::back`).
//! The pictures of the next items are decoded ahead on a prefetch thread, so a step shows
//! the new one at once (else its thumbnail until it is decoded).
//!
//! For an image it also offers the pipeline's say on it: a rotation the orient stage
//! proposed for review (Accept / Keep), and turning it by hand ([`actions::rotate`]: the
//! engine if it runs here, else the core directly). That can take a second (the file is
//! rewritten), so it runs on a worker; the turned file comes back through the pipeline
//! relay and is reloaded here (faces too: their regions are turned with the file).

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Condvar, Mutex};
use std::thread;
use std::time::SystemTime;

use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};

use crate::actions;
use crate::nav::{self, Scope, Target};
use crate::pipeline::Handle;
use crate::thumbs::{self, Pixels};
use crate::{AppWindow, DetailFace, DetailsBridge, FilmItem, SimilarItem};
use archive_core::catalog::faces::{FaceId, FaceStatus};
use archive_core::catalog::runs::RunStatus;
use archive_core::catalog::Catalog;
use archive_core::media::{classify, MediaKind};
use archive_core::pipeline::ORIENT;

/// Longest edge decoded: sharp on a 4K screen, ~50 MB of RGB at most.
const EDGE: u32 = 4096;
/// The other side, shown at most half as wide.
const OTHER_EDGE: u32 = 2048;
/// Items in the similar strip.
const SIMILAR: usize = 12;
/// Items in the filmstrip on either side of the shown one (more than fit a 4K screen).
const FILM: usize = 24;

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
    /// The items in the similar strip.
    similar: Vec<PathBuf>,
    similar_generation: u64,
    /// The picture shown is the decoded one already (prefetched), not a placeholder.
    decoded: bool,
    /// The last step went forward: prefetch ahead in that direction.
    forward: bool,
    /// The filmstrip: the items in it, and their thumbnails as far as loaded.
    film: Rc<VecModel<FilmItem>>,
    film_paths: Vec<PathBuf>,
    film_thumbs: HashMap<PathBuf, slint::Image>,
}

impl Shown {
    fn path(&self) -> Option<&Path> {
        self.target.as_ref().map(|t| t.path.as_path())
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
/// doesn't decode.
static LOADING: AtomicU64 = AtomicU64::new(0);
/// Retires filmstrip thumbnail workers of earlier items.
static FILM_GENERATION: AtomicU64 = AtomicU64::new(0);

/// What the engine is asked to do with the shown file.
#[derive(Clone, Copy, Debug)]
enum Action {
    Accept,
    Keep,
    Rotate(u16),
}

/// A face as read for the page.
struct FaceInfo {
    id: FaceId,
    face: DetailFace,
    ignored: bool,
}

pub fn install(ui: &AppWindow, root: PathBuf, pipeline: Handle) {
    CONTEXT.set(Some(Context { root, pipeline }));
    let details = ui.global::<DetailsBridge>();
    details.on_open_external(|| {
        SHOWN.with_borrow(|shown| {
            if let Some(path) = shown.path() {
                let _ = std::process::Command::new("xdg-open").arg(path).spawn();
            }
        })
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
        let target = SHOWN.with_borrow(|shown| {
            let target = shown.target.clone()?;
            match side {
                0 => Some(Target { face: None, ..target }),
                _ => Some(Target::new(shown.other.clone()?, target.scope)),
            }
        });
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
    details.on_open_similar(move |i| {
        log::debug!("details: double-click on similar #{i}");
        if let (Some(ui), Some(target)) = (weak.upgrade(), similar_target(i)) {
            nav::open(&ui, target);
        }
    });
    let weak = ui.as_weak();
    details.on_similar_menu(move |i| {
        if let (Some(ui), Some(target)) = (weak.upgrade(), similar_target(i)) {
            nav::context_menu(&ui, target);
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

/// Shows `target` on the Details page (the caller switches to it: see [`nav::open`]).
/// TODO: zoom, metadata editing.
pub fn show(ui: &AppWindow, mut target: Target) {
    locate(&mut target);
    log::debug!("details: show {target:?}");
    let path = target.path.clone();
    let kind = classify(&path);
    let name: SharedString = path.file_name().map_or_else(Default::default, |n| n.to_string_lossy().as_ref().into());
    let size = std::fs::metadata(&path).map_or(0, |m| m.len());
    let label = match kind {
        Some(MediaKind::Image) => "Image",
        Some(MediaKind::Video) => "Video",
        Some(MediaKind::Audio) => "Audio",
        None => "File",
    };
    let info = format!("{label} · {}", crate::review::human_size(size));
    let details = ui.global::<DetailsBridge>();
    details.set_scope_label(target.scope.label.as_str().into());
    details.set_scope_count(target.scope.len as i32);
    details.set_scope_index(target.scope.current.map_or(-1, |i| i as i32));
    let neighbours = neighbours(&target);
    SHOWN.with_borrow_mut(|shown| {
        shown.flash = target.face;
        shown.target = Some(target);
        shown.kind = kind;
        shown.info = info.clone();
        shown.other = None;
        shown.other_generation += 1;
        shown.similar.clear();
        shown.decoded = false;
    });
    details.set_has_item(true);
    details.set_name(name);
    details.set_path(path.display().to_string().into());
    details.set_info(info.as_str().into());
    details.set_image(slint::Image::default());
    details.set_image_ready(false);
    details.set_faces(ModelRc::default());
    details.set_flash_face(-1);
    details.set_review_cw(0);
    details.set_message(SharedString::default());
    details.set_working(false);
    details.set_other_side(SharedString::default());
    details.set_is_back(false);
    details.set_other_image(slint::Image::default());
    details.set_other_ready(false);
    details.set_other_faces(ModelRc::default());
    details.set_similar(ModelRc::default());
    // Up at once if prefetched, else its thumbnail until decoded.
    if kind.is_some() {
        if let Some(pixels) = prefetched(&path) {
            details.set_info(format!("{info} · {} × {}", pixels.width(), pixels.height()).into());
            details.set_image(slint::Image::from_rgb8(pixels));
            details.set_image_ready(true);
            SHOWN.with_borrow_mut(|shown| shown.decoded = true);
        } else if let Some(thumb) = SHOWN.with_borrow(|shown| shown.film_thumbs.get(&path).cloned()).filter(|t| t.size().width > 0) {
            details.set_image(thumb);
            details.set_image_ready(true);
        }
    }
    prefetch(&path, neighbours);
    load(ui);
    load_similar(ui);
    load_film(ui);
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
/// either end.
fn step(ui: &AppWindow, delta: i32) {
    let Some(scope) = SHOWN.with_borrow(|shown| shown.target.as_ref().map(|t| t.scope.clone())) else { return };
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
        return;
    }
    go_to(ui, i as usize);
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
    let order: [i64; 3] = if forward { [1, 2, -1] } else { [-1, -2, 1] };
    order.into_iter().filter_map(at).filter(|p| classify(p) == Some(MediaKind::Image)).cloned().collect()
}

/// Fills the filmstrip with the items around the shown one, and loads the thumbnails it
/// doesn't have yet on a worker, nearest first.
fn load_film(ui: &AppWindow) {
    let Some((scope, model)) = SHOWN.with_borrow(|shown| Some((shown.target.as_ref()?.scope.clone(), shown.film.clone()))) else { return };
    let details = ui.global::<DetailsBridge>();
    if scope.len < 2 {
        SHOWN.with_borrow_mut(|shown| {
            shown.film_paths.clear();
            shown.film_thumbs.clear();
        });
        model.set_vec(Vec::new());
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
    let generation = FILM_GENERATION.fetch_add(1, Ordering::Relaxed) + 1;
    if missing.is_empty() {
        return;
    }
    log::trace!("details: filmstrip #{generation}: {} thumbnails to load around #{center}", missing.len());
    let weak = ui.as_weak();
    thread::spawn(move || {
        thumbs::small_for_files(&missing, |i, small| {
            if FILM_GENERATION.load(Ordering::Relaxed) != generation {
                return false;
            }
            let path = missing[i].clone();
            let pixels = small.map(|rgb| Pixels::clone_from_slice(rgb.as_raw(), rgb.width(), rgb.height()));
            let _ = weak.upgrade_in_event_loop(move |_| film_thumb(path, pixels));
            true
        });
    });
}

/// A filmstrip thumbnail came in (none: the file has none; not asked for again).
fn film_thumb(path: PathBuf, pixels: Option<Pixels>) {
    SHOWN.with_borrow_mut(|shown| {
        let Some(i) = shown.film_paths.iter().position(|p| *p == path) else { return };
        let thumb = pixels.map(slint::Image::from_rgb8).unwrap_or_default();
        if let Some(mut item) = shown.film.row_data(i) {
            item.thumb = thumb.clone();
            shown.film.set_row_data(i, item);
        }
        shown.film_thumbs.insert(path, thumb);
    });
}

/// Shows nothing (the item shown is gone and there is nowhere to go back to).
fn clear(ui: &AppWindow) {
    log::debug!("details: cleared (item gone, nowhere to go back to)");
    SHOWN.with_borrow_mut(|shown| {
        shown.target = None;
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
    details.set_other_side(SharedString::default());
    details.set_other_ready(false);
    details.set_similar(ModelRc::default());
}

/// The item shown, to show it again (Back); without its face, which only flashes on arrival.
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
    let paired = catalog.and_then(|c| {
        let asset = c.find_asset(path).ok()??;
        if let Some(back) = c.back_of(asset).ok()? {
            return Some((c.asset_path(back).ok()?, false));
        }
        let front = c.front_of(asset).ok()??;
        Some((c.asset_path(front).ok()?, true))
    });
    paired.filter(|(p, _)| p.exists()).or_else(|| other_side(path))
}

/// Shows the other side of the photo in place of this one (no history: Back still returns
/// to where the photo was opened from).
fn flip(ui: &AppWindow) {
    let Some(target) = SHOWN.with_borrow(|shown| Some(Target::new(shown.other.clone()?, shown.target.clone()?.scope))) else {
        return;
    };
    show(ui, target);
}

/// The file at `path` changed on disk (e.g. it was rotated): reloads it if it is the one
/// shown (keeping the old picture up until the new one is decoded), the other side, or in
/// the similar strip.
pub fn file_changed(ui: &AppWindow, path: &Path) {
    let (this, other, similar) = SHOWN.with_borrow_mut(|shown| {
        let this = shown.path() == Some(path) && shown.kind.is_some();
        if this {
            let size = std::fs::metadata(path).map_or(0, |m| m.len());
            if let Some((label, _)) = shown.info.split_once(" · ") {
                shown.info = format!("{label} · {}", crate::review::human_size(size));
            }
        }
        (this, shown.other.as_deref() == Some(path), shown.similar.iter().any(|p| p == path))
    });
    if this || other || similar {
        log::debug!("details: {} changed (shown {this}, other side {other}, similar {similar})", path.display());
    }
    forget(path);
    if this {
        SHOWN.with_borrow_mut(|shown| shown.decoded = false);
        load(ui);
    } else if other && ui.global::<DetailsBridge>().get_side_by_side() {
        load_other(ui);
    }
    if similar {
        load_similar(ui);
    }
}

/// Files that left the library (e.g. to the trash): if it is the item shown, the page goes
/// back to where it was opened from (or shows nothing); the other side and the similar
/// strip drop it.
pub fn files_gone(ui: &AppWindow, gone: &[PathBuf]) {
    if gone.is_empty() {
        return;
    }
    let is_gone = |p: &Path| gone.iter().any(|g| g == p);
    let (this, other, similar) = SHOWN.with_borrow(|shown| {
        (
            shown.path().is_some_and(is_gone),
            shown.other.as_deref().is_some_and(is_gone),
            shown.similar.iter().any(|p| is_gone(p)),
        )
    });
    if this {
        log::debug!("details: the item shown left the library (page {})", ui.get_page());
        if ui.get_page() == nav::DETAILS && nav::can_go_back(ui) {
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
        let keep: Vec<bool> = SHOWN.with_borrow_mut(|shown| {
            let keep = shown.similar.iter().map(|p| !is_gone(p)).collect();
            shown.similar.retain(|p| !is_gone(p));
            keep
        });
        let items: Vec<SimilarItem> = details.get_similar().iter().zip(keep).filter(|(_, k)| *k).map(|(item, _)| item).collect();
        details.set_similar(ModelRc::new(VecModel::from(items)));
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
    let details = ui.global::<DetailsBridge>();
    details.set_loading(true);
    details.set_is_image(kind == MediaKind::Image);

    let root = root();
    let weak = ui.as_weak();
    log::debug!("details: load #{generation} {}", path.display());
    LOADING.store(generation, Ordering::Relaxed);
    thread::spawn(move || {
        let started = std::time::Instant::now();
        let catalog = root.filter(|_| kind == MediaKind::Image).and_then(|root| open_catalog(&root));
        let review = catalog.as_ref().and_then(|c| orient_review(c, &path));
        let faces = catalog.as_ref().map(|c| read_faces(c, &path)).unwrap_or_default();
        let other = if kind == MediaKind::Image { read_other_side(catalog.as_ref(), &path) } else { None };
        drop(catalog);
        if LOADING.load(Ordering::Relaxed) != generation {
            log::debug!("details: load #{generation} superseded before decoding - dropped");
            return;
        }
        let pixels = decode(&path, kind);
        log::debug!(
            "details: load #{generation} read in {:?}: decoded {}, {} faces, review {:?}, other side {:?}",
            started.elapsed(),
            pixels.as_ref().map_or_else(|| "nothing".to_owned(), |p| format!("{}x{}", p.width(), p.height())),
            faces.len(),
            review,
            other
        );
        let _ = weak.upgrade_in_event_loop(move |ui| {
            let Some((info, flash, decoded)) = SHOWN.with_borrow_mut(|shown| {
                (shown.generation == generation).then(|| {
                    let flash = if pixels.is_some() { shown.flash.take() } else { None };
                    (shown.info.clone(), flash, std::mem::replace(&mut shown.decoded, pixels.is_some()))
                })
            }) else {
                log::debug!("details: load #{generation} superseded - dropped");
                return;
            };
            let details = ui.global::<DetailsBridge>();
            details.set_loading(false);
            let (cw, detail) = review.unwrap_or_default();
            details.set_review_cw(cw as i32);
            details.set_review_detail(detail.into());
            match pixels {
                Some(pixels) => {
                    details.set_info(format!("{info} · {} × {}", pixels.width(), pixels.height()).into());
                    // Already up if it was prefetched.
                    if !decoded {
                        details.set_image(slint::Image::from_rgb8(pixels));
                    }
                    details.set_image_ready(true);
                }
                None => {
                    details.set_info(info.into());
                    details.set_image_ready(false);
                }
            }
            // Ignored faces (not a face, a stranger) aren't drawn, unless that's the one asked for.
            let faces: Vec<&FaceInfo> = faces.iter().filter(|f| !f.ignored || Some(f.id) == flash).collect();
            let flash_at = flash.and_then(|id| faces.iter().position(|f| f.id == id));
            if let Some(id) = flash {
                log::debug!("details: flash face {} -> box {flash_at:?}", id.0);
            }
            details.set_faces(ModelRc::new(VecModel::from(faces.iter().map(|f| f.face.clone()).collect::<Vec<_>>())));
            if let Some(i) = flash_at {
                details.set_flash_face(i as i32);
                details.set_flash_serial(details.get_flash_serial() + 1);
            }

            SHOWN.with_borrow_mut(|shown| shown.other = other.as_ref().map(|(p, _)| p.clone()));
            details.set_other_side(other.as_ref().map_or_else(SharedString::default, |(p, _)| p.display().to_string().into()));
            details.set_is_back(other.as_ref().is_some_and(|(_, is_back)| *is_back));
            if other.is_none() {
                details.set_other_ready(false);
            } else if details.get_side_by_side() {
                // Reloaded with this one too (after a rotation, say).
                load_other(&ui);
            }
        });
    });
}

/// When a file was decoded: a prefetched picture of an older version isn't used.
type Stamp = Option<(u64, SystemTime)>;

fn stamp(path: &Path) -> Stamp {
    let meta = std::fs::metadata(path).ok()?;
    Some((meta.len(), meta.modified().ok()?))
}

/// Pictures decoded ahead for the items next to the shown one ([`prefetch`]), shared by the
/// prefetch thread and the loads (which wait for a prefetch in flight rather than decode
/// the same file twice).
struct Prefetch {
    /// To decode, likeliest next first.
    wanted: Vec<PathBuf>,
    /// What may be kept decoded: the shown item and `wanted`.
    keep: Vec<PathBuf>,
    pending: Vec<PathBuf>,
    ready: Vec<(PathBuf, Stamp, Pixels)>,
    started: bool,
}

static PREFETCH: Mutex<Prefetch> = Mutex::new(Prefetch { wanted: Vec::new(), keep: Vec::new(), pending: Vec::new(), ready: Vec::new(), started: false });
/// Signalled whenever `PREFETCH` changes.
static PREFETCH_CHANGED: Condvar = Condvar::new();

impl Prefetch {
    fn ready(&self, path: &Path, stamp: Stamp) -> Option<Pixels> {
        let (_, _, pixels) = self.ready.iter().find(|(p, s, _)| p == path && stamp.is_some() && *s == stamp)?;
        Some(pixels.clone())
    }

    /// Keeps `pixels` for `path` if it is still wanted.
    fn put(&mut self, path: &Path, stamp: Stamp, pixels: &Pixels) {
        if self.keep.iter().any(|p| p == path) && self.ready(path, stamp).is_none() {
            self.ready.retain(|(p, _, _)| p != path);
            self.ready.push((path.to_path_buf(), stamp, pixels.clone()));
        }
    }
}

fn decode_now(path: &Path, kind: MediaKind) -> Option<Pixels> {
    thumbs::decode(path, kind, EDGE).map(|rgb| Pixels::clone_from_slice(rgb.as_raw(), rgb.width(), rgb.height()))
}

/// The picture of `path` for the page: prefetched, or decoded now (on a worker).
fn decode(path: &Path, kind: MediaKind) -> Option<Pixels> {
    let stamp = stamp(path);
    let mut prefetch = PREFETCH.lock().unwrap();
    loop {
        if let Some(pixels) = prefetch.ready(path, stamp) {
            log::trace!("details: {} was prefetched", path.display());
            return Some(pixels);
        }
        if !prefetch.pending.iter().any(|p| p == path) {
            break;
        }
        prefetch = PREFETCH_CHANGED.wait(prefetch).unwrap();
    }
    drop(prefetch);
    let pixels = decode_now(path, kind)?;
    // Kept for stepping back to it.
    PREFETCH.lock().unwrap().put(path, stamp, &pixels);
    Some(pixels)
}

/// The picture of `path`, if prefetched (on the UI thread: doesn't wait).
fn prefetched(path: &Path) -> Option<Pixels> {
    PREFETCH.lock().unwrap().ready(path, stamp(path))
}

/// Decodes `next` ahead on the prefetch thread (started on first use), dropping what was
/// decoded for items other than `shown` and those.
fn prefetch(shown: &Path, next: Vec<PathBuf>) {
    let mut prefetch = PREFETCH.lock().unwrap();
    prefetch.keep = next.iter().cloned().chain([shown.to_path_buf()]).collect();
    let keep = std::mem::take(&mut prefetch.keep);
    prefetch.ready.retain(|(p, _, _)| keep.contains(p));
    prefetch.keep = keep;
    prefetch.wanted = next;
    if !prefetch.started {
        prefetch.started = true;
        let _ = thread::Builder::new().name("details-prefetch".into()).spawn(prefetch_thread);
    }
    PREFETCH_CHANGED.notify_all();
}

/// Drops a prefetched picture of `path` (the file changed).
fn forget(path: &Path) {
    PREFETCH.lock().unwrap().ready.retain(|(p, _, _)| p != path);
}

fn prefetch_thread() {
    loop {
        let path = {
            let mut prefetch = PREFETCH.lock().unwrap();
            loop {
                let p = &*prefetch;
                let next = p.wanted.iter().find(|w| !p.pending.contains(w) && !p.ready.iter().any(|(r, _, _)| r == *w)).cloned();
                if let Some(next) = next {
                    prefetch.pending.push(next.clone());
                    break next;
                }
                prefetch = PREFETCH_CHANGED.wait(prefetch).unwrap();
            }
        };
        let started = std::time::Instant::now();
        let stamp = stamp(&path);
        let pixels = decode_now(&path, MediaKind::Image);
        log::trace!("details: prefetched {} in {:?} ({})", path.display(), started.elapsed(), if pixels.is_some() { "ok" } else { "failed" });
        let mut prefetch = PREFETCH.lock().unwrap();
        prefetch.pending.retain(|p| *p != path);
        // Not tried again if it failed.
        prefetch.wanted.retain(|p| *p != path);
        if let Some(pixels) = &pixels {
            prefetch.put(&path, stamp, pixels);
        }
        drop(prefetch);
        PREFETCH_CHANGED.notify_all();
    }
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
            thumbs::decode(&path, MediaKind::Image, OTHER_EDGE).map(|rgb| Pixels::clone_from_slice(rgb.as_raw(), rgb.width(), rgb.height()));
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

/// Finds the items of the shown one's scope most like it, on a worker, and fills the strip.
fn load_similar(ui: &AppWindow) {
    let Some((path, scope, generation)) = SHOWN.with_borrow_mut(|shown| {
        shown.similar_generation += 1;
        let target = shown.target.as_ref()?;
        (shown.kind == Some(MediaKind::Image)).then(|| (target.path.clone(), target.scope.clone(), shown.similar_generation))
    }) else {
        return;
    };
    let Some(root) = root() else { return };
    // Listed here (once per scope): a scope may only be read on the UI thread.
    let candidates = scope.paths();
    let weak = ui.as_weak();
    thread::spawn(move || {
        let found = open_catalog(&root).map(|c| similar(&c, &path, &candidates)).unwrap_or_default();
        log::debug!("details: {} similar among {} in {:?}", found.len(), candidates.len(), scope.label);
        let items: Vec<(PathBuf, f32, Option<Pixels>)> = found
            .into_iter()
            .map(|(path, score)| {
                let thumb = thumbs::small_for_file(&path).map(|rgb| Pixels::clone_from_slice(rgb.as_raw(), rgb.width(), rgb.height()));
                (path, score, thumb)
            })
            .collect();
        let _ = weak.upgrade_in_event_loop(move |ui| {
            let paths = SHOWN.with_borrow_mut(|shown| {
                (shown.similar_generation == generation).then(|| {
                    shown.similar = items.iter().map(|(p, _, _)| p.clone()).collect();
                })
            });
            if paths.is_none() {
                return;
            }
            let model: Vec<SimilarItem> = items
                .into_iter()
                .map(|(path, score, thumb)| SimilarItem {
                    thumb: thumb.map(slint::Image::from_rgb8).unwrap_or_default(),
                    name: path.file_name().map_or_else(Default::default, |n| n.to_string_lossy().as_ref().into()),
                    score: (score * 100.0).round() as i32,
                })
                .collect();
            ui.global::<DetailsBridge>().set_similar(ModelRc::new(VecModel::from(model)));
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

/// Similar item `i`, to open with the shown item's scope.
fn similar_target(i: i32) -> Option<Target> {
    SHOWN.with_borrow(|shown| {
        let path = shown.similar.get(usize::try_from(i).ok()?)?.clone();
        Some(Target::new(path, shown.target.as_ref()?.scope.clone()))
    })
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
            let name = face.person.map(|(_, name)| SharedString::from(name)).unwrap_or_default();
            let r = face.region;
            FaceInfo { id: face.face, face: DetailFace { x: r.x, y: r.y, w: r.w, h: r.h, name }, ignored: face.status == FaceStatus::Ignored }
        })
        .collect()
}

/// Asks for `action` on the shown file, on a worker (the engine replies once the file is
/// rewritten). A failure is shown under the picture.
fn ask(weak: &slint::Weak<AppWindow>, action: Action) {
    let Some(ui) = weak.upgrade() else { return };
    let Some(path) = SHOWN.with_borrow(|shown| shown.path().map(Path::to_path_buf)) else { return };
    let Some((root, pipeline)) = CONTEXT.with_borrow(|c| c.as_ref().map(|c| (c.root.clone(), c.pipeline.clone()))) else { return };
    let engine = pipeline.engine();
    log::debug!("details: {action:?} on {} (engine here: {})", path.display(), engine.is_some());
    if engine.is_none() && !matches!(action, Action::Rotate(_)) {
        ui.global::<DetailsBridge>().set_message("The pipeline isn't running in this window".into());
        return;
    }
    let details = ui.global::<DetailsBridge>();
    details.set_working(true);
    details.set_message(SharedString::default());
    let weak = weak.clone();
    thread::spawn(move || {
        let result = match (action, engine) {
            (Action::Rotate(cw), _) => actions::rotate(&root, &pipeline, &path, cw),
            (Action::Accept, Some(engine)) => engine.accept(&path),
            (Action::Keep, Some(engine)) => engine.keep(&path),
            (_, None) => Err("The pipeline isn't running in this window".to_owned()),
        };
        match &result {
            Ok(()) => log::info!("details: {action:?} {} done", path.display()),
            Err(e) => log::warn!("details: {action:?} {}: {e}", path.display()),
        }
        if result.is_ok() {
            // "N rotations to review" (a rotation is reported, a kept file isn't).
            pipeline.recount();
        }
        let _ = weak.upgrade_in_event_loop(move |ui| {
            if SHOWN.with_borrow(|shown| shown.path() != Some(path.as_path())) {
                return;
            }
            let details = ui.global::<DetailsBridge>();
            details.set_working(false);
            match result {
                // The picture itself is reloaded when the rotated file is reported.
                Ok(()) => details.set_review_cw(0),
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
    fn prefetched_pictures_are_kept_while_wanted_and_current() {
        let mut prefetch = Prefetch { wanted: Vec::new(), keep: vec!["a".into()], pending: Vec::new(), ready: Vec::new(), started: false };
        let pixels = Pixels::new(2, 2);
        let stamp = Some((1, SystemTime::UNIX_EPOCH));
        prefetch.put(Path::new("b"), stamp, &pixels);
        assert!(prefetch.ready.is_empty(), "not wanted");
        prefetch.put(Path::new("a"), stamp, &pixels);
        assert!(prefetch.ready(Path::new("a"), stamp).is_some());
        let changed = Some((2, SystemTime::UNIX_EPOCH));
        assert!(prefetch.ready(Path::new("a"), changed).is_none(), "file changed since");
        assert!(prefetch.ready(Path::new("a"), None).is_none(), "file gone");
        prefetch.put(Path::new("a"), changed, &pixels);
        assert_eq!(prefetch.ready.len(), 1, "replaced");
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
}
