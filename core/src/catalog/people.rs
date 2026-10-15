//! People: person tags under `People|`, kept in step with the faces named as them.
//!
//! A person is a [`TagKind::Person`] tag, `People|<Name>` (or deeper, `People|Family|Alice`).
//! Naming a face puts that tag, with the face's box as its region, on the face's asset as a
//! user tag (see [`Catalog::confirm_faces`]), so the person shows up in [`Catalog::tags_of`],
//! [searches](Catalog::search) and XMP sidecars (as an MWG face region) like any other tag.
//! The rules that keep tags and faces consistent:
//!
//! - Naming a face adds the tag; un-naming (unreview, ignore, naming as someone else) takes
//!   it off unless another face on the asset is still that person.
//! - [Renaming](Catalog::rename_person) renames the tag; renaming onto an existing person
//!   [merges](Catalog::merge_people) the two.
//! - [Deleting](Catalog::delete_person) a person un-names their faces and drops the tag.
//! - The user taking the tag off an asset ([`Catalog::remove_tag`]) un-names that person's
//!   faces there.
//!
//! Catalogs from before this was kept get the tags once on upgrade
//! ([`Catalog::sync_person_tags`] repairs any time).

use rusqlite::{params, Connection, OptionalExtension};

use super::faces::{FaceId, FaceStatus};
use super::{normalize_tag, AssetId, Catalog, Error, Region, Result, TagId, TagKind};

/// The root of every person tag.
pub const PEOPLE: &str = "People";

/// A face on an asset, for drawing its box and name over the image.
#[derive(Clone, Debug, PartialEq)]
pub struct FaceBox {
    pub face: FaceId,
    /// Top-left corner and size as fractions (0..=1) of the image as displayed (EXIF
    /// orientation applied; orient bakes it into the pixels anyway).
    pub region: Region,
    pub status: FaceStatus,
    /// Who the face is, if named: the person tag and its display name (`Alice` for
    /// `People|Family|Alice`).
    pub person: Option<(TagId, String)>,
    /// For unreviewed faces: the suggested person, its display name and the similarity.
    pub suggestion: Option<(TagId, String, f32)>,
    /// Boxed by the user ([`Catalog::add_face`]), not found by a scan.
    pub manual: bool,
}

/// The person tag path a typed name means: `Alice` is `People|Alice`; a path already under
/// `People|` (any case) is kept; any other path goes under `People|`.
pub fn person_path(name: &str) -> Result<String> {
    let path = normalize_tag(name)?;
    let first = path.split('|').next().unwrap_or_default();
    Ok(if first.eq_ignore_ascii_case(PEOPLE) && path.contains('|') { path } else { format!("{PEOPLE}|{path}") })
}

/// The last segment of a tag path: `People|Family|Alice` → `Alice`.
pub fn display_name(path: &str) -> &str {
    path.rsplit('|').next().unwrap_or(path).trim()
}

/// The names a name box offers for what was typed so far: the people (person tag paths, as
/// [`Catalog::leaf_tags`] lists them) whose display name contains `text` ignoring case, those
/// starting with it first, then alphabetically; at most `limit`. Each is offered by display
/// name, or by path where two people share it, so that [`Catalog::person`] finds that one.
pub fn matching_people<P: AsRef<str>>(paths: &[P], text: &str, limit: usize) -> Vec<String> {
    let text = text.trim().to_lowercase();
    if text.is_empty() {
        return Vec::new();
    }
    let mut matches: Vec<(bool, String, &str)> = paths
        .iter()
        .filter_map(|p| {
            let lower = display_name(p.as_ref()).to_lowercase();
            lower.contains(&text).then(|| (!lower.starts_with(&text), lower, p.as_ref()))
        })
        .collect();
    matches.sort();
    matches
        .iter()
        .take(limit)
        .map(|(_, lower, path)| {
            let shared = matches.iter().filter(|(_, other, _)| other == lower).count() > 1;
            if shared { (*path).to_owned() } else { display_name(path).to_owned() }
        })
        .collect()
}

