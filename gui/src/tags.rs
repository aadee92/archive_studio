//! Managing a tag from Review's tag tree: its right-click menu (Rename…, Merge into…,
//! Delete…) opens the dialog for it (`TagEdit` / `TagEditDialog` in `ui/menu.slint`). People's
//! list opens the same dialog for a person ([`edit`]): its menu, or a person dragged onto
//! another (Merge into…, filled in, to confirm). For a person the dialog says how many faces
//! and pictures are theirs, read with the catalog's tags, and Merge offers only people.
//!
//! Each change is an [`archive_core::ops`] call on a worker: [`ops::rename_tag`] (onto a
//! taken path the dialog offers to merge instead), [`ops::merge_tags`] (the box offers the
//! catalog's matching tags), [`ops::delete_tag`] (confirmed with how many items it is on).
//! Each is one change in the undo journal: the notice offers Undo. People go through the
//! person operations there, so their faces follow. The files whose tags changed are
//! reported with [`pipeline::changed`], which refreshes the grid (its tag tree included)
//! and Details; People re-reads if a person changed, and
//! Review's tag filter follows the tag ([`review::tag_moved`]).

use std::cell::RefCell;
use std::path::PathBuf;
use std::thread;

use slint::{ComponentHandle, ModelRc, SharedString, VecModel};

use crate::actions::{self, matching_tags};
use crate::pipeline;
use crate::review;
use crate::{AppWindow, TagEdit};
use archive_core::catalog::faces::{FaceFilter, FaceOrder};
use archive_core::catalog::people::{matching_people, PEOPLE};
use archive_core::catalog::{Catalog, TagKind};
use archive_core::ops::{self, Renamed};

struct State {
    root: PathBuf,
    /// The tag the dialog is for, and "rename", "merge" or "delete".
    path: String,
    mode: String,
    /// Every tag in the catalog, for Merge's matches.
    tags: Vec<String>,
    /// Retires the replies of a dialog closed meanwhile.
    generation: u64,
}

thread_local! {
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
}

fn with_state<R>(f: impl FnOnce(&mut State) -> R) -> Option<R> {
    STATE.with_borrow_mut(|state| state.as_mut().map(f))
}

pub fn install(ui: &AppWindow, root: PathBuf) {
    STATE.set(Some(State { root, path: String::new(), mode: String::new(), tags: Vec::new(), generation: 0 }));
    let edit = ui.global::<TagEdit>();
    let weak = ui.as_weak();
    edit.on_action(move |path, count, children, action| {
        log::debug!("tag menu: {action} {path:?} (on {count} items, tags under it {children})");
        if let Some(ui) = weak.upgrade() {
            open(&ui, path.into(), count.max(0) as usize, children, action.into(), None);
        }
    });
    let weak = ui.as_weak();
    edit.on_edited(move |text| {
        if let Some(ui) = weak.upgrade() {
            edited(&ui, &text);
        }
    });
    let weak = ui.as_weak();
    edit.on_submit(move |text, merge| {
        log::debug!("tag edit dialog: submit {text:?} (merge {merge})");
        if let Some(ui) = weak.upgrade() {
            submit(&ui, text.trim().to_owned(), merge);
        }
    });
    let weak = ui.as_weak();
    edit.on_cancel(move || {
        log::debug!("tag edit dialog: cancel");
        if let Some(ui) = weak.upgrade() {
            close(&ui);
        }
    });
}

/// "People / Ann" for `People|Ann`.
fn shown(path: &str) -> String {
    path.split('|').map(str::trim).collect::<Vec<_>>().join(" / ")
}

fn items(n: usize) -> String {
    if n == 1 { "1 item".into() } else { format!("{n} items") }
}

fn is_person(path: &str) -> bool {
    ops::parse_tag(path).0 == TagKind::Person
}

/// Opens the dialog for `action` ("rename", "merge" or "delete") on the person at `path`
/// (from People), its box filled with `initial` (the person they were dropped on).
pub fn edit(ui: &AppWindow, path: String, action: &str, initial: Option<String>) {
    log::debug!("person {action} {path:?} (box {initial:?})");
    open(ui, path, 0, false, action.to_owned(), initial);
}

/// "12 faces on 9 pictures".
fn faces_on(faces: usize, pictures: usize) -> String {
    let faces = if faces == 1 { "1 face".to_owned() } else { format!("{faces} faces") };
    let pictures = if pictures == 1 { "1 picture".to_owned() } else { format!("{pictures} pictures") };
    format!("{faces} on {pictures}")
}

