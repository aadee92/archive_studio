//! Fronts and backs: a photo's back scan, paired with the scan of its front.
//!
//! The V850 backend (via `photo-duplex`) saves a batch's fronts as `photo-NNNN-front.png`
//! and, once the user flips them and scans backs, each back next to its front as
//! `photo-NNNN-back.png` (a back it can't match is `unmatched-back-N.png`, unpaired). The
//! catalog pairs files by that naming whenever it first sees one: in the same directory, a
//! stem ending in `-back` / `_back` / ` back` (in any case) goes with the file whose stem is
//! the same base ending in `-front` / `_front` / ` front`, or the bare base, whatever the
//! extensions. The user can pair or unpair by hand ([`Catalog::set_pair`],
//! [`Catalog::unpair`]). Each front has at most one back and the other way round.

use rusqlite::{params, Connection, OptionalExtension};

use super::{AssetId, Catalog, Result};

pub(super) const SCHEMA: &str = "
CREATE TABLE asset_pairs (
    front INTEGER PRIMARY KEY REFERENCES assets(id) ON DELETE CASCADE,
    back INTEGER NOT NULL UNIQUE REFERENCES assets(id) ON DELETE CASCADE,
    -- 'name': paired by the file naming. 'user': paired by hand.
    source TEXT NOT NULL CHECK (source IN ('name', 'user'))
);
";

const SEPARATORS: [char; 3] = ['-', '_', ' '];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Side {
    Front,
    Back,
    /// No side in the name: may be the front of a `…-back` file.
    Plain,
}

/// `(directory with trailing '/', base, side)` of a relative path. The side words match in
/// any case; the base must match exactly.
fn parse(rel: &str) -> (&str, &str, Side) {
    let (dir, name) = match rel.rfind('/') {
        Some(i) => (&rel[..=i], &rel[i + 1..]),
        None => ("", rel),
    };
    let stem = match name.rfind('.') {
        Some(i) if i > 0 => &name[..i],
        _ => name,
    };
    for (word, side) in [("back", Side::Back), ("front", Side::Front)] {
        let Some(cut) = stem.len().checked_sub(word.len()) else { continue };
        if !stem.is_char_boundary(cut) || !stem[cut..].eq_ignore_ascii_case(word) {
            continue;
        }
        if let Some(base) = stem[..cut].strip_suffix(SEPARATORS) {
            if !base.is_empty() {
                return (dir, base, side);
            }
        }
    }
    (dir, stem, Side::Plain)
}

