//! Details page: one item enlarged, opened by double-clicking a tile in Review or a face
//! in People. The image is decoded off the UI thread; a newer open retires an older decode.
//!
//! For an image it also offers the pipeline's say on it: a rotation the orient stage
//! proposed for review (Accept / Keep), and turning it by hand. Those go to the engine,
//! whose answer can take a second (the file is rewritten on its thread), so they are asked
//! from a worker; the turned file comes back as `Rotated` and is reloaded here.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::thread;

use slint::{ComponentHandle, SharedString};

use crate::pipeline::Handle;
use crate::thumbs::{self, Pixels};
use crate::{AppWindow, DetailsBridge};
use archive_core::catalog::runs::RunStatus;
use archive_core::catalog::Catalog;
use archive_core::media::{classify, MediaKind};
use archive_core::pipeline::ORIENT;

/// Longest edge decoded: sharp on a 4K screen, ~50 MB of RGB at most.
const EDGE: u32 = 4096;
/// Sidebar index of the Details page.
const PAGE: i32 = 3;

/// The item shown.
#[derive(Default)]
struct Shown {
    path: Option<PathBuf>,
    kind: Option<MediaKind>,
    /// Kind and size, before the dimensions are known.
    info: String,
    /// Retires slower loads of earlier items (or of this one before it changed).
    generation: u64,
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

pub fn install(ui: &AppWindow, root: PathBuf, pipeline: Handle) {
    CONTEXT.set(Some(Context { root, pipeline }));
    let details = ui.global::<DetailsBridge>();
    details.on_open_external(|| {
        SHOWN.with_borrow(|shown| {
            if let Some(path) = &shown.path {
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
}

/// Shows `path` on the Details page and switches to it.
/// TODO: zoom, side-by-side compare, metadata editing.
pub fn show(ui: &AppWindow, path: PathBuf, name: SharedString, kind: MediaKind, size: u64) {
    let label = match kind {
        MediaKind::Image => "Image",
        MediaKind::Video => "Video",
        MediaKind::Audio => "Audio",
    };
    let info = format!("{label} · {}", crate::review::human_size(size));
    SHOWN.with_borrow_mut(|shown| {
        shown.path = Some(path.clone());
        shown.kind = Some(kind);
        shown.info = info.clone();
    });
    let details = ui.global::<DetailsBridge>();
    details.set_has_item(true);
    details.set_name(name);
    details.set_path(path.display().to_string().into());
    details.set_info(info.into());
    details.set_image(slint::Image::default());
    details.set_image_ready(false);
    details.set_review_cw(0);
    details.set_message(SharedString::default());
    details.set_working(false);
    load(ui);
    ui.set_page(PAGE);
}

/// [`show`] for a file known only by its path.
pub fn show_path(ui: &AppWindow, path: PathBuf) {
    let Some(kind) = classify(&path) else { return };
    let name = path.file_name().map_or_else(Default::default, |n| n.to_string_lossy().as_ref().into());
    let size = std::fs::metadata(&path).map_or(0, |m| m.len());
    show(ui, path, name, kind, size);
}

/// The file at `path` changed on disk (e.g. the pipeline rotated it): reloads it if it is
/// the one shown, keeping the old picture up until the new one is decoded.
pub fn file_changed(ui: &AppWindow, path: &Path) {
    let kind = SHOWN.with_borrow_mut(|shown| {
        if shown.path.as_deref() != Some(path) {
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

/// Decodes the shown item and reads its review state on a worker; a newer load wins.
fn load(ui: &AppWindow) {
    let Some((path, kind, generation)) = SHOWN.with_borrow_mut(|shown| {
        shown.generation += 1;
        Some((shown.path.clone()?, shown.kind?, shown.generation))
    }) else {
        return;
    };
    let details = ui.global::<DetailsBridge>();
    details.set_loading(true);
    details.set_is_image(kind == MediaKind::Image);

    let root = CONTEXT.with_borrow(|c| c.as_ref().map(|c| c.root.clone()));
    let weak = ui.as_weak();
    thread::spawn(move || {
        let review = if kind == MediaKind::Image { root.and_then(|root| orient_review(&root, &path)) } else { None };
        let pixels = thumbs::decode(&path, kind, EDGE).map(|rgb| Pixels::clone_from_slice(rgb.as_raw(), rgb.width(), rgb.height()));
        let _ = weak.upgrade_in_event_loop(move |ui| {
            let Some(info) = SHOWN.with_borrow(|shown| (shown.generation == generation).then(|| shown.info.clone())) else {
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
        });
    });
}

/// The rotation orient left for the user to decide on `path`, if any.
fn orient_review(root: &Path, path: &Path) -> Option<(u16, String)> {
    let catalog = Catalog::open(&Catalog::default_path(root), root).ok()?;
    let asset = catalog.find_asset(path).ok()??;
    match catalog.run_of(asset, ORIENT).ok()?? {
        (RunStatus::Review, Some(cw), detail) if cw % 360 != 0 => Some(((cw % 360) as u16, detail)),
        _ => None,
    }
}

/// Asks the engine to `action` the shown file, on a worker (the engine replies once the
/// file is rewritten). A failure is shown under the picture.
fn ask(weak: &slint::Weak<AppWindow>, action: Action) {
    let Some(ui) = weak.upgrade() else { return };
    let Some(path) = SHOWN.with_borrow(|shown| shown.path.clone()) else { return };
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
            if SHOWN.with_borrow(|shown| shown.path.as_deref() != Some(path.as_path())) {
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
