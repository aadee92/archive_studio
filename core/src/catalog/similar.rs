//! Near-duplicate images: a small signature per image, compared cheaply.
//!
//! A [`Signature`] is two 64-bit difference hashes of the upright image (dHash: whether each
//! pixel of a 9×8 grey thumbnail is brighter than its right neighbour, and of an 8×9 one
//! than the one below) and a coarse colour histogram (4 levels per channel, 64 bins). The
//! hashes survive rescaling, recompression, slight crops and exposure changes; the
//! histogram tells apart pictures with the same layout in different colours. The pipeline's
//! `signature` stage stores one per image ([`crate::pipeline::SIGNATURE`]), and
//! [`Catalog::similar`] ranks a candidate set against an asset. The hashes only find near
//! duplicates (re-scans, copies, edits, burst shots), so it also compares the CLIP
//! embeddings of the `describe` stage ([`super::semantic`]), which find the same scene or
//! event: the same room with other people in it, the same view with or without them.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use image::{imageops::FilterType, DynamicImage, ImageDecoder, ImageReader};
use rusqlite::{params, OptionalExtension};

use super::{AssetId, Catalog, Result};

pub(super) const SCHEMA: &str = "
CREATE TABLE image_signatures (
    asset_id INTEGER PRIMARY KEY REFERENCES assets(id) ON DELETE CASCADE,
    -- Horizontal and vertical dHash, as the bits of an i64.
    dhash INTEGER NOT NULL,
    vhash INTEGER NOT NULL,
    -- 64-bin RGB histogram, each bin's share of the pixels × 255.
    colors BLOB NOT NULL
);
";

/// Most hash bits (of 128) that may differ for two images to count as highly similar.
pub const MAX_DISTANCE: u32 = 16;
/// Least histogram intersection (0..=1) for two images to count as highly similar.
pub const MIN_COLOR_OVERLAP: f32 = 0.5;
/// Least cosine similarity of two images' CLIP embeddings for them to count as the same
/// scene or event. Measured on a few hundred family photos with this model: pairs taken
/// within half an hour have a median of 0.71 and 70% reach this; pairs days apart have a
/// median of 0.40 and 2% reach it (ranked last, below the real matches).
pub const MIN_SCENE_SIMILARITY: f32 = 0.62;

const BINS: usize = 64;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Signature {
    pub dhash: u64,
    pub vhash: u64,
    pub colors: [u8; BINS],
}

impl Signature {
    /// The signature of an image as displayed.
    pub fn of_image(image: &DynamicImage) -> Self {
        // A small thumbnail first: resizing the full image to 9×8 directly is slow.
        let small = image.thumbnail(64, 64);
        let grey = |w, h| image::imageops::resize(&small.to_luma8(), w, h, FilterType::Triangle);
        let h = grey(9, 8);
        let mut dhash = 0u64;
        for y in 0..8 {
            for x in 0..8 {
                dhash = dhash << 1 | u64::from(h.get_pixel(x, y)[0] > h.get_pixel(x + 1, y)[0]);
            }
        }
        let v = grey(8, 9);
        let mut vhash = 0u64;
        for y in 0..8 {
            for x in 0..8 {
                vhash = vhash << 1 | u64::from(v.get_pixel(x, y)[0] > v.get_pixel(x, y + 1)[0]);
            }
        }
        let rgb = small.to_rgb8();
        let mut counts = [0u32; BINS];
        for p in rgb.pixels() {
            let bin = usize::from(p[0] >> 6) << 4 | usize::from(p[1] >> 6) << 2 | usize::from(p[2] >> 6);
            counts[bin] += 1;
        }
        let total = counts.iter().sum::<u32>().max(1) as f32;
        let mut colors = [0u8; BINS];
        for (c, n) in colors.iter_mut().zip(counts) {
            *c = (n as f32 / total * 255.0).round() as u8;
        }
        Self { dhash, vhash, colors }
    }

