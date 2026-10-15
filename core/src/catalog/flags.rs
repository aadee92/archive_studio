//! What a front end shows on a tile at a glance, and filters by: whether an asset has faces
//! (and unknown ones), a back side, a rotation waiting for review, a failed pipeline stage,
//! tags of the user's. Read in bulk, a few queries for the whole library
//! ([`Catalog::asset_flags`]) or for a batch of assets ([`Catalog::flags_of`]), never one
//! asset at a time per tile.
//!
//! - **Faces**: faces that aren't ignored; *unknown*: one of them is not named yet.
//! - **Sides**: the asset is a front with a back ([`pairs`](super::pairs)), or is the back.
//! - **Review** / **failed**: a stage's run on the current file (size and mtime) wants the
//!   user to look (orient: a rotation to accept or keep) / failed
//!   ([`runs`](super::runs)). Runs on an older version of the file don't count.
//! - **Tagged**: the user put a tag on it (naming a face does too: its person tag). Tags a
//!   detector proposed don't count, so an asset without this flag is *untagged*.

use std::collections::HashMap;
use std::fmt;
use std::ops::{BitOr, BitOrAssign};
use std::path::PathBuf;

use super::runs::FINGERPRINT;
use super::{AssetId, Catalog, Result};

/// A set of [`AssetFlags`] constants.
#[derive(Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct AssetFlags(u8);

impl AssetFlags {
    pub const NONE: Self = Self(0);
    /// Has faces (not counting ignored ones).
    pub const FACES: Self = Self(1);
    /// Has a face nobody has named yet (ignored ones aside).
    pub const UNKNOWN_FACES: Self = Self(1 << 1);
    /// A front with a back.
    pub const HAS_BACK: Self = Self(1 << 2);
    /// The back of a front.
    pub const IS_BACK: Self = Self(1 << 3);
    /// A stage wants the user to look at the current file (a rotation to review).
    pub const REVIEW: Self = Self(1 << 4);
    /// A stage failed on the current file.
    pub const FAILED: Self = Self(1 << 5);
    /// Has a tag the user put on it (people named on it included).
    pub const TAGGED: Self = Self(1 << 6);

    /// Each flag with its name, for display and parsing.
    pub const NAMED: [(Self, &'static str); 7] = [
        (Self::FACES, "faces"),
        (Self::UNKNOWN_FACES, "unknown-faces"),
        (Self::HAS_BACK, "has-back"),
        (Self::IS_BACK, "is-back"),
        (Self::REVIEW, "review"),
        (Self::FAILED, "failed"),
        (Self::TAGGED, "tagged"),
    ];

    pub const fn bits(self) -> u8 {
        self.0
    }

    pub const fn from_bits(bits: u8) -> Self {
        Self(bits & 0x7f)
    }

    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// Has every flag of `other`.
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    /// Has any flag of `other`.
    pub const fn intersects(self, other: Self) -> bool {
        self.0 & other.0 != 0
    }

    /// Has all of `with` and none of `without`.
    pub const fn matches(self, with: Self, without: Self) -> bool {
        self.contains(with) && !self.intersects(without)
    }
}

impl BitOr for AssetFlags {
    type Output = Self;
    fn bitor(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }
}

impl BitOrAssign for AssetFlags {
    fn bitor_assign(&mut self, other: Self) {
        self.0 |= other.0;
    }
}

impl fmt::Debug for AssetFlags {
    /// `faces|unknown-faces`, or `none`.
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let names: Vec<&str> = Self::NAMED.iter().filter(|(flag, _)| self.contains(*flag)).map(|(_, name)| *name).collect();
        f.write_str(&if names.is_empty() { "none".to_owned() } else { names.join("|") })
    }
}

/// How many present assets need the user, for a status line; the same as counting the
/// [`AssetFlags::REVIEW`] and [`AssetFlags::FAILED`] flags.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AttentionCounts {
    pub review: usize,
    pub failed: usize,
}