/// What the dialog says it will do to a person who has `faces` named faces on `pictures`.
fn person_message(action: &str, name: &str, faces: usize, pictures: usize) -> String {
    let theirs = faces_on(faces, pictures);
    match action {
        "rename" => format!("{name}: {theirs}. A name alone keeps their place under People."),
        "merge" => format!("{name}'s {theirs} become the person you pick, and {name} is deleted."),
        _ => format!("Delete {name}? Their {theirs} go back to Unknown, and the tag comes off the pictures."),
    }
}

/// How many confirmed faces the person at `path` has, and on how many pictures.
fn person_counts(catalog: &Catalog, path: &str) -> Result<(usize, usize), String> {
    let err = |e: archive_core::catalog::Error| e.to_string();
    let Some((tag, _, _)) = catalog.find_tag(path).map_err(err)? else { return Ok((0, 0)) };
    let faces = catalog.faces(FaceFilter::Confirmed(tag), FaceOrder::Asset).map_err(err)?.len();
    Ok((faces, catalog.tag_assets(tag).map_err(err)?.len()))
}

/// Opens the dialog for `action` on the tag at `path`, which `count` items carry (with the
/// tags under it, if it has `children`), its box filled with `initial` (else the path for a
/// rename), and reads the catalog's tags for it on a worker (and a person's counts).
fn open(ui: &AppWindow, path: String, count: usize, children: bool, action: String, initial: Option<String>) {
    let Some((root, generation)) = with_state(|s| {
        s.generation += 1;
        s.path = path.clone();
        s.mode = action.clone();
        s.tags.clear();
        (s.root.clone(), s.generation)
    }) else {
        return;
    };
    let person = is_person(&path) && !children;
    let under = if children { " and the tags under it" } else { "" };
    let message = match action.as_str() {
        _ if person => "Reading…".to_owned(),
        "rename" => format!("On {}. The tags under it move along.", items(count)),
        "merge" => format!("Its {}{under} go to the tag you pick, and it is deleted.", items(count)),
        _ => {
            let faces = if person { " Faces named as them become unknown again." } else { "" };
            format!("Take it{under} off {} and delete it?{faces}", items(count))
        }
    };
    let edit = ui.global::<TagEdit>();
    edit.set_label(shown(&path).into());
    edit.set_message(message.into());
    let initial = initial.or_else(|| (action == "rename").then(|| path.clone()));
    edit.set_initial(initial.clone().unwrap_or_default().into());
    edit.set_person(person);
    edit.set_matches(ModelRc::default());
    edit.set_error(SharedString::default());
    edit.set_taken(SharedString::default());
    edit.set_working(false);
    edit.set_mode(action.as_str().into());
    let weak = ui.as_weak();
    thread::spawn(move || {
        let read = actions::open_catalog(&root).and_then(|catalog| {
            let known = catalog.find_tag(&path).map_err(|e| e.to_string())?.is_some();
            let counts = if person { Some(person_counts(&catalog, &path)?) } else { None };
            Ok((known, counts, catalog.all_tags().map_err(|e| e.to_string())?))
        });
        let _ = weak.upgrade_in_event_loop(move |ui| {
            if with_state(|s| s.generation) != Some(generation) {
                return;
            }
            match read {
                Ok((known, counts, tags)) => {
                    log::debug!("tag edit dialog: {} tags in the catalog, {path:?} among them {known}; person counts {counts:?}", tags.len());
                    if let Some((faces, pictures)) = counts {
                        let message = person_message(&action, archive_core::catalog::people::display_name(&path), faces, pictures);
                        ui.global::<TagEdit>().set_message(message.into());
                    }
                    if !known {
                        let why = "This tag comes from folder names or another program's sidecar, not the catalog: change it there.";
                        ui.global::<TagEdit>().set_error(why.into());
                    }
                    with_state(|s| s.tags = tags);
                }
                Err(e) => {
                    log::warn!("tag edit dialog: reading the tags: {e}");
                    ui.global::<TagEdit>().set_error(e.into());
                }
            }
        });
    });
}

