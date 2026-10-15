//! What the pipeline ([`crate::pipeline`]) has done to each asset, so it knows what is
//! still due after a restart, a changed file or a model upgrade.
//!
//! Each asset carries the size and modification time it was last seen with (its
//! fingerprint, `"size:mtime"`). A stage's run on an asset records the fingerprint it ran
//! on and the stage version; the stage is due again when either no longer matches.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use rusqlite::{params, OptionalExtension};

use super::{AssetId, Catalog, Result};
use crate::media::MediaKind;

pub(super) const SCHEMA: &str = "
ALTER TABLE assets ADD COLUMN kind TEXT;
-- The file as last seen: size in bytes, modification time in ns since the epoch. NULL until
-- the pipeline has looked at it.
ALTER TABLE assets ADD COLUMN size INTEGER;
ALTER TABLE assets ADD COLUMN mtime INTEGER;
-- The file is gone. The asset is kept (it holds the user's tags and faces) in case it was
-- moved out and comes back.
ALTER TABLE assets ADD COLUMN missing INTEGER NOT NULL DEFAULT 0;

CREATE TABLE stage_runs (
    asset_id INTEGER NOT NULL REFERENCES assets(id) ON DELETE CASCADE,
    stage TEXT NOT NULL,
    version TEXT NOT NULL,
    -- The asset's fingerprint ('size:mtime') the stage ran on.
    input TEXT NOT NULL,
    -- 'user': the user decided (never redone by a newer version). 'review': the stage wants
    -- the user to look. 'failed': not retried until the file changes.
    status TEXT NOT NULL CHECK (status IN ('done', 'user', 'review', 'failed')),
    -- Stage specific: orient's applied or proposed clockwise rotation.
    value INTEGER,
    detail TEXT NOT NULL DEFAULT '',
    at INTEGER NOT NULL,
    PRIMARY KEY (asset_id, stage)
) WITHOUT ROWID;

-- Library-wide pipeline counters, e.g. faces written since the last identify.
CREATE TABLE pipeline_state (
    key TEXT PRIMARY KEY,
    value INTEGER NOT NULL
) WITHOUT ROWID;
";

pub(super) const FINGERPRINT: &str = "(a.size || ':' || a.mtime)";

/// An asset's file as the catalog last saw it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FileState {
    pub asset: AssetId,
    pub size: Option<u64>,
    pub mtime: Option<i64>,
    pub missing: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunStatus {
    Done,
    User,
    Review,
    Failed,
}

impl RunStatus {
    fn name(self) -> &'static str {
        match self {
            Self::Done => "done",
            Self::User => "user",
            Self::Review => "review",
            Self::Failed => "failed",
        }
    }
}

/// A stage run to record with [`Catalog::set_run`].
#[derive(Clone, Debug)]
pub struct Run<'a> {
    pub stage: &'a str,
    pub version: &'a str,
    pub input: &'a str,
    pub status: RunStatus,
    pub value: Option<i64>,
    pub detail: &'a str,
}

/// An asset a stage is due on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Due {
    pub asset: AssetId,
    pub path: PathBuf,
    /// The fingerprint the stage will run on.
    pub input: String,
}

/// Which assets [`Catalog::due`] considers.
#[derive(Clone, Copy, Debug)]
pub struct StageQuery<'a> {
    pub stage: &'a str,
    pub version: &'a str,
    pub kind: MediaKind,
    /// A stage that must have run on the current file first (and not failed).
    pub after: Option<&'a str>,
}

/// A stage's runs on the current files of present assets, by status.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RunCounts {
    pub done: usize,
    pub review: usize,
    pub failed: usize,
    pub pending: usize,
}

/// `"size:mtime"`, as [`Due::input`] and [`Run::input`] carry it.
pub fn fingerprint(size: u64, mtime: i64) -> String {
    format!("{size}:{mtime}")
}

/// The size and modification time (ns since the epoch) of the file at `path`.
pub fn stat(path: &Path) -> std::io::Result<(u64, i64)> {
    let meta = std::fs::metadata(path)?;
    let mtime = meta
        .modified()?
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos().min(i64::MAX as u128) as i64);
    Ok((meta.len(), mtime))
}