impl Catalog {
    /// The person a typed name means, creating `People|<name>` if there is none: an exact
    /// person path (`People|Family|Alice`), else the one person whose display name matches
    /// ignoring case (`alice`), else a new person.
    pub fn person(&self, name: &str) -> Result<TagId> {
        let trimmed = normalize_tag(name)?;
        if let Some((id, _, TagKind::Person)) = self.find_tag(&trimmed)? {
            if trimmed.contains('|') {
                return Ok(id);
            }
        }
        if !trimmed.contains('|') {
            let matching: Vec<TagId> = self
                .leaf_tags(TagKind::Person)?
                .into_iter()
                .filter(|(_, path)| display_name(path).eq_ignore_ascii_case(&trimmed))
                .map(|(id, _)| id)
                .collect();
            if let [only] = matching[..] {
                return Ok(only);
            }
        }
        self.tag(&person_path(name)?, TagKind::Person)
    }

    /// Names `faces` as the person `name` means ([`person`](Self::person)), tagging their
    /// assets. Returns the person.
    pub fn name_faces(&mut self, faces: &[FaceId], name: &str) -> Result<TagId> {
        let result = self.name_faces_inner(faces, name);
        super::logged(format_args!("name_faces {} as {name:?}", super::face_ids(faces)), result)
    }

    fn name_faces_inner(&mut self, faces: &[FaceId], name: &str) -> Result<TagId> {
        let person = self.person(name)?;
        self.confirm_faces(faces, person)?;
        Ok(person)
    }

    /// Renames a person. `new_name` is a display name (kept under the same parent:
    /// `People|Family|Al` → `People|Family|Alice`) or a full path. If that person already
    /// exists, the two are [merged](Self::merge_people) into it. Returns the person now.
    pub fn rename_person(&mut self, person: TagId, new_name: &str) -> Result<TagId> {
        let result = self.rename_person_inner(person, new_name);
        super::logged(format_args!("rename_person {} to {new_name:?}", person.0), result)
    }

    fn rename_person_inner(&mut self, person: TagId, new_name: &str) -> Result<TagId> {
        let old = self.person_tag_path(person)?;
        let new_path = if new_name.contains('|') {
            person_path(new_name)?
        } else {
            let leaf = normalize_tag(new_name)?;
            match old.rsplit_once('|') {
                Some((parent, _)) => format!("{parent}|{leaf}"),
                None => person_path(&leaf)?,
            }
        };
        match self.find_tag(&new_path)? {
            Some((existing, _, _)) if existing != person => {
                self.merge_people(person, existing)?;
                Ok(existing)
            }
            _ => {
                self.rename_tag(person, &new_path)?;
                Ok(person)
            }
        }
    }

    /// `from` and `into` are the same person: `from`'s faces, suggestions, rejections and
    /// tags move to `into`, and `from` is deleted. `from` must have no people under it.
    pub fn merge_people(&mut self, from: TagId, into: TagId) -> Result<()> {
        let result = self.merge_people_inner(from, into);
        super::logged(format_args!("merge_people {} into {}", from.0, into.0), result)
    }

    fn merge_people_inner(&mut self, from: TagId, into: TagId) -> Result<()> {
        if from == into {
            return Ok(());
        }
        self.person_tag_path(into)?;
        self.childless_person(from)?;
        let tx = self.conn.transaction()?;
        tx.execute("UPDATE faces SET person = ?2 WHERE person = ?1", [from.0, into.0])?;
        tx.execute("UPDATE faces SET suggested = ?2 WHERE suggested = ?1", [from.0, into.0])?;
        tx.execute("INSERT OR IGNORE INTO face_rejections (face_id, tag_id) SELECT face_id, ?2 FROM face_rejections WHERE tag_id = ?1", [
            from.0, into.0,
        ])?;
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
        tx.execute("DELETE FROM tags WHERE id = ?1", [from.0])?;
        backfill_person_tags(&tx)?;
        tx.commit()?;
        Ok(())
    }

    /// Deletes a person: their faces go back to unreviewed, suggestions of them are
    /// dropped, and their tag comes off every asset. The person must have no people under it.
    pub fn delete_person(&mut self, person: TagId) -> Result<()> {
        let result = self.delete_person_inner(person);
        super::logged(format_args!("delete_person {}", person.0), result)
    }