    /// The signature of the image file at `path`, upright (EXIF orientation applied).
    pub fn of_file(path: &Path) -> std::result::Result<Self, String> {
        let fail = |e: &dyn std::fmt::Display| format!("{}: {e}", path.display());
        let mut decoder = ImageReader::open(path)
            .map_err(|e| fail(&e))?
            .with_guessed_format()
            .map_err(|e| fail(&e))?
            .into_decoder()
            .map_err(|e| fail(&e))?;
        let orientation = decoder.orientation().map_err(|e| fail(&e))?;
        let mut image = DynamicImage::from_decoder(decoder).map_err(|e| fail(&e))?;
        image.apply_orientation(orientation);
        Ok(Self::of_image(&image))
    }

    /// Hash bits that differ, 0..=128.
    pub fn distance(&self, other: &Self) -> u32 {
        (self.dhash ^ other.dhash).count_ones() + (self.vhash ^ other.vhash).count_ones()
    }

    /// Histogram intersection, 0..=1.
    pub fn color_overlap(&self, other: &Self) -> f32 {
        let shared: u32 = self.colors.iter().zip(&other.colors).map(|(&a, &b)| u32::from(a.min(b))).sum();
        let own = self.colors.iter().map(|&a| u32::from(a)).sum::<u32>().max(other.colors.iter().map(|&b| u32::from(b)).sum()).max(1);
        shared as f32 / own as f32
    }

    /// How alike the two are, in 0..=1, if they are highly similar ([`MAX_DISTANCE`],
    /// [`MIN_COLOR_OVERLAP`]); `None` otherwise.
    pub fn similarity(&self, other: &Self) -> Option<f32> {
        let distance = self.distance(other);
        let overlap = self.color_overlap(other);
        (distance <= MAX_DISTANCE && overlap >= MIN_COLOR_OVERLAP).then(|| 0.75 * (1.0 - distance as f32 / 128.0) + 0.25 * overlap)
    }
}

impl Catalog {
    pub fn set_signature(&self, asset: AssetId, signature: &Signature) -> Result<()> {
        self.conn
            .prepare_cached(
                "INSERT INTO image_signatures (asset_id, dhash, vhash, colors) VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT DO UPDATE SET dhash = excluded.dhash, vhash = excluded.vhash, colors = excluded.colors",
            )?
            .execute(params![asset.0, signature.dhash as i64, signature.vhash as i64, &signature.colors[..]])?;
        Ok(())
    }

    pub fn signature(&self, asset: AssetId) -> Result<Option<Signature>> {
        let mut stmt = self.conn.prepare_cached("SELECT dhash, vhash, colors FROM image_signatures WHERE asset_id = ?1")?;
        Ok(stmt.query_row([asset.0], read_signature).optional()?)
    }

