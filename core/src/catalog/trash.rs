//! The library's trash: deleting an asset is reversible until the trash is emptied.
//!
//! [`Catalog::trash`] moves the file (and its XMP sidecar) to
//! `<root>/.archive-studio/trash/<when>-<asset id>/<path in the library>` and moves the asset
//! there in the catalog too, marked missing and trashed: it keeps its tags, faces, pipeline
//! runs and pairing, so [`Catalog::restore`] puts everything back as it was (a rename keeps
//! the file's mtime, so no stage runs again). Trashed assets don't appear in
//! [searches](Catalog::search), and the pipeline neither watches the hidden trash nor runs
//! stages on missing assets. Trashing a front also trashes its back (a back is part of the
//! same photo), and restoring it restores the back. [`Catalog::empty_trash`] deletes for good
//! what was trashed some time ago, [`Catalog::purge`] chosen items; [`Catalog::trash_totals`]
//! counts what is in the trash cheaply (for a front end's sidebar).

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
    /// The file's size as the catalog last saw it (else as it is in the trash), if known.
    pub size: Option<u64>,
    /// Its back, if it is a front: restoring or purging it takes the back along when that
    /// is in the trash too.
    pub back: Option<AssetId>,
}

/// How much is in the trash ([`Catalog::trash_totals`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TrashTotals {
    /// Assets (a front and its back are two).
    pub items: usize,
    /// Their files' sizes as the catalog last saw them.
    pub bytes: u64,
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
        let result = self.trash_inner(asset);
        super::logged(format_args!("trash asset {}", asset.0), result)
    }

    fn trash_inner(&mut self, asset: AssetId) -> Result<Vec<Trashed>> {
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
            let size = std::fs::metadata(&in_trash).ok().map(|m| m.len());
            let back = self.back_of(asset)?;
            done.push(Trashed { asset, original, in_trash, trashed_at: at, size, back });
        }
        Ok(done)
    }

    /// Everything in the trash, most recently trashed first. `in_trash` is the file to draw a
    /// thumbnail from.
    pub fn trashed(&self) -> Result<Vec<Trashed>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT a.id, a.trashed_from, a.path, a.trashed_at, a.size, p.back FROM assets a LEFT JOIN asset_pairs p ON p.front = a.id
             WHERE a.trashed_from IS NOT NULL ORDER BY a.trashed_at DESC, a.id",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((
                AssetId(row.get(0)?),
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, Option<i64>>(4)?,
                row.get::<_, Option<i64>>(5)?.map(AssetId),
            ))
        })?;
        let rows: Vec<_> = rows.collect::<rusqlite::Result<_>>()?;
        Ok(rows
            .into_iter()
            .map(|(asset, from, path, at, size, back)| {
                let in_trash = self.root.join(path);
                // Files the pipeline never looked at have no size in the catalog.
                let size = size.map(|s| s as u64).or_else(|| std::fs::metadata(&in_trash).ok().map(|m| m.len()));
                Trashed { asset, original: self.root.join(from), in_trash, trashed_at: at, size, back }
            })
            .collect())
    }

    /// How many assets are in the trash and how much their files take, in one query.
    pub fn trash_totals(&self) -> Result<TrashTotals> {
        let mut stmt =
            self.conn.prepare_cached("SELECT count(*), coalesce(sum(size), 0) FROM assets WHERE trashed_from IS NOT NULL")?;
        let (items, bytes): (i64, i64) = stmt.query_row([], |row| Ok((row.get(0)?, row.get(1)?)))?;
        Ok(TrashTotals { items: items as usize, bytes: bytes as u64 })
    }

    /// Puts a trashed asset back where it was, with its back if that is in the trash too.
    /// Fails if something else now occupies its old path. Returns the restored paths.
    pub fn restore(&mut self, asset: AssetId) -> Result<Vec<PathBuf>> {
        let result = self.restore_inner(asset);
        super::logged(format_args!("restore asset {}", asset.0), result)
    }

    fn restore_inner(&mut self, asset: AssetId) -> Result<Vec<PathBuf>> {
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
        let result = self.empty_trash_inner(older_than);
        super::logged(format_args!("empty_trash older than {older_than:?}"), result)
    }

    fn empty_trash_inner(&mut self, older_than: Duration) -> Result<usize> {
        let cutoff = now() - older_than.as_secs() as i64;
        let items: Vec<Trashed> = self.trashed()?.into_iter().filter(|t| t.trashed_at <= cutoff).collect();
        self.delete_for_good(&items)
    }

    /// Deletes these trashed assets for good, as [`empty_trash`](Self::empty_trash) does,
    /// each with its back if that is in the trash too (as [`restore`](Self::restore) would
    /// have brought it back). Assets not in the trash are left alone. Returns how many
    /// assets were deleted.
    pub fn purge(&mut self, assets: &[AssetId]) -> Result<usize> {
        let result = self.purge_inner(assets);
        super::logged(format_args!("purge assets {:?}", assets.iter().map(|a| a.0).collect::<Vec<_>>()), result)
    }

    fn purge_inner(&mut self, assets: &[AssetId]) -> Result<usize> {
        let trashed = self.trashed()?;
        let backs: Vec<AssetId> = trashed.iter().filter(|t| assets.contains(&t.asset)).filter_map(|t| t.back).collect();
        let items: Vec<Trashed> = trashed.into_iter().filter(|t| assets.contains(&t.asset) || backs.contains(&t.asset)).collect();
        self.delete_for_good(&items)
    }

    /// Removes trashed items' files and sidecars, then their assets.
    fn delete_for_good(&mut self, items: &[Trashed]) -> Result<usize> {
        let trash_root = self.root.join(TRASH_DIR);
        let mut removed = 0;
        for item in items {
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
    fn purge_and_totals() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let mut cat = Catalog::open_in_memory(root);
        for (name, bytes) in [("photo-0001-front.png", 10), ("photo-0001-back.png", 20), ("a.png", 300), ("b.png", 4000)] {
            std::fs::write(root.join(name), vec![0u8; bytes]).unwrap();
        }
        let id = |cat: &Catalog, name: &str| cat.asset(&root.join(name)).unwrap();
        let (front, back, a, b) = (id(&cat, "photo-0001-front.png"), id(&cat, "photo-0001-back.png"), id(&cat, "a.png"), id(&cat, "b.png"));
        assert_eq!(cat.trash_totals().unwrap(), TrashTotals::default());

        for asset in [front, a, b] {
            cat.trash(asset).unwrap();
        }
        let trashed = cat.trashed().unwrap();
        assert_eq!(trashed.len(), 4);
        // Never seen by the pipeline: the size comes from the file in the trash.
        let size = |asset| trashed.iter().find(|t| t.asset == asset).unwrap().size;
        assert_eq!((size(front), size(back), size(b)), (Some(10), Some(20), Some(4000)));
        assert_eq!(trashed.iter().find(|t| t.asset == front).unwrap().back, Some(back));
        assert_eq!(trashed.iter().find(|t| t.asset == back).unwrap().back, None);
        assert_eq!(cat.trash_totals().unwrap().items, 4);
        cat.conn.execute("UPDATE assets SET size = 4000 WHERE id = ?1", [b.0]).unwrap();
        assert_eq!(cat.trash_totals().unwrap(), TrashTotals { items: 4, bytes: 4000 }, "sizes the catalog knows");

        // A front takes its trashed back along; what isn't in the trash is left alone.
        let other = {
            std::fs::write(root.join("c.png"), "c").unwrap();
            id(&cat, "c.png")
        };
        assert_eq!(cat.purge(&[front, other]).unwrap(), 2);
        assert!(cat.find_asset(&root.join("c.png")).unwrap().is_some() && root.join("c.png").exists());
        let left: Vec<AssetId> = cat.trashed().unwrap().iter().map(|t| t.asset).collect();
        assert_eq!(left, [a, b]);
        assert!(!trashed[0].in_trash.exists());
        assert_eq!(cat.purge(&[a]).unwrap(), 1);
        assert_eq!(cat.trash_totals().unwrap(), TrashTotals { items: 1, bytes: 4000 });
        assert_eq!(cat.purge(&[a]).unwrap(), 0, "already gone");
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