    fn delete_person_inner(&mut self, person: TagId) -> Result<()> {
        self.childless_person(person)?;
        let tx = self.conn.transaction()?;
        tx.execute(
            "UPDATE faces SET person = NULL, ignored = 0, suggested = NULL, similarity = NULL, cluster = NULL WHERE person = ?1",
            [person.0],
        )?;
        tx.execute("UPDATE faces SET suggested = NULL, similarity = NULL WHERE suggested = ?1", [person.0])?;
        tx.execute("DELETE FROM asset_tags WHERE tag_id = ?1", [person.0])?;
        tx.execute("DELETE FROM tags WHERE id = ?1", [person.0])?;
        tx.commit()?;
        Ok(())
    }

    /// Puts each named face's person tag on its asset where it is missing (e.g. it was
    /// rejected by hand, or the catalog predates person tags). Returns how many were added.
    /// Runs once on its own when an older catalog is upgraded.
    pub fn sync_person_tags(&mut self) -> Result<usize> {
        let result = self.sync_person_tags_inner();
        super::logged(format_args!("sync_person_tags"), result)
    }

    fn sync_person_tags_inner(&mut self) -> Result<usize> {
        let tx = self.conn.transaction()?;
        let n = backfill_person_tags(&tx)?;
        tx.commit()?;
        Ok(n)
    }

