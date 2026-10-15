//! The library catalog: a SQLite database of a library's assets and their tags, so tags
//! can be searched without touching the files.
//!
//! - Tags are hierarchical paths, `|`-separated (`People|Family|Alice`, `Scenes|Beach`),
//!   the same convention as XMP `lr:hierarchicalSubject`, plus a [`TagKind`] saying what
//!   the tag denotes. Paths are unique ignoring ASCII case.
//! - A tag is put on an asset either by the user or by a detector (scene / object
//!   classifier, face recognizer, …) with a confidence, optionally on a [`Region`] of the
//!   image. The user has the last word: a user removal of a tag a detector proposed is kept
//!   as a rejection, so running the detector again doesn't bring the tag back.
//! - Every change to an asset's tags marks it for [`Catalog::sync_sidecars`], which writes
//!   the tags into the file's XMP sidecar (see [`xmp`]).
//! - Assets are stored by path relative to the library root, `/`-separated, so a library
//!   can move as a whole. The database belongs inside the library
//!   ([`Catalog::default_path`]), unlike the thumbnail cache: it holds user data.
//!
//! Faces found in the images, and the review that names them, are in [`faces`].
//!
//! See [`query`] for the search syntax. Each thread opens its own [`Catalog`]; WAL lets
//! readers run alongside a writer.

pub mod faces;
pub mod flags;
pub mod pairs;
pub mod people;
pub mod query;
pub mod runs;
pub mod semantic;
pub mod similar;
pub mod trash;
pub mod xmp;

use std::fmt;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use rusqlite::{params, Connection, OptionalExtension};

/// 1: assets and tags. 2: faces. 3: file states and pipeline runs. 4: front/back pairs,
/// image signatures, the trash, and person tags backfilled from confirmed faces. 5: image
/// embeddings for search by description. 6: faces boxed by hand.
const SCHEMA_VERSION: i32 = 6;

const SCHEMA_4: &str = "
-- While the asset is in the trash: where it was (relative to the root) and when it was
-- trashed (seconds since the epoch). Its path is then its place in the trash.
ALTER TABLE assets ADD COLUMN trashed_from TEXT;
ALTER TABLE assets ADD COLUMN trashed_at INTEGER;
";

const SCHEMA: &str = "
CREATE TABLE assets (
    id INTEGER PRIMARY KEY,
    -- Relative to the library root, '/'-separated.
    path TEXT NOT NULL UNIQUE,
    -- How many times the tags changed since they were last written to the sidecar.
    needs_sync INTEGER NOT NULL DEFAULT 0,
    -- The tag paths last written to the sidecar, one per line.
    sidecar_tags TEXT NOT NULL DEFAULT ''
);
CREATE TABLE tags (
    id INTEGER PRIMARY KEY,
    -- 'People|Family|Alice'.
    path TEXT NOT NULL UNIQUE COLLATE NOCASE,
    kind TEXT NOT NULL CHECK (kind IN ('keyword', 'scene', 'object', 'person', 'place', 'event'))
);
CREATE TABLE asset_tags (
    asset_id INTEGER NOT NULL REFERENCES assets(id) ON DELETE CASCADE,
    tag_id INTEGER NOT NULL REFERENCES tags(id) ON DELETE CASCADE,
    -- 'user', or the detector that proposed the tag.
    source TEXT NOT NULL,
    -- Detector score in 0..=1; NULL for the user.
    confidence REAL,
    -- Doesn't apply. User rows: the user rejected the tag, whatever detectors say. Detector
    -- rows: the user has ruled on this tag (their row says it all). Kept up by triggers.
    hidden INTEGER NOT NULL DEFAULT 0,
    -- Optional region, top-left corner and size as fractions of the (orientation-corrected) image.
    x REAL, y REAL, w REAL, h REAL,
    PRIMARY KEY (asset_id, tag_id, source)
) WITHOUT ROWID;
-- Covers searches: (tag_id, confidence) plus the primary key.
CREATE INDEX asset_tags_by_tag ON asset_tags (tag_id, confidence) WHERE NOT hidden;

CREATE VIEW applied_tags AS SELECT * FROM asset_tags WHERE NOT hidden;

