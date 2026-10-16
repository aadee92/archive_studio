//! The undo journal: what the user changed, kept in the catalog so any front end can undo
//! and redo it, after a restart too.
//!
//! - A change is one user operation ([`Catalog::journaled`], which [`ops`](crate::ops)
//!   wraps around each one): while it runs, temporary triggers on this connection record
//!   every row it inserts, deletes or updates in the user's tables ([`JOURNALED`]: tags,
//!   tags on assets, faces, their crops and rejections, people's covers, front/back pairs),
//!   with the rows the catalog's triggers and foreign keys change along with them. An
//!   update keeps only the columns it changed, plus the key. What other connections write
//!   meanwhile (the pipeline's) isn't recorded.
//! - Undo replays a change's rows backwards, each inverted; redo replays them forwards.
//!   Changes to tags on assets mark them for sidecar sync again, as any change does. A tag
//!   the change created is only dropped by an undo while nothing else uses it by then.
//! - A change can also carry a [`FileAction`] for what isn't catalog rows: moving to the
//!   trash and back, turning a picture. The front end's operation performs it
//!   ([`ops::undo`](crate::ops::undo)) before [`Catalog::apply_change`] marks it done.
//! - A stack: [`next_undo`](Catalog::next_undo) is the latest change not undone,
//!   [`next_redo`](Catalog::next_redo) the earliest undone one; recording a change drops
//!   the undone ones. The last [`KEEP`] changes are kept. Deleting from the trash for good
//!   isn't undoable, and drops the changes that touched what was deleted.

use std::cell::Cell;
use std::collections::HashMap;

use rusqlite::types::Value;
use rusqlite::{params, params_from_iter, OptionalExtension};

use super::runs::{self, RunStatus};
use super::{AssetId, Catalog, Result};

/// Schema 8: the undo journal.
pub(super) const SCHEMA_8: &str = "
CREATE TABLE undo_changes (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    -- What the user did, for a menu or a notice: 'Tag 12 items Holidays'.
    description TEXT NOT NULL,
    -- When (ms since the epoch).
    at INTEGER NOT NULL,
    -- Undone, waiting to be redone.
    undone INTEGER NOT NULL DEFAULT 0,
    -- Besides rows: 'trash', 'restore' or 'orient' (see FileAction), with its assets
    -- (space-separated ids) and, for 'orient', the clockwise turn.
    action TEXT CHECK (action IN ('trash', 'restore', 'orient')),
    targets TEXT NOT NULL DEFAULT '',
    cw INTEGER
);
-- Every asset a change touched, to report after an undo and to forget changes of assets
-- deleted for good.
CREATE TABLE undo_assets (
    change_id INTEGER NOT NULL REFERENCES undo_changes(id) ON DELETE CASCADE,
    asset_id INTEGER NOT NULL,
    PRIMARY KEY (change_id, asset_id)
) WITHOUT ROWID;
CREATE INDEX undo_assets_by_asset ON undo_assets (asset_id);
-- The rows a change wrote, in order.
CREATE TABLE undo_rows (
    change_id INTEGER NOT NULL REFERENCES undo_changes(id) ON DELETE CASCADE,
    seq INTEGER NOT NULL,
    tbl TEXT NOT NULL,
    op TEXT NOT NULL CHECK (op IN ('insert', 'delete', 'update')),
    PRIMARY KEY (change_id, seq)
) WITHOUT ROWID;
-- Their values, as stored (no type affinity): all columns of an inserted or deleted row,
-- the key and the changed columns of an updated one.
CREATE TABLE undo_cells (
    change_id INTEGER NOT NULL REFERENCES undo_changes(id) ON DELETE CASCADE,
    seq INTEGER NOT NULL,
    col TEXT NOT NULL,
    old,
    new,
    PRIMARY KEY (change_id, seq, col)
) WITHOUT ROWID;
-- An 'orient' change: the orient stage's run on each asset turned, before and after.
CREATE TABLE undo_runs (
    change_id INTEGER NOT NULL REFERENCES undo_changes(id) ON DELETE CASCADE,
    asset_id INTEGER NOT NULL,
    side TEXT NOT NULL CHECK (side IN ('before', 'after')),
    version TEXT NOT NULL,
    status TEXT NOT NULL,
    value INTEGER,
    detail TEXT NOT NULL,
    PRIMARY KEY (change_id, asset_id, side)
) WITHOUT ROWID;
";

