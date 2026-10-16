//! What a front end does to one asset from a context menu: rotate, delete (to the trash),
//! tag, and show its properties. Each works on a [`Catalog`] alone, with or without a
//! pipeline [`Engine`](crate::pipeline::Engine) running on the library.
//!
//! - [`rotate`]: the user's rotation, final like every user decision (the orientation model
//!   never redoes it). With an engine at hand, [`Engine::rotate`](crate::pipeline::Engine::rotate)
//!   does the same on the engine thread.
//! - [`delete`]: to the library's trash, reversible with [`Catalog::restore`] (see
//!   [`catalog::trash`](crate::catalog::trash)); a front takes its back with it.
//! - Tags: [`add_tag`] and [`remove_tag`] for what a user typed (see [`parse_tag`]),
//!   [`Catalog::add_tag_path`], [`Catalog::remove_tag_path`]; people:
//!   [`Catalog::name_faces`] and the rest of [`catalog::people`](crate::catalog::people).
//!   Taking a person tag off a picture un-names that person's faces on it (they go back to
//!   the unknown faces), rather than being refused: the tag is what says they are there.
//! - Tags as a whole: [`tag_tree`] (with how many pictures each is on), [`rename_tag`]
//!   (asks before merging onto a taken path), [`merge_tags`], [`delete_tag`], with their
//!   subtrees; people go through the person operations so their faces follow (see
//!   [`catalog::tags`](crate::catalog::tags)). Each returns the assets whose tags changed.
//! - Front and back: [`pair`] a back scan with its front by hand, [`unpair`],
//!   [`swap_sides`] when they were paired the wrong way round (see
//!   [`catalog::pairs`](crate::catalog::pairs)).
//! - Faces on one picture: [`add_face`] for one the detector missed (boxed by the user);
//!   [`Catalog::not_person`], [`Catalog::ignore_faces`] and [`Catalog::delete_faces`] for the
//!   ones there.
//! - [`properties`]: everything known about the asset, for a properties dialog.
//! - Search by description: [`text_search`] gives the text model (the engine's, else one
//!   loaded once per process) for [`TextSearch::search`]; [`similar_images`](pipeline::describe::similar_images) finds
//!   pictures that look like one; [`Catalog::embedding_coverage`] says how much of the
//!   library either can find.
//! - Many at once (a multi-selection): [`delete_all`] / [`restore_all`] (one Undo for all
//!   of them), [`add_tag_all`]. Each returns what happened to every asset, or is all or
//!   nothing, as documented.
//! - The trash as a whole: [`Catalog::trashed`], [`Catalog::trash_totals`],
//!   [`Catalog::purge`] (chosen items for good), [`Catalog::empty_trash`].
//! - Failures: [`Catalog::failures`] lists them; [`retry`] runs them again, through the
//!   engine when one runs here.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use image::{ImageDecoder, ImageReader};

use crate::catalog::faces::{self as catalog_faces, FaceId};
use crate::catalog::people::FaceBox;
use crate::catalog::runs::{Retry, RunStatus, StageRun};
use crate::catalog::trash::Trashed;
use crate::catalog::people::{display_name, person_path, PEOPLE};
use crate::catalog::tags::{self, TagNode};
use crate::catalog::{AppliedTag, AssetId, Catalog, Region, TagId, TagKind};
use crate::exif::{self, Exif};
use crate::media::{classify, MediaKind};
use crate::pipeline::describe::TextSearch;
use crate::pipeline::{self, Engine, Models, ORIENT};

/// Turns the asset's image `cw` degrees clockwise (90, 180 or 270), rewriting the file
/// losslessly; see [`pipeline::rotate_file`].
pub fn rotate(catalog: &Catalog, asset: AssetId, cw: u16) -> Result<(), String> {
    let path = catalog.asset_path(asset).map_err(|e| e.to_string())?;
    let result = pipeline::rotate_file(catalog, &path, cw);
    match &result {
        Ok(()) => log::debug!("rotate asset {} ({}) {cw}°: done", asset.0, path.display()),
        Err(e) => log::warn!("rotate asset {} ({}) {cw}°: {e}", asset.0, path.display()),
    }
    result
}

/// Moves the asset (and its back, if it is a front) to the library's trash; see
/// [`Catalog::trash`]. Undo with [`Catalog::restore`].
pub fn delete(catalog: &mut Catalog, asset: AssetId) -> crate::catalog::Result<Vec<Trashed>> {
    catalog.trash(asset)
}

/// What a bulk operation did to each asset it was given, in the order given.
pub type Each<T> = Vec<(AssetId, crate::catalog::Result<T>)>;

/// Moves each of `assets` to the trash as [`delete`] does: one failing (e.g. its file can't
/// be moved) doesn't stop the others. An asset already in the trash (a back trashed with
/// its front earlier in the list) gives an empty list. Undo with [`restore_all`] of the
/// assets that trashed something.
pub fn delete_all(catalog: &mut Catalog, assets: &[AssetId]) -> Each<Vec<Trashed>> {
    let done: Each<Vec<Trashed>> = assets.iter().map(|&asset| (asset, catalog.trash(asset))).collect();
    let failed = done.iter().filter(|(_, r)| r.is_err()).count();
    let files: usize = done.iter().filter_map(|(_, r)| r.as_ref().ok()).map(Vec::len).sum();
    log::debug!("delete_all {} assets: {files} files to the trash, {failed} failed", assets.len());
    done
}