fn edited(ui: &AppWindow, text: &str) {
    let edit = ui.global::<TagEdit>();
    edit.set_taken(SharedString::default());
    edit.set_error(SharedString::default());
    let offered: Vec<SharedString> = with_state(|s| {
        if s.mode != "merge" {
            return Vec::new();
        }
        if is_person(&s.path) {
            // Only people, not the groups above them, and not this one.
            let own = s.path.to_ascii_lowercase();
            let people: Vec<&String> = s
                .tags
                .iter()
                .filter(|t| {
                    let lower = t.to_ascii_lowercase();
                    is_person(t) && lower != own && !lower.eq_ignore_ascii_case(PEOPLE)
                        && !s.tags.iter().any(|u| u.to_ascii_lowercase().starts_with(&format!("{lower}|")))
                })
                .collect();
            return matching_people(&people, text, 6).into_iter().map(SharedString::from).collect();
        }
        // Not the tag itself or one under it.
        let own = s.path.to_ascii_lowercase();
        let tags: Vec<String> = s
            .tags
            .iter()
            .filter(|t| {
                let t = t.to_ascii_lowercase();
                t != own && !t.starts_with(&format!("{own}|"))
            })
            .cloned()
            .collect();
        matching_tags(&tags, text).into_iter().map(SharedString::from).collect()
    })
    .unwrap_or_default();
    edit.set_matches(ModelRc::new(VecModel::from(offered)));
}

fn close(ui: &AppWindow) {
    ui.global::<TagEdit>().set_mode(SharedString::default());
    with_state(|s| s.generation += 1);
}

/// What a change did, for the notice and the tag filter.
struct Done {
    /// Where the tag went (`None`: deleted), as the catalog spells it.
    to: Option<String>,
    files: Vec<PathBuf>,
    person: bool,
}

/// Does what the dialog asks on a worker: rename to `text` (merging into the tag there if
/// `merge`), merge into `text`, or delete.
fn submit(ui: &AppWindow, text: String, merge: bool) {
    let Some((root, path, mode, generation)) = with_state(|s| (s.root.clone(), s.path.clone(), s.mode.clone(), s.generation)) else {
        return;
    };
    if mode != "delete" && text.is_empty() {
        return;
    }
    let edit = ui.global::<TagEdit>();
    edit.set_working(true);
    edit.set_error(SharedString::default());
    let weak = ui.as_weak();
    thread::spawn(move || {
        let person = is_person(&path);
        let result: Result<Result<Done, String>, String> = actions::open_catalog(&root).and_then(|mut catalog| {
            let changed = match mode.as_str() {
                "rename" => ops::rename_tag(&mut catalog, &path, &text, merge).map(|renamed| match renamed {
                    Renamed::Done(done) => Ok((Some(done.to), done.assets)),
                    Renamed::Taken(taken) => Err(taken),
                }),
                "merge" => ops::merge_tags(&mut catalog, &path, &text).map(|done| Ok((Some(done.to), done.assets))),
                _ => ops::delete_tag(&mut catalog, &path).map(|assets| Ok((None, assets))),
            }
            .map_err(|e| e.to_string())?;
            Ok(changed.map(|(to, assets)| Done { to, files: assets.iter().filter_map(|&a| catalog.asset_path(a).ok()).collect(), person }))
        });
        if let Ok(Ok(done)) = &result {
            log::info!("tag {mode} {path:?} -> {:?}: {} files changed", done.to, done.files.len());
            for file in &done.files {
                pipeline::changed(file, false);
            }
        }
        let _ = weak.upgrade_in_event_loop(move |ui| {
            if with_state(|s| s.generation) != Some(generation) {
                return;
            }
            let edit = ui.global::<TagEdit>();
            edit.set_working(false);
            match result {
                Ok(Ok(done)) => {
                    close(&ui);
                    review::tag_moved(&ui, &path, done.to.as_deref());
                    if done.person {
                        crate::people::faces_changed(&ui);
                    }
                    let what = match (&done.to, mode.as_str()) {
                        (Some(to), "rename") => format!("Renamed {} to {}", shown(&path), shown(to)),
                        (Some(to), _) => format!("Merged {} into {}", shown(&path), shown(to)),
                        (None, _) => format!("Deleted {} ({})", shown(&path), items(done.files.len())),
                    };
                    actions::notify(&ui, what, false, true);
                }
                Ok(Err(taken)) => edit.set_taken(taken.into()),
                Err(e) => edit.set_error(e.into()),
            }
        });
    });
}
