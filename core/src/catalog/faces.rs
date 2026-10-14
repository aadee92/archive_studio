//! Faces found in the library's images, and the review that names them.
//!
//! The workflow:
//!
//! 1. **Find.** [`scan_image`] detects and embeds every face in a file (it needs no catalog,
//!    so a host can run it on worker threads) and [`Catalog::set_faces`] stores the result:
//!    box, detector score, embedding and a small crop for display.
//! 2. **Group and suggest.** Before anyone is named, [`Catalog::cluster_faces`] groups the
//!    unreviewed faces by likeness. Once some are confirmed, [`Catalog::refresh_suggestions`]
//!    proposes a person for each unreviewed face: the person owning the most similar
//!    *confirmed face*, not the most similar average, so a child photo is matched against
//!    confirmed child photos rather than a blur of every age.
//! 3. **Review.** [`Catalog::faces`] lists a selection (one person's suggestions, a cluster, …)
//!    in [`FaceOrder::Similarity`], a path where each face resembles the one before, so a
//!    run of faces ages gradually instead of jumping from child to grandparent. The user
//!    [confirms](Catalog::confirm_faces), [rejects a suggestion](Catalog::reject_suggestions),
//!    or [ignores](Catalog::ignore_faces) faces, in bulk.
//!
//! A confirmed face also puts its person tag, with the face's region, on the asset as a user
//! tag, so searches and XMP sidecars (as face regions) see it like any other tag.

use std::collections::{HashMap, HashSet};
use std::io::Cursor;
use std::path::Path;

use faces::{cluster, order, Embedding, Faces};
use image::{codecs::jpeg::JpegEncoder, ImageDecoder, ImageReader, RgbImage};
use rusqlite::{params, OptionalExtension, Transaction};

use super::{AssetId, Catalog, Error, Region, Result, TagId, TagKind, USER};

pub(super) const SCHEMA: &str = "
CREATE TABLE faces (
    id INTEGER PRIMARY KEY,
    asset_id INTEGER NOT NULL REFERENCES assets(id) ON DELETE CASCADE,
    -- Face box: top-left corner and size, as fractions of the orientation-corrected image.
    x REAL NOT NULL, y REAL NOT NULL, w REAL NOT NULL, h REAL NOT NULL,
    -- Detector score in 0..=1.
    score REAL NOT NULL,
    -- Unit-length embedding, little-endian f32s.
    embedding BLOB NOT NULL,
    -- The user's ruling: who this is, or not worth naming (not a face, a stranger). Neither
    -- means unreviewed.
    person INTEGER REFERENCES tags(id) ON DELETE SET NULL,
    ignored INTEGER NOT NULL DEFAULT 0,
    -- For unreviewed faces: the person owning the most similar confirmed face, and that
    -- cosine similarity. Rewritten by refresh_suggestions.
    suggested INTEGER REFERENCES tags(id) ON DELETE SET NULL,
    similarity REAL,
    -- Group from the last clustering of unreviewed faces.
    cluster INTEGER
);
CREATE INDEX faces_by_asset ON faces (asset_id);
CREATE INDEX faces_by_person ON faces (person) WHERE person IS NOT NULL;
CREATE INDEX faces_by_suggestion ON faces (suggested, similarity) WHERE person IS NULL AND NOT ignored;
CREATE INDEX faces_by_cluster ON faces (cluster) WHERE cluster IS NOT NULL;

-- Small JPEG of each face with some context, for review grids. Kept apart from faces so
-- scans of faces stay small.
CREATE TABLE face_crops (
    face_id INTEGER PRIMARY KEY REFERENCES faces(id) ON DELETE CASCADE,
    jpeg BLOB NOT NULL
);

-- People the user said a face is not, so suggestions don't offer them again.
CREATE TABLE face_rejections (
    face_id INTEGER NOT NULL REFERENCES faces(id) ON DELETE CASCADE,
    tag_id INTEGER NOT NULL REFERENCES tags(id) ON DELETE CASCADE,
    PRIMARY KEY (face_id, tag_id)
) WITHOUT ROWID;

-- Assets scanned for faces, and with which model, so a scan finding none isn't repeated.
CREATE TABLE face_scans (
    asset_id INTEGER PRIMARY KEY REFERENCES assets(id) ON DELETE CASCADE,
    model TEXT NOT NULL
);
";

/// The name [`scan_image`]'s faces are stored under: InsightFace's `buffalo_l` models.
pub const MODEL: &str = "buffalo_l";