/// Puts each of `assets` back from the trash as [`Catalog::restore`] does (with its back);
/// one failing (something else is at its old path now) doesn't stop the others. An asset
/// that isn't in the trash (any more) gives an empty list.
pub fn restore_all(catalog: &mut Catalog, assets: &[AssetId]) -> Each<Vec<PathBuf>> {
    let done: Each<Vec<PathBuf>> = assets.iter().map(|&asset| (asset, catalog.restore(asset))).collect();
    let failed = done.iter().filter(|(_, r)| r.is_err()).count();
    let files: usize = done.iter().filter_map(|(_, r)| r.as_ref().ok()).map(Vec::len).sum();
    log::debug!("restore_all {} assets: {files} files restored, {failed} failed", assets.len());
    done
}

/// What a typed tag means: `kind:path` picks the kind (`place:Home|Kitchen`); a path under
/// `People|` is a person; anything else is a keyword. The path is returned trimmed.
pub fn parse_tag(text: &str) -> (TagKind, String) {
    let text = text.trim();
    if let Some((kind, rest)) = text.split_once(':').and_then(|(k, rest)| TagKind::from_name(k.trim()).map(|k| (k, rest.trim()))) {
        return (kind, rest.to_owned());
    }
    let first = text.split('|').next().unwrap_or_default().trim();
    let kind = if first.eq_ignore_ascii_case(PEOPLE) && text.contains('|') { TagKind::Person } else { TagKind::Keyword };
    (kind, text.to_owned())
}

/// The user puts the tag they typed on the asset (see [`parse_tag`]); a person is found as
/// [`Catalog::person`] finds one. Returns the tag.
pub fn add_tag(catalog: &Catalog, asset: AssetId, text: &str) -> crate::catalog::Result<TagId> {
    let parsed = parse_tag(text);
    let result = add_parsed_tag(catalog, asset, parsed.clone());
    match &result {
        Ok(tag) => log::debug!("add_tag {text:?} ({:?} {:?}) to asset {}: tag {}", parsed.0, parsed.1, asset.0, tag.0),
        Err(e) => log::warn!("add_tag {text:?} to asset {}: {e}", asset.0),
    }
    result
}

/// The user puts the tag they typed (see [`parse_tag`]) on every one of `assets`, in one
/// transaction: all of them, or none on an error. Returns the tag.
pub fn add_tag_all(catalog: &mut Catalog, assets: &[AssetId], text: &str) -> crate::catalog::Result<TagId> {
    let parsed = parse_tag(text);
    let result = match &parsed {
        (TagKind::Person, path) => catalog.person(path),
        (kind, path) => catalog.tag(path, *kind),
    }
    .and_then(|tag| catalog.add_tag_to_all(assets, tag).map(|()| tag));
    match &result {
        Ok(tag) => log::debug!("add_tag_all {text:?} ({:?} {:?}) to {} assets: tag {}", parsed.0, parsed.1, assets.len(), tag.0),
        Err(e) => log::warn!("add_tag_all {text:?} to {} assets: {e}", assets.len()),
    }
    result
}

fn add_parsed_tag(catalog: &Catalog, asset: AssetId, parsed: (TagKind, String)) -> crate::catalog::Result<TagId> {
    match parsed {
        (TagKind::Person, path) => {
            let person = catalog.person(&path)?;
            catalog.add_tag(asset, person, None)?;
            Ok(person)
        }
        (kind, path) => catalog.add_tag_path(asset, &path, kind),
    }
}

/// The user takes the tag they typed off the asset: matched against the asset's own tags
/// ignoring case and spaces around `|`, of the kind given if typed as `kind:path` (see
/// [`parse_tag`]); a bare name with no tag of that path is a person by that name
/// (`Alice` → `People|Alice`). See [`Catalog::remove_tag`]: a tag a detector proposed is
/// remembered as rejected, and taking a person tag off un-names that person's faces on the
/// asset. Returns false (changing nothing) if no such tag is on it.
pub fn remove_tag(catalog: &mut Catalog, asset: AssetId, text: &str) -> crate::catalog::Result<bool> {
    let result = remove_typed_tag(catalog, asset, text);
    match &result {
        Ok(removed) => log::debug!("remove_tag {text:?} from asset {}: removed {removed}", asset.0),
        Err(e) => log::warn!("remove_tag {text:?} from asset {}: {e}", asset.0),
    }
    result
}

fn remove_typed_tag(catalog: &mut Catalog, asset: AssetId, text: &str) -> crate::catalog::Result<bool> {
    let explicit = text.split_once(':').is_some_and(|(k, _)| TagKind::from_name(k.trim()).is_some());
    let (kind, path) = parse_tag(text);
    let same = |a: &str, b: &str| {
        let (mut a, mut b) = (a.split('|').map(str::trim), b.split('|').map(str::trim));
        loop {
            match (a.next(), b.next()) {
                (None, None) => return true,
                (Some(x), Some(y)) if x.eq_ignore_ascii_case(y) => {}
                _ => return false,
            }
        }
    };
    let tags = catalog.tags_of(asset)?;
    let found = tags.iter().find(|t| same(&t.path, &path) && (!explicit || t.kind == kind)).or_else(|| {
        let bare = !explicit && !path.contains('|');
        tags.iter().find(|t| bare && t.kind == TagKind::Person && display_name(&t.path).eq_ignore_ascii_case(&path))
    });
    let Some(tag) = found.map(|t| t.tag) else { return Ok(false) };
    catalog.remove_tag(asset, tag)?;
    Ok(true)
}

