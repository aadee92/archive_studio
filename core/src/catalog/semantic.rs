//! Search by description: a CLIP embedding per image, ranked against the embedding of a
//! text (see the `clip` crate).
//!
//! The pipeline's `describe` stage stores one per image ([`crate::pipeline::DESCRIBE`]),
//! along with the tags it proposes from it. Embeddings from different models can't be
//! compared, so each records its model and a search only looks at those of the query's.
//!
//! Searching reads every embedding (2 KB each): fine for tens of thousands of images; a
//! much larger library would want an index.

use std::path::PathBuf;

pub use clip::Embedding;
use rusqlite::{params, OptionalExtension};

use super::{AssetId, Catalog, Result};

pub(super) const SCHEMA: &str = "
CREATE TABLE image_embeddings (
    asset_id INTEGER PRIMARY KEY REFERENCES assets(id) ON DELETE CASCADE,
    -- The model that made it, e.g. 'tinyclip-8m-16-yfcc15m'.
    model TEXT NOT NULL,
    -- Unit-length little-endian f32s.
    embedding BLOB NOT NULL
);
";

impl Catalog {
    pub fn set_image_embedding(&self, asset: AssetId, model: &str, embedding: &Embedding) -> Result<()> {
        self.conn
            .prepare_cached(
                "INSERT INTO image_embeddings (asset_id, model, embedding) VALUES (?1, ?2, ?3)
                 ON CONFLICT DO UPDATE SET model = excluded.model, embedding = excluded.embedding",
            )?
            .execute(params![asset.0, model, embedding.to_bytes()])?;
        Ok(())
    }

    /// The asset's embedding and the model that made it.
    pub fn image_embedding(&self, asset: AssetId) -> Result<Option<(String, Embedding)>> {
        let mut stmt = self.conn.prepare_cached("SELECT model, embedding FROM image_embeddings WHERE asset_id = ?1")?;
        let row = stmt.query_row([asset.0], |row| Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?))).optional()?;
        Ok(row.and_then(|(model, bytes)| Some((model, Embedding::from_bytes(&bytes)?))))
    }

    /// The images (not trashed or missing) whose `model` embedding is most similar to
    /// `query`, best first, with their cosine similarity, at most `limit`.
    pub fn search_embedding(&self, query: &Embedding, model: &str, limit: usize) -> Result<Vec<(PathBuf, f32)>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT a.path, e.embedding FROM image_embeddings e JOIN assets a ON a.id = e.asset_id
             WHERE e.model = ?1 AND a.trashed_from IS NULL AND NOT a.missing",
        )?;
        let rows = stmt.query_map([model], |row| Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?)))?;
        let mut scored = Vec::new();
        for row in rows {
            let (path, bytes) = row?;
            if let Some(embedding) = Embedding::from_bytes(&bytes) {
                scored.push((path, embedding.similarity(query)));
            }
        }
        let ranked = rank(scored, Cutoff::NONE, limit);
        Ok(ranked.into_iter().map(|(rel, similarity)| (self.root.join(rel), similarity)).collect())
    }

    /// How far the describe stage has come for `model`: images (not trashed or missing) with
    /// an embedding of it, of all such images. A front end shows it while a search can only
    /// find some of the library ("120 of 400 described").
    pub fn embedding_coverage(&self, model: &str) -> Result<Coverage> {
        let (described, images): (i64, i64) = self.conn.prepare_cached(
            "SELECT count(e.asset_id), count(*) FROM assets a
             LEFT JOIN image_embeddings e ON e.asset_id = a.id AND e.model = ?1
             WHERE a.kind = 'image' AND a.trashed_from IS NULL AND NOT a.missing",
        )?
        .query_row([model], |row| Ok((row.get(0)?, row.get(1)?)))?;
        Ok(Coverage { described: described as usize, images: images as usize })
    }
}

/// See [`Catalog::embedding_coverage`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Coverage {
    pub described: usize,
    pub images: usize,
}

/// Which matches of a search are worth showing: at least `min` similar to the query, and
/// no more than `window` less similar than the best match. CLIP similarities are all alike
/// for what doesn't fit; what fits stands out by a few hundredths.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Cutoff {
    pub min: f32,
    pub window: f32,
}

impl Cutoff {
    /// Keeps everything.
    pub const NONE: Self = Self { min: f32::NEG_INFINITY, window: f32::INFINITY };
}