CREATE TRIGGER asset_tags_user_insert AFTER INSERT ON asset_tags WHEN NEW.source = 'user'
BEGIN UPDATE asset_tags SET hidden = 1 WHERE asset_id = NEW.asset_id AND tag_id = NEW.tag_id AND source <> 'user'; END;
CREATE TRIGGER asset_tags_user_delete AFTER DELETE ON asset_tags WHEN OLD.source = 'user'
BEGIN UPDATE asset_tags SET hidden = 0 WHERE asset_id = OLD.asset_id AND tag_id = OLD.tag_id AND source <> 'user'; END;
CREATE TRIGGER asset_tags_detector_insert AFTER INSERT ON asset_tags WHEN NEW.source <> 'user' AND EXISTS (
    SELECT 1 FROM asset_tags WHERE asset_id = NEW.asset_id AND tag_id = NEW.tag_id AND source = 'user')
BEGIN UPDATE asset_tags SET hidden = 1 WHERE asset_id = NEW.asset_id AND tag_id = NEW.tag_id AND source = NEW.source; END;

CREATE TRIGGER asset_tags_insert AFTER INSERT ON asset_tags
BEGIN UPDATE assets SET needs_sync = needs_sync + 1 WHERE id = NEW.asset_id; END;
CREATE TRIGGER asset_tags_update AFTER UPDATE ON asset_tags
BEGIN UPDATE assets SET needs_sync = needs_sync + 1 WHERE id IN (OLD.asset_id, NEW.asset_id); END;
CREATE TRIGGER asset_tags_delete AFTER DELETE ON asset_tags
BEGIN UPDATE assets SET needs_sync = needs_sync + 1 WHERE id = OLD.asset_id; END;
CREATE TRIGGER tags_update AFTER UPDATE ON tags
BEGIN UPDATE assets SET needs_sync = needs_sync + 1 WHERE id IN (SELECT asset_id FROM asset_tags WHERE tag_id = NEW.id); END;
";

/// The source name of user rows; detectors may not use it.
const USER: &str = "user";

#[derive(Debug)]
pub enum Error {
    Sql(rusqlite::Error),
    /// The search string doesn't parse; the message says why.
    Query(String),
    /// A path outside the library root or not valid UTF-8.
    Path(PathBuf),
    /// An empty tag path or segment, a kind that clashes with the existing tag's, a
    /// detector named `user`, or a confidence outside 0..=1.
    Invalid(String),
    /// The database was written by a newer Archive Studio.
    NewerSchema(i32),
    /// A file operation (trash, restore) failed; the message says on what.
    Io(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Self::Sql(e) => write!(f, "catalog database: {e}"),
            Self::Query(msg) => write!(f, "search: {msg}"),
            Self::Path(path) => write!(f, "{} is not a UTF-8 path inside the library", path.display()),
            Self::Invalid(msg) => f.write_str(msg),
            Self::NewerSchema(v) => write!(f, "catalog schema {v} is newer than this build supports ({SCHEMA_VERSION})"),
            Self::Io(msg) => f.write_str(msg),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Sql(e) => Some(e),
            _ => None,
        }
    }
}

impl From<rusqlite::Error> for Error {
    fn from(e: rusqlite::Error) -> Self {
        Self::Sql(e)
    }
}

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct AssetId(pub i64);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TagId(pub i64);

/// What a tag denotes. Decides how it is written to metadata (a person with a region
/// becomes an XMP face region, a place can fill in location fields, …).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TagKind {
    /// Free-form, e.g. `Projects|Grandma's album`.
    Keyword,
    /// What the whole picture shows: beach, birthday party, kitchen.
    Scene,
    /// A thing in the picture: dog, car, cake.
    Object,
    Person,
    Place,
    Event,
}

impl TagKind {
    pub const ALL: [Self; 6] = [Self::Keyword, Self::Scene, Self::Object, Self::Person, Self::Place, Self::Event];

    /// The stored name, also the `kind:` prefix in searches.
    pub fn name(self) -> &'static str {
        match self {
            Self::Keyword => "keyword",
            Self::Scene => "scene",
            Self::Object => "object",
            Self::Person => "person",
            Self::Place => "place",
            Self::Event => "event",
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.name().eq_ignore_ascii_case(name))
    }
}

/// Part of an image: top-left corner and size, as fractions of the orientation-corrected
/// image's width and height.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Region {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

/// Who put a tag on an asset.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Source {
    User,
    /// A detector, named with its model, e.g. `faces/arcface-r100`.
    Detector(String),
}