    /// The assets among `candidates` (e.g. the grid's current view) from the same scene or
    /// event as `asset`, most similar first, at most `limit`: `(asset, score in 0..=1)`.
    /// Two signals, either one enough: CLIP embeddings ([`MIN_SCENE_SIMILARITY`]), which
    /// match the same place with other people in it, and the hash signature, which catches
    /// copies and edits of images not described yet. Empty if `asset` has neither.
    pub fn similar(&self, asset: AssetId, candidates: &[AssetId], limit: usize) -> Result<Vec<(AssetId, f32)>> {
        let signature = self.signature(asset)?;
        let embedding = self.image_embedding(asset)?.filter(|(model, _)| model == clip::MODEL).map(|(_, e)| e);
        if signature.is_none() && embedding.is_none() {
            return Ok(Vec::new());
        }
        let wanted: HashSet<i64> = candidates.iter().map(|a| a.0).filter(|&a| a != asset.0).collect();
        let mut scores: HashMap<i64, f32> = HashMap::new();
        let mut keep = |id: i64, score: f32| {
            let best = scores.entry(id).or_insert(score);
            *best = best.max(score);
        };
        if let Some(target) = &signature {
            let by_id = "SELECT dhash, vhash, colors FROM image_signatures WHERE asset_id = ?1";
            let all = "SELECT asset_id, dhash, vhash, colors FROM image_signatures";
            for (id, other) in self.rows_of(&wanted, by_id, all, |row, at| signature_at(row, at).map(Some))? {
                if let Some(score) = target.similarity(&other) {
                    keep(id, score);
                }
            }
        }
        if let Some(target) = &embedding {
            let by_id = "SELECT model, embedding FROM image_embeddings WHERE asset_id = ?1";
            let all = "SELECT asset_id, model, embedding FROM image_embeddings";
            let read = |row: &rusqlite::Row, at| {
                let model: String = row.get(at)?;
                Ok(Some(row.get_ref(at + 1)?.as_blob()?).filter(|_| model == clip::MODEL).and_then(clip::Embedding::from_bytes))
            };
            for (id, other) in self.rows_of(&wanted, by_id, all, read)? {
                let score = target.similarity(&other);
                if score >= MIN_SCENE_SIMILARITY {
                    keep(id, score);
                }
            }
        }
        let mut found: Vec<_> = scores.into_iter().map(|(id, score)| (AssetId(id), score)).collect();
        found.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0 .0.cmp(&b.0 .0)));
        found.truncate(limit);
        Ok(found)
    }

    /// `read`'s value (from column `at` on) for each of `wanted` that has a row: a few are
    /// looked up one by one with `by_id`, many in one pass over `all` (the id, then the same
    /// columns).
    fn rows_of<T>(
        &self,
        wanted: &HashSet<i64>,
        by_id: &str,
        all: &str,
        read: impl Fn(&rusqlite::Row, usize) -> rusqlite::Result<Option<T>>,
    ) -> Result<Vec<(i64, T)>> {
        let mut out = Vec::new();
        if wanted.len() < 2000 {
            let mut stmt = self.conn.prepare_cached(by_id)?;
            for &id in wanted {
                if let Some(value) = stmt.query_row([id], |row| read(row, 0)).optional()?.flatten() {
                    out.push((id, value));
                }
            }
        } else {
            let mut stmt = self.conn.prepare_cached(all)?;
            let mut rows = stmt.query([])?;
            while let Some(row) = rows.next()? {
                let id: i64 = row.get(0)?;
                if wanted.contains(&id) {
                    if let Some(value) = read(row, 1)? {
                        out.push((id, value));
                    }
                }
            }
        }
        Ok(out)
    }
}

fn read_signature(row: &rusqlite::Row) -> rusqlite::Result<Signature> {
    signature_at(row, 0)
}

/// The signature in columns `at..at + 3`.
fn signature_at(row: &rusqlite::Row, at: usize) -> rusqlite::Result<Signature> {
    Ok(Signature {
        dhash: row.get::<_, i64>(at)? as u64,
        vhash: row.get::<_, i64>(at + 1)? as u64,
        colors: colors(row.get_ref(at + 2)?.as_blob()?),
    })
}

fn colors(blob: &[u8]) -> [u8; BINS] {
    let mut out = [0; BINS];
    for (o, b) in out.iter_mut().zip(blob) {
        *o = *b;
    }
    out
}

#[cfg(test)]
mod tests {
    use image::{Rgb, RgbImage};

    use super::*;

    /// A picture: a gradient with a bright block at (`bx`, `by`).
    fn picture(w: u32, h: u32, bx: u32, by: u32, tint: [u8; 3]) -> DynamicImage {
        let image = RgbImage::from_fn(w, h, |x, y| {
            let base = (x * 120 / w + y * 60 / h) as u8;
            let block = x / (w / 8) == bx && y / (h / 8) == by;
            let v = if block { 250 } else { base };
            Rgb([v.saturating_add(tint[0]), v.saturating_add(tint[1]), v.saturating_add(tint[2])])
        });
        DynamicImage::ImageRgb8(image)
    }