fn kind_name(kind: MediaKind) -> &'static str {
    match kind {
        MediaKind::Image => "image",
        MediaKind::Video => "video",
        MediaKind::Audio => "audio",
    }
}

fn now() -> i64 {
    SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}

impl Catalog {
    /// The library root, absolute.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Every asset's file state, by path relative to the root.
    pub fn file_states(&self) -> Result<HashMap<String, FileState>> {
        let mut stmt = self.conn.prepare_cached("SELECT id, path, size, mtime, missing FROM assets")?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(1)?,
                FileState {
                    asset: AssetId(row.get(0)?),
                    size: row.get::<_, Option<i64>>(2)?.map(|s| s as u64),
                    mtime: row.get(3)?,
                    missing: row.get(4)?,
                },
            ))
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// The state of the asset at `path`, if the catalog has it.
    pub fn file_state(&self, path: &Path) -> Result<Option<FileState>> {
        let rel = self.relative(path)?;
        let mut stmt = self.conn.prepare_cached("SELECT id, size, mtime, missing FROM assets WHERE path = ?1")?;
        Ok(stmt
            .query_row([rel], |row| {
                Ok(FileState {
                    asset: AssetId(row.get(0)?),
                    size: row.get::<_, Option<i64>>(1)?.map(|s| s as u64),
                    mtime: row.get(2)?,
                    missing: row.get(3)?,
                })
            })
            .optional()?)
    }

    /// Records the file at `path` as present with this size and mtime, adding the asset if
    /// needed. Returns the asset and whether anything changed: a new file, a different
    /// fingerprint (which makes every stage due again) or a file that is back.
    pub fn record_file(&self, path: &Path, kind: MediaKind, size: u64, mtime: i64) -> Result<(AssetId, bool)> {
        let before = self.file_state(path)?;
        let asset = match before {
            Some(state) => state.asset,
            None => self.asset(path)?,
        };
        let changed = before.is_none_or(|s| s.size != Some(size) || s.mtime != Some(mtime) || s.missing);
        if changed {
            self.conn
                .prepare_cached("UPDATE assets SET kind = ?2, size = ?3, mtime = ?4, missing = 0 WHERE id = ?1")?
                .execute(params![asset.0, kind_name(kind), size as i64, mtime])?;
        }
        Ok((asset, changed))
    }

    /// Marks the asset's file as gone (or back, with `missing` false).
    pub fn set_missing(&self, asset: AssetId, missing: bool) -> Result<()> {
        self.conn.prepare_cached("UPDATE assets SET missing = ?2 WHERE id = ?1")?.execute(params![asset.0, missing])?;
        Ok(())
    }

    /// The asset's current fingerprint, if its file has been seen.
    pub fn current_input(&self, asset: AssetId) -> Result<Option<String>> {
        let sql = format!("SELECT {FINGERPRINT} FROM assets a WHERE a.id = ?1 AND NOT a.missing");
        Ok(self.conn.prepare_cached(&sql)?.query_row([asset.0], |row| row.get::<_, Option<String>>(0)).optional()?.flatten())
    }

    fn due_condition(query: &StageQuery) -> String {
        let after = match query.after {
            Some(_) => "AND EXISTS (SELECT 1 FROM stage_runs p WHERE p.asset_id = a.id AND p.stage = ?4 \
                        AND p.input = (a.size || ':' || a.mtime) AND p.status <> 'failed')",
            None => "AND ?4 IS NULL",
        };
        format!(
            "FROM assets a LEFT JOIN stage_runs r ON r.asset_id = a.id AND r.stage = ?1
             WHERE NOT a.missing AND a.kind = ?3 AND a.size IS NOT NULL {after}
               AND (r.asset_id IS NULL OR r.input <> {FINGERPRINT} OR (r.version <> ?2 AND r.status <> 'user'))"
        )
    }

    /// Up to `limit` assets `query`'s stage is due on, newest first.
    pub fn due(&self, query: &StageQuery, limit: usize) -> Result<Vec<Due>> {
        let sql = format!("SELECT a.id, a.path, {FINGERPRINT} {} ORDER BY a.id DESC LIMIT ?5", Self::due_condition(query));
        let mut stmt = self.conn.prepare_cached(&sql)?;
        let rows = stmt.query_map(
            params![query.stage, query.version, kind_name(query.kind), query.after, limit as i64],
            |row| Ok((AssetId(row.get(0)?), row.get::<_, String>(1)?, row.get::<_, String>(2)?)),
        )?;
        let rows: Vec<_> = rows.collect::<rusqlite::Result<_>>()?;
        Ok(rows.into_iter().map(|(asset, rel, input)| Due { asset, path: self.root.join(rel), input }).collect())
    }

    /// How `query`'s stage stands on the present assets.
    pub fn run_counts(&self, query: &StageQuery) -> Result<RunCounts> {
        let pending_sql = format!("SELECT count(*) {}", Self::due_condition(query));
        let pending: i64 = self.conn.prepare_cached(&pending_sql)?.query_row(
            params![query.stage, query.version, kind_name(query.kind), query.after],
            |row| row.get(0),
        )?;
        let sql = format!(
            "SELECT r.status, count(*) FROM stage_runs r JOIN assets a ON a.id = r.asset_id
             WHERE r.stage = ?1 AND NOT a.missing AND r.input = {FINGERPRINT}
               AND (r.version = ?2 OR r.status = 'user') GROUP BY r.status"
        );
        let mut counts = RunCounts { pending: pending as usize, ..Default::default() };
        let mut stmt = self.conn.prepare_cached(&sql)?;
        let rows = stmt.query_map(params![query.stage, query.version], |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)))?;
        for row in rows {
            let (status, n) = row?;
            match status.as_str() {
                "done" | "user" => counts.done += n as usize,
                "review" => counts.review += n as usize,
                _ => counts.failed += n as usize,
            }
        }
        Ok(counts)
    }

    /// Records a stage's run on `asset`, replacing the previous one.
    pub fn set_run(&self, asset: AssetId, run: &Run) -> Result<()> {
        self.conn
            .prepare_cached(
                "INSERT INTO stage_runs (asset_id, stage, version, input, status, value, detail, at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                 ON CONFLICT DO UPDATE SET version = excluded.version, input = excluded.input,
                     status = excluded.status, value = excluded.value, detail = excluded.detail, at = excluded.at",
            )?
            .execute(params![asset.0, run.stage, run.version, run.input, run.status.name(), run.value, run.detail, now()])?;
        Ok(())
    }

    /// The stage's last run on `asset`: status, value and detail.
    pub fn run_of(&self, asset: AssetId, stage: &str) -> Result<Option<(RunStatus, Option<i64>, String)>> {
        let mut stmt = self.conn.prepare_cached("SELECT status, value, detail FROM stage_runs WHERE asset_id = ?1 AND stage = ?2")?;
        let row = stmt
            .query_row(params![asset.0, stage], |row| Ok((row.get::<_, String>(0)?, row.get(1)?, row.get(2)?)))
            .optional()?;
        Ok(row.map(|(status, value, detail)| (status_of(&status), value, detail)))
    }

    /// The present assets whose current file has a run of `stage` with `status`, by path:
    /// (asset, path, value, detail).
    pub fn runs_with_status(&self, stage: &str, status: RunStatus) -> Result<Vec<(AssetId, PathBuf, Option<i64>, String)>> {
        let sql = format!(
            "SELECT a.id, a.path, r.value, r.detail FROM stage_runs r JOIN assets a ON a.id = r.asset_id
             WHERE r.stage = ?1 AND r.status = ?2 AND NOT a.missing AND r.input = {FINGERPRINT} ORDER BY a.path"
        );
        let mut stmt = self.conn.prepare_cached(&sql)?;
        let rows = stmt.query_map(params![stage, status.name()], |row| {
            Ok((AssetId(row.get(0)?), row.get::<_, String>(1)?, row.get(2)?, row.get(3)?))
        })?;
        let rows: Vec<_> = rows.collect::<rusqlite::Result<_>>()?;
        Ok(rows.into_iter().map(|(asset, rel, value, detail)| (asset, self.root.join(rel), value, detail)).collect())
    }

    /// Forgets the failed runs of `stage` (all stages if `None`), so they are retried.
    pub fn clear_failed(&self, stage: Option<&str>) -> Result<usize> {
        Ok(self
            .conn
            .prepare_cached("DELETE FROM stage_runs WHERE status = 'failed' AND (?1 IS NULL OR stage = ?1)")?
            .execute([stage])?)
    }

    /// Turns the face boxes and tag regions of `asset` with the image, `cw` degrees
    /// clockwise (90, 180 or 270), after its file was rotated.
    pub fn rotate_regions(&self, asset: AssetId, cw: u16) -> Result<()> {
        // SQLite evaluates every right-hand side on the old row.
        let set = match cw % 360 {
            90 => "x = 1 - y - h, y = x, w = h, h = w",
            180 => "x = 1 - x - w, y = 1 - y - h",
            270 => "x = y, y = 1 - x - w, w = h, h = w",
            _ => return Ok(()),
        };
        self.conn.execute(&format!("UPDATE faces SET {set} WHERE asset_id = ?1"), [asset.0])?;
        self.conn.execute(&format!("UPDATE asset_tags SET {set} WHERE asset_id = ?1 AND x IS NOT NULL"), [asset.0])?;
        Ok(())
    }

    /// A library-wide pipeline counter (0 if never set).
    pub fn pipeline_value(&self, key: &str) -> Result<i64> {
        let mut stmt = self.conn.prepare_cached("SELECT value FROM pipeline_state WHERE key = ?1")?;
        Ok(stmt.query_row([key], |row| row.get(0)).optional()?.unwrap_or(0))
    }

    /// Adds `delta` to a library-wide pipeline counter, or sets it with `reset`.
    pub fn bump_pipeline_value(&self, key: &str, delta: i64, reset: bool) -> Result<()> {
        self.conn
            .prepare_cached(
                "INSERT INTO pipeline_state (key, value) VALUES (?1, ?2)
                 ON CONFLICT DO UPDATE SET value = CASE WHEN ?3 THEN ?2 ELSE value + ?2 END",
            )?
            .execute(params![key, delta, reset])?;
        Ok(())
    }

    /// Every stage's last run on `asset`, by stage name.
    pub fn stage_runs(&self, asset: AssetId) -> Result<Vec<StageRun>> {
        let sql = format!(
            "SELECT r.stage, r.version, r.status, r.value, r.detail, r.at, r.input = {FINGERPRINT} AND NOT a.missing
             FROM stage_runs r JOIN assets a ON a.id = r.asset_id WHERE r.asset_id = ?1 ORDER BY r.stage"
        );
        let mut stmt = self.conn.prepare_cached(&sql)?;
        let rows = stmt.query_map([asset.0], |row| {
            Ok(StageRun {
                stage: row.get(0)?,
                version: row.get(1)?,
                status: status_of(row.get_ref(2)?.as_str()?),
                value: row.get(3)?,
                detail: row.get(4)?,
                at: row.get(5)?,
                current: row.get::<_, Option<bool>>(6)?.unwrap_or(false),
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }
}

/// A stage's last run on an asset, for a properties view.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StageRun {
    pub stage: String,
    pub version: String,
    pub status: RunStatus,
    pub value: Option<i64>,
    pub detail: String,
    /// Seconds since the epoch.
    pub at: i64,
    /// It ran on the file as it is now (rather than on an older version of it).
    pub current: bool,
}

fn status_of(name: &str) -> RunStatus {
    match name {
        "done" => RunStatus::Done,
        "user" => RunStatus::User,
        "review" => RunStatus::Review,
        _ => RunStatus::Failed,
    }
}
