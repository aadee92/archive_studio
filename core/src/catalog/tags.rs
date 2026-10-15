//! Tags as a whole rather than on one asset: how many pictures each is on, and renaming,
//! merging and deleting a tag along with everything under it.
//!
//! - [`Catalog::tag_counts`]: every tag with the number of live assets (not in the trash)
//!   carrying it or a tag under it, in one query; [`tree`] (or a [`TagCounter`] fed with
//!   each asset's tags, for a front end that has them in memory) lays them out as the
//!   hierarchy (`People|Ann` under `People`).
//! - [`Catalog::rename_tag`] moves a tag and its subtree to a new path; onto a path that is
//!   taken it fails, changing nothing (merging is a separate, deliberate step).
//! - [`Catalog::merge_tags`]: `from` and the tags under it become `into` and the matching
//!   tags under it (created where missing); faces named as them follow.
//! - [`Catalog::delete_tag`] takes a tag and its subtree off every asset and forgets them;
//!   faces named by a person tag among them are un-named, as [`Catalog::remove_tag`] does
//!   on one asset. A detector's tag comes back where the detector runs again.
//!
//! Each change marks the assets it touches for [sidecar sync](Catalog::sync_sidecars), and
//! ancestors left with nothing on or under them (made on the way by [`Catalog::tag`]) are
//! dropped. `ops` has the user-level versions, which route people to
//! [`people`](super::people)'s operations; [`Catalog::tag_assets`] says which assets a
//! change will touch, to refresh them.

use std::collections::HashMap;

use rusqlite::{params, Connection, Transaction};

use super::people::{backfill_person_tags, display_name};
use super::{kind_of, logged, normalize_tag, AssetId, Catalog, Error, Result, TagId, TagKind};

/// The tag at `?1` and every tag under it, ignoring ASCII case: a condition on `path`.
const SUBTREE: &str = "(lower(path) = lower(?1) OR lower(substr(path, 1, length(?1) + 1)) = lower(?1 || '|'))";

/// A tag and how many assets carry it or a tag under it.
#[derive(Clone, Debug, PartialEq)]
pub struct TagCount {
    pub tag: TagId,
    pub path: String,
    pub kind: TagKind,
    /// Live assets (not in the trash), each counted once however many of the subtree's tags
    /// it has. Detector tags count from the confidence asked for, as in searches.
    pub count: usize,
}

/// One line of a tag tree: depth-first, children after their parent, siblings by name
/// ignoring case.
#[derive(Clone, Debug, PartialEq)]
pub struct TagNode {
    /// `People|Ann`.
    pub path: String,
    /// The last segment, `Ann`.
    pub name: String,
    /// 0 for a top-level tag.
    pub depth: usize,
    /// Assets carrying the tag or a tag under it.
    pub count: usize,
    pub has_children: bool,
}

/// Lays out tags with their counts (each including the tags under it) as a tree. An
/// ancestor missing from `counts` gets a line of its own, with its largest child's count
/// (a lower bound).
pub fn tree<S: AsRef<str>>(counts: impl IntoIterator<Item = (S, usize)>) -> Vec<TagNode> {
    // Keyed by the lowercase segments: sorting the keys puts a parent before its children.
    let mut nodes: std::collections::BTreeMap<Vec<String>, (String, usize)> = std::collections::BTreeMap::new();
    for (path, count) in counts {
        let segments: Vec<&str> = path.as_ref().split('|').map(str::trim).collect();
        for end in 1..=segments.len() {
            let key: Vec<String> = segments[..end].iter().map(|s| s.to_ascii_lowercase()).collect();
            let own = end == segments.len();
            let entry = nodes.entry(key).or_insert_with(|| (segments[..end].join("|"), 0));
            if own {
                entry.0 = segments.join("|");
                entry.1 = count;
            } else {
                entry.1 = entry.1.max(count);
            }
        }
    }
    let keys: Vec<&Vec<String>> = nodes.keys().collect();
    nodes
        .iter()
        .enumerate()
        .map(|(i, (key, (path, count)))| TagNode {
            name: display_name(path).to_owned(),
            path: path.clone(),
            depth: key.len() - 1,
            count: *count,
            has_children: keys.get(i + 1).is_some_and(|next| next.len() > key.len() && next.starts_with(key)),
        })
        .collect()
}