/// The assets whose path starts with `prefix` (a range on the path index).
fn starting_with(conn: &Connection, prefix: &str) -> Result<Vec<(i64, String)>> {
    let mut stmt = conn.prepare_cached("SELECT id, path FROM assets WHERE path >= ?1 AND path < ?1 || char(1114111)")?;
    let rows = stmt.query_map([prefix], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Pairs the asset at `rel` (just added) with its other side by name, if it is there.
pub(super) fn pair_by_name(conn: &Connection, asset: AssetId, rel: &str) -> Result<()> {
    if rel.starts_with(".archive-studio/") {
        return Ok(());
    }
    let (dir, base, side) = parse(rel);
    let others = starting_with(conn, &format!("{dir}{base}"))?;
    let find = |want: &[Side]| {
        // A `-front` file beats a bare one.
        want.iter().find_map(|w| {
            others.iter().find(|(id, path)| {
                let (d, b, s) = parse(path);
                *id != asset.0 && d == dir && b == base && s == *w
            })
        })
    };
    let pair = match side {
        Side::Back => find(&[Side::Front, Side::Plain]).map(|(front, _)| (*front, asset.0)),
        Side::Front | Side::Plain => find(&[Side::Back]).map(|(back, _)| (asset.0, *back)),
    };
    if let Some((front, back)) = pair {
        conn.prepare_cached("INSERT OR IGNORE INTO asset_pairs (front, back, source) VALUES (?1, ?2, 'name')")?
            .execute([front, back])?;
    }
    Ok(())
}

/// Pairs every asset by name (catalogs from before pairs were kept). Returns the pairs made.
pub(super) fn pair_all_by_name(conn: &Connection) -> Result<usize> {
    let all: Vec<(i64, String)> = {
        let mut stmt = conn.prepare("SELECT id, path FROM assets WHERE path LIKE '%back%' ORDER BY id")?;
        // (LIKE is case-insensitive for ASCII.)
        let rows = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    let before: i64 = conn.query_row("SELECT count(*) FROM asset_pairs", [], |row| row.get(0))?;
    for (id, rel) in all {
        if parse(&rel).2 == Side::Back {
            pair_by_name(conn, AssetId(id), &rel)?;
        }
    }
    let after: i64 = conn.query_row("SELECT count(*) FROM asset_pairs", [], |row| row.get(0))?;
    Ok((after - before) as usize)
}

impl Catalog {
    /// The scan of the back of the photo whose front is `front`.
    pub fn back_of(&self, front: AssetId) -> Result<Option<AssetId>> {
        let mut stmt = self.conn.prepare_cached("SELECT back FROM asset_pairs WHERE front = ?1")?;
        Ok(stmt.query_row([front.0], |row| row.get(0)).optional()?.map(AssetId))
    }

    /// The scan of the front of the photo whose back is `back`.
    pub fn front_of(&self, back: AssetId) -> Result<Option<AssetId>> {
        let mut stmt = self.conn.prepare_cached("SELECT front FROM asset_pairs WHERE back = ?1")?;
        Ok(stmt.query_row([back.0], |row| row.get(0)).optional()?.map(AssetId))
    }

    /// The user pairs `back` with `front`, replacing any pairing either had.
    pub fn set_pair(&mut self, front: AssetId, back: AssetId) -> Result<()> {
        if front == back {
            return Err(super::Error::Invalid("a photo can't be its own back".into()));
        }
        let tx = self.conn.transaction()?;
        tx.execute("DELETE FROM asset_pairs WHERE front IN (?1, ?2) OR back IN (?1, ?2)", [front.0, back.0])?;
        tx.execute("INSERT INTO asset_pairs (front, back, source) VALUES (?1, ?2, 'user')", params![front.0, back.0])?;
        tx.commit()?;
        Ok(())
    }

    /// Takes `asset` (a front or a back) out of its pair.
    pub fn unpair(&self, asset: AssetId) -> Result<()> {
        self.conn.execute("DELETE FROM asset_pairs WHERE front = ?1 OR back = ?1", [asset.0])?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_parse_into_sides() {
        assert_eq!(parse("a/photo-0001-back.png"), ("a/", "photo-0001", Side::Back));
        assert_eq!(parse("photo-0001-FRONT.jpg"), ("", "photo-0001", Side::Front));
        assert_eq!(parse("x/y/Grandma_Back.tif"), ("x/y/", "Grandma", Side::Back));
        assert_eq!(parse("Grandma.jpg"), ("", "Grandma", Side::Plain));
        assert_eq!(parse("comeback.jpg"), ("", "comeback", Side::Plain));
        assert_eq!(parse("unmatched-back-1.png"), ("", "unmatched-back-1", Side::Plain));
    }

    #[test]
    fn backs_pair_with_their_fronts_whichever_comes_first() {
        let dir = tempfile::tempdir().unwrap();
        let mut cat = Catalog::open_in_memory(dir.path());
        let p = |rel: &str| dir.path().join(rel);
        std::fs::create_dir(p("box")).unwrap();
        let f1 = cat.asset(&p("box/photo-0001-front.png")).unwrap();
        let b1 = cat.asset(&p("box/photo-0001-back.png")).unwrap();
        // The back first this time, and a bare front.
        let b2 = cat.asset(&p("box/Grandma_back.jpg")).unwrap();
        let f2 = cat.asset(&p("box/Grandma.jpg")).unwrap();
        // Another directory doesn't pair.
        let other = cat.asset(&p("photo-0001-front.png")).unwrap();
        let lone = cat.asset(&p("box/unmatched-back-1.png")).unwrap();

        assert_eq!(cat.back_of(f1).unwrap(), Some(b1));
        assert_eq!(cat.front_of(b1).unwrap(), Some(f1));
        assert_eq!(cat.back_of(f2).unwrap(), Some(b2));
        assert_eq!(cat.back_of(other).unwrap(), None);
        assert_eq!(cat.front_of(lone).unwrap(), None);

        // By hand: the lone back goes with the other front.
        cat.set_pair(other, lone).unwrap();
        assert_eq!(cat.back_of(other).unwrap(), Some(lone));
        cat.set_pair(other, b1).unwrap();
        assert_eq!((cat.back_of(other).unwrap(), cat.back_of(f1).unwrap()), (Some(b1), None));
        cat.unpair(b1).unwrap();
        assert_eq!(cat.front_of(b1).unwrap(), None);
        assert!(cat.set_pair(f1, f1).is_err());
    }
}