/// A tag a detector found, for [`Catalog::set_detections`].
#[derive(Clone, Debug)]
pub struct Detection {
    pub tag: TagId,
    /// In 0..=1.
    pub confidence: f32,
    pub region: Option<Region>,
}

/// A tag that applies to an asset (one per source that proposed it).
#[derive(Clone, Debug)]
pub struct AppliedTag {
    pub tag: TagId,
    pub path: String,
    pub kind: TagKind,
    pub source: Source,
    /// `None` for the user.
    pub confidence: Option<f32>,
    pub region: Option<Region>,
}

/// Detector tags below this confidence don't match searches by default.
pub const DEFAULT_MIN_CONFIDENCE: f32 = 0.5;

pub struct Catalog {
    conn: Connection,
    /// The root made absolute, and canonicalized: asset paths are accepted under either.
    root: PathBuf,
    canonical_root: PathBuf,
}

/// Logs a change to the catalog with its outcome (debug; a failure at warn) and passes the
/// outcome on.
pub(crate) fn logged<T: std::fmt::Debug>(what: std::fmt::Arguments, result: Result<T>) -> Result<T> {
    match &result {
        Ok(value) => log::debug!("{what} -> {value:?}"),
        Err(e) => log::warn!("{what} failed: {e}"),
    }
    result
}

/// Face ids for the log: the first few, and how many there are.
pub(crate) fn face_ids(faces: &[faces::FaceId]) -> String {
    let shown: Vec<String> = faces.iter().take(16).map(|f| f.0.to_string()).collect();
    let more = if faces.len() > 16 { format!(", … ({} in all)", faces.len()) } else { String::new() };
    format!("[{}{more}]", shown.join(", "))
}

impl Catalog {
    /// `<root>/.archive-studio/catalog.sqlite`.
    pub fn default_path(root: &Path) -> PathBuf {
        root.join(".archive-studio").join("catalog.sqlite")
    }

    /// Opens the catalog at `db` for the library under `root`, creating it if needed.
    pub fn open(db: &Path, root: &Path) -> Result<Self> {
        if let Some(dir) = db.parent() {
            std::fs::create_dir_all(dir).map_err(|_| Error::Path(dir.to_owned()))?;
        }
        Self::init(Connection::open(db)?, root)
    }

    /// Switches the database to WAL (a no-op once it is). Switching a new database fails
    /// at once, without the busy handler, while another connection is opening it too (a
    /// host opens several when it opens a new library), so retry for as long as the busy
    /// timeout would wait.
    fn use_wal(conn: &Connection) -> Result<()> {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            match conn.pragma_update_and_check(None, "journal_mode", "WAL", |row| row.get::<_, String>(0)) {
                Ok(_) => return Ok(()),
                Err(rusqlite::Error::SqliteFailure(e, _))
                    if e.code == rusqlite::ErrorCode::DatabaseBusy && std::time::Instant::now() < deadline =>
                {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(e) => return Err(e.into()),
            }
        }
    }

    fn init(mut conn: Connection, root: &Path) -> Result<Self> {
        conn.busy_timeout(Duration::from_secs(5))?;
        Self::use_wal(&conn)?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", true)?;
        // Broad searches probe each asset's tags in path order, i.e. all over the table: keep
        // it in memory (200k assets × 6 tags is ~50 MB) rather than re-reading from the OS.
        conn.pragma_update(None, "cache_size", -128 * 1024)?;
        conn.pragma_update(None, "mmap_size", 1i64 << 30)?;
        // Up to date: skip the write lock, which another connection may hold for a while (a
        // migration's backfill, a batch of pipeline writes) - opens on a UI thread must not wait.
        let current: i32 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if current == SCHEMA_VERSION {
            return Self::opened(conn, root);
        }
        // Taking the write lock first: another thread may be creating the schema too.
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        // Each version's additions, applied in turn from the database's version.
        let version: i32 = tx.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if version > SCHEMA_VERSION {
            log::error!("catalog {}: schema {version} is newer than this build ({SCHEMA_VERSION})", root.display());
            return Err(Error::NewerSchema(version));
        }
        if version < SCHEMA_VERSION {
            match version {
                0 => log::info!("catalog for {}: creating (schema {SCHEMA_VERSION})", root.display()),
                _ => log::info!("catalog for {}: migrating schema {version} -> {SCHEMA_VERSION}", root.display()),
            }
        }
        for (added_in, schema) in [(1, SCHEMA), (2, faces::SCHEMA), (3, runs::SCHEMA)] {
            if version < added_in {
                tx.execute_batch(schema)?;
            }
        }
        if version < 4 {
            tx.execute_batch(SCHEMA_4)?;
            tx.execute_batch(pairs::SCHEMA)?;
            tx.execute_batch(similar::SCHEMA)?;
            // One-time backfills: libraries named people and scanned backs before these
            // were kept in the catalog.
            let tagged = people::backfill_person_tags(&tx)?;
            let paired = pairs::pair_all_by_name(&tx)?;
            log::info!("catalog migration to schema 4: {tagged:?} person tags backfilled, {paired:?} front/back pairs");
        }
        if version < 5 {
            tx.execute_batch(semantic::SCHEMA)?;
        }
        if version < 6 {
            tx.execute_batch(faces::SCHEMA_6)?;
        }
        if version < SCHEMA_VERSION {
            tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        }
        tx.commit()?;
        Self::opened(conn, root)
    }