/// Asset ids per `IN (…)` list in [`Catalog::flags_of`].
const CHUNK: usize = 500;

impl Catalog {
    /// The flags of every asset whose file is in the library (not missing, not in the
    /// trash), by absolute path; assets without any flag are included, as
    /// [`AssetFlags::NONE`]. A handful of scans however large the library.
    pub fn asset_flags(&self) -> Result<HashMap<PathBuf, AssetFlags>> {
        let mut flags = self.collect_flags(None)?;
        let mut stmt = self.conn.prepare_cached("SELECT id, path FROM assets WHERE NOT missing AND trashed_from IS NULL")?;
        let rows = stmt.query_map([], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)))?;
        let mut out = HashMap::new();
        for row in rows {
            let (id, rel) = row?;
            out.insert(self.root.join(rel), flags.remove(&id).unwrap_or_default());
        }
        Ok(out)
    }

    /// The flags of each of `assets` (indexed lookups, in chunks), for refreshing a batch of
    /// files. Assets without any flag (or unknown) are left out.
    pub fn flags_of(&self, assets: &[AssetId]) -> Result<HashMap<AssetId, AssetFlags>> {
        let mut out = HashMap::new();
        for chunk in assets.chunks(CHUNK) {
            let ids: Vec<String> = chunk.iter().map(|a| a.0.to_string()).collect();
            let found = self.collect_flags(Some(&ids.join(",")))?;
            out.extend(chunk.iter().filter_map(|a| found.get(&a.0).map(|&f| (*a, f))));
        }
        Ok(out)
    }

    /// The assets in the library with every flag of `with` and none of `without`, by path
    /// (e.g. `with` [`FAILED`](AssetFlags::FAILED); `without` [`TAGGED`](AssetFlags::TAGGED)
    /// for the untagged ones).
    pub fn assets_flagged(&self, with: AssetFlags, without: AssetFlags) -> Result<Vec<PathBuf>> {
        let mut paths: Vec<PathBuf> =
            self.asset_flags()?.into_iter().filter(|(_, flags)| flags.matches(with, without)).map(|(path, _)| path).collect();
        paths.sort();
        Ok(paths)
    }

    /// How many assets in the library want a review / had a stage fail, on their current
    /// file. Cheap enough for a status line updated while the pipeline runs.
    pub fn attention_counts(&self) -> Result<AttentionCounts> {
        let sql = format!(
            "SELECT count(DISTINCT CASE WHEN r.status = 'review' THEN r.asset_id END),
                    count(DISTINCT CASE WHEN r.status = 'failed' THEN r.asset_id END)
             FROM stage_runs r JOIN assets a ON a.id = r.asset_id
             WHERE r.status IN ('review', 'failed') AND NOT a.missing AND a.trashed_from IS NULL AND r.input = {FINGERPRINT}"
        );
        let (review, failed): (i64, i64) = self.conn.prepare_cached(&sql)?.query_row([], |row| Ok((row.get(0)?, row.get(1)?)))?;
        Ok(AttentionCounts { review: review as usize, failed: failed as usize })
    }

    /// Flags by asset id, of every asset or of those in `ids` (a comma-separated list of
    /// ids). Assets with no flag are left out.
    fn collect_flags(&self, ids: Option<&str>) -> Result<HashMap<i64, AssetFlags>> {
        let only = |column: &str| ids.map_or(String::new(), |ids| format!(" AND {column} IN ({ids})"));
        let mut flags: HashMap<i64, AssetFlags> = HashMap::new();

        let sql = format!("SELECT asset_id, max(person IS NULL) FROM faces WHERE NOT ignored{} GROUP BY asset_id", only("asset_id"));
        let mut stmt = self.conn.prepare(&sql)?;
        for row in stmt.query_map([], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, bool>(1)?)))? {
            let (id, unknown) = row?;
            *flags.entry(id).or_default() |= if unknown { AssetFlags::FACES | AssetFlags::UNKNOWN_FACES } else { AssetFlags::FACES };
        }

        let pairs = ids.map_or(String::new(), |ids| format!(" WHERE front IN ({ids}) OR back IN ({ids})"));
        let mut stmt = self.conn.prepare(&format!("SELECT front, back FROM asset_pairs{pairs}"))?;
        for row in stmt.query_map([], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)))? {
            let (front, back) = row?;
            *flags.entry(front).or_default() |= AssetFlags::HAS_BACK;
            *flags.entry(back).or_default() |= AssetFlags::IS_BACK;
        }

        let sql = format!(
            "SELECT r.asset_id, max(r.status = 'review'), max(r.status = 'failed')
             FROM stage_runs r JOIN assets a ON a.id = r.asset_id
             WHERE r.status IN ('review', 'failed') AND NOT a.missing AND r.input = {FINGERPRINT}{}
             GROUP BY r.asset_id",
            only("r.asset_id")
        );
        let mut stmt = self.conn.prepare(&sql)?;
        for row in stmt.query_map([], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, bool>(1)?, row.get::<_, bool>(2)?)))? {
            let (id, review, failed) = row?;
            let entry = flags.entry(id).or_default();
            if review {
                *entry |= AssetFlags::REVIEW;
            }
            if failed {
                *entry |= AssetFlags::FAILED;
            }
        }

        let sql = format!("SELECT DISTINCT asset_id FROM asset_tags WHERE source = 'user' AND NOT hidden{}", only("asset_id"));
        let mut stmt = self.conn.prepare(&sql)?;
        for row in stmt.query_map([], |row| row.get::<_, i64>(0))? {
            *flags.entry(row?).or_default() |= AssetFlags::TAGGED;
        }
        Ok(flags)
    }
}

