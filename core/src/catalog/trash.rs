//! The library's trash: deleting an asset is reversible until the trash is emptied.
//!
//! [`Catalog::trash`] moves the file (and its XMP sidecar) to
//! `<root>/.archive-studio/trash/<when>-<asset id>/<path in the library>` and moves the asset
//! there in the catalog too, marked missing and trashed: it keeps its tags, faces, pipeline
//! runs and pairing, so [`Catalog::restore`] puts everything back as it was (a rename keeps
//! the file's mtime, so no stage runs again). Trashed assets don't appear in
//! [searches](Catalog::search), and the pipeline neither watches the hidden trash nor runs
//! stages on missing assets. Trashing a front also trashes its back (a back is part of the
//! same photo), and restoring it restores the back. [`Catalog::empty_trash`] deletes for good.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use rusqlite::{params, OptionalExtension};

use super::{xmp, AssetId, Catalog, Error, Result};

/// The trash directory, relative to the library root.
pub const TRASH_DIR: &str = ".archive-studio/trash";

/// An asset in the trash.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Trashed {
    pub asset: AssetId,
    /// Where it was, and returns to on restore.
    pub original: PathBuf,
    /// Where its file is now.
    pub in_trash: PathBuf,
    /// Seconds since the epoch.
    pub trashed_at: i64,
}

fn now() -> i64 {
    SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}

/// Moves `from` (and its sidecar, if any) to `to`, creating directories. A missing file is
/// fine: the asset may already be gone from disk.
fn move_file(from: &Path, to: &Path) -> Result<()> {
    let io = |e: std::io::Error, path: &Path| Error::Io(format!("{}: {e}", path.display()));
    if let Some(dir) = to.parent() {
        std::fs::create_dir_all(dir).map_err(|e| io(e, dir))?;
    }
    if from.exists() {
        if to.exists() {
            return Err(Error::Io(format!("{} already exists", to.display())));
        }
        std::fs::rename(from, to).map_err(|e| io(e, from))?;
    }
    let sidecar = xmp::sidecar_path(from);
    if sidecar.exists() {
        std::fs::rename(&sidecar, xmp::sidecar_path(to)).map_err(|e| io(e, &sidecar))?;
    }
    Ok(())
}

impl Catalog {
    /// Moves `asset` to the trash, and its back if it is a front. Returns what was trashed.
    pub fn trash(&mut self, asset: AssetId) -> Result<Vec<Trashed>> {
        let mut assets = vec![asset];
        assets.extend(self.back_of(asset)?);
        let at = now();
        let mut done = Vec::new();
        for asset in assets {
            let (rel, trashed): (String, Option<String>) = self.conn.query_row(
                "SELECT path, trashed_from FROM assets WHERE id = ?1",
                [asset.0],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?;
            if trashed.is_some() {
                continue;
            }
            let trash_rel = format!("{TRASH_DIR}/{at}-{}/{rel}", asset.0);
            let (original, in_trash) = (self.root.join(&rel), self.root.join(&trash_rel));
            move_file(&original, &in_trash)?;
            let updated = self.conn.execute(
                "UPDATE assets SET path = ?2, trashed_from = ?3, trashed_at = ?4, missing = 1 WHERE id = ?1",
                params![asset.0, trash_rel, rel, at],
            );
            if let Err(e) = updated {
                let _ = move_file(&in_trash, &original);
                return Err(e.into());
            }
            done.push(Trashed { asset, original, in_trash, trashed_at: at });
        }
        Ok(done)
    }

    /// Everything in the trash, most recently trashed first.
    pub fn trashed(&self) -> Result<Vec<Trashed>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT id, trashed_from, path, trashed_at FROM assets WHERE trashed_from IS NOT NULL ORDER BY trashed_at DESC, id",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((AssetId(row.get(0)?), row.get::<_, String>(1)?, row.get::<_, String>(2)?, row.get::<_, i64>(3)?))
        })?;
        let rows: Vec<_> = rows.collect::<rusqlite::Result<_>>()?;
        Ok(rows
            .into_iter()
            .map(|(asset, from, path, at)| Trashed { asset, original: self.root.join(from), in_trash: self.root.join(path), trashed_at: at })
            .collect())
    }

    /// Puts a trashed asset back where it was, with its back if that is in the trash too.
    /// Fails if something else now occupies its old path. Returns the restored paths.
    pub fn restore(&mut self, asset: AssetId) -> Result<Vec<PathBuf>> {
        let mut assets = vec![asset];
        assets.extend(self.back_of(asset)?);
        let mut restored = Vec::new();
        for asset in assets {
            let row: Option<(String, Option<String>)> = self
                .conn
                .query_row("SELECT path, trashed_from FROM assets WHERE id = ?1", [asset.0], |row| Ok((row.get(0)?, row.get(1)?)))
                .optional()?;
            let Some((trash_rel, Some(rel))) = row else { continue };
            let taken: bool = self.conn.query_row("SELECT EXISTS (SELECT 1 FROM assets WHERE path = ?1)", [&rel], |row| row.get(0))?;
            let original = self.root.join(&rel);
            if taken || original.exists() {
                return Err(Error::Io(format!("can't restore {}: something else is there now", original.display())));
            }
            let in_trash = self.root.join(&trash_rel);
            move_file(&in_trash, &original)?;
            let present = original.exists();
            self.conn.execute(
                "UPDATE assets SET path = ?2, trashed_from = NULL, trashed_at = NULL, missing = ?3 WHERE id = ?1",
                params![asset.0, rel, !present],
            )?;
            if let Some(dir) = in_trash.parent() {
                remove_empty_dirs(dir, &self.root.join(TRASH_DIR));
            }
            restored.push(original);
        }
        Ok(restored)
    }

    /// Deletes for good what was trashed at least `older_than` ago (everything with
    /// `Duration::ZERO`): files, sidecars and catalog data. Returns how many assets.
    pub fn empty_trash(&mut self, older_than: Duration) -> Result<usize> {
        let cutoff = now() - older_than.as_secs() as i64;
        let trash_root = self.root.join(TRASH_DIR);
        let mut removed = 0;
        for item in self.trashed()?.into_iter().filter(|t| t.trashed_at <= cutoff) {
            for path in [item.in_trash.clone(), xmp::sidecar_path(&item.in_trash)] {
                match std::fs::remove_file(&path) {
                    Ok(()) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => return Err(Error::Io(format!("{}: {e}", path.display()))),
                }
            }
            self.conn.execute("DELETE FROM assets WHERE id = ?1", [item.asset.0])?;
            if let Some(dir) = item.in_trash.parent() {
                remove_empty_dirs(dir, &trash_root);
            }
            removed += 1;
        }
        Ok(removed)
    }
}

