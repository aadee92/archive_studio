//! Thumbnail storage tiers below the decoded-image LRU:
//! an in-memory LRU of compressed JPEG bytes, then a persistent SQLite database.
//!
//! Entries are keyed by [`Fingerprint::key`], a hash of (canonical path, size, mtime, tier), so an
//! edited file simply misses and its stale thumbnail is never served. Nothing is ever
//! invalidated in place; old rows are just never read again.

use std::collections::HashMap;
use std::hash::Hash;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

use image::codecs::jpeg::JpegEncoder;
use image::{ImageFormat, RgbImage};
use rusqlite::{Connection, OptionalExtension};

use super::Tier;

const JPEG_QUALITY: u8 = 85;

/// Identifies one version of a file: (canonical path, size, mtime).
#[derive(Clone, Copy)]
pub struct Fingerprint(u64);

impl Fingerprint {
    /// `None` if the file can't be stat'ed.
    pub fn of(path: &Path) -> Option<Self> {
        let path = path.canonicalize().ok()?;
        let meta = path.metadata().ok()?;
        let mtime = meta.modified().ok()?.duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos();
        let hash = fnv1a(FNV_OFFSET, path.as_os_str().as_encoded_bytes());
        Some(Self(fnv1a(fnv1a(hash, &meta.len().to_le_bytes()), &mtime.to_le_bytes())))
    }

    /// Cache key of this file's thumbnail at `tier`.
    pub fn key(self, tier: Tier) -> i64 {
        fnv1a(self.0, &tier.edge().to_le_bytes()) as i64
    }
}

const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;

/// FNV-1a: stable across builds and platforms, unlike `DefaultHasher`.
fn fnv1a(mut hash: u64, bytes: &[u8]) -> u64 {
    for &byte in bytes {
        hash = (hash ^ byte as u64).wrapping_mul(0x0100_0000_01b3);
    }
    hash
}

pub fn encode(image: &RgbImage) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(32 * 1024);
    JpegEncoder::new_with_quality(&mut out, JPEG_QUALITY).encode_image(image).ok()?;
    Some(out)
}

pub fn decode(bytes: &[u8]) -> Option<RgbImage> {
    image::load(Cursor::new(bytes), ImageFormat::Jpeg).ok().map(|i| i.into_rgb8())
}

/// `$XDG_CACHE_HOME/archive-studio/thumbs.sqlite`, falling back to `~/.cache`.
pub fn default_db_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")))?;
    Some(base.join("archive-studio").join("thumbs.sqlite"))
}

/// A thumbnail database whose schema is known to be usable. Each thread
/// [`connect`](Self::connect)s on its own; WAL lets readers run alongside a writer.
#[derive(Clone)]
pub struct DiskCache {
    path: PathBuf,
}

impl DiskCache {
    /// Creates the database if needed. `None` (run without a disk cache) if it is
    /// unopenable or corrupt.
    pub fn open(path: &Path) -> Option<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).ok()?;
        }
        let conn = Connection::open(path).ok()?;
        conn.pragma_update(None, "journal_mode", "WAL").ok()?;
        conn.execute_batch("CREATE TABLE IF NOT EXISTS thumbs (key INTEGER PRIMARY KEY, jpeg BLOB NOT NULL, color INTEGER);")
            .ok()?;
        // Databases from before the `color` column (0xRRGGBB average, NULL if unknown).
        let has_color = conn
            .prepare("SELECT 1 FROM pragma_table_info('thumbs') WHERE name = 'color'")
            .and_then(|mut stmt| stmt.exists([]))
            .ok()?;
        if !has_color {
            conn.execute_batch("ALTER TABLE thumbs ADD COLUMN color INTEGER;").ok()?;
        }
        // Reads a page, to catch a database that isn't one; `count(*)` would scan every
        // thumbnail, on the UI thread at startup.
        conn.query_row("SELECT 1 FROM thumbs LIMIT 1", [], |_| Ok(())).optional().ok()?;
        Some(Self { path: path.to_owned() })
    }

    pub fn connect(&self) -> Option<DiskConn> {
        let conn = Connection::open(&self.path).ok()?;
        conn.busy_timeout(Duration::from_secs(2)).ok()?;
        conn.pragma_update(None, "synchronous", "NORMAL").ok()?;
        Some(DiskConn(conn))
    }
}