    /// The faces on `asset`, left to right, with who they are, for drawing over the image.
    /// Ignored faces are included (status [`FaceStatus::Ignored`]); skip them to hide them.
    pub fn face_boxes(&self, asset: AssetId) -> Result<Vec<FaceBox>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT f.id, f.x, f.y, f.w, f.h, f.person, p.path, f.ignored, f.suggested, s.path, f.similarity, f.manual
             FROM faces f LEFT JOIN tags p ON p.id = f.person LEFT JOIN tags s ON s.id = f.suggested
             WHERE f.asset_id = ?1 ORDER BY f.x, f.id",
        )?;
        let rows = stmt.query_map([asset.0], |row| {
            let person: Option<(i64, String)> = match (row.get::<_, Option<i64>>(5)?, row.get::<_, Option<String>>(6)?) {
                (Some(id), Some(path)) => Some((id, path)),
                _ => None,
            };
            let status = match (&person, row.get::<_, bool>(7)?) {
                (Some((id, _)), _) => FaceStatus::Confirmed(TagId(*id)),
                (None, true) => FaceStatus::Ignored,
                (None, false) => FaceStatus::Unreviewed,
            };
            let suggestion = match (row.get::<_, Option<i64>>(8)?, row.get::<_, Option<String>>(9)?, row.get::<_, Option<f32>>(10)?) {
                (Some(id), Some(path), Some(sim)) if status == FaceStatus::Unreviewed => {
                    Some((TagId(id), display_name(&path).to_owned(), sim))
                }
                _ => None,
            };
            Ok(FaceBox {
                face: FaceId(row.get(0)?),
                region: Region { x: row.get(1)?, y: row.get(2)?, w: row.get(3)?, h: row.get(4)? },
                status,
                person: person.map(|(id, path)| (TagId(id), display_name(&path).to_owned())),
                suggestion,
                manual: row.get(11)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// The path of a person tag; an error if it isn't one.
    fn person_tag_path(&self, person: TagId) -> Result<String> {
        let row: Option<(String, String)> = self
            .conn
            .query_row("SELECT path, kind FROM tags WHERE id = ?1", [person.0], |row| Ok((row.get(0)?, row.get(1)?)))
            .optional()?;
        match row {
            Some((path, kind)) if kind == TagKind::Person.name() => Ok(path),
            Some((path, kind)) => Err(Error::Invalid(format!("tag {path} is a {kind}, not a person"))),
            None => Err(Error::Invalid(format!("no tag {}", person.0))),
        }
    }

    fn childless_person(&self, person: TagId) -> Result<String> {
        let path = self.person_tag_path(person)?;
        let children: bool = self.conn.query_row(
            "SELECT EXISTS (SELECT 1 FROM tags WHERE lower(substr(path, 1, length(?1) + 1)) = lower(?1 || '|'))",
            [&path],
            |row| row.get(0),
        )?;
        if children {
            return Err(Error::Invalid(format!("{path} has people under it")));
        }
        Ok(path)
    }
}

/// Adds the missing user person tag (with the face's region) for every named face.
pub(super) fn backfill_person_tags(conn: &Connection) -> Result<usize> {
    Ok(conn.execute(
        "INSERT INTO asset_tags (asset_id, tag_id, source, hidden, x, y, w, h)
             SELECT f.asset_id, f.person, ?1, 0, f.x, f.y, f.w, f.h FROM faces f
             WHERE f.person IS NOT NULL AND NOT EXISTS (
                 SELECT 1 FROM asset_tags t WHERE t.asset_id = f.asset_id AND t.tag_id = f.person AND t.source = ?1 AND NOT t.hidden)
         ON CONFLICT DO UPDATE SET hidden = 0, x = excluded.x, y = excluded.y, w = excluded.w, h = excluded.h",
        params![super::USER],
    )?)
}

#[cfg(test)]
mod tests {
    use super::super::faces::{FaceFilter, FaceOrder, NewFace, MODEL};
    use super::*;
    use faces::Embedding;

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

        /// An asset with `n` faces side by side.
        fn photo(&mut self, name: &str, n: usize) -> (AssetId, Vec<FaceId>) {
            let asset = self.cat.asset(&self.dir.path().join(name)).unwrap();
            let found: Vec<_> = (0..n)
                .map(|i| NewFace {
                    region: Region { x: i as f32 * 0.3, y: 0.1, w: 0.2, h: 0.2 },
                    score: 0.9,
                    embedding: Embedding::new(vec![1.0, i as f32, 0.0]),
                    crop: vec![],
                })
                .collect();
            self.cat.set_faces(asset, MODEL, &found).unwrap();
            let faces = self.cat.faces(FaceFilter::Asset(asset), FaceOrder::Asset).unwrap();
            (asset, faces.into_iter().map(|f| f.id).collect())
        }

        fn tags(&self, asset: AssetId) -> Vec<String> {
            self.cat.tags_of(asset).unwrap().into_iter().map(|t| t.path).collect()
        }

        fn search(&self, q: &str) -> usize {
            self.cat.search(q, 0.5).unwrap().len()
        }
    }

    #[test]
    fn naming_tags_under_people_and_resolves_names() {
        let mut lib = Lib::new();
        let (a, fa) = lib.photo("a.jpg", 2);
        let alice = lib.cat.name_faces(&fa[..1], "Alice").unwrap();
        assert_eq!(lib.tags(a), ["People|Alice"]);
        assert_eq!(lib.cat.person("alice").unwrap(), alice, "by display name");
        assert_eq!(lib.cat.person("people|ALICE").unwrap(), alice, "by path");
        assert_eq!(person_path("Family|Bob").unwrap(), "People|Family|Bob");
        assert_eq!(lib.search("people|alice"), 1);
        assert_eq!(lib.search("person:alice"), 1);
        assert!(lib.cat.applied_tag_paths().unwrap().contains(&"People|Alice".to_owned()));

        let boxes = lib.cat.face_boxes(a).unwrap();
        assert_eq!(boxes.len(), 2);
        assert_eq!(boxes[0].person, Some((alice, "Alice".to_owned())));
        assert_eq!(boxes[0].region, Region { x: 0.0, y: 0.1, w: 0.2, h: 0.2 });
        assert_eq!((boxes[1].person.clone(), boxes[1].status), (None, FaceStatus::Unreviewed));
    }

    #[test]
    fn rename_merge_and_delete_keep_tags_in_step() {
        let mut lib = Lib::new();
        let (a, fa) = lib.photo("a.jpg", 2);
        let (b, fb) = lib.photo("b.jpg", 1);
        let al = lib.cat.name_faces(&fa[..1], "Al").unwrap();
        let alice = lib.cat.name_faces(&fb, "Alice").unwrap();
        lib.cat.name_faces(&fa[1..], "Bob").unwrap();

        // Renaming onto an existing person merges.
        assert_eq!(lib.cat.rename_person(al, "Alice").unwrap(), alice);
        assert_eq!(lib.tags(a), ["People|Alice", "People|Bob"]);
        assert_eq!(lib.cat.faces(FaceFilter::Confirmed(alice), FaceOrder::Asset).unwrap().len(), 2);
        assert!(lib.cat.find_tag("People|Al").unwrap().is_none());

        // A plain rename.
        assert_eq!(lib.cat.rename_person(alice, "Alicia").unwrap(), alice);
        assert_eq!(lib.tags(b), ["People|Alicia"]);
        assert!(lib.cat.needing_sync().unwrap().contains(&b));

        // Deleting un-names the faces and drops the tag.
        lib.cat.delete_person(alice).unwrap();
        assert_eq!(lib.tags(a), ["People|Bob"]);
        assert!(lib.tags(b).is_empty());
        assert_eq!(lib.cat.faces(FaceFilter::Unreviewed, FaceOrder::Asset).unwrap().len(), 2);
        assert!(lib.cat.delete_person(lib.cat.tag("Dog", TagKind::Object).unwrap()).is_err());
    }

    #[test]
    fn removing_the_tag_unnames_the_faces_and_regions_follow_the_remaining_face() {
        let mut lib = Lib::new();
        let (a, f) = lib.photo("a.jpg", 2);
        let alice = lib.cat.name_faces(&f, "Alice").unwrap();
        // The tag stays while another face is Alice, with that face's box.
        lib.cat.unreview_faces(&f[1..]).unwrap();
        let tags = lib.cat.tags_of(a).unwrap();
        assert_eq!(tags[0].region, Some(Region { x: 0.0, y: 0.1, w: 0.2, h: 0.2 }));
        lib.cat.remove_tag(a, alice).unwrap();
        assert!(lib.tags(a).is_empty());
        assert!(lib.cat.faces(FaceFilter::Confirmed(alice), FaceOrder::Asset).unwrap().is_empty());
    }

    #[test]
    fn name_box_matches() {
        let people = ["People|Alice", "People|Family|Alice", "People|Malice", "People|Bob", "People|alina"];
        assert_eq!(matching_people(&people, " ALI", 10), ["People|Alice", "People|Family|Alice", "alina", "Malice"]);
        assert_eq!(matching_people(&people, "ali", 2), ["People|Alice", "People|Family|Alice"]);
        assert_eq!(matching_people(&people, "bo", 10), ["Bob"]);
        assert!(matching_people(&people, "  ", 10).is_empty());
    }

    #[test]
    fn backfill_restores_missing_person_tags() {
        let mut lib = Lib::new();
        let (a, f) = lib.photo("a.jpg", 1);
        lib.cat.name_faces(&f, "Alice").unwrap();
        // As an old catalog would be: the face named, no tag.
        lib.cat.conn.execute("DELETE FROM asset_tags", []).unwrap();
        assert!(lib.tags(a).is_empty());
        assert_eq!(lib.cat.sync_person_tags().unwrap(), 1);
        assert_eq!(lib.tags(a), ["People|Alice"]);
        assert_eq!(lib.cat.sync_person_tags().unwrap(), 0);
    }

    #[test]
    fn a_version_3_catalog_is_backfilled_on_open() {
        let dir = tempfile::tempdir().unwrap();
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(super::super::SCHEMA).unwrap();
        conn.execute_batch(super::super::faces::SCHEMA).unwrap();
        conn.execute_batch(super::super::runs::SCHEMA).unwrap();
        conn.pragma_update(None, "user_version", 3).unwrap();
        conn.execute_batch(
            "INSERT INTO assets (id, path) VALUES (1, 'scan/photo-0001-front.png'), (2, 'scan/photo-0001-back.png');
             INSERT INTO tags (id, path, kind) VALUES (1, 'People', 'person'), (2, 'People|Alice', 'person');
             INSERT INTO faces (asset_id, x, y, w, h, score, embedding, person) VALUES (1, 0.1, 0.2, 0.3, 0.4, 0.9, x'', 2);",
        )
        .unwrap();
        let cat = Catalog::init(conn, dir.path()).unwrap();
        let tags = cat.tags_of(AssetId(1)).unwrap();
        assert_eq!(tags.len(), 1);
        assert_eq!((tags[0].path.as_str(), tags[0].region), ("People|Alice", Some(Region { x: 0.1, y: 0.2, w: 0.3, h: 0.4 })));
        assert_eq!(cat.back_of(AssetId(1)).unwrap(), Some(AssetId(2)));
    }
}