    #[test]
    fn rescaled_copies_are_similar_and_different_pictures_are_not() {
        let a = Signature::of_image(&picture(400, 300, 2, 3, [0, 0, 0]));
        let resized = Signature::of_image(&picture(200, 150, 2, 3, [0, 0, 0]));
        assert!(a.distance(&resized) <= 4, "{}", a.distance(&resized));
        assert!(a.similarity(&resized).unwrap() > 0.9);
        // Mirrored layout: the hashes differ.
        let mirrored = Signature::of_image(&DynamicImage::ImageRgb8(picture(400, 300, 2, 3, [0, 0, 0]).to_rgb8()).fliph());
        assert_eq!(a.similarity(&mirrored), None);
        // Same layout, very different colours.
        let tinted = Signature::of_image(&picture(400, 300, 2, 3, [0, 0, 140]));
        assert!(a.color_overlap(&tinted) < MIN_COLOR_OVERLAP);
        assert_eq!(a.similarity(&tinted), None);
    }

    #[test]
    fn similar_ranks_the_candidates() {
        let dir = tempfile::tempdir().unwrap();
        let cat = Catalog::open_in_memory(dir.path());
        let ids: Vec<AssetId> = (0..4).map(|i| cat.asset(&dir.path().join(format!("{i}.png"))).unwrap()).collect();
        let base = picture(400, 300, 2, 3, [0, 0, 0]);
        cat.set_signature(ids[0], &Signature::of_image(&base)).unwrap();
        cat.set_signature(ids[1], &Signature::of_image(&base.resize_exact(300, 225, FilterType::Triangle))).unwrap();
        cat.set_signature(ids[2], &Signature::of_image(&base.fliph())).unwrap();
        // ids[3] has none yet.
        assert_eq!(cat.signature(ids[0]).unwrap(), Some(Signature::of_image(&base)));
        let found = cat.similar(ids[0], &ids, 10).unwrap();
        assert_eq!(found.iter().map(|f| f.0).collect::<Vec<_>>(), [ids[1]]);
        assert!(cat.similar(ids[3], &ids, 10).unwrap().is_empty());
        assert!(cat.similar(ids[0], &ids, 0).unwrap().is_empty());
    }

    /// A unit vector at cosine `similarity` to the first axis.
    fn at(similarity: f32) -> clip::Embedding {
        let mut v = vec![0.0; 8];
        v[0] = similarity;
        v[1] = (1.0 - similarity * similarity).sqrt();
        clip::Embedding::new(v)
    }

    #[test]
    fn similar_finds_the_same_scene_by_embedding() {
        let dir = tempfile::tempdir().unwrap();
        let cat = Catalog::open_in_memory(dir.path());
        let ids: Vec<AssetId> = (0..5).map(|i| cat.asset(&dir.path().join(format!("{i}.png"))).unwrap()).collect();
        // Different pictures to the hashes: only the embeddings can match them.
        cat.set_signature(ids[0], &Signature::of_image(&picture(400, 300, 2, 3, [0, 0, 0]))).unwrap();
        cat.set_signature(ids[1], &Signature::of_image(&picture(400, 300, 6, 1, [0, 0, 140]))).unwrap();
        cat.set_image_embedding(ids[0], clip::MODEL, &at(1.0)).unwrap();
        cat.set_image_embedding(ids[1], clip::MODEL, &at(0.7)).unwrap();
        cat.set_image_embedding(ids[2], clip::MODEL, &at(0.9)).unwrap();
        cat.set_image_embedding(ids[3], clip::MODEL, &at(0.4)).unwrap();
        // Another model's embeddings don't compare.
        cat.set_image_embedding(ids[4], "other-model", &at(1.0)).unwrap();
        let found = cat.similar(ids[0], &ids, 10).unwrap();
        assert_eq!(found.iter().map(|f| f.0).collect::<Vec<_>>(), [ids[2], ids[1]]);
        // An asset with an embedding but no signature is matched too.
        assert!(cat.similar(ids[2], &ids, 10).unwrap().iter().any(|f| f.0 == ids[0]));
    }
}