/// Removes `dir` and its parents while they are empty, up to (not including) `stop`.
fn remove_empty_dirs(dir: &Path, stop: &Path) {
    let mut dir = dir.to_owned();
    while dir.starts_with(stop) && dir != stop && std::fs::remove_dir(&dir).is_ok() {
        match dir.parent() {
            Some(parent) => dir = parent.to_owned(),
            None => break,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::{TagKind, DEFAULT_MIN_CONFIDENCE};
    use super::*;

    #[test]
    fn trash_and_restore_keep_everything_including_the_back() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let mut cat = Catalog::open_in_memory(root);
        std::fs::create_dir(root.join("box")).unwrap();
        for name in ["box/photo-0001-front.png", "box/photo-0001-back.png", "box/photo-0001-front.png.xmp", "other.png"] {
            std::fs::write(root.join(name), name).unwrap();
        }
        let front = cat.asset(&root.join("box/photo-0001-front.png")).unwrap();
        let back = cat.asset(&root.join("box/photo-0001-back.png")).unwrap();
        cat.asset(&root.join("other.png")).unwrap();
        cat.add_tag(front, cat.tag("Dog", TagKind::Object).unwrap(), None).unwrap();

        let trashed = cat.trash(front).unwrap();
        assert_eq!(trashed.iter().map(|t| t.asset).collect::<Vec<_>>(), [front, back]);
        assert!(!root.join("box/photo-0001-front.png").exists() && !root.join("box/photo-0001-back.png").exists());
        assert!(trashed[0].in_trash.exists() && xmp::sidecar_path(&trashed[0].in_trash).exists());
        assert!(trashed[0].in_trash.starts_with(root.join(TRASH_DIR)));
        assert_eq!(cat.search("", DEFAULT_MIN_CONFIDENCE).unwrap(), [root.join("other.png")]);
        assert_eq!(cat.search("dog", DEFAULT_MIN_CONFIDENCE).unwrap(), Vec::<PathBuf>::new());
        assert_eq!(cat.trashed().unwrap().len(), 2);
        // The original paths are free in the catalog.
        assert_eq!(cat.find_asset(&root.join("box/photo-0001-front.png")).unwrap(), None);

        let restored = cat.restore(front).unwrap();
        assert_eq!(restored.len(), 2);
        assert_eq!(std::fs::read_to_string(root.join("box/photo-0001-front.png")).unwrap(), "box/photo-0001-front.png");
        assert!(root.join("box/photo-0001-front.png.xmp").exists());
        assert_eq!(cat.find_asset(&root.join("box/photo-0001-front.png")).unwrap(), Some(front));
        assert_eq!(cat.back_of(front).unwrap(), Some(back));
        assert_eq!(cat.search("dog", DEFAULT_MIN_CONFIDENCE).unwrap(), [root.join("box/photo-0001-front.png")]);
        assert!(cat.trashed().unwrap().is_empty());
        assert!(!root.join(TRASH_DIR).join("x").exists());

        // Emptying deletes for good.
        cat.trash(back).unwrap();
        assert_eq!(cat.empty_trash(Duration::from_secs(3600)).unwrap(), 0);
        assert_eq!(cat.empty_trash(Duration::ZERO).unwrap(), 1);
        assert!(cat.trashed().unwrap().is_empty());
        assert_eq!(cat.back_of(front).unwrap(), None);
        assert_eq!(std::fs::read_dir(root.join(TRASH_DIR)).unwrap().count(), 0);
    }

    #[test]
    fn restore_refuses_to_overwrite() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let mut cat = Catalog::open_in_memory(root);
        std::fs::write(root.join("a.png"), "old").unwrap();
        let a = cat.asset(&root.join("a.png")).unwrap();
        cat.trash(a).unwrap();
        std::fs::write(root.join("a.png"), "new").unwrap();
        assert!(matches!(cat.restore(a), Err(Error::Io(_))));
        assert_eq!(std::fs::read_to_string(root.join("a.png")).unwrap(), "new");
    }
}