/// A tag renamed or merged: where it is now, and the assets whose tags changed (to refresh).
#[derive(Clone, Debug, PartialEq)]
pub struct Retagged {
    /// The tag's path now (the tag merged into), as the catalog spells it.
    pub to: String,
    pub assets: Vec<AssetId>,
}

/// What [`rename_tag`] did.
#[derive(Clone, Debug, PartialEq)]
pub enum Renamed {
    /// Renamed (or merged, if asked to).
    Done(Retagged),
    /// The new path is another tag's (as it is spelled): nothing changed. Merge into it?
    Taken(String),
}

/// The tags in the catalog as a tree (`People|Ann` under `People`), each with how many
/// pictures (not in the trash) carry it or a tag under it; detector tags from
/// [`DEFAULT_MIN_CONFIDENCE`](crate::catalog::DEFAULT_MIN_CONFIDENCE), as searches count
/// them. See [`Catalog::tag_counts`].
pub fn tag_tree(catalog: &Catalog) -> crate::catalog::Result<Vec<TagNode>> {
    Ok(tags::tree(catalog.tag_counts(crate::catalog::DEFAULT_MIN_CONFIDENCE)?.into_iter().map(|c| (c.path, c.count))))
}

/// The user renames the tag at `from` (as typed, `kind:` allowed) to `to`, along with the
/// tags under it ([`Catalog::rename_tag`]). A person stays a person under `People|`: `to`
/// without a `|` keeps their place (`People|Family|Al` → `People|Family|Alice`), and the
/// rename goes through [`Catalog::rename_person`]; another tag can't move under `People|`.
/// If `to` is another tag's path, that is [`Renamed::Taken`] unless `merge`, which merges
/// the two as [`merge_tags`] does. `People` itself can't be renamed.
pub fn rename_tag(catalog: &mut Catalog, from: &str, to: &str, merge: bool) -> crate::catalog::Result<Renamed> {
    let result = (|| {
        let (tag, path, kind) = existing_tag(catalog, from)?;
        let target = match kind {
            TagKind::Person if !to.contains('|') => match path.rsplit_once('|') {
                Some((parent, _)) => format!("{parent}|{}", to.trim()),
                None => person_path(to)?,
            },
            _ => target_path(kind, to)?,
        };
        retag(catalog, (tag, &path, kind), &target, merge)
    })();
    match &result {
        Ok(Renamed::Done(done)) => log::debug!("rename_tag {from:?} to {to:?} (merge {merge}): now {:?}, {} assets changed", done.to, done.assets.len()),
        Ok(Renamed::Taken(path)) => log::debug!("rename_tag {from:?} to {to:?}: {path} is taken, nothing changed"),
        Err(e) => log::warn!("rename_tag {from:?} to {to:?} (merge {merge}): {e}"),
    }
    result
}

/// The user merges the tag at `from` into `into` (both as typed): the pictures, faces and
/// tags under `from` become `into`'s ([`Catalog::merge_tags`]; two people without people
/// under them go through [`Catalog::merge_people`]). A person can only be merged into a
/// person, named by path or by name (`Alice`, if only one person is called that); a tag
/// into one that doesn't exist yet is a rename.
pub fn merge_tags(catalog: &mut Catalog, from: &str, into: &str) -> crate::catalog::Result<Retagged> {
    let result = (|| {
        let (tag, path, kind) = existing_tag(catalog, from)?;
        let target = match kind {
            TagKind::Person if !into.contains('|') => {
                let named: Vec<String> = catalog
                    .leaf_tags(TagKind::Person)?
                    .into_iter()
                    .map(|(_, p)| p)
                    .filter(|p| display_name(p).eq_ignore_ascii_case(into.trim()))
                    .collect();
                match named.as_slice() {
                    [only] => only.clone(),
                    _ => person_path(into)?,
                }
            }
            _ => target_path(kind, into)?,
        };
        match retag(catalog, (tag, &path, kind), &target, true)? {
            Renamed::Done(done) => Ok(done),
            Renamed::Taken(path) => Err(crate::catalog::Error::Invalid(format!("{path} is taken"))),
        }
    })();
    match &result {
        Ok(done) => log::debug!("merge_tags {from:?} into {into:?}: now {:?}, {} assets changed", done.to, done.assets.len()),
        Err(e) => log::warn!("merge_tags {from:?} into {into:?}: {e}"),
    }
    result
}

/// The user deletes the tag at `path` (as typed) and the tags under it: off every picture,
/// faces named as them un-named ([`Catalog::delete_tag`]; a person without people under
/// them through [`Catalog::delete_person`]). Not undoable: a front end confirms first, with
/// the count from [`tag_tree`]. `People` itself can't be deleted. Returns the assets whose
/// tags changed.
pub fn delete_tag(catalog: &mut Catalog, path: &str) -> crate::catalog::Result<Vec<AssetId>> {
    let result = (|| {
        let (tag, path, kind) = existing_tag(catalog, path)?;
        not_people_root(&path)?;
        let assets = catalog.tag_assets(tag)?;
        if kind == TagKind::Person && !has_children(catalog, &path)? {
            catalog.delete_person(tag)?;
        } else {
            catalog.delete_tag(tag)?;
        }
        Ok(assets)
    })();
    match &result {
        Ok(assets) => log::debug!("delete_tag {path:?}: off {} assets", assets.len()),
        Err(e) => log::warn!("delete_tag {path:?}: {e}"),
    }
    result
}