/// Counts tags per asset in memory, for a front end that holds every asset's tags (and
/// keeps the counts up as assets come and go): each asset counts once for each of its tags
/// and each of their ancestors. Paths compare ignoring ASCII case; the first spelling seen
/// is kept.
#[derive(Clone, Debug, Default)]
pub struct TagCounter {
    counts: HashMap<String, (String, usize)>,
}

impl TagCounter {
    /// An asset with these tags came.
    pub fn add<'a>(&mut self, tags: impl IntoIterator<Item = &'a str>) {
        for (key, path) in with_ancestors(tags) {
            self.counts.entry(key).or_insert_with(|| (path, 0)).1 += 1;
        }
    }

    /// An asset with these tags (as they were [added](Self::add)) went.
    pub fn remove<'a>(&mut self, tags: impl IntoIterator<Item = &'a str>) {
        for (key, _) in with_ancestors(tags) {
            if let Some(entry) = self.counts.get_mut(&key) {
                entry.1 = entry.1.saturating_sub(1);
                if entry.1 == 0 {
                    self.counts.remove(&key);
                }
            }
        }
    }

    pub fn is_empty(&self) -> bool {
        self.counts.is_empty()
    }

    /// How many assets carry the tag at `path` or a tag under it.
    pub fn count(&self, path: &str) -> usize {
        self.counts.get(&key_of(path)).map_or(0, |(_, n)| *n)
    }

    pub fn tree(&self) -> Vec<TagNode> {
        tree(self.counts.values().map(|(path, n)| (path.as_str(), *n)))
    }
}

fn key_of(path: &str) -> String {
    path.split('|').map(|s| s.trim().to_ascii_lowercase()).collect::<Vec<_>>().join("|")
}

/// Each of `tags` and its ancestors once: (key, path as spelled).
fn with_ancestors<'a>(tags: impl IntoIterator<Item = &'a str>) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for tag in tags {
        let segments: Vec<&str> = tag.split('|').map(str::trim).collect();
        for end in 1..=segments.len() {
            let path = segments[..end].join("|");
            out.entry(path.to_ascii_lowercase()).or_insert(path);
        }
    }
    out
}

