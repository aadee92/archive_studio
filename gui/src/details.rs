//! Details page: one item enlarged, opened through [`nav::open`] (double-click in Review,
//! People or Acquire). The image is decoded off the UI thread, together with its faces from
//! the catalog; a newer open retires an older decode.
//!
//! Faces are drawn as boxes over the picture (placed by the page in displayed-image
//! coordinates, so they follow the window), named when someone confirmed who they are. A
//! face opened from People is flashed once the picture is up.
//!
//! For an image it also offers the pipeline's say on it: a rotation the orient stage
//! proposed for review (Accept / Keep), and turning it by hand. Those go to the engine,
//! whose answer can take a second (the file is rewritten on its thread), so they are asked
//! from a worker; the turned file comes back as `Rotated` and is reloaded here (faces too:
//! the engine turns their regions with the file).

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::thread;

use slint::{ComponentHandle, ModelRc, SharedString, VecModel};

use crate::nav::{self, Scope, Target};
use crate::pipeline::Handle;
use crate::thumbs::{self, Pixels};
use crate::{AppWindow, DetailFace, DetailsBridge};
use archive_core::catalog::faces::{FaceFilter, FaceId, FaceOrder, FaceStatus};
use archive_core::catalog::runs::RunStatus;
use archive_core::catalog::{Catalog, TagKind};
use archive_core::media::{classify, MediaKind};
use archive_core::pipeline::ORIENT;

/// Longest edge decoded: sharp on a 4K screen, ~50 MB of RGB at most.
const EDGE: u32 = 4096;

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

/// What the engine is asked to do with the shown file.
#[derive(Clone, Copy)]
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
    details.on_context_menu(move || {
        let target = SHOWN.with_borrow(|shown| shown.target.clone());
        if let (Some(ui), Some(target)) = (weak.upgrade(), target) {
            nav::context_menu(&ui, target);
        }
    });
}

/// Shows `target` on the Details page (the caller switches to it: see [`nav::open`]).
/// TODO: zoom, metadata editing.
pub fn show(ui: &AppWindow, target: Target) {
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
    details.set_other_side(other_side(&path).map_or_else(SharedString::default, |p| p.display().to_string().into()));
    SHOWN.with_borrow_mut(|shown| {
        shown.flash = target.face;
        shown.target = Some(target);
        shown.kind = kind;
        shown.info = info.clone();
    });
    details.set_has_item(true);
    details.set_name(name);
    details.set_path(path.display().to_string().into());
    details.set_info(info.into());
    details.set_image(slint::Image::default());
    details.set_image_ready(false);
    details.set_faces(ModelRc::default());
    details.set_flash_face(-1);
    details.set_review_cw(0);
    details.set_message(SharedString::default());
    details.set_working(false);
    load(ui);
}

/// The item shown, to show it again (Back); without its face, which only flashes on arrival.
pub fn snapshot() -> Option<Target> {
    SHOWN.with_borrow(|shown| shown.target.clone()).map(|target| Target { face: None, ..target })
}

/// The view the shown item was opened from, as it was then: the hook for a "similar in your
/// filter" strip.
#[allow(dead_code)] // Not built yet.
pub fn scope() -> Option<Scope> {
    SHOWN.with_borrow(|shown| shown.target.as_ref().map(|t| t.scope.clone()))
}

/// The other side of a two-sided photo: `photo-0001-front.png` ↔ `photo-0001-back.png`, as
/// the Epson V850 backend saves them. Hook for a flip button / side-by-side view.
fn other_side(path: &Path) -> Option<PathBuf> {
    let stem = path.file_stem()?.to_str()?;
    let other = if let Some(base) = stem.strip_suffix("-front") {
        format!("{base}-back")
    } else {
        format!("{}-front", stem.strip_suffix("-back")?)
    };
    let mut other = path.with_file_name(other);
    if let Some(ext) = path.extension() {
        other.set_extension(ext);
    }
    other.exists().then_some(other)
}