/// The tag at a typed path (`kind:` prefix dropped), or an error naming it.
fn existing_tag(catalog: &Catalog, text: &str) -> crate::catalog::Result<(TagId, String, TagKind)> {
    let (_, path) = parse_tag(text);
    catalog.find_tag(&path)?.ok_or_else(|| crate::catalog::Error::Invalid(format!("no tag {path}")))
}

/// Where a tag of `kind` goes for a typed path: a person under `People|`, anything else
/// anywhere but there.
fn target_path(kind: TagKind, text: &str) -> crate::catalog::Result<String> {
    let (_, path) = parse_tag(text);
    if kind == TagKind::Person {
        return person_path(&path);
    }
    let first = path.split('|').next().unwrap_or_default().trim();
    if first.eq_ignore_ascii_case(PEOPLE) && path.contains('|') {
        return Err(crate::catalog::Error::Invalid(format!("{PEOPLE}|… is for people; {path} would be a {}", kind.name())));
    }
    Ok(path)
}

fn not_people_root(path: &str) -> crate::catalog::Result<()> {
    if path.eq_ignore_ascii_case(PEOPLE) {
        return Err(crate::catalog::Error::Invalid(format!("{PEOPLE} holds everyone named; rename, merge or delete people one by one")));
    }
    Ok(())
}

fn has_children(catalog: &Catalog, path: &str) -> crate::catalog::Result<bool> {
    let prefix = format!("{}|", path.to_ascii_lowercase());
    Ok(catalog.all_tags()?.iter().any(|t| t.to_ascii_lowercase().starts_with(&prefix)))
}

/// Renames `tag` to `target`, or merges it into the tag there if there is one and `merge`.
fn retag(catalog: &mut Catalog, (tag, path, kind): (TagId, &str, TagKind), target: &str, merge: bool) -> crate::catalog::Result<Renamed> {
    use crate::catalog::Error;
    not_people_root(path)?;
    let assets = catalog.tag_assets(tag)?;
    let now = match catalog.find_tag(target)? {
        Some((other, spelled, other_kind)) if other != tag => {
            if !merge {
                return Ok(Renamed::Taken(spelled));
            }
            if (kind == TagKind::Person) != (other_kind == TagKind::Person) {
                return Err(Error::Invalid(format!("{path} is a {} and {spelled} a {}: only people merge with people", kind.name(), other_kind.name())));
            }
            if kind == TagKind::Person && !has_children(catalog, path)? {
                catalog.merge_people(tag, other)?;
            } else {
                catalog.merge_tags(tag, other)?;
            }
            other
        }
        _ if kind == TagKind::Person => catalog.rename_person(tag, target)?,
        _ => {
            catalog.rename_tag(tag, target)?;
            tag
        }
    };
    Ok(Renamed::Done(Retagged { to: catalog.tag_path(now)?, assets }))
}

/// The user pairs `back` (a scan of a photo's back) with `front`: both images in the
/// library, not in the trash, and not the same. A pairing either had is replaced (see
/// [`Catalog::set_pair`]). Returns the assets whose other side changed: the two, and
/// whatever they were paired with before.
pub fn pair(catalog: &mut Catalog, front: AssetId, back: AssetId) -> crate::catalog::Result<Vec<AssetId>> {
    let result = pair_checked(catalog, front, back);
    match &result {
        Ok(changed) => log::debug!("pair front {} back {}: changed {changed:?}", front.0, back.0),
        Err(e) => log::warn!("pair front {} back {}: {e}", front.0, back.0),
    }
    result
}

fn pair_checked(catalog: &mut Catalog, front: AssetId, back: AssetId) -> crate::catalog::Result<Vec<AssetId>> {
    use crate::catalog::Error;
    if front == back {
        return Err(Error::Invalid("a photo can't be its own back".into()));
    }
    for asset in [front, back] {
        let path = catalog.asset_path(asset)?;
        let name = path.file_name().map_or_else(|| path.display().to_string(), |n| n.to_string_lossy().into_owned());
        if classify(&path) != Some(MediaKind::Image) {
            return Err(Error::Invalid(format!("{name} isn't an image")));
        }
        if catalog.is_trashed(asset)? {
            return Err(Error::Invalid(format!("{name} is in the trash")));
        }
    }
    let mut changed = vec![front, back];
    for asset in [front, back] {
        for other in [catalog.back_of(asset)?, catalog.front_of(asset)?].into_iter().flatten() {
            if !changed.contains(&other) {
                changed.push(other);
            }
        }
    }
    catalog.set_pair(front, back)?;
    Ok(changed)
}

/// Takes the asset (a front or a back) out of its pair. Returns the other side it had,
/// `None` if it had none.
pub fn unpair(catalog: &mut Catalog, asset: AssetId) -> crate::catalog::Result<Option<AssetId>> {
    let result = (|| {
        let other = catalog.back_of(asset)?.or(catalog.front_of(asset)?);
        if other.is_some() {
            catalog.unpair(asset)?;
        }
        Ok(other)
    })();
    crate::catalog::logged(format_args!("unpair asset {}", asset.0), result)
}

/// The asset's pair was the wrong way round: its front becomes the back and the other way
/// round. Returns the new `(front, back)`; an error if the asset isn't paired.
pub fn swap_sides(catalog: &mut Catalog, asset: AssetId) -> crate::catalog::Result<(AssetId, AssetId)> {
    let result = (|| {
        let (front, back) = match (catalog.back_of(asset)?, catalog.front_of(asset)?) {
            (Some(back), _) => (asset, back),
            (_, Some(front)) => (front, asset),
            _ => return Err(crate::catalog::Error::Invalid("it has no other side to swap with".into())),
        };
        catalog.set_pair(back, front)?;
        Ok((back, front))
    })();
    crate::catalog::logged(format_args!("swap_sides asset {}", asset.0), result)
}