impl Catalog {
    /// Every tag with how many live assets carry it or a tag under it (see [`TagCount`]),
    /// by path; detector tags count from `min_confidence` (as in [searches](Self::search)).
    /// One query.
    pub fn tag_counts(&self, min_confidence: f32) -> Result<Vec<TagCount>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT g.id, g.path, g.kind, COUNT(DISTINCT t.asset_id) FROM tags g
             LEFT JOIN tags d ON lower(d.path) = lower(g.path) OR lower(substr(d.path, 1, length(g.path) + 1)) = lower(g.path || '|')
             LEFT JOIN applied_tags t ON t.tag_id = d.id AND (t.confidence IS NULL OR t.confidence >= ?1)
                 AND t.asset_id IN (SELECT id FROM assets WHERE trashed_from IS NULL)
             GROUP BY g.id ORDER BY g.path COLLATE NOCASE",
        )?;
        let rows = stmt.query_map([min_confidence], |row| {
            Ok(TagCount {
                tag: TagId(row.get(0)?),
                path: row.get(1)?,
                kind: kind_of(row.get_ref(2)?.as_str()?),
                count: row.get::<_, i64>(3)? as usize,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// The live assets (not in the trash) on which the tag or a tag under it is, applied or
    /// rejected: what renaming, merging or deleting it changes. By id.
    pub fn tag_assets(&self, tag: TagId) -> Result<Vec<AssetId>> {
        let path = self.tag_path(tag)?;
        let mut stmt = self.conn.prepare_cached(&format!(
            "SELECT DISTINCT t.asset_id FROM asset_tags t JOIN assets a ON a.id = t.asset_id
             WHERE a.trashed_from IS NULL AND t.tag_id IN (SELECT id FROM tags WHERE {SUBTREE}) ORDER BY t.asset_id"
        ))?;
        let rows = stmt.query_map([&path], |row| row.get(0).map(AssetId))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// The tag's path; an error if there is no such tag.
    pub(crate) fn tag_path(&self, tag: TagId) -> Result<String> {
        use rusqlite::OptionalExtension;
        self.conn
            .query_row("SELECT path FROM tags WHERE id = ?1", [tag.0], |row| row.get(0))
            .optional()?
            .ok_or_else(|| Error::Invalid(format!("no tag {}", tag.0)))
    }

    /// Renames a tag and everything under it (`People|Fam` → `People|Family` also renames
    /// `People|Fam|Alice`); a change of case only is a rename too. Missing ancestors of the
    /// new path are made (of the tag's kind), and the old ones left with nothing on or under
    /// them dropped. Fails, changing nothing, if a new path is already taken (see
    /// [`merge_tags`](Self::merge_tags)).
    pub fn rename_tag(&mut self, tag: TagId, new_path: &str) -> Result<()> {
        let result = self.rename_tag_inner(tag, new_path);
        logged(format_args!("rename_tag {} to {new_path:?}", tag.0), result)
    }

    fn rename_tag_inner(&mut self, tag: TagId, new_path: &str) -> Result<()> {
        let new_path = normalize_tag(new_path)?;
        let tx = self.conn.transaction()?;
        let (old, kind): (String, String) = tx.query_row("SELECT path, kind FROM tags WHERE id = ?1", [tag.0], |row| Ok((row.get(0)?, row.get(1)?)))?;
        if old == new_path {
            return Ok(());
        }
        let moved = subtree(&tx, &old)?;
        let ids: Vec<i64> = moved.iter().map(|(id, _)| *id).collect();
        for (_, path) in &moved {
            let to = format!("{new_path}{}", &path[old.len()..]);
            if let Some((id, taken)) = find(&tx, &to)? {
                if !ids.contains(&id) {
                    return Err(Error::Invalid(format!("{taken} already exists")));
                }
            }
        }
        tx.execute(&format!("UPDATE tags SET path = ?2 || substr(path, length(?1) + 1) WHERE {SUBTREE}"), [&old, &new_path])?;
        make_ancestors(&tx, &new_path, &kind)?;
        prune_ancestors(&tx, &old)?;
        tx.commit()?;
        Ok(())
    }

    /// `from` and the tags under it are the same as `into` and the tags under it: each tag of
    /// the subtree moves onto its counterpart under `into` (`Trips|1998|Rome` onto
    /// `Holidays|1998|Rome`), or is renamed to that path where there is none. Tags on
    /// assets, faces named or suggested as them and rejections move along; `from`'s tags are
    /// then gone. `into` mustn't be `from` or under it.
    pub fn merge_tags(&mut self, from: TagId, into: TagId) -> Result<()> {
        let result = self.merge_tags_inner(from, into);
        logged(format_args!("merge_tags {} into {}", from.0, into.0), result)
    }

    fn merge_tags_inner(&mut self, from: TagId, into: TagId) -> Result<()> {
        let (from_path, into_path) = (self.tag_path(from)?, self.tag_path(into)?);
        if key_of(&into_path) == key_of(&from_path) || key_of(&into_path).starts_with(&format!("{}|", key_of(&from_path))) {
            return Err(Error::Invalid(format!("can't merge {from_path} into itself or a tag under it")));
        }
        let tx = self.conn.transaction()?;
        // Parents first: a renamed parent doesn't change its children's targets.
        for (id, path) in subtree(&tx, &from_path)? {
            let to = format!("{into_path}{}", &path[from_path.len()..]);
            match find(&tx, &to)? {
                Some((target, _)) => {
                    move_tag_rows(&tx, TagId(id), TagId(target))?;
                    tx.execute("DELETE FROM tags WHERE id = ?1", [id])?;
                }
                None => {
                    tx.execute("UPDATE tags SET path = ?2 WHERE id = ?1", params![id, to])?;
                }
            }
        }
        backfill_person_tags(&tx)?;
        prune_ancestors(&tx, &from_path)?;
        tx.commit()?;
        Ok(())
    }

    /// Takes the tag and every tag under it off every asset (rejections included) and
    /// forgets them. Faces named as one of them go back to unknown and suggestions of them
    /// are dropped. Returns nothing; [`tag_assets`](Self::tag_assets) beforehand says where
    /// it was.
    pub fn delete_tag(&mut self, tag: TagId) -> Result<()> {
        let result = self.delete_tag_inner(tag);
        logged(format_args!("delete_tag {}", tag.0), result)
    }

    fn delete_tag_inner(&mut self, tag: TagId) -> Result<()> {
        let path = self.tag_path(tag)?;
        let tx = self.conn.transaction()?;
        for (id, _) in subtree(&tx, &path)? {
            drop_tag_rows(&tx, TagId(id))?;
            tx.execute("DELETE FROM tags WHERE id = ?1", [id])?;
        }
        prune_ancestors(&tx, &path)?;
        tx.commit()?;
        Ok(())
    }
}

/// The tag at `path` and those under it, parents first.
fn subtree(conn: &Connection, path: &str) -> Result<Vec<(i64, String)>> {
    let mut stmt = conn.prepare_cached(&format!("SELECT id, path FROM tags WHERE {SUBTREE} ORDER BY length(path), id"))?;
    let rows = stmt.query_map([path], |row| Ok((row.get(0)?, row.get(1)?)))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

fn find(conn: &Connection, path: &str) -> Result<Option<(i64, String)>> {
    use rusqlite::OptionalExtension;
    let mut stmt = conn.prepare_cached("SELECT id, path FROM tags WHERE path = ?1")?;
    Ok(stmt.query_row([path], |row| Ok((row.get(0)?, row.get(1)?))).optional()?)
}

/// Makes the ancestors of `path` that don't exist, as `kind`.
fn make_ancestors(conn: &Connection, path: &str, kind: &str) -> Result<()> {
    let mut insert = conn.prepare_cached("INSERT INTO tags (path, kind) VALUES (?1, ?2) ON CONFLICT (path) DO NOTHING")?;
    for (end, _) in path.match_indices('|') {
        insert.execute([&path[..end], kind])?;
    }
    Ok(())
}

/// Drops the ancestors of `path`, nearest first, while they have nothing on any asset, no
/// face refers to them and no tag is under them.
pub(super) fn prune_ancestors(conn: &Connection, path: &str) -> Result<()> {
    let mut stmt = conn.prepare_cached(&format!(
        "DELETE FROM tags WHERE lower(path) = lower(?1)
             AND NOT EXISTS (SELECT 1 FROM tags c WHERE lower(substr(c.path, 1, length(?1) + 1)) = lower(?1 || '|'))
             AND NOT EXISTS (SELECT 1 FROM asset_tags t WHERE t.tag_id = tags.id)
             AND NOT EXISTS (SELECT 1 FROM faces f WHERE f.person = tags.id OR f.suggested = tags.id)
             AND NOT EXISTS (SELECT 1 FROM face_rejections r WHERE r.tag_id = tags.id)"
    ))?;
    let mut end = path.len();
    while let Some(i) = path[..end].rfind('|') {
        if stmt.execute([&path[..i]])? == 0 {
            break;
        }
        end = i;
    }
    Ok(())
}

/// Moves everything that refers to `from` onto `into`: faces named or suggested as it,
/// rejections, and its rows on assets (a user's row and a detector's merge as for one tag:
/// applied if either was, the region kept where there is one).
pub(super) fn move_tag_rows(tx: &Transaction, from: TagId, into: TagId) -> Result<()> {
    tx.execute("UPDATE faces SET person = ?2 WHERE person = ?1", [from.0, into.0])?;
    tx.execute("UPDATE faces SET suggested = ?2 WHERE suggested = ?1", [from.0, into.0])?;
    tx.execute("INSERT OR IGNORE INTO face_rejections (face_id, tag_id) SELECT face_id, ?2 FROM face_rejections WHERE tag_id = ?1", [from.0, into.0])?;
    tx.execute("DELETE FROM face_rejections WHERE tag_id = ?1 AND face_id IN (SELECT id FROM faces WHERE person = ?1)", [into.0])?;
    tx.execute(
        "INSERT INTO asset_tags (asset_id, tag_id, source, confidence, hidden, x, y, w, h)
             SELECT asset_id, ?2, source, confidence, hidden, x, y, w, h FROM asset_tags WHERE tag_id = ?1
         ON CONFLICT DO UPDATE SET hidden = hidden AND excluded.hidden,
             x = CASE WHEN x IS NULL THEN excluded.x ELSE x END, y = CASE WHEN x IS NULL THEN excluded.y ELSE y END,
             w = CASE WHEN x IS NULL THEN excluded.w ELSE w END, h = CASE WHEN x IS NULL THEN excluded.h ELSE h END",
        [from.0, into.0],
    )?;
    tx.execute("DELETE FROM asset_tags WHERE tag_id = ?1", [from.0])?;
    Ok(())
}

/// Takes `tag` off every asset and un-names the faces named as it (they go back to
/// unknown); suggestions of it are dropped.
pub(super) fn drop_tag_rows(tx: &Transaction, tag: TagId) -> Result<()> {
    tx.execute(
        "UPDATE faces SET person = NULL, ignored = 0, suggested = NULL, similarity = NULL, cluster = NULL WHERE person = ?1",
        [tag.0],
    )?;
    tx.execute("UPDATE faces SET suggested = NULL, similarity = NULL WHERE suggested = ?1", [tag.0])?;
    tx.execute("DELETE FROM asset_tags WHERE tag_id = ?1", [tag.0])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::faces::{FaceFilter, FaceOrder, NewFace, MODEL};
    use faces::Embedding;
    use crate::catalog::{Region, DEFAULT_MIN_CONFIDENCE};

    struct Lib {
        dir: tempfile::TempDir,
        cat: Catalog,
    }

    impl Lib {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let cat = Catalog::open_in_memory(dir.path());
            Self { dir, cat }
        }

        fn asset(&self, name: &str, tags: &[&str]) -> AssetId {
            let asset = self.cat.asset(&self.dir.path().join(name)).unwrap();
            for tag in tags {
                self.cat.add_tag_path(asset, tag, TagKind::Keyword).unwrap();
            }
            asset
        }

        fn tags(&self, asset: AssetId) -> Vec<String> {
            self.cat.tags_of(asset).unwrap().into_iter().map(|t| t.path).collect()
        }

        fn id(&self, path: &str) -> TagId {
            self.cat.find_tag(path).unwrap().unwrap().0
        }

        fn counts(&self) -> Vec<(String, usize)> {
            self.cat.tag_counts(DEFAULT_MIN_CONFIDENCE).unwrap().into_iter().map(|c| (c.path, c.count)).collect()
        }

        /// A face on `asset` named `name`.
        fn named_face(&mut self, asset: AssetId, name: &str) -> TagId {
            let face = NewFace { region: Region { x: 0.1, y: 0.1, w: 0.2, h: 0.2 }, score: 0.9, embedding: Embedding::new(vec![1.0]), crop: vec![] };
            self.cat.set_faces(asset, MODEL, &[face]).unwrap();
            let faces = self.cat.faces(FaceFilter::Asset(asset), FaceOrder::Asset).unwrap();
            self.cat.name_faces(&[faces[0].id], name).unwrap()
        }
    }

    #[test]
    fn counts_cover_subtrees_once_per_live_asset() {
        let mut lib = Lib::new();
        let a = lib.asset("a.jpg", &["Holidays|1998", "Holidays|1999"]);
        lib.asset("b.jpg", &["Holidays|1998", "Dog"]);
        let c = lib.asset("c.jpg", &["Dog"]);
        // A rejected detector tag and a weak one don't count.
        let beach = lib.cat.tag("Scenes|Beach", TagKind::Scene).unwrap();
        let det = |confidence| crate::catalog::Detection { tag: beach, confidence, region: None };
        lib.cat.set_detections(a, "clip", &[det(0.9)]).unwrap();
        lib.cat.set_detections(c, "clip", &[det(0.3)]).unwrap();
        lib.cat.remove_tag(a, beach).unwrap();
        let trashed = lib.asset("d.jpg", &["Dog"]);
        lib.cat.conn.execute("UPDATE assets SET trashed_from = 'd.jpg' WHERE id = ?1", [trashed.0]).unwrap();
        assert_eq!(
            lib.counts(),
            [("Dog".into(), 2), ("Holidays".into(), 2), ("Holidays|1998".into(), 2), ("Holidays|1999".into(), 1), ("Scenes".into(), 0), ("Scenes|Beach".into(), 0)]
        );

        let nodes = tree(lib.counts());
        let lines: Vec<String> = nodes.iter().map(|n| format!("{}{} {}{}", "  ".repeat(n.depth), n.name, n.count, if n.has_children { "+" } else { "" })).collect();
        assert_eq!(lines, ["Dog 2", "Holidays 2+", "  1998 2", "  1999 1", "Scenes 0+", "  Beach 0"]);
    }

    #[test]
    fn counter_counts_ancestors_once_and_follows_removals() {
        let mut counter = TagCounter::default();
        counter.add(["People|Ann", "People|Bob"]);
        counter.add(["people|ann", "Dog"]);
        assert_eq!((counter.count("People"), counter.count("PEOPLE|ann"), counter.count("Dog")), (2, 2, 1));
        let tree: Vec<(String, usize, usize)> = counter.tree().into_iter().map(|n| (n.path, n.depth, n.count)).collect();
        assert_eq!(tree, [("Dog".into(), 0, 1), ("People".into(), 0, 2), ("People|Ann".into(), 1, 2), ("People|Bob".into(), 1, 1)]);
        counter.remove(["People|Ann", "People|Bob"]);
        counter.remove(["people|ann", "Dog"]);
        assert!(counter.is_empty());
    }

    #[test]
    fn tree_fills_in_missing_parents() {
        let nodes = tree([("A|B|C", 3), ("a|d", 1)]);
        let lines: Vec<(&str, usize, usize, bool)> = nodes.iter().map(|n| (n.path.as_str(), n.depth, n.count, n.has_children)).collect();
        assert_eq!(lines, [("A", 0, 3, true), ("A|B", 1, 3, true), ("A|B|C", 2, 3, false), ("a|d", 1, 1, false)]);
    }

    #[test]
    fn rename_moves_children_makes_parents_and_drops_empty_ones() {
        let mut lib = Lib::new();
        let a = lib.asset("a.jpg", &["Holidays|1998|Rome", "Holidays|1998"]);
        lib.cat.mark_synced(a);
        let y1998 = lib.id("holidays|1998");
        assert_eq!(lib.cat.tag_assets(y1998).unwrap(), [a]);
        lib.cat.rename_tag(y1998, "Trips|Summer 1998").unwrap();
        assert_eq!(lib.tags(a), ["Trips|Summer 1998", "Trips|Summer 1998|Rome"]);
        assert_eq!(lib.cat.needing_sync().unwrap(), [a], "sidecar to rewrite");
        // `Trips` was made; `Holidays` had nothing left and went.
        assert_eq!(lib.cat.all_tags().unwrap(), ["Trips", "Trips|Summer 1998", "Trips|Summer 1998|Rome"]);

        // A change of case.
        lib.cat.rename_tag(y1998, "trips|summer 1998").unwrap();
        assert_eq!(lib.tags(a), ["trips|summer 1998", "trips|summer 1998|Rome"]);

        // Onto a taken path (itself or under it): nothing changes.
        lib.asset("b.jpg", &["Rome"]);
        let rome = lib.id("trips|summer 1998|rome");
        assert!(matches!(lib.cat.rename_tag(rome, "ROME"), Err(Error::Invalid(_))));
        lib.asset("c.jpg", &["Paris|Rome"]);
        assert!(lib.cat.rename_tag(y1998, "Paris").is_err(), "Paris|Rome is taken");
        assert_eq!(lib.tags(a), ["trips|summer 1998", "trips|summer 1998|Rome"]);
    }

    #[test]
    fn merge_moves_subtrees_tags_and_faces() {
        let mut lib = Lib::new();
        let a = lib.asset("a.jpg", &["Trips|1998|Rome", "Trips|1999"]);
        let b = lib.asset("b.jpg", &["Holidays|1998|Rome", "Trips|1998"]);
        let trips = lib.id("trips");
        let holidays = lib.id("holidays");
        assert!(lib.cat.merge_tags(holidays, lib.id("holidays|1998")).is_err(), "into its own subtree");
        lib.cat.merge_tags(trips, holidays).unwrap();
        assert_eq!(lib.tags(a), ["Holidays|1998|Rome", "Holidays|1999"]);
        assert_eq!(lib.tags(b), ["Holidays|1998", "Holidays|1998|Rome"]);
        assert!(lib.cat.find_tag("Trips").unwrap().is_none());
        assert_eq!(lib.counts().iter().find(|(p, _)| p == "Holidays|1998|Rome").unwrap().1, 2);

        // People: faces follow, as with merge_people.
        let c = lib.asset("c.jpg", &[]);
        let d = lib.asset("d.jpg", &[]);
        let al = lib.named_face(c, "Family|Al");
        let alice = lib.named_face(d, "Alice");
        lib.cat.merge_tags(lib.id("People|Family"), lib.id("People")).unwrap();
        assert_eq!(lib.tags(c), ["People|Al"]);
        assert_eq!(lib.id("People|Al"), al, "renamed onto the free path");
        lib.cat.merge_tags(al, alice).unwrap();
        assert_eq!(lib.tags(c), ["People|Alice"]);
        assert_eq!(lib.cat.faces(FaceFilter::Confirmed(alice), FaceOrder::Asset).unwrap().len(), 2);
    }

    #[test]
    fn delete_takes_the_subtree_off_and_unnames_faces() {
        let mut lib = Lib::new();
        let a = lib.asset("a.jpg", &["Holidays|1998", "Dog"]);
        let ann = lib.named_face(a, "Family|Ann");
        lib.cat.mark_synced(a);
        lib.cat.delete_tag(lib.id("Holidays")).unwrap();
        assert_eq!(lib.tags(a), ["Dog", "People|Family|Ann"]);
        assert_eq!(lib.cat.needing_sync().unwrap(), [a]);

        lib.cat.delete_tag(lib.id("People|Family")).unwrap();
        assert_eq!(lib.tags(a), ["Dog"]);
        assert!(lib.cat.faces(FaceFilter::Confirmed(ann), FaceOrder::Asset).unwrap().is_empty());
        assert_eq!(lib.cat.faces(FaceFilter::Unreviewed, FaceOrder::Asset).unwrap().len(), 1);
        assert_eq!(lib.cat.all_tags().unwrap(), ["Dog"], "People had nothing left");
    }
}
