//! What a front end does to one asset from a context menu: rotate, delete (to the trash),
//! tag, and show its properties. Each works on a [`Catalog`] alone, with or without a
//! pipeline [`Engine`](crate::pipeline::Engine) running on the library.
//!
//! - [`rotate`]: the user's rotation, final like every user decision (the orientation model
//!   never redoes it). With an engine at hand, [`Engine::rotate`](crate::pipeline::Engine::rotate)
//!   does the same on the engine thread.
//! - [`delete`]: to the library's trash, reversible with [`Catalog::restore`] (see
//!   [`catalog::trash`](crate::catalog::trash)); a front takes its back with it.
//! - Tags: [`Catalog::add_tag_path`], [`Catalog::remove_tag_path`]; people:
//!   [`Catalog::name_faces`] and the rest of [`catalog::people`](crate::catalog::people).
//! - [`properties`]: everything known about the asset, for a properties dialog.

use std::path::PathBuf;
use std::time::SystemTime;

use image::{ImageDecoder, ImageReader};

use crate::catalog::people::FaceBox;
use crate::catalog::runs::{RunStatus, StageRun};
use crate::catalog::trash::Trashed;
use crate::catalog::{AppliedTag, AssetId, Catalog};
use crate::exif::{self, Exif};
use crate::media::{classify, MediaKind};
use crate::pipeline::{self, ORIENT};

/// Turns the asset's image `cw` degrees clockwise (90, 180 or 270), rewriting the file
/// losslessly; see [`pipeline::rotate_file`].
pub fn rotate(catalog: &Catalog, asset: AssetId, cw: u16) -> Result<(), String> {
    let path = catalog.asset_path(asset).map_err(|e| e.to_string())?;
    pipeline::rotate_file(catalog, &path, cw)
}

/// Moves the asset (and its back, if it is a front) to the library's trash; see
/// [`Catalog::trash`]. Undo with [`Catalog::restore`].
pub fn delete(catalog: &mut Catalog, asset: AssetId) -> crate::catalog::Result<Vec<Trashed>> {
    catalog.trash(asset)
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
