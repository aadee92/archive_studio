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
//! - Front and back: [`pair`] a back scan with its front by hand, [`unpair`],
//!   [`swap_sides`] when they were paired the wrong way round (see
//!   [`catalog::pairs`](crate::catalog::pairs)).
//! - Faces on one picture: [`add_face`] for one the detector missed (boxed by the user);
//!   [`Catalog::not_person`], [`Catalog::ignore_faces`] and [`Catalog::delete_faces`] for the
//!   ones there.
//! - [`properties`]: everything known about the asset, for a properties dialog.

use std::path::PathBuf;
use std::time::SystemTime;

use image::{ImageDecoder, ImageReader};

use crate::catalog::faces::{self as catalog_faces, FaceId};
use crate::catalog::people::FaceBox;
use crate::catalog::runs::{RunStatus, StageRun};
use crate::catalog::trash::Trashed;
use crate::catalog::people::{display_name, PEOPLE};
use crate::catalog::{AppliedTag, AssetId, Catalog, Region, TagId, TagKind};
use crate::exif::{self, Exif};
use crate::media::{classify, MediaKind};
use crate::pipeline::{self, Models, ORIENT};

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