/// How many changes the journal keeps.
pub const KEEP: usize = 200;

/// The tables a change is recorded in, each with the columns an update records besides its
/// key (to tell which assets it touched).
pub const JOURNALED: [(&str, &[&str]); 7] = [
    ("tags", &[]),
    ("asset_tags", &[]),
    ("faces", &["asset_id"]),
    ("face_crops", &[]),
    ("face_rejections", &[]),
    ("person_covers", &[]),
    ("asset_pairs", &["back"]),
];

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ChangeId(pub i64);

/// A change in the journal.
#[derive(Clone, Debug, PartialEq)]
pub struct Change {
    pub id: ChangeId,
    pub description: String,
    /// When it was made, ms since the epoch.
    pub at: i64,
    /// Undone (it can be redone).
    pub undone: bool,
}

/// What a change did besides catalog rows, which the front end's operation undoes and redoes
/// ([`ops::undo`](crate::ops::undo)).
#[derive(Clone, Debug, PartialEq)]
pub enum FileAction {
    /// These assets (and their backs) went to the trash: undone by restoring them.
    Trash(Vec<AssetId>),
    /// These came back from the trash: undone by trashing them again.
    Restore(Vec<AssetId>),
    /// The pictures were turned `cw` degrees clockwise (0: not turned, a proposed rotation
    /// was settled as it is), and their orient runs changed.
    Orient { cw: u16, turned: Vec<Turned> },
}

/// A picture an [`FileAction::Orient`] turned: its orient run before and after.
#[derive(Clone, Debug, PartialEq)]
pub struct Turned {
    pub asset: AssetId,
    pub before: Option<RunState>,
    pub after: Option<RunState>,
}

/// A stage's run on an asset, as [`Catalog::set_run_state`] puts it back.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunState {
    pub version: String,
    pub status: RunStatus,
    pub value: Option<i64>,
    pub detail: String,
}

/// A journaled table: its columns and primary key.
struct Table {
    name: &'static str,
    columns: Vec<String>,
    key: Vec<String>,
}

/// Per connection: whether the capture triggers are installed, and whether a change is
/// being recorded.
#[derive(Default)]
pub(super) struct Capture {
    installed: Cell<bool>,
    active: Cell<bool>,
}