/// The file at `path` changed on disk (e.g. the pipeline rotated it): reloads it if it is
/// the one shown, keeping the old picture up until the new one is decoded.
pub fn file_changed(ui: &AppWindow, path: &Path) {
    let kind = SHOWN.with_borrow_mut(|shown| {
        if shown.path() != Some(path) {
            return None;
        }
        let size = std::fs::metadata(path).map_or(0, |m| m.len());
        if let Some((label, _)) = shown.info.split_once(" · ") {
            shown.info = format!("{label} · {}", crate::review::human_size(size));
        }
        shown.kind
    });
    if kind.is_some() {
        load(ui);
    }
}

/// Decodes the shown item and reads its review state and faces on a worker; a newer load
/// wins.
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

    let root = CONTEXT.with_borrow(|c| c.as_ref().map(|c| c.root.clone()));
    let weak = ui.as_weak();
    thread::spawn(move || {
        let catalog = root.filter(|_| kind == MediaKind::Image).and_then(|root| Catalog::open(&Catalog::default_path(&root), &root).ok());
        let review = catalog.as_ref().and_then(|c| orient_review(c, &path));
        let faces = catalog.as_ref().map(|c| read_faces(c, &path)).unwrap_or_default();
        drop(catalog);
        let pixels = thumbs::decode(&path, kind, EDGE).map(|rgb| Pixels::clone_from_slice(rgb.as_raw(), rgb.width(), rgb.height()));
        let _ = weak.upgrade_in_event_loop(move |ui| {
            let Some((info, flash)) = SHOWN.with_borrow_mut(|shown| {
                (shown.generation == generation).then(|| (shown.info.clone(), if pixels.is_some() { shown.flash.take() } else { None }))
            }) else {
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
                    details.set_image(slint::Image::from_rgb8(pixels));
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
            details.set_faces(ModelRc::new(VecModel::from(faces.iter().map(|f| f.face.clone()).collect::<Vec<_>>())));
            if let Some(i) = flash_at {
                details.set_flash_face(i as i32);
                details.set_flash_serial(details.get_flash_serial() + 1);
            }
        });
    });
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
    let Ok(faces) = catalog.faces(FaceFilter::Asset(asset), FaceOrder::Asset) else { return Vec::new() };
    let names: HashMap<_, _> = catalog.leaf_tags(TagKind::Person).unwrap_or_default().into_iter().collect();
    faces
        .into_iter()
        .map(|face| {
            let name = match face.status {
                FaceStatus::Confirmed(tag) => names.get(&tag).map(|path| display_name(path).into()).unwrap_or_default(),
                _ => SharedString::default(),
            };
            let r = face.region;
            FaceInfo { id: face.id, face: DetailFace { x: r.x, y: r.y, w: r.w, h: r.h, name }, ignored: face.status == FaceStatus::Ignored }
        })
        .collect()
}

/// The last level of a tag path: "People|Alice" -> "Alice".
fn display_name(path: &str) -> &str {
    path.rsplit('|').next().unwrap_or(path).trim()
}

/// Asks the engine to `action` the shown file, on a worker (the engine replies once the
/// file is rewritten). A failure is shown under the picture.
fn ask(weak: &slint::Weak<AppWindow>, action: Action) {
    let Some(ui) = weak.upgrade() else { return };
    let Some(path) = SHOWN.with_borrow(|shown| shown.path().map(Path::to_path_buf)) else { return };
    let Some((pipeline, engine)) = CONTEXT.with_borrow(|c| c.as_ref().and_then(|c| Some((c.pipeline.clone(), c.pipeline.engine()?)))) else {
        ui.global::<DetailsBridge>().set_message("The pipeline isn't running in this window".into());
        return;
    };
    let details = ui.global::<DetailsBridge>();
    details.set_working(true);
    details.set_message(SharedString::default());
    let weak = weak.clone();
    thread::spawn(move || {
        let result = match action {
            Action::Accept => engine.accept(&path),
            Action::Keep => engine.keep(&path),
            Action::Rotate(cw) => engine.rotate(&path, cw),
        };
        drop(engine);
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
                // The picture itself is reloaded when the engine's `Rotated` arrives.
                Ok(()) => details.set_review_cw(0),
                Err(e) => {
                    eprintln!("{}: {e}", path.display());
                    details.set_message(e.into());
                }
            }
        });
    });
}