/// The user boxed a face the detector missed at `region` of the asset's image (fractions of
/// the image as displayed): it becomes a manual face with its crop and, with `models` (an
/// engine's [`face_models`](crate::pipeline::Engine::face_models), else [`face_models`]), an
/// embedding, so suggestions and clusters take it in. Decodes the file: call it on a worker.
/// Returns the new face, unnamed.
pub fn add_face(catalog: &mut Catalog, asset: AssetId, region: Region, models: Option<&faces::Faces>) -> Result<FaceId, String> {
    let path = catalog.asset_path(asset).map_err(|e| e.to_string())?;
    let result = catalog_faces::scan_region(models, &path, region).and_then(|face| catalog.add_face(asset, &face).map_err(|e| e.to_string()));
    match &result {
        Ok(face) => log::debug!("add_face on asset {} ({}): face {} (embedded {})", asset.0, path.display(), face.0, models.is_some()),
        Err(e) => log::warn!("add_face on asset {} ({}) at {region:?}: {e}", asset.0, path.display()),
    }
    result
}

/// Loads the face models from the models directory ([`Models::locate`]), for a host without
/// an engine to borrow them from. Takes a few seconds: call it on a worker, once.
pub fn face_models() -> Result<faces::Faces, String> {
    let dir = Models::locate().faces.ok_or("no face model (buffalo_l in the models directory)")?;
    let started = std::time::Instant::now();
    let models = faces::Faces::from_dir(&dir).map_err(|e| format!("face model: {e}"))?;
    log::debug!("face models loaded from {} in {:?}", dir.display(), started.elapsed());
    Ok(models)
}

/// Forgets the failed runs `which` names so their stages run again: by `engine` if it runs
/// on the library in this process (it then looks for work at once), else in the catalog
/// (an engine elsewhere picks them up the next time it looks). Returns how many runs.
pub fn retry(catalog: &Catalog, engine: Option<&Engine>, which: Retry) -> Result<usize, String> {
    match engine {
        Some(engine) => engine.retry(which),
        None => catalog.retry(&which).map_err(|e| e.to_string()),
    }
}

/// The text model [`text_search`] loaded, kept for the life of the process.
static OWN_TEXT_SEARCH: Mutex<Option<Arc<TextSearch>>> = Mutex::new(None);

/// The CLIP text model for searching by description ([`TextSearch::search`]): `engine`'s if
/// it has loaded one, else one loaded from the models directory ([`Models::locate`]) on the
/// first call and kept. Loading takes a moment: call it on a worker. Fails without the model
/// (tried again on the next call, in case it was installed meanwhile).
pub fn text_search(engine: Option<&Engine>) -> Result<Arc<TextSearch>, String> {
    if let Some(search) = engine.and_then(Engine::text_search) {
        return Ok(search);
    }
    // Held while loading, so concurrent callers wait for one load.
    let mut own = OWN_TEXT_SEARCH.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(search) = own.as_ref() {
        return Ok(search.clone());
    }
    let dir = Models::locate().describe.ok_or("description search needs the tinyclip-8m model (in the models directory)")?;
    let started = std::time::Instant::now();
    let search = Arc::new(TextSearch::from_dir(&dir).map_err(|e| format!("CLIP model: {e}"))?);
    log::debug!("CLIP text model loaded from {} in {:?}", dir.display(), started.elapsed());
    *own = Some(search.clone());
    Ok(search)
}

/// How the asset's rotation stands, from the orient stage.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Rotation {
    /// The last rotation applied to the file, clockwise degrees (0: left as it was).
    pub applied_cw: Option<u16>,
    /// A rotation the model proposes and waits for the user to accept.
    pub proposed_cw: Option<u16>,
    /// The user decided (rotated or kept it as is); the model won't change it.
    pub by_user: bool,
    /// E.g. `rotated 90° (97%)`, `turned 180° by you`.
    pub detail: String,
}

/// Everything known about an asset.
#[derive(Clone, Debug)]
pub struct Properties {
    pub asset: AssetId,
    pub path: PathBuf,
    pub kind: Option<MediaKind>,
    /// The file isn't on disk.
    pub missing: bool,
    pub trashed: bool,
    pub size: Option<u64>,
    pub modified: Option<SystemTime>,
    /// Where the filesystem records it.
    pub created: Option<SystemTime>,
    /// Width and height as displayed (EXIF orientation applied).
    pub dimensions: Option<(u32, u32)>,
    /// `Png`, `Jpeg`, …
    pub format: Option<String>,
    pub exif: Exif,
    pub tags: Vec<AppliedTag>,
    pub faces: Vec<FaceBox>,
    pub rotation: Rotation,
    /// Each pipeline stage's last run on it.
    pub stages: Vec<StageRun>,
    /// The other side of the photo: its back if this is a front, its front if a back.
    pub front: Option<AssetId>,
    pub back: Option<AssetId>,
}