fn quoted(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

impl Catalog {
    /// Runs the user operation `f`, recording what it changes in the catalog as one undoable
    /// change called `description` (`Tag 12 items Holidays`). A change recorded when `f`
    /// fails holds what it did before failing (nothing, if it rolled back). Inside another
    /// journaled operation, `f` is part of that one.
    pub fn journaled<T, E: From<super::Error>>(
        &mut self,
        description: &str,
        f: impl FnOnce(&mut Self) -> std::result::Result<T, E>,
    ) -> std::result::Result<T, E> {
        if self.capture.active.get() {
            return f(self);
        }
        self.start_capture()?;
        self.capture.active.set(true);
        let result = f(self);
        self.capture.active.set(false);
        let recorded = self.conn.execute("UPDATE undo_capture_on SET active = 0", []).map_err(super::Error::from).and_then(|_| self.record(description, None, &[]));
        if let Err(e) = recorded {
            log::warn!("journal: recording {description:?} failed: {e}");
        }
        result
    }

    /// Records a change that is only a [`FileAction`] (it wrote no journaled rows), touching
    /// `assets` besides the action's own.
    pub fn record_action(&self, description: &str, action: &FileAction, assets: &[AssetId]) -> Result<ChangeId> {
        self.install_capture()?;
        self.conn.execute_batch("DELETE FROM undo_capture_rows; DELETE FROM undo_capture_cells;")?;
        let id = self.record(description, Some(action), assets)?;
        Ok(id.expect("a change with an action is always recorded"))
    }

    /// The latest change that isn't undone, with its file action.
    pub fn next_undo(&self) -> Result<Option<(Change, Option<FileAction>)>> {
        self.next_change("undone = 0 ORDER BY id DESC")
    }

    /// The earliest undone change (undone last), with its file action.
    pub fn next_redo(&self) -> Result<Option<(Change, Option<FileAction>)>> {
        self.next_change("undone = 1 ORDER BY id")
    }

    fn next_change(&self, condition: &str) -> Result<Option<(Change, Option<FileAction>)>> {
        let sql = format!("SELECT id, description, at, undone, action, targets, cw FROM undo_changes WHERE {condition} LIMIT 1");
        let row = self
            .conn
            .prepare_cached(&sql)?
            .query_row([], |row| {
                Ok((
                    Change { id: ChangeId(row.get(0)?), description: row.get(1)?, at: row.get(2)?, undone: row.get(3)? },
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, Option<i64>>(6)?,
                ))
            })
            .optional()?;
        let Some((change, action, targets, cw)) = row else { return Ok(None) };
        let targets: Vec<AssetId> = targets.split_whitespace().filter_map(|t| t.parse().ok()).map(AssetId).collect();
        let action = match action.as_deref() {
            Some("trash") => Some(FileAction::Trash(targets)),
            Some("restore") => Some(FileAction::Restore(targets)),
            Some("orient") => Some(FileAction::Orient {
                cw: cw.unwrap_or(0) as u16,
                turned: targets
                    .into_iter()
                    .map(|asset| {
                        Ok(Turned { asset, before: self.journal_run(change.id, asset, "before")?, after: self.journal_run(change.id, asset, "after")? })
                    })
                    .collect::<Result<_>>()?,
            }),
            _ => None,
        };
        Ok(Some((change, action)))
    }

    fn journal_run(&self, change: ChangeId, asset: AssetId, side: &str) -> Result<Option<RunState>> {
        let mut stmt =
            self.conn.prepare_cached("SELECT version, status, value, detail FROM undo_runs WHERE change_id = ?1 AND asset_id = ?2 AND side = ?3")?;
        Ok(stmt
            .query_row(params![change.0, asset.0, side], |row| {
                Ok(RunState { version: row.get(0)?, status: runs::status_of(row.get_ref(1)?.as_str()?), value: row.get(2)?, detail: row.get(3)? })
            })
            .optional()?)
    }

    /// The assets `change` touched.
    pub fn change_assets(&self, change: ChangeId) -> Result<Vec<AssetId>> {
        let mut stmt = self.conn.prepare_cached("SELECT asset_id FROM undo_assets WHERE change_id = ?1 ORDER BY asset_id")?;
        let rows = stmt.query_map([change.0], |row| Ok(AssetId(row.get(0)?)))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Undoes (`undo`) or redoes the catalog rows of `change` and marks it so, in one
    /// transaction; its file action, if any, must have been performed first. Fails, changing
    /// nothing, if `change` isn't the next one that way, or its rows clash with what changed
    /// since (e.g. an asset it touched was deleted). Returns the assets it touched.
    pub fn apply_change(&self, change: ChangeId, undo: bool) -> Result<Vec<AssetId>> {
        let result = self.apply_change_inner(change, undo);
        super::logged(format_args!("journal: {} change {}", if undo { "undo" } else { "redo" }, change.0), result)
    }

    fn apply_change_inner(&self, change: ChangeId, undo: bool) -> Result<Vec<AssetId>> {
        let next = if undo { self.next_undo()? } else { self.next_redo()? };
        if next.as_ref().map(|(c, _)| c.id) != Some(change) {
            return Err(super::Error::Invalid(format!("change {} isn't the next to {}", change.0, if undo { "undo" } else { "redo" })));
        }
        let tables = self.journaled_tables()?;
        let tx = self.conn.unchecked_transaction()?;
        // Rows come back in an order foreign keys may not like midway (a face before its tag).
        tx.pragma_update(None, "defer_foreign_keys", true)?;
        let rows: Vec<(i64, String, String)> = {
            let sql = format!("SELECT seq, tbl, op FROM undo_rows WHERE change_id = ?1 ORDER BY seq {}", if undo { "DESC" } else { "ASC" });
            let mut stmt = tx.prepare(&sql)?;
            let rows = stmt.query_map([change.0], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?;
            rows.collect::<rusqlite::Result<_>>()?
        };
        let mut cells: HashMap<i64, Vec<(String, Value, Value)>> = HashMap::new();
        {
            let mut stmt = tx.prepare("SELECT seq, col, old, new FROM undo_cells WHERE change_id = ?1")?;
            let mut query = stmt.query([change.0])?;
            while let Some(row) = query.next()? {
                cells.entry(row.get(0)?).or_default().push((row.get(1)?, row.get(2)?, row.get(3)?));
            }
        }
        for (seq, tbl, op) in &rows {
            let Some(table) = tables.iter().find(|t| t.name == tbl) else { continue };
            let row = cells.remove(seq).unwrap_or_default();
            // Each cell as (value before, value after) in the direction replayed.
            let row: Vec<(String, Value, Value)> = if undo { row.into_iter().map(|(c, old, new)| (c, new, old)).collect() } else { row };
            let op = match (op.as_str(), undo) {
                ("insert", false) | ("delete", true) => "insert",
                ("insert", true) => "uncreate",
                ("delete", false) => "delete",
                _ => "update",
            };
            replay(&tx, table, op, &row)?;
        }
        tx.execute("UPDATE undo_changes SET undone = ?2 WHERE id = ?1", params![change.0, undo])?;
        tx.commit()?;
        self.change_assets(change)
    }

    /// Drops `change` from the journal, e.g. once it can't be undone.
    pub fn drop_change(&self, change: ChangeId) -> Result<()> {
        let result = self.conn.execute("DELETE FROM undo_changes WHERE id = ?1", [change.0]).map(|n| n > 0).map_err(Into::into);
        super::logged(format_args!("journal: drop change {}", change.0), result).map(|_| ())
    }

    /// The journal, latest first: up to `limit` changes, undone ones (which can be redone)
    /// included.
    pub fn history(&self, limit: usize) -> Result<Vec<Change>> {
        let mut stmt = self.conn.prepare_cached("SELECT id, description, at, undone FROM undo_changes ORDER BY id DESC LIMIT ?1")?;
        let rows = stmt.query_map([limit as i64], |row| {
            Ok(Change { id: ChangeId(row.get(0)?), description: row.get(1)?, at: row.get(2)?, undone: row.get(3)? })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Forgets the changes that touched `asset` (it is being deleted for good).
    pub(super) fn forget_changes_of(&self, asset: AssetId) -> Result<usize> {
        Ok(self.conn.execute("DELETE FROM undo_changes WHERE id IN (SELECT change_id FROM undo_assets WHERE asset_id = ?1)", [asset.0])?)
    }

    /// Puts the run of `stage` on `asset` back as `state` (on the file as it is now), or
    /// forgets it with `None`, so the stage runs again.
    pub fn set_run_state(&self, asset: AssetId, stage: &str, state: Option<&RunState>) -> Result<()> {
        let result = (|| {
            match state {
                Some(state) => {
                    let input = self.current_input(asset)?.unwrap_or_default();
                    let run = runs::Run { stage, version: &state.version, input: &input, status: state.status, value: state.value, detail: &state.detail };
                    self.set_run(asset, &run)
                }
                None => {
                    self.conn.execute("DELETE FROM stage_runs WHERE asset_id = ?1 AND stage = ?2", params![asset.0, stage])?;
                    Ok(())
                }
            }
        })();
        super::logged(format_args!("set_run_state asset {} {stage} {state:?}", asset.0), result)
    }

    /// The run of `stage` on `asset`, as [`set_run_state`](Self::set_run_state) takes it.
    pub fn run_state(&self, asset: AssetId, stage: &str) -> Result<Option<RunState>> {
        let mut stmt = self.conn.prepare_cached("SELECT version, status, value, detail FROM stage_runs WHERE asset_id = ?1 AND stage = ?2")?;
        Ok(stmt
            .query_row(params![asset.0, stage], |row| {
                Ok(RunState { version: row.get(0)?, status: runs::status_of(row.get_ref(1)?.as_str()?), value: row.get(2)?, detail: row.get(3)? })
            })
            .optional()?)
    }

    fn journaled_tables(&self) -> Result<Vec<Table>> {
        let mut tables = Vec::new();
        for (name, _) in JOURNALED {
            let mut stmt = self.conn.prepare(&format!("PRAGMA main.table_info({})", quoted(name)))?;
            let mut columns: Vec<(String, i64)> = stmt.query_map([], |row| Ok((row.get(1)?, row.get(5)?)))?.collect::<rusqlite::Result<_>>()?;
            let mut key: Vec<(String, i64)> = columns.iter().filter(|(_, pk)| *pk > 0).cloned().collect();
            key.sort_by_key(|(_, pk)| *pk);
            tables.push(Table { name, columns: columns.drain(..).map(|(c, _)| c).collect(), key: key.into_iter().map(|(c, _)| c).collect() });
        }
        Ok(tables)
    }

    /// Creates this connection's capture tables and triggers, once.
    fn install_capture(&self) -> Result<()> {
        if self.capture.installed.get() {
            return Ok(());
        }
        let mut sql = String::from(
            "CREATE TEMP TABLE IF NOT EXISTS undo_capture_on (active INTEGER NOT NULL);
             DELETE FROM undo_capture_on;
             INSERT INTO undo_capture_on VALUES (0);
             CREATE TEMP TABLE IF NOT EXISTS undo_capture_rows (seq INTEGER PRIMARY KEY, tbl TEXT NOT NULL, op TEXT NOT NULL);
             CREATE TEMP TABLE IF NOT EXISTS undo_capture_cells (seq INTEGER NOT NULL, col TEXT NOT NULL, old, new);\n",
        );
        let seq = "(SELECT max(seq) FROM temp.undo_capture_rows)";
        let when = "WHEN (SELECT active FROM temp.undo_capture_on)";
        for (table, (_, always)) in self.journaled_tables()?.iter().zip(JOURNALED) {
            let name = table.name;
            let target = quoted(name);
            let all = |side: &str| {
                let values: Vec<String> = table
                    .columns
                    .iter()
                    .map(|c| match side {
                        "NEW" => format!("({seq}, '{c}', NULL, NEW.{})", quoted(c)),
                        _ => format!("({seq}, '{c}', OLD.{}, NULL)", quoted(c)),
                    })
                    .collect();
                format!("INSERT INTO undo_capture_cells (seq, col, old, new) VALUES {};", values.join(", "))
            };
            let changed: Vec<String> = table
                .columns
                .iter()
                .map(|c| {
                    let q = quoted(c);
                    let filter = if table.key.contains(c) || always.contains(&c.as_str()) { String::new() } else { format!(" WHERE OLD.{q} IS NOT NEW.{q}") };
                    format!("INSERT INTO undo_capture_cells (seq, col, old, new) SELECT {seq}, '{c}', OLD.{q}, NEW.{q}{filter};")
                })
                .collect();
            sql += &format!(
                "DROP TRIGGER IF EXISTS temp.undo_{name}_insert;
                 CREATE TEMP TRIGGER undo_{name}_insert AFTER INSERT ON main.{target} {when}
                 BEGIN INSERT INTO undo_capture_rows (tbl, op) VALUES ('{name}', 'insert'); {} END;
                 DROP TRIGGER IF EXISTS temp.undo_{name}_delete;
                 CREATE TEMP TRIGGER undo_{name}_delete AFTER DELETE ON main.{target} {when}
                 BEGIN INSERT INTO undo_capture_rows (tbl, op) VALUES ('{name}', 'delete'); {} END;
                 DROP TRIGGER IF EXISTS temp.undo_{name}_update;
                 CREATE TEMP TRIGGER undo_{name}_update AFTER UPDATE ON main.{target} {when}
                 BEGIN INSERT INTO undo_capture_rows (tbl, op) VALUES ('{name}', 'update'); {} END;\n",
                all("NEW"),
                all("OLD"),
                changed.join(" "),
            );
        }
        self.conn.execute_batch(&sql)?;
        self.capture.installed.set(true);
        log::debug!("journal: capture triggers installed on this connection");
        Ok(())
    }

    fn start_capture(&self) -> Result<()> {
        self.install_capture()?;
        self.conn.execute_batch("DELETE FROM undo_capture_rows; DELETE FROM undo_capture_cells; UPDATE undo_capture_on SET active = 1;")?;
        Ok(())
    }

    /// Moves what was captured (and `action`) into the journal as a new change, unless that
    /// is nothing. Drops the undone changes and the oldest beyond [`KEEP`].
    fn record(&self, description: &str, action: Option<&FileAction>, assets: &[AssetId]) -> Result<Option<ChangeId>> {
        let tx = self.conn.unchecked_transaction()?;
        // Updates that changed nothing (a value set to what it was).
        tx.execute_batch(
            "DELETE FROM undo_capture_rows WHERE op = 'update' AND NOT EXISTS (
                 SELECT 1 FROM undo_capture_cells c WHERE c.seq = undo_capture_rows.seq AND c.old IS NOT c.new);
             DELETE FROM undo_capture_cells WHERE seq NOT IN (SELECT seq FROM undo_capture_rows);",
        )?;
        let rows: i64 = tx.query_row("SELECT count(*) FROM undo_capture_rows", [], |row| row.get(0))?;
        if rows == 0 && action.is_none() {
            tx.commit()?;
            log::debug!("journal: {description:?} changed nothing");
            return Ok(None);
        }
        tx.execute("DELETE FROM undo_changes WHERE undone = 1", [])?;
        let (kind, targets, cw) = match action {
            None => (None, Vec::new(), None),
            Some(FileAction::Trash(a)) => (Some("trash"), a.clone(), None),
            Some(FileAction::Restore(a)) => (Some("restore"), a.clone(), None),
            Some(FileAction::Orient { cw, turned }) => (Some("orient"), turned.iter().map(|t| t.asset).collect(), Some(*cw as i64)),
        };
        let text: Vec<String> = targets.iter().map(|a| a.0.to_string()).collect();
        tx.execute(
            "INSERT INTO undo_changes (description, at, action, targets, cw) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![description, super::people::now_ms(), kind, text.join(" "), cw],
        )?;
        let id = tx.last_insert_rowid();
        tx.execute("INSERT INTO undo_rows (change_id, seq, tbl, op) SELECT ?1, seq, tbl, op FROM undo_capture_rows", [id])?;
        tx.execute("INSERT INTO undo_cells (change_id, seq, col, old, new) SELECT ?1, seq, col, old, new FROM undo_capture_cells", [id])?;
        // The assets it touched: named by the rows, or the faces in them.
        tx.execute(
            "INSERT OR IGNORE INTO undo_assets (change_id, asset_id)
             SELECT ?1, v FROM (
                 SELECT c.old AS v FROM undo_capture_cells c JOIN undo_capture_rows r USING (seq)
                     WHERE (r.tbl IN ('asset_tags', 'faces') AND c.col = 'asset_id') OR (r.tbl = 'asset_pairs' AND c.col IN ('front', 'back'))
                 UNION
                 SELECT c.new FROM undo_capture_cells c JOIN undo_capture_rows r USING (seq)
                     WHERE (r.tbl IN ('asset_tags', 'faces') AND c.col = 'asset_id') OR (r.tbl = 'asset_pairs' AND c.col IN ('front', 'back'))
                 UNION
                 SELECT f.asset_id FROM undo_capture_cells c JOIN undo_capture_rows r USING (seq) JOIN main.faces f ON f.id = coalesce(c.new, c.old)
                     WHERE r.tbl IN ('face_crops', 'face_rejections', 'person_covers') AND c.col = 'face_id'
             ) WHERE v IS NOT NULL",
            [id],
        )?;
        {
            let mut insert = tx.prepare("INSERT OR IGNORE INTO undo_assets (change_id, asset_id) VALUES (?1, ?2)")?;
            for asset in targets.iter().chain(assets) {
                insert.execute([id, asset.0])?;
            }
        }
        if let Some(FileAction::Orient { turned, .. }) = action {
            let mut insert =
                tx.prepare("INSERT INTO undo_runs (change_id, asset_id, side, version, status, value, detail) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)")?;
            for t in turned {
                for (side, run) in [("before", &t.before), ("after", &t.after)] {
                    if let Some(run) = run {
                        insert.execute(params![id, t.asset.0, side, run.version, run.status.name(), run.value, run.detail])?;
                    }
                }
            }
        }
        let dropped = tx.execute("DELETE FROM undo_changes WHERE id NOT IN (SELECT id FROM undo_changes ORDER BY id DESC LIMIT ?1)", [KEEP as i64])?;
        tx.execute_batch("DELETE FROM undo_capture_rows; DELETE FROM undo_capture_cells;")?;
        tx.commit()?;
        log::debug!("journal: change {id} {description:?}: {rows} rows, action {kind:?} {text:?}{}", if dropped > 0 { format!(", {dropped} old changes dropped") } else { String::new() });
        Ok(Some(ChangeId(id)))
    }
}

/// Writes one recorded row to `table`, as `op` from its cells' first value to their second:
/// `insert`, `update`, `delete`, or `uncreate` (a delete undoing an insert).
fn replay(conn: &rusqlite::Connection, table: &Table, op: &str, row: &[(String, Value, Value)]) -> Result<()> {
    let name = quoted(table.name);
    let value = |col: &str, after: bool| row.iter().find(|(c, _, _)| c == col).map(|(_, before, now)| if after { now.clone() } else { before.clone() });
    // The row as it was before this step: its key there.
    let key: Vec<Value> = table.key.iter().map(|k| value(k, false).unwrap_or(Value::Null)).collect();
    let matches = table.key.iter().enumerate().map(|(i, k)| format!("{} = ?{}", quoted(k), i + 1)).collect::<Vec<_>>().join(" AND ");
    match op {
        "delete" | "uncreate" => {
            // A tag the change made goes only while nothing else uses it: the pipeline may
            // have put it on something since.
            let unused = if table.name == "tags" && op == "uncreate" {
                " AND NOT EXISTS (SELECT 1 FROM asset_tags WHERE tag_id = tags.id)
                  AND NOT EXISTS (SELECT 1 FROM faces WHERE person = tags.id OR suggested = tags.id)
                  AND NOT EXISTS (SELECT 1 FROM tags c WHERE lower(substr(c.path, 1, length(tags.path) + 1)) = lower(tags.path || '|'))"
            } else {
                ""
            };
            conn.execute(&format!("DELETE FROM {name} WHERE {matches}{unused}"), params_from_iter(&key))?;
        }
        "insert" => {
            let columns: Vec<&String> = row.iter().map(|(c, _, _)| c).filter(|c| table.columns.contains(c)).collect();
            let values: Vec<Value> = columns.iter().map(|c| value(c, true).unwrap_or(Value::Null)).collect();
            let placeholders: Vec<String> = (1..=columns.len()).map(|i| format!("?{i}")).collect();
            let set: Vec<String> = columns.iter().filter(|c| !table.key.contains(c)).map(|c| format!("{0} = excluded.{0}", quoted(c))).collect();
            let conflict = if set.is_empty() { "DO NOTHING".to_owned() } else { format!("DO UPDATE SET {}", set.join(", ")) };
            let keys: Vec<String> = table.key.iter().map(|k| quoted(k)).collect();
            let sql = format!(
                "INSERT INTO {name} ({}) VALUES ({}) ON CONFLICT ({}) {conflict}",
                columns.iter().map(|c| quoted(c)).collect::<Vec<_>>().join(", "),
                placeholders.join(", "),
                keys.join(", "),
            );
            conn.execute(&sql, params_from_iter(&values))?;
        }
        _ => {
            let set: Vec<(&String, Value)> =
                row.iter().filter(|(c, before, now)| before != now && table.columns.contains(c)).map(|(c, _, now)| (c, now.clone())).collect();
            if set.is_empty() {
                return Ok(());
            }
            let n = key.len();
            let assignments: Vec<String> = set.iter().enumerate().map(|(i, (c, _))| format!("{} = ?{}", quoted(c), n + i + 1)).collect();
            let values: Vec<Value> = key.into_iter().chain(set.into_iter().map(|(_, v)| v)).collect();
            conn.execute(&format!("UPDATE {name} SET {} WHERE {matches}", assignments.join(", ")), params_from_iter(&values))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