    fn opened(conn: Connection, root: &Path) -> Result<Self> {
        let root = std::path::absolute(root).map_err(|_| Error::Path(root.to_owned()))?;
        let canonical_root = root.canonicalize().map_err(|_| Error::Path(root.clone()))?;
        Ok(Self { conn, root, canonical_root })
    }

    /// The id of the asset at `path` (absolute, or relative to the current directory, like
    /// every path the catalog takes), adding it if it isn't in the catalog yet.
    pub fn asset(&self, path: &Path) -> Result<AssetId> {
        let rel = self.relative(path)?;
        let added = self.conn.prepare_cached("INSERT INTO assets (path) VALUES (?1) ON CONFLICT (path) DO NOTHING")?.execute([&rel])?;
        let id = AssetId(self.conn.prepare_cached("SELECT id FROM assets WHERE path = ?1")?.query_row([&rel], |row| row.get(0))?);
        if added > 0 {
            pairs::pair_by_name(&self.conn, id, &rel)?;
        }
        Ok(id)
    }

    /// The id of the asset at `path`, if the catalog has it.
    pub fn find_asset(&self, path: &Path) -> Result<Option<AssetId>> {
        let rel = self.relative(path)?;
        let mut stmt = self.conn.prepare_cached("SELECT id FROM assets WHERE path = ?1")?;
        Ok(stmt.query_row([&rel], |row| row.get(0)).optional()?.map(AssetId))
    }

    /// The asset's absolute path.
    pub fn asset_path(&self, asset: AssetId) -> Result<PathBuf> {
        let rel: String = self.conn.prepare_cached("SELECT path FROM assets WHERE id = ?1")?.query_row([asset.0], |row| row.get(0))?;
        Ok(self.root.join(rel))
    }

    /// Records that the file at `from` is now at `to`, keeping its tags.
    pub fn move_asset(&self, from: &Path, to: &Path) -> Result<()> {
        let (from, to) = (self.relative(from)?, self.relative(to)?);
        self.conn.prepare_cached("UPDATE assets SET path = ?2 WHERE path = ?1")?.execute([from, to])?;
        Ok(())
    }

    /// Forgets the asset at `path` and its tags (e.g. the file was deleted).
    pub fn remove_asset(&self, path: &Path) -> Result<()> {
        let rel = self.relative(path)?;
        self.conn.prepare_cached("DELETE FROM assets WHERE path = ?1")?.execute([rel])?;
        Ok(())
    }