/// `scored` best first, cut off (see [`Cutoff`]) and at most `limit` long.
pub fn rank<T>(mut scored: Vec<(T, f32)>, cutoff: Cutoff, limit: usize) -> Vec<(T, f32)> {
    scored.sort_by(|a, b| b.1.total_cmp(&a.1));
    let best = scored.first().map_or(0.0, |(_, similarity)| *similarity);
    let floor = cutoff.min.max(best - cutoff.window);
    let keep = scored.partition_point(|(_, similarity)| *similarity >= floor).min(limit);
    scored.truncate(keep);
    scored
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unit(v: &[f32]) -> Embedding {
        Embedding::new(v.to_vec())
    }

    #[test]
    fn ranks_by_similarity_within_the_model() {
        let dir = tempfile::tempdir().unwrap();
        let cat = Catalog::open_in_memory(dir.path());
        let asset = |name: &str| cat.asset(&dir.path().join(name)).unwrap();
        let (beach, dog, gone, other) = (asset("beach.jpg"), asset("dog.jpg"), asset("gone.jpg"), asset("other.jpg"));
        cat.set_image_embedding(beach, "m", &unit(&[1.0, 0.1, 0.0])).unwrap();
        cat.set_image_embedding(dog, "m", &unit(&[0.2, 1.0, 0.0])).unwrap();
        cat.set_image_embedding(gone, "m", &unit(&[1.0, 0.0, 0.0])).unwrap();
        cat.set_missing(gone, true).unwrap();
        cat.set_image_embedding(other, "older-model", &unit(&[1.0, 0.0, 0.0])).unwrap();

        let found = cat.search_embedding(&unit(&[1.0, 0.0, 0.0]), "m", 10).unwrap();
        let names: Vec<_> = found.iter().map(|(p, _)| p.file_name().unwrap().to_str().unwrap()).collect();
        assert_eq!(names, ["beach.jpg", "dog.jpg"]);
        assert!(found[0].1 > 0.99 && found[1].1 < 0.3);
        assert_eq!(cat.search_embedding(&unit(&[1.0, 0.0, 0.0]), "m", 1).unwrap().len(), 1);

        // Replaced, not added to.
        cat.set_image_embedding(beach, "m", &unit(&[0.0, 0.0, 1.0])).unwrap();
        let (model, stored) = cat.image_embedding(beach).unwrap().unwrap();
        assert_eq!((model.as_str(), stored), ("m", unit(&[0.0, 0.0, 1.0])));
    }

    #[test]
    fn rank_cuts_off_below_the_floor_and_the_window() {
        let scored = vec![("a", 0.21), ("b", 0.30), ("c", 0.25), ("d", 0.10), ("e", 0.29)];
        fn names(ranked: Vec<(&'static str, f32)>) -> Vec<&'static str> {
            ranked.into_iter().map(|(n, _)| n).collect()
        }
        assert_eq!(names(rank(scored.clone(), Cutoff::NONE, 10)), ["b", "e", "c", "a", "d"]);
        assert_eq!(names(rank(scored.clone(), Cutoff::NONE, 2)), ["b", "e"]);
        // Within 0.06 of the best: 0.24 up.
        assert_eq!(names(rank(scored.clone(), Cutoff { min: 0.2, window: 0.06 }, 10)), ["b", "e", "c"]);
        // The floor wins over a wide window.
        assert_eq!(names(rank(scored.clone(), Cutoff { min: 0.26, window: 1.0 }, 10)), ["b", "e"]);
        // Nothing reaches the floor.
        assert!(rank(scored, Cutoff { min: 0.5, window: 1.0 }, 10).is_empty());
        assert!(rank(Vec::<((), f32)>::new(), Cutoff::NONE, 10).is_empty());
    }

    #[test]
    fn coverage_counts_live_images() {
        let dir = tempfile::tempdir().unwrap();
        let cat = Catalog::open_in_memory(dir.path());
        let image = |name: &str| {
            cat.record_file(&dir.path().join(name), crate::media::MediaKind::Image, 1, 1).unwrap().0
        };
        let (a, b, gone) = (image("a.jpg"), image("b.jpg"), image("gone.jpg"));
        image("c.jpg");
        cat.record_file(&dir.path().join("v.mp4"), crate::media::MediaKind::Video, 1, 1).unwrap();
        cat.set_missing(gone, true).unwrap();
        assert_eq!(cat.embedding_coverage("m").unwrap(), Coverage { described: 0, images: 3 });
        cat.set_image_embedding(a, "m", &unit(&[1.0, 0.0])).unwrap();
        cat.set_image_embedding(b, "older-model", &unit(&[1.0, 0.0])).unwrap();
        cat.set_image_embedding(gone, "m", &unit(&[1.0, 0.0])).unwrap();
        assert_eq!(cat.embedding_coverage("m").unwrap(), Coverage { described: 1, images: 3 });
    }
}