/// Cosine similarity a face needs to its nearest confirmed face to be suggested.
pub const DEFAULT_MIN_SIMILARITY: f32 = 0.4;

/// New faces overlapping a reviewed face by more than this are the same face found again.
const SAME_FACE_IOU: f32 = 0.5;
/// Crops show this much more than the face box, to give some context.
const CROP_CONTEXT: f32 = 1.6;
const CROP_EDGE: u32 = 160;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct FaceId(pub i64);

/// A face found by [`scan_image`], for [`Catalog::set_faces`].
#[derive(Clone, Debug)]
pub struct NewFace {
    pub region: Region,
    pub score: f32,
    pub embedding: Embedding,
    /// JPEG.
    pub crop: Vec<u8>,
}

/// A stored face.
#[derive(Clone, Debug, PartialEq)]
pub struct Face {
    pub id: FaceId,
    pub asset: AssetId,
    pub region: Region,
    pub score: f32,
    pub status: FaceStatus,
    /// For unreviewed faces: the suggested person and the similarity behind it.
    pub suggestion: Option<(TagId, f32)>,
    pub cluster: Option<i64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FaceStatus {
    Unreviewed,
    Confirmed(TagId),
    Ignored,
}

/// Which faces [`Catalog::faces`] lists.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FaceFilter {
    All,
    Asset(AssetId),
    Unreviewed,
    /// Unreviewed with no suggestion.
    Unknown,
    /// Unreviewed and suggested as this person.
    Suggested(TagId),
    /// Confirmed as this person.
    Confirmed(TagId),
    /// Unreviewed and in this cluster.
    Cluster(i64),
    Ignored,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FaceOrder {
    /// Each face resembles the one before ([`order::similarity_path`]).
    Similarity,
    /// Most similar suggestion first, then by detector score.
    Confidence,
    /// By asset path, then left to right.
    Asset,
}

/// A person with faces, for a people list.
#[derive(Clone, Debug, PartialEq)]
pub struct PersonFaces {
    pub tag: TagId,
    pub path: String,
    pub confirmed: usize,
    /// Unreviewed faces suggested as this person.
    pub suggested: usize,
}

/// Detects and embeds the faces in the image at `path`, highest score first, on the
/// orientation-corrected image. Needs no catalog: run it on as many threads as there are
/// cores to spare (with [`faces::set_threads`] left at 1) and hand the results to one writer.
pub fn scan_image(models: &Faces, path: &Path) -> std::result::Result<Vec<NewFace>, String> {
    let fail = |e: &dyn std::fmt::Display| format!("{}: {e}", path.display());
    let mut decoder = ImageReader::open(path)
        .map_err(|e| fail(&e))?
        .with_guessed_format()
        .map_err(|e| fail(&e))?
        .into_decoder()
        .map_err(|e| fail(&e))?;
    let orientation = decoder.orientation().map_err(|e| fail(&e))?;
    let mut image = image::DynamicImage::from_decoder(decoder).map_err(|e| fail(&e))?;
    image.apply_orientation(orientation);
    let image = image.to_rgb8();
    let (iw, ih) = (image.width() as f32, image.height() as f32);

    let found = models.analyze(&image).map_err(|e| fail(&e))?;
    found
        .into_iter()
        .map(|f| {
            let [x1, y1, x2, y2] = f.detection.bbox;
            let (x1, y1) = (x1.clamp(0.0, iw), y1.clamp(0.0, ih));
            let (x2, y2) = (x2.clamp(x1, iw), y2.clamp(y1, ih));
            Ok(NewFace {
                region: Region { x: x1 / iw, y: y1 / ih, w: (x2 - x1) / iw, h: (y2 - y1) / ih },
                score: f.detection.score.clamp(0.0, 1.0),
                embedding: f.embedding,
                crop: crop_jpeg(&image, [x1, y1, x2, y2]).map_err(|e| fail(&e))?,
            })
        })
        .collect()
}

/// A square around the box, [`CROP_CONTEXT`] times its larger side and kept inside the
/// image, scaled to [`CROP_EDGE`].
fn crop_jpeg(image: &RgbImage, [x1, y1, x2, y2]: [f32; 4]) -> image::ImageResult<Vec<u8>> {
    let (iw, ih) = image.dimensions();
    let side = ((x2 - x1).max(y2 - y1) * CROP_CONTEXT).max(1.0).min(iw.min(ih) as f32);
    let (cx, cy) = ((x1 + x2) / 2.0, (y1 + y2) / 2.0);
    let left = (cx - side / 2.0).clamp(0.0, iw as f32 - side) as u32;
    let top = (cy - side / 2.0).clamp(0.0, ih as f32 - side) as u32;
    let side = side as u32;
    let crop = image::imageops::crop_imm(image, left, top, side, side).to_image();
    let crop = crate::resize::fit(&crop, CROP_EDGE);
    let mut jpeg = Vec::new();
    JpegEncoder::new_with_quality(Cursor::new(&mut jpeg), 85).encode_image(&crop)?;
    Ok(jpeg)
}

impl Catalog {
    /// Whether `asset` was scanned for faces with `model`.
    pub fn face_scanned(&self, asset: AssetId, model: &str) -> Result<bool> {
        let mut stmt = self.conn.prepare_cached("SELECT 1 FROM face_scans WHERE asset_id = ?1 AND model = ?2")?;
        Ok(stmt.exists(params![asset.0, model])?)
    }