#[cfg(test)]
mod tests {
    use faces::Embedding;

    use super::super::faces::{FaceFilter, FaceOrder, NewFace, MODEL};
    use super::super::runs::{Run, RunStatus};
    use super::super::{Detection, Region, TagKind};
    use super::*;
    use crate::media::MediaKind;

    fn face(x: f32) -> NewFace {
        NewFace { region: Region { x, y: 0.1, w: 0.1, h: 0.1 }, score: 0.9, embedding: Embedding::new(vec![1.0, 0.0]), crop: vec![0xff, 0xd8] }
    }

    fn run(cat: &Catalog, asset: AssetId, stage: &str, input: &str, status: RunStatus) {
        let run = Run { stage, version: "v1", input, status, value: None, detail: "" };
        cat.set_run(asset, &run).unwrap();
    }

    #[test]
    fn flags_in_bulk_and_by_asset() {
        let dir = tempfile::tempdir().unwrap();
        let mut cat = Catalog::open_in_memory(dir.path());
        let path = |name: &str| dir.path().join(name);
        let seen = |name: &str| cat.record_file(&path(name), MediaKind::Image, 10, 20).unwrap().0;

        let plain = seen("plain.jpg");
        let named = seen("named.jpg");
        let unknown = seen("unknown.jpg");
        let ignored = seen("ignored.jpg");
        // Paired by name.
        let front = seen("photo-front.png");
        let back = seen("photo-back.png");
        let review = seen("review.jpg");
        let failed = seen("failed.jpg");
        let stale = seen("stale.jpg");
        let detected = seen("detected.jpg");
        let tagged = seen("tagged.jpg");
        let gone = seen("gone.jpg");

        cat.set_faces(named, MODEL, &[face(0.1), face(0.4)]).unwrap();
        let faces: Vec<_> = cat.faces(FaceFilter::Asset(named), FaceOrder::Asset).unwrap().into_iter().map(|f| f.id).collect();
        cat.name_faces(&faces, "Ann").unwrap();
        cat.set_faces(unknown, MODEL, &[face(0.1)]).unwrap();
        cat.set_faces(ignored, MODEL, &[face(0.1)]).unwrap();
        let faces: Vec<_> = cat.faces(FaceFilter::Asset(ignored), FaceOrder::Asset).unwrap().into_iter().map(|f| f.id).collect();
        cat.ignore_faces(&faces).unwrap();

        run(&cat, review, "orient", "10:20", RunStatus::Review);
        run(&cat, failed, "orient", "10:20", RunStatus::Done);
        run(&cat, failed, "faces", "10:20", RunStatus::Failed);
        // On an older version of the file.
        run(&cat, stale, "faces", "9:9", RunStatus::Failed);
        run(&cat, gone, "faces", "10:20", RunStatus::Failed);
        cat.set_missing(gone, true).unwrap();

        let scene = cat.tag("Scenes|Beach", TagKind::Scene).unwrap();
        cat.set_detections(detected, "clip", &[Detection { tag: scene, confidence: 0.9, region: None }]).unwrap();
        cat.add_tag_path(tagged, "Trips|Rome", TagKind::Keyword).unwrap();

        let all = cat.asset_flags().unwrap();
        let of = |name: &str| all[&path(name)];
        assert_eq!(of("plain.jpg"), AssetFlags::NONE);
        assert_eq!(of("named.jpg"), AssetFlags::FACES | AssetFlags::TAGGED, "named faces tag the picture");
        assert_eq!(of("unknown.jpg"), AssetFlags::FACES | AssetFlags::UNKNOWN_FACES);
        assert_eq!(of("ignored.jpg"), AssetFlags::NONE, "ignored faces don't count");
        assert_eq!(of("photo-front.png"), AssetFlags::HAS_BACK);
        assert_eq!(of("photo-back.png"), AssetFlags::IS_BACK);
        assert_eq!(of("review.jpg"), AssetFlags::REVIEW);
        assert_eq!(of("failed.jpg"), AssetFlags::FAILED);
        assert_eq!(of("stale.jpg"), AssetFlags::NONE, "a failure on an older file doesn't count");
        assert_eq!(of("detected.jpg"), AssetFlags::NONE, "detector tags aren't the user's");
        assert_eq!(of("tagged.jpg"), AssetFlags::TAGGED);
        assert!(!all.contains_key(&path("gone.jpg")), "missing files are left out");
        assert_eq!(all.len(), 11);

        let some = cat.flags_of(&[plain, named, unknown, back, review, failed, tagged, stale, front]).unwrap();
        for (asset, name) in [(named, "named.jpg"), (unknown, "unknown.jpg"), (back, "photo-back.png"), (review, "review.jpg"), (failed, "failed.jpg"), (tagged, "tagged.jpg"), (front, "photo-front.png")] {
            assert_eq!(some.get(&asset).copied(), Some(of(name)), "{name}");
        }
        assert!(!some.contains_key(&plain) && !some.contains_key(&stale), "no flags, left out");
        assert!(!cat.flags_of(&[back]).unwrap()[&back].contains(AssetFlags::HAS_BACK), "only the asset's own flags");
        let many: Vec<AssetId> = (0..1200).map(|i| AssetId(i % 20 + 1)).collect();
        assert_eq!(cat.flags_of(&many).unwrap().len(), some.len(), "chunked");

        let names = |with, without| -> Vec<String> {
            let paths = cat.assets_flagged(with, without).unwrap();
            paths.iter().map(|p| p.file_name().unwrap().to_string_lossy().into_owned()).collect()
        };
        assert_eq!(names(AssetFlags::UNKNOWN_FACES, AssetFlags::NONE), ["unknown.jpg"]);
        assert_eq!(names(AssetFlags::FACES, AssetFlags::TAGGED), ["unknown.jpg"]);
        assert_eq!(names(AssetFlags::NONE, AssetFlags::TAGGED).len(), 9);
        assert_eq!(cat.attention_counts().unwrap(), AttentionCounts { review: 1, failed: 1 });
        assert_eq!(format!("{:?}", of("unknown.jpg")), "faces|unknown-faces");
    }
}