/// Reads the asset's properties: catalog data, plus the file's metadata and image header
/// (cheap: no pixels are decoded).
pub fn properties(catalog: &Catalog, asset: AssetId) -> crate::catalog::Result<Properties> {
    let path = catalog.asset_path(asset)?;
    let meta = std::fs::metadata(&path).ok();
    let (mut dimensions, mut format, mut exif) = (None, None, Exif::default());
    if classify(&path) == Some(MediaKind::Image) {
        if let Ok(reader) = ImageReader::open(&path).and_then(|r| r.with_guessed_format()) {
            format = reader.format().map(|f| format!("{f:?}"));
            if let Ok(mut decoder) = reader.into_decoder() {
                let (w, h) = decoder.dimensions();
                if let Ok(Some(raw)) = decoder.exif_metadata() {
                    exif = exif::parse(&raw);
                }
                let turned = matches!(exif.orientation, Some(5..=8));
                dimensions = Some(if turned { (h, w) } else { (w, h) });
            }
        }
    }
    let stages = catalog.stage_runs(asset)?;
    Ok(Properties {
        asset,
        kind: classify(&path),
        missing: meta.is_none(),
        trashed: catalog.is_trashed(asset)?,
        size: meta.as_ref().map(|m| m.len()),
        modified: meta.as_ref().and_then(|m| m.modified().ok()),
        created: meta.as_ref().and_then(|m| m.created().ok()),
        dimensions,
        format,
        exif,
        tags: catalog.tags_of(asset)?,
        faces: catalog.face_boxes(asset)?,
        rotation: rotation(&stages),
        front: catalog.front_of(asset)?,
        back: catalog.back_of(asset)?,
        stages,
        path,
    })
}