    /// Stores what a scan of `asset` with `model` found. Rescanning replaces the unreviewed
    /// faces and keeps the reviewed ones; a new face overlapping a reviewed one is that face
    /// found again and is dropped.
    pub fn set_faces(&mut self, asset: AssetId, model: &str, found: &[NewFace]) -> Result<()> {
        let tx = self.conn.transaction()?;
        tx.execute("DELETE FROM faces WHERE asset_id = ?1 AND person IS NULL AND NOT ignored", [asset.0])?;
        let reviewed: Vec<Region> = {
            let mut stmt = tx.prepare("SELECT x, y, w, h FROM faces WHERE asset_id = ?1")?;
            let rows = stmt.query_map([asset.0], |row| Ok(Region { x: row.get(0)?, y: row.get(1)?, w: row.get(2)?, h: row.get(3)? }))?;
            rows.collect::<rusqlite::Result<_>>()?
        };
        {
            let mut face = tx.prepare(
                "INSERT INTO faces (asset_id, x, y, w, h, score, embedding) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            )?;
            let mut crop = tx.prepare("INSERT INTO face_crops (face_id, jpeg) VALUES (?1, ?2)")?;
            for f in found.iter().filter(|f| reviewed.iter().all(|r| iou(r, &f.region) <= SAME_FACE_IOU)) {
                let r = f.region;
                face.execute(params![asset.0, r.x, r.y, r.w, r.h, f.score, f.embedding.to_bytes()])?;
                crop.execute(params![tx.last_insert_rowid(), f.crop])?;
            }
        }
        tx.execute(
            "INSERT INTO face_scans (asset_id, model) VALUES (?1, ?2) ON CONFLICT DO UPDATE SET model = excluded.model",
            params![asset.0, model],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// The faces `filter` selects, in `order`.
    pub fn faces(&self, filter: FaceFilter, order: FaceOrder) -> Result<Vec<Face>> {
        const UNREVIEWED: &str = "person IS NULL AND NOT ignored";
        let (condition, arg): (String, Option<i64>) = match filter {
            FaceFilter::All => ("1".into(), None),
            FaceFilter::Asset(a) => ("asset_id = ?1".into(), Some(a.0)),
            FaceFilter::Unreviewed => (UNREVIEWED.into(), None),
            FaceFilter::Unknown => (format!("{UNREVIEWED} AND suggested IS NULL"), None),
            FaceFilter::Suggested(t) => (format!("{UNREVIEWED} AND suggested = ?1"), Some(t.0)),
            FaceFilter::Confirmed(t) => ("person = ?1".into(), Some(t.0)),
            FaceFilter::Cluster(c) => (format!("{UNREVIEWED} AND cluster = ?1"), Some(c)),
            FaceFilter::Ignored => ("ignored".into(), None),
        };
        let sort = match order {
            FaceOrder::Confidence => "ORDER BY f.similarity IS NULL, f.similarity DESC, f.score DESC, f.id",
            FaceOrder::Asset | FaceOrder::Similarity => "ORDER BY a.path, f.x, f.id",
        };
        let sql = format!(
            "SELECT f.id, f.asset_id, f.x, f.y, f.w, f.h, f.score, f.person, f.ignored, f.suggested, f.similarity, f.cluster, f.embedding
             FROM faces f JOIN assets a ON a.id = f.asset_id WHERE {condition} {sort}"
        );
        let mut stmt = self.conn.prepare_cached(&sql)?;
        let params = rusqlite::params_from_iter(arg);
        let rows = stmt.query_map(params, |row| {
            let person: Option<i64> = row.get(7)?;
            let status = match (person, row.get::<_, bool>(8)?) {
                (Some(p), _) => FaceStatus::Confirmed(TagId(p)),
                (None, true) => FaceStatus::Ignored,
                (None, false) => FaceStatus::Unreviewed,
            };
            let suggestion = match (row.get::<_, Option<i64>>(9)?, row.get::<_, Option<f32>>(10)?) {
                (Some(t), Some(s)) if status == FaceStatus::Unreviewed => Some((TagId(t), s)),
                _ => None,
            };
            let face = Face {
                id: FaceId(row.get(0)?),
                asset: AssetId(row.get(1)?),
                region: Region { x: row.get(2)?, y: row.get(3)?, w: row.get(4)?, h: row.get(5)? },
                score: row.get(6)?,
                status,
                suggestion,
                cluster: row.get(11)?,
            };
            Ok((face, embedding(row.get_ref(12)?.as_blob()?)))
        })?;
        let (faces, embeddings): (Vec<Face>, Vec<Embedding>) = rows.collect::<rusqlite::Result<Vec<_>>>()?.into_iter().unzip();
        Ok(match order {
            FaceOrder::Similarity => {
                let mut slots: Vec<Option<Face>> = faces.into_iter().map(Some).collect();
                order::similarity_path(&embeddings).into_iter().map(|i| slots[i].take().unwrap()).collect()
            }
            _ => faces,
        })
    }

    /// The face's crop (JPEG), if it has one.
    pub fn face_crop(&self, face: FaceId) -> Result<Option<Vec<u8>>> {
        let mut stmt = self.conn.prepare_cached("SELECT jpeg FROM face_crops WHERE face_id = ?1")?;
        Ok(stmt.query_row([face.0], |row| row.get(0)).optional()?)
    }

    /// Every person tag with confirmed or suggested faces, by path.
    pub fn people_with_faces(&self) -> Result<Vec<PersonFaces>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT g.id, g.path,
                 (SELECT count(*) FROM faces WHERE person = g.id),
                 (SELECT count(*) FROM faces WHERE suggested = g.id AND person IS NULL AND NOT ignored)
             FROM tags g WHERE g.kind = 'person'
                 AND (EXISTS (SELECT 1 FROM faces WHERE person = g.id)
                      OR EXISTS (SELECT 1 FROM faces WHERE suggested = g.id AND person IS NULL AND NOT ignored))
             ORDER BY g.path COLLATE NOCASE",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(PersonFaces {
                tag: TagId(row.get(0)?),
                path: row.get(1)?,
                confirmed: row.get::<_, i64>(2)? as usize,
                suggested: row.get::<_, i64>(3)? as usize,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// The clusters from the last [`cluster_faces`](Self::cluster_faces), with how many
    /// unreviewed faces each still has, largest first.
    pub fn face_clusters(&self) -> Result<Vec<(i64, usize)>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT cluster, count(*) AS n FROM faces WHERE cluster IS NOT NULL AND person IS NULL AND NOT ignored
             GROUP BY cluster ORDER BY n DESC, cluster",
        )?;
        let rows = stmt.query_map([], |row| Ok((row.get(0)?, row.get::<_, i64>(1)? as usize)))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// The user says these faces are `person` (a person tag). Each face's region goes on its
    /// asset as a user tag.
    pub fn confirm_faces(&mut self, faces: &[FaceId], person: TagId) -> Result<()> {
        let kind: String = self.conn.query_row("SELECT kind FROM tags WHERE id = ?1", [person.0], |row| row.get(0))?;
        if kind != TagKind::Person.name() {
            return Err(Error::Invalid(format!("tag {} is a {kind}, not a person", person.0)));
        }
        let tx = self.conn.transaction()?;
        for &face in faces {
            release(&tx, face, Some(person))?;
            tx.execute(
                "UPDATE faces SET person = ?2, ignored = 0, suggested = NULL, similarity = NULL, cluster = NULL WHERE id = ?1",
                [face.0, person.0],
            )?;
            tx.execute("DELETE FROM face_rejections WHERE face_id = ?1 AND tag_id = ?2", [face.0, person.0])?;
            tx.execute(
                "INSERT INTO asset_tags (asset_id, tag_id, source, hidden, x, y, w, h)
                     SELECT asset_id, ?2, ?3, 0, x, y, w, h FROM faces WHERE id = ?1
                 ON CONFLICT DO UPDATE SET hidden = 0, x = excluded.x, y = excluded.y, w = excluded.w, h = excluded.h",
                params![face.0, person.0, USER],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// The user says these faces are not who they were suggested as. The suggestion is
    /// dropped and not offered again; the next [`refresh_suggestions`](Self::refresh_suggestions)
    /// may offer someone else.
    pub fn reject_suggestions(&mut self, faces: &[FaceId]) -> Result<()> {
        let tx = self.conn.transaction()?;
        for &face in faces {
            tx.execute(
                "INSERT OR IGNORE INTO face_rejections (face_id, tag_id)
                     SELECT id, suggested FROM faces WHERE id = ?1 AND suggested IS NOT NULL",
                [face.0],
            )?;
            tx.execute("UPDATE faces SET suggested = NULL, similarity = NULL WHERE id = ?1", [face.0])?;
        }
        tx.commit()?;
        Ok(())
    }

    /// The user says these faces aren't worth naming (not a face, a stranger in a crowd).
    pub fn ignore_faces(&mut self, faces: &[FaceId]) -> Result<()> {
        self.set_unnamed(faces, true)
    }

    /// Undoes a confirmation or an ignore: the faces are unreviewed again.
    pub fn unreview_faces(&mut self, faces: &[FaceId]) -> Result<()> {
        self.set_unnamed(faces, false)
    }

    fn set_unnamed(&mut self, faces: &[FaceId], ignored: bool) -> Result<()> {
        let tx = self.conn.transaction()?;
        for &face in faces {
            release(&tx, face, None)?;
            tx.execute(
                "UPDATE faces SET person = NULL, ignored = ?2, suggested = NULL, similarity = NULL, cluster = NULL WHERE id = ?1",
                params![face.0, ignored],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Suggests a person for every unreviewed face: the person owning its most similar
    /// confirmed face, if that similarity is at least `min_similarity` (see
    /// [`DEFAULT_MIN_SIMILARITY`]) and the user hasn't rejected that person for the face.
    /// Returns how many faces have a suggestion.
    pub fn refresh_suggestions(&mut self, min_similarity: f32) -> Result<usize> {
        let confirmed: Vec<(i64, Embedding)> = self.load_embeddings("SELECT person, embedding FROM faces WHERE person IS NOT NULL")?;
        let unreviewed: Vec<(i64, Embedding)> =
            self.load_embeddings("SELECT id, embedding FROM faces WHERE person IS NULL AND NOT ignored")?;
        let mut rejected: HashMap<i64, HashSet<i64>> = HashMap::new();
        {
            let mut stmt = self.conn.prepare("SELECT face_id, tag_id FROM face_rejections")?;
            let rows = stmt.query_map([], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)))?;
            for row in rows {
                let (face, tag) = row?;
                rejected.entry(face).or_default().insert(tag);
            }
        }

        let best = |(face, e): &(i64, Embedding)| -> Option<(i64, f32)> {
            let no = rejected.get(face);
            confirmed
                .iter()
                .filter(|(person, _)| no.is_none_or(|no| !no.contains(person)))
                .map(|(person, c)| (*person, e.similarity(c)))
                .max_by(|a, b| a.1.total_cmp(&b.1))
                .filter(|&(_, s)| s >= min_similarity)
        };
        let suggestions = parallel_map(&unreviewed, best);

        let tx = self.conn.transaction()?;
        {
            let mut update = tx.prepare_cached("UPDATE faces SET suggested = ?2, similarity = ?3 WHERE id = ?1")?;
            for ((face, _), s) in unreviewed.iter().zip(&suggestions) {
                update.execute(params![face, s.map(|s| s.0), s.map(|s| s.1)])?;
            }
        }
        tx.commit()?;
        Ok(suggestions.iter().flatten().count())
    }

    /// Groups the unreviewed faces by likeness ([`cluster::cluster`]), replacing the previous
    /// clusters. For naming people before anyone is confirmed, and finding people nobody has
    /// named yet. Returns the number of clusters.
    pub fn cluster_faces(&mut self, params: &cluster::Params) -> Result<usize> {
        let unreviewed = self.load_embeddings("SELECT id, embedding FROM faces WHERE person IS NULL AND NOT ignored")?;
        let embeddings: Vec<Embedding> = unreviewed.iter().map(|(_, e)| e.clone()).collect();
        let labels = cluster::cluster(&embeddings, params);
        let tx = self.conn.transaction()?;
        tx.execute("UPDATE faces SET cluster = NULL WHERE cluster IS NOT NULL", [])?;
        {
            let mut update = tx.prepare_cached("UPDATE faces SET cluster = ?2 WHERE id = ?1")?;
            for ((face, _), label) in unreviewed.iter().zip(&labels) {
                if let Some(label) = label {
                    update.execute(params![face, *label as i64])?;
                }
            }
        }
        tx.commit()?;
        Ok(labels.iter().flatten().max().map_or(0, |m| m + 1))
    }

    /// `(first column, embedding)` for each row of `sql`.
    fn load_embeddings(&self, sql: &str) -> Result<Vec<(i64, Embedding)>> {
        let mut stmt = self.conn.prepare_cached(sql)?;
        let rows = stmt.query_map([], |row| Ok((row.get(0)?, embedding(row.get_ref(1)?.as_blob()?))))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }
}

/// Before `face` stops being confirmed as its current person (or becomes `keep`): takes that
/// person's tag off the asset, unless another face there is still them.
fn release(tx: &Transaction, face: FaceId, keep: Option<TagId>) -> Result<()> {
    let Some((asset, person)): Option<(i64, i64)> = tx
        .query_row("SELECT asset_id, person FROM faces WHERE id = ?1 AND person IS NOT NULL", [face.0], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })
        .optional()?
    else {
        return Ok(());
    };
    if Some(TagId(person)) == keep {
        return Ok(());
    }
    tx.execute(
        "DELETE FROM asset_tags WHERE asset_id = ?1 AND tag_id = ?2 AND source = ?4
             AND NOT EXISTS (SELECT 1 FROM faces WHERE asset_id = ?1 AND person = ?2 AND id <> ?3)",
        params![asset, person, face.0, USER],
    )?;
    Ok(())
}

fn embedding(bytes: &[u8]) -> Embedding {
    // Only set_faces writes embeddings, always whole f32s.
    Embedding::from_bytes(bytes).unwrap_or_else(|| Embedding::new(Vec::new()))
}

fn iou(a: &Region, b: &Region) -> f32 {
    let w = ((a.x + a.w).min(b.x + b.w) - a.x.max(b.x)).max(0.0);
    let h = ((a.y + a.h).min(b.y + b.h) - a.y.max(b.y)).max(0.0);
    let inter = w * h;
    let union = a.w * a.h + b.w * b.h - inter;
    if union > 0.0 { inter / union } else { 0.0 }
}

/// `f` over `items` on all cores, in order.
fn parallel_map<T: Sync, R: Send>(items: &[T], f: impl Fn(&T) -> R + Sync) -> Vec<R> {
    let threads = std::thread::available_parallelism().map_or(1, |t| t.get());
    let chunk = items.len().div_ceil(threads).max(64);
    std::thread::scope(|s| {
        let f = &f;
        let workers: Vec<_> = items.chunks(chunk).map(|c| s.spawn(move || c.iter().map(f).collect::<Vec<_>>())).collect();
        workers.into_iter().flat_map(|w| w.join().unwrap()).collect()
    })
}

#[cfg(test)]
mod tests {
    use rusqlite::Connection;

    use super::*;

    struct Lib {
        dir: tempfile::TempDir,
        cat: Catalog,
    }

    /// A unit vector at `deg` degrees in the plane of the first two axes.
    fn at(deg: f32) -> Embedding {
        let r = deg.to_radians();
        Embedding::new(vec![r.cos(), r.sin(), 0.0])
    }

    fn face(x: f32, deg: f32) -> NewFace {
        NewFace { region: Region { x, y: 0.1, w: 0.1, h: 0.1 }, score: 0.9, embedding: at(deg), crop: vec![0xff, 0xd8] }
    }

    impl Lib {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let cat = Catalog::open_in_memory(dir.path());
            Self { dir, cat }
        }

        /// An asset holding faces at these embedding angles, side by side.
        fn photo(&mut self, name: &str, degs: &[f32]) -> (AssetId, Vec<FaceId>) {
            let asset = self.cat.asset(&self.dir.path().join(name)).unwrap();
            let found: Vec<_> = degs.iter().enumerate().map(|(i, &d)| face(i as f32 * 0.2, d)).collect();
            self.cat.set_faces(asset, MODEL, &found).unwrap();
            (asset, self.ids(FaceFilter::Asset(asset)))
        }

        fn ids(&self, filter: FaceFilter) -> Vec<FaceId> {
            self.cat.faces(filter, FaceOrder::Asset).unwrap().into_iter().map(|f| f.id).collect()
        }

        fn person(&self, name: &str) -> TagId {
            self.cat.tag(&format!("People|{name}"), TagKind::Person).unwrap()
        }

        fn user_tags(&self, asset: AssetId) -> Vec<(String, Option<Region>)> {
            let tags = self.cat.tags_of(asset).unwrap();
            tags.into_iter().filter(|t| t.source == super::super::Source::User).map(|t| (t.path, t.region)).collect()
        }

        fn suggestion(&self, face: FaceId) -> Option<TagId> {
            let all = self.cat.faces(FaceFilter::All, FaceOrder::Asset).unwrap();
            all.into_iter().find(|f| f.id == face).unwrap().suggestion.map(|s| s.0)
        }
    }

    #[test]
    fn confirming_tags_the_asset_with_the_face_region() {
        let mut lib = Lib::new();
        let alice = lib.person("Alice");
        let (asset, faces) = lib.photo("a.jpg", &[0.0, 90.0]);
        lib.cat.confirm_faces(&faces[1..], alice).unwrap();
        let region = Region { x: 0.2, y: 0.1, w: 0.1, h: 0.1 };
        assert_eq!(lib.user_tags(asset), [("People|Alice".to_owned(), Some(region))]);
        assert_eq!(lib.ids(FaceFilter::Confirmed(alice)), [faces[1]]);
        assert_eq!(lib.ids(FaceFilter::Unreviewed), [faces[0]]);

        lib.cat.unreview_faces(&faces[1..]).unwrap();
        assert_eq!(lib.user_tags(asset), []);
        assert_eq!(lib.ids(FaceFilter::Unreviewed), faces);
    }

    #[test]
    fn the_tag_stays_while_another_face_on_the_asset_is_that_person() {
        let mut lib = Lib::new();
        let (alice, bob) = (lib.person("Alice"), lib.person("Bob"));
        let (asset, faces) = lib.photo("mirror.jpg", &[0.0, 5.0]);
        lib.cat.confirm_faces(&faces, alice).unwrap();
        lib.cat.confirm_faces(&faces[..1], bob).unwrap();
        let names: Vec<String> = lib.user_tags(asset).into_iter().map(|t| t.0).collect();
        assert_eq!(names, ["People|Alice", "People|Bob"]);
        lib.cat.ignore_faces(&faces[1..]).unwrap();
        assert_eq!(lib.user_tags(asset).into_iter().map(|t| t.0).collect::<Vec<_>>(), ["People|Bob"]);
        assert_eq!(lib.ids(FaceFilter::Ignored), [faces[1]]);
    }

    #[test]
    fn only_person_tags_can_be_confirmed() {
        let mut lib = Lib::new();
        let beach = lib.cat.tag("Scenes|Beach", TagKind::Scene).unwrap();
        let (_, faces) = lib.photo("a.jpg", &[0.0]);
        assert!(matches!(lib.cat.confirm_faces(&faces, beach), Err(Error::Invalid(_))));
    }

    #[test]
    fn suggestions_follow_the_nearest_confirmed_face_not_the_average() {
        let mut lib = Lib::new();
        let (alice, bob) = (lib.person("Alice"), lib.person("Bob"));
        // Alice as a child (0°) and as an adult (100°); Bob at 140°. Alice's average sits at
        // 50°, further from a 100°-ish face than Bob is.
        let (_, a) = lib.photo("alice.jpg", &[0.0, 100.0]);
        let (_, b) = lib.photo("bob.jpg", &[140.0]);
        lib.cat.confirm_faces(&a, alice).unwrap();
        lib.cat.confirm_faces(&b, bob).unwrap();
        let (_, new) = lib.photo("new.jpg", &[108.0, 135.0, 10.0, 260.0]);
        assert_eq!(lib.cat.refresh_suggestions(DEFAULT_MIN_SIMILARITY).unwrap(), 3);
        let got: Vec<_> = new.iter().map(|&f| lib.suggestion(f)).collect();
        assert_eq!(got, [Some(alice), Some(bob), Some(alice), None]);
        assert_eq!(lib.ids(FaceFilter::Suggested(alice)), [new[0], new[2]]);
        assert_eq!(lib.ids(FaceFilter::Unknown), [new[3]]);

        let people = lib.cat.people_with_faces().unwrap();
        let counts: Vec<_> = people.iter().map(|p| (p.path.as_str(), p.confirmed, p.suggested)).collect();
        assert_eq!(counts, [("People|Alice", 2, 2), ("People|Bob", 1, 1)]);
    }

    #[test]
    fn a_rejected_suggestion_is_not_offered_again() {
        let mut lib = Lib::new();
        let (alice, bob) = (lib.person("Alice"), lib.person("Bob"));
        let (_, a) = lib.photo("alice.jpg", &[0.0]);
        let (_, b) = lib.photo("bob.jpg", &[40.0]);
        lib.cat.confirm_faces(&a, alice).unwrap();
        lib.cat.confirm_faces(&b, bob).unwrap();
        let (_, new) = lib.photo("new.jpg", &[15.0]);
        lib.cat.refresh_suggestions(DEFAULT_MIN_SIMILARITY).unwrap();
        assert_eq!(lib.suggestion(new[0]), Some(alice));
        lib.cat.reject_suggestions(&new).unwrap();
        assert_eq!(lib.suggestion(new[0]), None);
        lib.cat.refresh_suggestions(DEFAULT_MIN_SIMILARITY).unwrap();
        assert_eq!(lib.suggestion(new[0]), Some(bob));
        // Confirming as the rejected person anyway clears the rejection.
        lib.cat.confirm_faces(&new, alice).unwrap();
        lib.cat.unreview_faces(&new).unwrap();
        lib.cat.refresh_suggestions(DEFAULT_MIN_SIMILARITY).unwrap();
        assert_eq!(lib.suggestion(new[0]), Some(alice));
    }

    #[test]
    fn rescanning_keeps_reviewed_faces() {
        let mut lib = Lib::new();
        let alice = lib.person("Alice");
        let (asset, faces) = lib.photo("a.jpg", &[0.0, 90.0]);
        lib.cat.confirm_faces(&faces[..1], alice).unwrap();
        assert!(lib.cat.face_scanned(asset, MODEL).unwrap());
        // The rescan finds the confirmed face again (slightly moved) and a new one.
        let mut again = face(0.01, 1.0);
        again.region.y = 0.11;
        lib.cat.set_faces(asset, MODEL, &[again, face(0.6, 200.0)]).unwrap();
        let after = lib.cat.faces(FaceFilter::Asset(asset), FaceOrder::Asset).unwrap();
        assert_eq!(after.len(), 2);
        assert_eq!(after[0].id, faces[0]);
        assert_eq!(after[0].status, FaceStatus::Confirmed(alice));
        assert_eq!(after[1].region.x, 0.6);
        assert_eq!(lib.cat.face_crop(after[1].id).unwrap(), Some(vec![0xff, 0xd8]));
    }

    #[test]
    fn clusters_group_unreviewed_faces() {
        let mut lib = Lib::new();
        let (_, a) = lib.photo("a.jpg", &[0.0, 120.0]);
        let (_, b) = lib.photo("b.jpg", &[5.0, 125.0]);
        let (_, c) = lib.photo("c.jpg", &[10.0, 240.0]);
        let params = cluster::Params { min_faces: 2, ..Default::default() };
        assert_eq!(lib.cat.cluster_faces(&params).unwrap(), 2);
        assert_eq!(lib.cat.face_clusters().unwrap(), [(0, 3), (1, 2)]);
        assert_eq!(lib.ids(FaceFilter::Cluster(0)), [a[0], b[0], c[0]]);
        assert_eq!(lib.ids(FaceFilter::Cluster(1)), [a[1], b[1]]);
    }

    #[test]
    fn similarity_order_walks_from_one_end() {
        let mut lib = Lib::new();
        let degs = [40.0, 0.0, 30.0, 10.0, 20.0];
        let faces: Vec<FaceId> = degs.iter().enumerate().map(|(i, &d)| lib.photo(&format!("{i}.jpg"), &[d]).1[0]).collect();
        let ordered = lib.cat.faces(FaceFilter::All, FaceOrder::Similarity).unwrap();
        let mut got: Vec<f32> = ordered.iter().map(|f| degs[faces.iter().position(|&id| id == f.id).unwrap()]).collect();
        if got[0] > got[4] {
            got.reverse();
        }
        assert_eq!(got, [0.0, 10.0, 20.0, 30.0, 40.0]);
    }

    #[test]
    fn a_version_1_catalog_gains_the_face_tables() {
        let dir = tempfile::tempdir().unwrap();
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(super::super::SCHEMA).unwrap();
        conn.pragma_update(None, "user_version", 1).unwrap();
        conn.execute("INSERT INTO assets (path) VALUES ('old.jpg')", []).unwrap();
        let mut cat = Catalog::init(conn, dir.path()).unwrap();
        let asset = cat.find_asset(&dir.path().join("old.jpg")).unwrap().unwrap();
        cat.set_faces(asset, MODEL, &[face(0.0, 0.0)]).unwrap();
        assert_eq!(cat.faces(FaceFilter::All, FaceOrder::Asset).unwrap().len(), 1);
    }
}