    /// The id of the tag at `path` (`People|Family|Alice`; segments are trimmed), creating
    /// it as `kind` if needed, along with any missing ancestors (so `People|Family` can be
    /// browsed and renamed). An existing tag of another kind is an error.
    pub fn tag(&self, path: &str, kind: TagKind) -> Result<TagId> {
        let path = normalize_tag(path)?;
        let mut insert = self.conn.prepare_cached("INSERT INTO tags (path, kind) VALUES (?1, ?2) ON CONFLICT (path) DO NOTHING")?;
        for (end, _) in path.match_indices('|') {
            insert.execute([&path[..end], kind.name()])?;
        }
        insert.execute([path.as_str(), kind.name()])?;
        let (id, path, existing): (i64, String, String) = self
            .conn
            .prepare_cached("SELECT id, path, kind FROM tags WHERE path = ?1")?
            .query_row([&path], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?;
        if existing != kind.name() {
            return Err(Error::Invalid(format!("tag {path} is a {existing}, not a {}", kind.name())));
        }
        Ok(TagId(id))
    }

    /// The tag at `path`, if it exists, with its stored spelling and kind.
    pub fn find_tag(&self, path: &str) -> Result<Option<(TagId, String, TagKind)>> {
        let path = normalize_tag(path)?;
        let mut stmt = self.conn.prepare_cached("SELECT id, path, kind FROM tags WHERE path = ?1")?;
        Ok(stmt.query_row([path], |row| Ok((TagId(row.get(0)?), row.get(1)?, kind_of(row.get_ref(2)?.as_str()?)))).optional()?)
    }

    /// Returns all distinct tag paths currently stored in the catalog.
    pub fn all_tags(&self) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare_cached("SELECT path FROM tags ORDER BY path COLLATE NOCASE")?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// The tags of `kind` with no tags under them (`People|Alice`, not the `People` that
    /// [`tag`](Self::tag) made on the way), by path.
    pub fn leaf_tags(&self, kind: TagKind) -> Result<Vec<(TagId, String)>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT g.id, g.path FROM tags g WHERE g.kind = ?1
                 AND NOT EXISTS (SELECT 1 FROM tags c WHERE lower(substr(c.path, 1, length(g.path) + 1)) = lower(g.path || '|'))
             ORDER BY g.path COLLATE NOCASE",
        )?;
        let rows = stmt.query_map([kind.name()], |row| Ok((TagId(row.get(0)?), row.get(1)?)))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Returns all distinct tags applied to at least one asset.
    pub fn applied_tag_paths(&self) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT DISTINCT g.path FROM tags g JOIN applied_tags t ON t.tag_id = g.id ORDER BY g.path COLLATE NOCASE"
        )?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Renames a tag and everything under it (`People|Fam` → `People|Family` also renames
    /// `People|Fam|Alice`). Fails, changing nothing, if a new path is already taken.
    pub fn rename_tag(&mut self, tag: TagId, new_path: &str) -> Result<()> {
        log::debug!("rename_tag {} -> {new_path:?}", tag.0);
        let new_path = normalize_tag(new_path)?;
        let tx = self.conn.transaction()?;
        let old: String = tx.query_row("SELECT path FROM tags WHERE id = ?1", [tag.0], |row| row.get(0))?;
        tx.execute(
            "UPDATE tags SET path = ?2 || substr(path, length(?1) + 1)
             WHERE path = ?1 OR (path > ?1 || '|' AND path < ?1 || '}')",
            [&old, &new_path],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// The user puts `tag` on `asset` (again, if they had removed it).
    pub fn add_tag(&self, asset: AssetId, tag: TagId, region: Option<Region>) -> Result<()> {
        let r = region.map(|r| [r.x, r.y, r.w, r.h]);
        self.conn
            .prepare_cached(
                "INSERT INTO asset_tags (asset_id, tag_id, source, hidden, x, y, w, h) VALUES (?1, ?2, 'user', 0, ?3, ?4, ?5, ?6)
                 ON CONFLICT DO UPDATE SET hidden = 0, x = excluded.x, y = excluded.y, w = excluded.w, h = excluded.h",
            )?
            .execute(params![asset.0, tag.0, r.map(|r| r[0]), r.map(|r| r[1]), r.map(|r| r[2]), r.map(|r| r[3])])?;
        Ok(())
    }

    /// The user puts `tag` on every one of `assets`, in one transaction: all of them or,
    /// on an error (e.g. an asset that doesn't exist), none.
    pub fn add_tag_to_all(&mut self, assets: &[AssetId], tag: TagId) -> Result<()> {
        let result = (|| {
            let tx = self.conn.transaction()?;
            {
                let mut insert = tx.prepare_cached(
                    "INSERT INTO asset_tags (asset_id, tag_id, source, hidden) VALUES (?1, ?2, 'user', 0)
                     ON CONFLICT DO UPDATE SET hidden = 0",
                )?;
                for asset in assets {
                    insert.execute([asset.0, tag.0])?;
                }
            }
            tx.commit()?;
            Ok(assets.len())
        })();
        logged(format_args!("add_tag_to_all tag {} on {} assets", tag.0, assets.len()), result).map(|_| ())
    }

    /// The user takes `tag` off `asset`. If a detector proposed it, that is remembered as a
    /// rejection, so the detector can't put it back.
    ///
    /// Taking a person tag off also un-names that person's faces on the asset (they go
    /// back to unreviewed), so faces and `People|…` tags never disagree.
    pub fn remove_tag(&mut self, asset: AssetId, tag: TagId) -> Result<()> {
        log::debug!("remove_tag {} from asset {}", tag.0, asset.0);
        let tx = self.conn.transaction()?;
        tx.execute(
            "UPDATE faces SET person = NULL, ignored = 0, suggested = NULL, similarity = NULL, cluster = NULL
             WHERE asset_id = ?1 AND person = ?2",
            [asset.0, tag.0],
        )?;
        let detected: bool = tx.query_row(
            "SELECT EXISTS (SELECT 1 FROM asset_tags WHERE asset_id = ?1 AND tag_id = ?2 AND source <> 'user')",
            [asset.0, tag.0],
            |row| row.get(0),
        )?;
        if detected {
            tx.execute(
                "INSERT INTO asset_tags (asset_id, tag_id, source, hidden) VALUES (?1, ?2, 'user', 1)
                 ON CONFLICT DO UPDATE SET hidden = 1, x = NULL, y = NULL, w = NULL, h = NULL",
                [asset.0, tag.0],
            )?;
        } else {
            tx.execute("DELETE FROM asset_tags WHERE asset_id = ?1 AND tag_id = ?2 AND source = 'user'", [asset.0, tag.0])?;
        }
        tx.commit()?;
        Ok(())
    }

    /// The user puts the tag at `path` on `asset`, creating it as `kind` if needed (see
    /// [`tag`](Self::tag)). For naming a face use [`name_faces`](Self::name_faces) instead,
    /// which keeps the face and its tag together.
    pub fn add_tag_path(&self, asset: AssetId, path: &str, kind: TagKind) -> Result<TagId> {
        let tag = self.tag(path, kind)?;
        self.add_tag(asset, tag, None)?;
        Ok(tag)
    }

    /// The user takes the tag at `path` off `asset` (see [`remove_tag`](Self::remove_tag)).
    /// False if there is no such tag.
    pub fn remove_tag_path(&mut self, asset: AssetId, path: &str) -> Result<bool> {
        let Some((tag, _, _)) = self.find_tag(path)? else { return Ok(false) };
        self.remove_tag(asset, tag)?;
        Ok(true)
    }

    /// Whether the asset is in the [trash](trash).
    pub fn is_trashed(&self, asset: AssetId) -> Result<bool> {
        let mut stmt = self.conn.prepare_cached("SELECT trashed_from IS NOT NULL FROM assets WHERE id = ?1")?;
        Ok(stmt.query_row([asset.0], |row| row.get(0))?)
    }

    /// Replaces everything `detector` found on `asset` with `found`, so re-running a
    /// detector (or a newer model under the same name) doesn't pile up stale rows.
    pub fn set_detections(&mut self, asset: AssetId, detector: &str, found: &[Detection]) -> Result<()> {
        if detector.is_empty() || detector == USER {
            return Err(Error::Invalid(format!("invalid detector name {detector:?}")));
        }
        if let Some(d) = found.iter().find(|d| !(0.0..=1.0).contains(&d.confidence)) {
            return Err(Error::Invalid(format!("confidence {} is outside 0..=1", d.confidence)));
        }
        let tx = self.conn.transaction()?;
        tx.execute("DELETE FROM asset_tags WHERE asset_id = ?1 AND source = ?2", params![asset.0, detector])?;
        {
            // A detector reporting the same tag twice (two dogs) keeps the more confident one.
            let mut insert = tx.prepare(
                "INSERT INTO asset_tags (asset_id, tag_id, source, confidence, x, y, w, h) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                 ON CONFLICT DO UPDATE SET confidence = excluded.confidence,
                     x = excluded.x, y = excluded.y, w = excluded.w, h = excluded.h
                 WHERE excluded.confidence > confidence",
            )?;
            for d in found {
                let r = d.region.map(|r| [r.x, r.y, r.w, r.h]);
                insert.execute(params![
                    asset.0,
                    d.tag.0,
                    detector,
                    d.confidence,
                    r.map(|r| r[0]),
                    r.map(|r| r[1]),
                    r.map(|r| r[2]),
                    r.map(|r| r[3])
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// The tags that apply to `asset`, whatever their confidence, by tag path.
    pub fn tags_of(&self, asset: AssetId) -> Result<Vec<AppliedTag>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT t.tag_id, g.path, g.kind, t.source, t.confidence, t.x, t.y, t.w, t.h
             FROM applied_tags t JOIN tags g ON g.id = t.tag_id
             WHERE t.asset_id = ?1 ORDER BY g.path, t.source",
        )?;
        let rows = stmt.query_map([asset.0], |row| {
            let source: String = row.get(3)?;
            let region = match (row.get(5)?, row.get(6)?, row.get(7)?, row.get(8)?) {
                (Some(x), Some(y), Some(w), Some(h)) => Some(Region { x, y, w, h }),
                _ => None,
            };
            Ok(AppliedTag {
                tag: TagId(row.get(0)?),
                path: row.get(1)?,
                kind: kind_of(row.get_ref(2)?.as_str()?),
                source: if source == USER { Source::User } else { Source::Detector(source) },
                confidence: row.get(4)?,
                region,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// The assets matching `query` (see [`query`]), by path. Detector tags count only from
    /// `min_confidence` up (see [`DEFAULT_MIN_CONFIDENCE`]); the user's always do.
    pub fn search(&self, query: &str, min_confidence: f32) -> Result<Vec<PathBuf>> {
        let expr = query::parse(query).map_err(Error::Query)?;
        let (condition, values) = query::to_sql(&expr, min_confidence);
        let sql = format!("SELECT a.path FROM assets a WHERE a.trashed_from IS NULL AND ({condition}) ORDER BY a.path");
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(values), |row| row.get::<_, String>(0))?;
        rows.map(|rel| Ok(self.root.join(rel?))).collect()
    }

    /// Assets whose tags changed since they were last written to their sidecars.
    pub fn needing_sync(&self) -> Result<Vec<AssetId>> {
        let mut stmt = self.conn.prepare_cached("SELECT id FROM assets WHERE needs_sync ORDER BY id")?;
        let rows = stmt.query_map([], |row| row.get(0).map(AssetId))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Writes the tags of every asset that [needs it](Self::needing_sync) to its XMP sidecar
    /// (`photo.jpg.xmp`), keeping whatever else the sidecar holds; see [`xmp`]. Detector
    /// tags are written from `min_confidence` up. An asset that fails (missing file,
    /// unreadable sidecar) is reported and left for next time.
    pub fn sync_sidecars(&self, min_confidence: f32) -> Result<SyncReport> {
        let pending: Vec<(i64, i64, String, String)> = {
            let mut stmt = self.conn.prepare_cached("SELECT id, needs_sync, path, sidecar_tags FROM assets WHERE needs_sync ORDER BY id")?;
            let rows = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)))?;
            rows.collect::<rusqlite::Result<_>>()?
        };
        let mut report = SyncReport::default();
        for (id, version, rel, written) in pending {
            let image = self.root.join(&rel);
            let tags = self.tags_to_write(AssetId(id), min_confidence)?;
            let ours: Vec<String> = written.lines().map(str::to_owned).collect();
            match write_sidecar(&image, &tags, &ours) {
                Ok(()) => {
                    // What the sidecar now holds is recorded either way; it only counts as in
                    // sync if the tags didn't change meanwhile.
                    let paths: Vec<&str> = tags.iter().map(|t| t.path.as_str()).collect();
                    self.conn
                        .prepare_cached(
                            "UPDATE assets SET sidecar_tags = ?3,
                                 needs_sync = CASE WHEN needs_sync = ?2 THEN 0 ELSE needs_sync END
                             WHERE id = ?1",
                        )?
                        .execute(params![id, version, paths.join("\n")])?;
                    report.written += 1;
                }
                Err(e) => report.failed.push((image, e)),
            }
        }
        Ok(report)
    }

    /// One entry per applied tag: the user's row if there is one, else the most confident
    /// detector's (with any detector's region if it has none).
    fn tags_to_write(&self, asset: AssetId, min_confidence: f32) -> Result<Vec<xmp::Tag>> {
        let rank = |t: &AppliedTag| t.confidence.unwrap_or(f32::INFINITY);
        let mut rows: Vec<AppliedTag> =
            self.tags_of(asset)?.into_iter().filter(|t| t.confidence.is_none_or(|c| c >= min_confidence)).collect();
        // Within each tag, best first.
        rows.sort_by(|a, b| a.path.cmp(&b.path).then(rank(b).total_cmp(&rank(a))));
        let mut out: Vec<xmp::Tag> = Vec::new();
        for t in rows {
            match out.last_mut() {
                Some(last) if last.path == t.path => last.region = last.region.or(t.region),
                _ => out.push(xmp::Tag { path: t.path, kind: t.kind, region: t.region }),
            }
        }
        Ok(out)
    }

    /// `path` (absolute, or relative to the current directory) relative to the library
    /// root, `/`-separated.
    fn relative(&self, path: &Path) -> Result<String> {
        let bad = || Error::Path(path.to_owned());
        let abs = std::path::absolute(path).map_err(|_| bad())?;
        let plain = |rel: &Path| rel.components().all(|c| matches!(c, Component::Normal(_) | Component::CurDir));
        let rel = match abs.strip_prefix(&self.root).or_else(|_| abs.strip_prefix(&self.canonical_root)) {
            Ok(rel) if plain(rel) => rel.to_owned(),
            // Reached through a symlink or `..`: resolve the directory (the file itself may
            // not exist yet).
            _ => {
                let (dir, name) = (abs.parent().ok_or_else(bad)?, abs.file_name().ok_or_else(bad)?);
                let dir = dir.canonicalize().map_err(|_| bad())?;
                dir.strip_prefix(&self.canonical_root).map_err(|_| bad())?.join(name)
            }
        };
        let mut parts = Vec::new();
        for component in rel.components() {
            match component {
                Component::Normal(part) => parts.push(part.to_str().ok_or_else(bad)?),
                Component::CurDir => {}
                _ => return Err(bad()),
            }
        }
        if parts.is_empty() {
            return Err(bad());
        }
        Ok(parts.join("/"))
    }
}

/// Result of [`Catalog::sync_sidecars`].
#[derive(Debug, Default)]
pub struct SyncReport {
    pub written: usize,
    /// Each asset that couldn't be synced, and why.
    pub failed: Vec<(PathBuf, String)>,
}

fn write_sidecar(image: &Path, tags: &[xmp::Tag], ours: &[String]) -> std::result::Result<(), String> {
    if !image.is_file() {
        return Err("file not found".into());
    }
    let sidecar = xmp::sidecar_path(image);
    let existing = match std::fs::read(&sidecar) {
        Ok(bytes) => Some(String::from_utf8(bytes).map_err(|_| format!("{} is not UTF-8", sidecar.display()))?),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(format!("{}: {e}", sidecar.display())),
    };
    let geometry = if tags.iter().any(|t| t.region.is_some()) { xmp::Geometry::of(image) } else { None };
    match xmp::merge(existing.as_deref(), tags, geometry, ours).map_err(|e| format!("{}: {e}", sidecar.display()))? {
        xmp::Outcome::Write(text) => {
            // Write-then-rename, so a crash never leaves half a sidecar.
            let mut tmp = sidecar.clone().into_os_string();
            tmp.push(".tmp");
            std::fs::write(&tmp, text)
                .and_then(|()| std::fs::rename(&tmp, &sidecar))
                .map_err(|e| format!("{}: {e}", sidecar.display()))
        }
        xmp::Outcome::Delete => std::fs::remove_file(&sidecar).map_err(|e| format!("{}: {e}", sidecar.display())),
        xmp::Outcome::Nothing => Ok(()),
    }
}

/// `path` with each segment trimmed; an error if it or any segment is empty, or it has
/// control characters.
fn normalize_tag(path: &str) -> Result<String> {
    let segments: Vec<&str> = path.split('|').map(str::trim).collect();
    if segments.iter().any(|s| s.is_empty()) || path.chars().any(char::is_control) {
        return Err(Error::Invalid(format!("invalid tag path {path:?}")));
    }
    Ok(segments.join("|"))
}

fn kind_of(name: &str) -> TagKind {
    // The CHECK constraint keeps other names out.
    TagKind::from_name(name).unwrap_or(TagKind::Keyword)
}

#[cfg(test)]
impl Catalog {
    fn open_in_memory(root: &Path) -> Self {
        Self::init(Connection::open_in_memory().unwrap(), root).unwrap()
    }

    /// As if the asset's sidecar had just been written.
    fn mark_synced(&self, asset: AssetId) {
        self.conn.execute("UPDATE assets SET needs_sync = 0 WHERE id = ?1", [asset.0]).unwrap();
    }
}

#[cfg(test)]
mod tests;