fn rotation(stages: &[StageRun]) -> Rotation {
    let Some(run) = stages.iter().find(|r| r.stage == ORIENT) else { return Rotation::default() };
    let cw = run.value.map(|v| v as u16);
    match run.status {
        RunStatus::User => Rotation { applied_cw: cw, proposed_cw: None, by_user: true, detail: run.detail.clone() },
        RunStatus::Review => Rotation { applied_cw: None, proposed_cw: cw, by_user: false, detail: run.detail.clone() },
        RunStatus::Done => Rotation { applied_cw: cw, proposed_cw: None, by_user: false, detail: run.detail.clone() },
        RunStatus::Failed => Rotation { detail: run.detail.clone(), ..Rotation::default() },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::{Region, TagKind};

    #[test]
    fn typed_tags() {
        assert_eq!(parse_tag(" beach "), (TagKind::Keyword, "beach".to_owned()));
        assert_eq!(parse_tag("place: Home|Kitchen"), (TagKind::Place, "Home|Kitchen".to_owned()));
        assert_eq!(parse_tag("people|Alice"), (TagKind::Person, "people|Alice".to_owned()));
        assert_eq!(parse_tag("People"), (TagKind::Keyword, "People".to_owned()));
        assert_eq!(parse_tag("12:30"), (TagKind::Keyword, "12:30".to_owned()));

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let cat = Catalog::open(&Catalog::default_path(&root), &root).unwrap();
        let a = cat.asset(&root.join("a.png")).unwrap();
        let alice = cat.person("Alice").unwrap();
        assert_eq!(add_tag(&cat, a, "People|Alice").unwrap(), alice);
        add_tag(&cat, a, "event:Wedding").unwrap();
        let tags: Vec<_> = cat.tags_of(a).unwrap().into_iter().map(|t| (t.path, t.kind)).collect();
        assert_eq!(tags, [("People|Alice".to_owned(), TagKind::Person), ("Wedding".to_owned(), TagKind::Event)]);
    }

    #[test]
    fn removing_a_typed_tag() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        image::RgbImage::from_pixel(40, 20, image::Rgb([200, 150, 120])).save(root.join("a.png")).unwrap();
        let mut cat = Catalog::open(&Catalog::default_path(&root), &root).unwrap();
        let a = cat.asset(&root.join("a.png")).unwrap();
        add_tag(&cat, a, "Holidays|1998").unwrap();
        add_tag(&cat, a, "place:Home").unwrap();
        let face = add_face(&mut cat, a, Region { x: 0.5, y: 0.0, w: 0.25, h: 0.5 }, None).unwrap();
        cat.name_faces(&[face], "Alice").unwrap();
        let paths = |cat: &Catalog| cat.tags_of(a).unwrap().into_iter().map(|t| t.path).collect::<Vec<_>>();
        assert_eq!(paths(&cat), ["Holidays|1998", "Home", "People|Alice"]);

        assert!(!remove_tag(&mut cat, a, "Holidays").unwrap(), "only the tag itself");
        assert!(!remove_tag(&mut cat, a, "event:Home").unwrap(), "of the kind typed");
        assert!(remove_tag(&mut cat, a, " holidays | 1998 ").unwrap());
        assert!(remove_tag(&mut cat, a, "place:home").unwrap());
        assert!(!remove_tag(&mut cat, a, "home").unwrap(), "already off");
        // A person by name: their face on it goes back to unknown.
        assert!(remove_tag(&mut cat, a, "alice").unwrap());
        assert!(paths(&cat).is_empty());
        let boxes = cat.face_boxes(a).unwrap();
        assert_eq!((boxes.len(), boxes[0].person.is_none()), (1, true));
    }

    #[test]
    fn managing_tags() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        image::RgbImage::from_pixel(40, 20, image::Rgb([200, 150, 120])).save(root.join("a.png")).unwrap();
        let mut cat = Catalog::open(&Catalog::default_path(&root), &root).unwrap();
        let a = cat.asset(&root.join("a.png")).unwrap();
        let b = cat.asset(&root.join("b.png")).unwrap();
        add_tag(&cat, a, "Holidays|1998").unwrap();
        add_tag(&cat, b, "Trips|1998").unwrap();
        add_tag(&cat, b, "People|Bob").unwrap();
        let face = add_face(&mut cat, a, Region { x: 0.5, y: 0.0, w: 0.25, h: 0.5 }, None).unwrap();
        cat.name_faces(&[face], "Family|Al").unwrap();
        let paths = |cat: &Catalog, asset| cat.tags_of(asset).unwrap().into_iter().map(|t| t.path).collect::<Vec<_>>();
        let tree = |cat: &Catalog| tag_tree(cat).unwrap().into_iter().map(|n| format!("{}{} {}", "-".repeat(n.depth), n.name, n.count)).collect::<Vec<_>>();
        assert_eq!(tree(&cat), ["Holidays 1", "-1998 1", "People 2", "-Bob 1", "-Family 1", "--Al 1", "Trips 1", "-1998 1"]);

        // Onto a taken path: asked first, then merged.
        assert_eq!(rename_tag(&mut cat, "trips", "HOLIDAYS", false).unwrap(), Renamed::Taken("Holidays".into()));
        assert_eq!(rename_tag(&mut cat, "trips", "HOLIDAYS", true).unwrap(), Renamed::Done(Retagged { to: "Holidays".into(), assets: vec![b] }));
        assert_eq!(paths(&cat, b), ["Holidays|1998", "People|Bob"]);
        assert!(matches!(rename_tag(&mut cat, "Holidays", "People|Holidays", false), Err(_)), "not a person");
        assert!(rename_tag(&mut cat, "nothing", "x", false).is_err());

        // A person keeps their place, and is renamed as a person (faces follow).
        assert_eq!(
            rename_tag(&mut cat, "People|Family|Al", "Alan", false).unwrap(),
            Renamed::Done(Retagged { to: "People|Family|Alan".into(), assets: vec![a] })
        );
        assert_eq!(cat.face_boxes(a).unwrap()[0].person.as_ref().map(|p| p.1.clone()), Some("Alan".into()));
        // Merged by name into another person; a keyword into a person isn't.
        assert!(merge_tags(&mut cat, "Holidays", "People|Bob").is_err());
        assert_eq!(merge_tags(&mut cat, "People|Family|Alan", "bob").unwrap(), Retagged { to: "People|Bob".into(), assets: vec![a] });
        assert_eq!(paths(&cat, a), ["Holidays|1998", "People|Bob"]);
        assert_eq!(cat.face_boxes(a).unwrap()[0].person.as_ref().map(|p| p.1.clone()), Some("Bob".into()));

        assert!(delete_tag(&mut cat, "People").is_err());
        assert!(rename_tag(&mut cat, "People", "Folks", false).is_err());
        assert_eq!(delete_tag(&mut cat, "people|bob").unwrap(), [a, b]);
        assert_eq!(paths(&cat, a), ["Holidays|1998"]);
        assert!(cat.face_boxes(a).unwrap()[0].person.is_none(), "un-named");
        assert_eq!(tree(&cat), ["Holidays 2", "-1998 2"]);
    }

    #[test]
    fn pairing_by_hand() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        for name in ["photo-0001-front.png", "photo-0001-back.png", "x.png", "y.png"] {
            image::RgbImage::new(4, 4).save(root.join(name)).unwrap();
        }
        std::fs::write(root.join("clip.mp4"), b"").unwrap();
        let mut cat = Catalog::open(&Catalog::default_path(&root), &root).unwrap();
        let [f1, b1, x, y, clip] =
            ["photo-0001-front.png", "photo-0001-back.png", "x.png", "y.png", "clip.mp4"].map(|n| cat.asset(&root.join(n)).unwrap());
        assert_eq!(cat.back_of(f1).unwrap(), Some(b1), "paired by name");

        assert!(pair(&mut cat, x, x).is_err());
        assert!(pair(&mut cat, x, clip).is_err(), "not an image");
        assert_eq!(pair(&mut cat, x, y).unwrap(), [x, y]);
        assert_eq!((cat.back_of(x).unwrap(), cat.front_of(y).unwrap()), (Some(y), Some(x)));
        // Re-pairing replaces both old pairs, and says who lost their other side.
        assert_eq!(pair(&mut cat, f1, y).unwrap(), [f1, y, b1, x]);
        assert_eq!((cat.back_of(f1).unwrap(), cat.front_of(b1).unwrap(), cat.back_of(x).unwrap()), (Some(y), None, None));

        assert_eq!(swap_sides(&mut cat, y).unwrap(), (y, f1));
        assert_eq!((cat.back_of(y).unwrap(), cat.front_of(f1).unwrap()), (Some(f1), Some(y)));
        assert!(swap_sides(&mut cat, x).is_err(), "not paired");

        assert_eq!(unpair(&mut cat, f1).unwrap(), Some(y));
        assert_eq!(unpair(&mut cat, f1).unwrap(), None);
        assert_eq!(cat.back_of(y).unwrap(), None);

        delete(&mut cat, x).unwrap();
        assert!(pair(&mut cat, x, b1).is_err(), "in the trash");
    }

    #[test]
    fn many_at_once() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        for name in ["photo-0001-front.png", "photo-0001-back.png", "x.png", "y.png"] {
            image::RgbImage::new(4, 4).save(root.join(name)).unwrap();
        }
        let mut cat = Catalog::open(&Catalog::default_path(&root), &root).unwrap();
        let [front, back, x, y] = ["photo-0001-front.png", "photo-0001-back.png", "x.png", "y.png"].map(|n| cat.asset(&root.join(n)).unwrap());

        // Tags: all or nothing.
        let tag = add_tag_all(&mut cat, &[x, y, front], "Holidays|1998").unwrap();
        for a in [x, y, front] {
            assert_eq!(cat.tags_of(a).unwrap().iter().map(|t| t.tag).collect::<Vec<_>>(), [tag]);
        }
        assert!(add_tag_all(&mut cat, &[back, AssetId(9999)], "place:Home").is_err());
        assert!(cat.tags_of(back).unwrap().is_empty(), "rolled back");
        let alice = add_tag_all(&mut cat, &[x, y], "People|Alice").unwrap();
        assert_eq!(cat.person("Alice").unwrap(), alice);

        // Delete: each on its own; the back goes with its front, once.
        let done = delete_all(&mut cat, &[front, x, AssetId(9999), back]);
        let counts: Vec<Option<usize>> = done.iter().map(|(_, r)| r.as_ref().ok().map(Vec::len)).collect();
        assert_eq!(counts, [Some(2), Some(1), None, Some(0)]);
        assert!(!root.join("x.png").exists() && !root.join("photo-0001-back.png").exists());
        assert!(root.join("y.png").exists());

        // One undo for everything that went.
        let trashed: Vec<AssetId> = done.iter().filter(|(_, r)| r.as_ref().is_ok_and(|t| !t.is_empty())).map(|(a, _)| *a).collect();
        let restored = restore_all(&mut cat, &trashed);
        assert_eq!(restored.iter().map(|(_, r)| r.as_ref().unwrap().len()).sum::<usize>(), 3);
        assert!(root.join("x.png").exists() && root.join("photo-0001-back.png").exists());
        assert!(!cat.is_trashed(back).unwrap() && !cat.is_trashed(x).unwrap());
        assert_eq!(cat.back_of(front).unwrap(), Some(back), "still a pair");
        let again = restore_all(&mut cat, &[x]);
        assert!(again[0].1.as_ref().unwrap().is_empty(), "not in the trash any more");
    }

    #[test]
    fn a_face_added_by_hand_follows_the_rotation() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        image::RgbImage::from_pixel(40, 20, image::Rgb([200, 150, 120])).save(root.join("a.png")).unwrap();
        let mut cat = Catalog::open(&Catalog::default_path(&root), &root).unwrap();
        let a = cat.asset(&root.join("a.png")).unwrap();
        let face = add_face(&mut cat, a, Region { x: 0.5, y: 0.0, w: 0.25, h: 0.5 }, None).unwrap();
        assert!(cat.face_crop(face).unwrap().is_some_and(|jpeg| !jpeg.is_empty()));
        let boxes = cat.face_boxes(a).unwrap();
        assert_eq!((boxes.len(), boxes[0].manual), (1, true));
        cat.name_faces(&[face], "Alice").unwrap();

        rotate(&cat, a, 90).unwrap();
        let turned = Region { x: 0.5, y: 0.5, w: 0.5, h: 0.25 };
        assert_eq!(cat.face_boxes(a).unwrap()[0].region, turned);
        assert_eq!(cat.tags_of(a).unwrap()[0].region, Some(turned));

        cat.delete_faces(&[face]).unwrap();
        assert!(cat.face_boxes(a).unwrap().is_empty() && cat.tags_of(a).unwrap().is_empty());
        assert!(add_face(&mut cat, a, Region { x: 2.0, y: 0.0, w: 0.1, h: 0.1 }, None).is_err());
    }

    #[test]
    fn rotate_tag_inspect_and_delete() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        image::RgbImage::new(40, 20).save(root.join("a.png")).unwrap();
        let mut cat = Catalog::open(&Catalog::default_path(&root), &root).unwrap();
        pipeline::catch_up(&cat).unwrap();
        let a = cat.find_asset(&root.join("a.png")).unwrap().unwrap();
        let alice = cat.tag("People|Alice", TagKind::Person).unwrap();
        cat.add_tag(a, alice, Some(Region { x: 0.0, y: 0.0, w: 0.5, h: 1.0 })).unwrap();
        cat.add_tag_path(a, "Places|Home", TagKind::Place).unwrap();

        let p = properties(&cat, a).unwrap();
        assert_eq!((p.dimensions, p.format.as_deref(), p.size.is_some()), (Some((40, 20)), Some("Png"), true));
        assert_eq!(p.tags.len(), 2);
        assert_eq!(p.rotation, Rotation::default());

        rotate(&cat, a, 90).unwrap();
        let p = properties(&cat, a).unwrap();
        assert_eq!(p.dimensions, Some((20, 40)));
        assert_eq!((p.rotation.applied_cw, p.rotation.by_user), (Some(90), true));
        assert!(p.stages.iter().any(|s| s.stage == ORIENT && s.current && s.status == RunStatus::User));
        let region = p.tags.iter().find(|t| t.path == "People|Alice").unwrap().region.unwrap();
        assert_eq!(region, Region { x: 0.0, y: 0.0, w: 1.0, h: 0.5 });
        assert!(rotate(&cat, a, 45).is_err());

        assert!(cat.remove_tag_path(a, "places|home").unwrap());
        assert!(!cat.remove_tag_path(a, "Nope").unwrap());
        assert_eq!(properties(&cat, a).unwrap().tags.len(), 1);

        let trashed = delete(&mut cat, a).unwrap();
        assert_eq!(trashed.len(), 1);
        let p = properties(&cat, a).unwrap();
        assert!(p.trashed && p.path == trashed[0].in_trash && !p.missing);
        cat.restore(a).unwrap();
        assert!(!properties(&cat, a).unwrap().trashed);
    }
}
