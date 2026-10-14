//! People mode: the person tags with faces, "Unknown" first, and the face crops of the one
//! selected.
//!
//! A worker thread reads the people and the selected person's faces from the catalog (the
//! unknown faces are put in similarity order, which is quadratic). Crops are small JPEGs in
//! the catalog, decoded on the UI thread only for the rows on screen and cached.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::thread;

use slint::{ComponentHandle, Image, Model, ModelNotify, ModelRc, ModelTracker, SharedString, VecModel};

use crate::thumbs::Pixels;
use crate::{details, AppWindow, MediaKind, MediaRow, MediaTile, PeopleBridge, PersonItem};
use archive_core::catalog::faces::{FaceFilter, FaceId, FaceOrder};
use archive_core::catalog::{Catalog, TagId};

/// `PersonItem.key` of the faces nobody has named.
const UNKNOWN: i32 = -1;
/// Decoded crops kept; ~75 KB each at the stored 160 px.
const CROP_CACHE: usize = 3000;

struct FaceRef {
    id: FaceId,
    path: PathBuf,
    detail: SharedString,
}

/// What a worker read for one selection.
struct Loaded {
    people: Vec<PersonItem>,
    selected: i32,
    title: String,
    faces: Vec<FaceRef>,
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
        root,
        catalog,
        faces: RefCell::default(),
        columns: Cell::new(1),
        crops: RefCell::default(),
        generation: Cell::new(0),
        notify: ModelNotify::default(),
    });
    GRID.set(Some(grid.clone()));

    let bridge = ui.global::<PeopleBridge>();
    bridge.set_rows(ModelRc::from(grid.clone()));
    bridge.on_refresh(|| with_grid(|g| g.load(None)));
    bridge.on_select(|key| with_grid(|g| g.load(Some(key))));
    bridge.on_columns_changed(|n| {
        with_grid(|g| {
            g.columns.set(n.max(1) as usize);
            g.notify.reset();
        })
    });
    bridge.on_open(|id| {
        with_grid(|g| {
            let path = g.faces.borrow().iter().find(|f| f.id.0 == id as i64).map(|f| f.path.clone());
            if let (Some(ui), Some(path)) = (g.ui.upgrade(), path) {
                details::show_path(&ui, path);
            }
        })
    });
}

/// Sidebar index of the People page.
const PAGE: i32 = 2;

/// Re-reads the catalog if the page is on screen (e.g. the pipeline just grouped faces);
/// otherwise it is re-read when next shown anyway.
pub fn refresh_if_shown(ui: &AppWindow) {
    if ui.get_page() == PAGE {
        with_grid(|g| g.load(None));
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
        let n = loaded.faces.len();
        bridge.set_people(ModelRc::new(VecModel::from(loaded.people)));
        bridge.set_selected(loaded.selected);
        bridge.set_title(loaded.title.into());
        bridge.set_status_text(if n == 1 { "1 face".into() } else { format!("{n} faces").into() });
        bridge.set_empty(n == 0);
        *self.faces.borrow_mut() = loaded.faces;
        self.notify.reset();
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
    type Data = MediaRow;

    fn row_count(&self) -> usize {
        self.faces.borrow().len().div_ceil(self.columns.get())
    }

    fn row_data(&self, row: usize) -> Option<MediaRow> {
        let columns = self.columns.get();
        let faces = self.faces.borrow();
        let start = row * columns;
        let slice = faces.get(start..faces.len().min(start + columns))?;
        let tiles: Vec<MediaTile> = slice
            .iter()
            .map(|face| {
                let thumb = self.crop(face.id);
                MediaTile {
                    index: face.id.0 as i32,
                    name: SharedString::default(),
                    kind: MediaKind::Image,
                    detail: face.detail.clone(),
                    thumb_ready: thumb.is_some(),
                    thumb: thumb.unwrap_or_default(),
                    color: slint::Color::default(),
                    selected: false,
                }
            })
            .collect();
        Some(MediaRow { tiles: ModelRc::new(VecModel::from(tiles)) })
    }

    fn model_tracker(&self) -> &dyn ModelTracker {
        &self.notify
    }
}

/// The last level of a tag path: "People|Alice" -> "Alice".
fn display_name(path: &str) -> &str {
    path.rsplit('|').next().unwrap_or(path).trim()
}

fn file_name(path: &Path) -> SharedString {
    path.file_name().map_or_else(Default::default, |n| n.to_string_lossy().as_ref().into())
}

/// Worker thread: the people list, and the faces of `key` (Unknown if that person is gone).
fn read(root: &Path, key: i32) -> Result<Loaded, String> {
    let catalog = Catalog::open(&Catalog::default_path(root), root).map_err(|e| e.to_string())?;
    let err = |e: archive_core::catalog::Error| e.to_string();

    let unknown = catalog.faces(FaceFilter::Unknown, FaceOrder::Asset).map_err(err)?;
    let named = catalog.people_with_faces().map_err(err)?;
    let mut people = vec![PersonItem { key: UNKNOWN, name: "Unknown".into(), count: unknown.len() as i32 }];
    people.extend(named.iter().map(|p| PersonItem {
        key: p.tag.0 as i32,
        name: display_name(&p.path).into(),
        count: (p.confirmed + p.suggested) as i32,
    }));

    let person = named.iter().find(|p| p.tag.0 as i32 == key);
    let (selected, title, faces) = match person {
        // Confirmed faces, then the ones only suggested as this person, best match first.
        Some(p) => {
            let tag = TagId(p.tag.0);
            let mut faces = catalog.faces(FaceFilter::Confirmed(tag), FaceOrder::Asset).map_err(err)?;
            faces.extend(catalog.faces(FaceFilter::Suggested(tag), FaceOrder::Confidence).map_err(err)?);
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
            let detail = match face.suggestion {
                Some((_, similarity)) if person.is_some() => format!("Suggested · {:.0}%", similarity * 100.0).into(),
                _ => file_name(&path),
            };
            Some(FaceRef { id: face.id, path, detail })
        })
        .collect();
    Ok(Loaded { people, selected, title, faces })
}
