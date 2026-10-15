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

use clip::Embedding;
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
        let mut ranked = Vec::new();
        for row in rows {
            let (path, bytes) = row?;
            if let Some(embedding) = Embedding::from_bytes(&bytes) {
                ranked.push((path, embedding.similarity(query)));
            }
        }
        ranked.sort_by(|a, b| b.1.total_cmp(&a.1));
        ranked.truncate(limit);
        Ok(ranked.into_iter().map(|(rel, similarity)| (self.root.join(rel), similarity)).collect())
    }
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
}