/// One thread's connection. Errors are treated as misses / dropped writes.
pub struct DiskConn(Connection);

impl DiskConn {
    /// The JPEG stored under `key` and its average color, if one was recorded.
    pub fn get(&self, key: i64) -> Option<(Vec<u8>, Option<u32>)> {
        let mut stmt = self.0.prepare_cached("SELECT jpeg, color FROM thumbs WHERE key = ?1").ok()?;
        stmt.query_row([key], |row| Ok((row.get(0)?, row.get(1)?))).optional().ok().flatten()
    }

    /// Writes all `(key, jpeg, color)` entries in one transaction; `false` if nothing was stored.
    pub fn put(&mut self, entries: &[(i64, &[u8], u32)]) -> bool {
        let Ok(tx) = self.0.transaction() else { return false };
        let inserted = tx
            .prepare_cached("INSERT OR REPLACE INTO thumbs (key, jpeg, color) VALUES (?1, ?2, ?3)")
            .is_ok_and(|mut stmt| {
                entries.iter().all(|(key, jpeg, color)| stmt.execute(rusqlite::params![key, jpeg, color]).is_ok())
            });
        inserted && tx.commit().is_ok()
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.0.query_row("SELECT count(*) FROM thumbs", [], |r| r.get::<_, i64>(0)).unwrap() as usize
    }
}

/// Least-recently-used map bounded by the summed byte size of its values.
pub struct Lru<K, V> {
    /// key -> (value, bytes, last-used tick)
    map: HashMap<K, (V, usize, u64)>,
    bytes: usize,
    budget: usize,
    tick: u64,
}

impl<K: Hash + Eq + Copy, V: Clone> Lru<K, V> {
    pub fn new(budget: usize) -> Self {
        Self { map: HashMap::new(), bytes: 0, budget, tick: 0 }
    }

    pub fn get(&mut self, key: &K) -> Option<V> {
        self.tick += 1;
        let (value, _, used) = self.map.get_mut(key)?;
        *used = self.tick;
        Some(value.clone())
    }

    /// Inserts, then evicts the least recently used entries down to 90% of the budget
    /// in one pass. The new entry itself is never evicted.
    pub fn insert(&mut self, key: K, value: V, bytes: usize) {
        self.tick += 1;
        if let Some((_, old, _)) = self.map.insert(key, (value, bytes, self.tick)) {
            self.bytes -= old;
        }
        self.bytes += bytes;
        if self.bytes <= self.budget {
            return;
        }
        let mut by_age: Vec<(u64, K, usize)> =
            self.map.iter().filter(|(k, _)| **k != key).map(|(k, (_, n, used))| (*used, *k, *n)).collect();
        by_age.sort_unstable_by_key(|e| e.0);
        let target = self.budget / 10 * 9;
        for (_, k, n) in by_age {
            if self.bytes <= target {
                break;
            }
            self.map.remove(&k);
            self.bytes -= n;
        }
    }

    /// Whether `key` is present, marking it recently used.
    pub fn touch(&mut self, key: &K) -> bool {
        self.tick += 1;
        let tick = self.tick;
        self.map.get_mut(key).map(|e| e.2 = tick).is_some()
    }

    pub fn remove(&mut self, key: &K) {
        if let Some((_, n, _)) = self.map.remove(key) {
            self.bytes -= n;
        }
    }

    #[cfg(test)]
    pub fn bytes(&self) -> usize {
        self.bytes
    }

    #[cfg(test)]
    pub fn contains(&self, key: &K) -> bool {
        self.map.contains_key(key)
    }
}
