//! Per-user settings a front end keeps across runs (window size, the last page, how a view
//! was set up): a small JSON file, by default `$XDG_CONFIG_HOME/archive-studio/settings.json`
//! ([`default_path`]).
//!
//! What is stored is up to the front end: values under string keys, read back typed with
//! [`Section::get`]. Settings for the whole app are in [`Settings::app`], those that only make
//! sense for one library (e.g. tag tree nodes collapsed) in [`Settings::library`], keyed by
//! the library's root.
//!
//! ```json
//! { "version": 1, "app": { "window.width": 1280.0 }, "libraries": { "/photos": { … } } }
//! ```
//!
//! Loading never fails: a missing file is empty settings; one that can't be read or parsed is
//! logged, moved aside to `settings.json.bad` (so the user's file isn't lost to the next
//! save) and replaced by empty settings; a value of the wrong type reads as absent. A file
//! written by a newer version is read as far as it is understood, and its other keys are kept
//! when saving. [`Settings::save`] writes a temporary file and renames it over the old one,
//! so a crash mid-write leaves the previous settings.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::{Map, Value};

/// The file format [`Settings::save`] writes. Bump it (and convert in `Settings::parse`)
/// when the meaning of stored keys changes.
pub const VERSION: u64 = 1;

/// `$XDG_CONFIG_HOME/archive-studio/settings.json`, falling back to `~/.config`.
pub fn default_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))?;
    Some(base.join("archive-studio").join("settings.json"))
}

/// Values under string keys.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Section(Map<String, Value>);

impl Section {
    /// The value at `key`, if there is one of type `T`.
    pub fn get<T: DeserializeOwned>(&self, key: &str) -> Option<T> {
        let value = self.0.get(key)?;
        match T::deserialize(value) {
            Ok(v) => Some(v),
            Err(e) => {
                log::debug!("settings: {key} = {value} ignored ({e})");
                None
            }
        }
    }

    /// Sets `key` to `value` (a value that doesn't serialize is logged and left out).
    pub fn set<T: Serialize>(&mut self, key: &str, value: T) {
        match serde_json::to_value(value) {
            Ok(v) => {
                self.0.insert(key.to_owned(), v);
            }
            Err(e) => log::warn!("settings: {key} not stored ({e})"),
        }
    }

    pub fn remove(&mut self, key: &str) {
        self.0.remove(key);
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// The settings file's contents.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Settings {
    /// Settings of the whole app.
    pub app: Section,
    /// Settings of one library each, by its root as given to [`Settings::library`].
    libraries: BTreeMap<String, Section>,
    /// Top-level keys of a newer version, kept as they were.
    other: Map<String, Value>,
}

impl Settings {
    /// Reads `path`; see the module doc for what happens when it is missing or unusable.
    pub fn load(path: &Path) -> Self {
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                log::debug!("settings: no {} yet", path.display());
                return Self::default();
            }
            Err(e) => {
                log::warn!("settings: can't read {} ({e}); using defaults", path.display());
                return Self::default();
            }
        };
        match Self::parse(&text) {
            Ok(settings) => {
                log::debug!(
                    "settings: read {} ({} app keys, {} libraries)",
                    path.display(),
                    settings.app.0.len(),
                    settings.libraries.len()
                );
                settings
            }
            Err(e) => {
                let mut bad = path.as_os_str().to_owned();
                bad.push(".bad");
                let moved = fs::rename(path, &bad);
                log::warn!(
                    "settings: {} is unusable ({e}); using defaults{}",
                    path.display(),
                    match moved {
                        Ok(()) => format!(", the file is kept as {}", Path::new(&bad).display()),
                        Err(e) => format!(" (can't move it aside: {e})"),
                    }
                );
                Self::default()
            }
        }
    }

    fn parse(text: &str) -> Result<Self, String> {
        let Value::Object(mut top) = serde_json::from_str::<Value>(text).map_err(|e| e.to_string())? else {
            return Err("not a JSON object".into());
        };
        let version = top.remove("version").and_then(|v| v.as_u64()).ok_or("no version")?;
        if version > VERSION {
            log::info!("settings: written by a newer version ({version}, this is {VERSION}); reading what is understood");
        }
        let app = match top.remove("app") {
            Some(Value::Object(map)) => Section(map),
            _ => Section::default(),
        };
        let libraries = match top.remove("libraries") {
            Some(Value::Object(map)) => map
                .into_iter()
                .filter_map(|(root, v)| match v {
                    Value::Object(map) => Some((root, Section(map))),
                    _ => None,
                })
                .collect(),
            _ => BTreeMap::new(),
        };
        Ok(Self { app, libraries, other: top })
    }

    /// The settings of the library at `root` (empty if there are none yet).
    pub fn library(&self, root: &Path) -> Section {
        self.libraries.get(&library_key(root)).cloned().unwrap_or_default()
    }

    /// Replaces the settings of the library at `root` (an empty section removes them).
    pub fn set_library(&mut self, root: &Path, section: Section) {
        let key = library_key(root);
        if section.is_empty() {
            self.libraries.remove(&key);
        } else {
            self.libraries.insert(key, section);
        }
    }

    /// The file's text.
    pub fn to_json(&self) -> String {
        let mut top = self.other.clone();
        top.insert("version".into(), VERSION.into());
        top.insert("app".into(), Value::Object(self.app.0.clone()));
        let libraries = self.libraries.iter().map(|(root, s)| (root.clone(), Value::Object(s.0.clone()))).collect();
        top.insert("libraries".into(), Value::Object(libraries));
        serde_json::to_string_pretty(&Value::Object(top)).expect("JSON values serialize")
    }

    /// Writes the settings to `path` (its folder made if need be), atomically: a temporary
    /// file next to it, renamed over it.
    pub fn save(&self, path: &Path) -> io::Result<()> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        // Unique per save, so two saves at once (a worker and the window closing) don't
        // write into the same temporary file.
        static SAVES: AtomicU64 = AtomicU64::new(0);
        let mut tmp = path.as_os_str().to_owned();
        tmp.push(format!(".{}-{}.tmp", std::process::id(), SAVES.fetch_add(1, Ordering::Relaxed)));
        let tmp = PathBuf::from(tmp);
        let result = fs::write(&tmp, self.to_json() + "\n").and_then(|()| fs::rename(&tmp, path));
        match &result {
            Ok(()) => log::debug!("settings: saved {}", path.display()),
            Err(e) => {
                let _ = fs::remove_file(&tmp);
                log::warn!("settings: can't save {}: {e}", path.display());
            }
        }
        result
    }
}

/// A library's key: its root, absolute.
fn library_key(root: &Path) -> String {
    std::path::absolute(root).unwrap_or_else(|_| root.to_path_buf()).to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_round_trip_through_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub").join("settings.json");
        let mut s = Settings::default();
        s.app.set("window.width", 1400.5f32);
        s.app.set("window.maximized", true);
        s.app.set("review.sort", 2);
        let mut lib = Section::default();
        lib.set("tags.collapsed", vec!["people", "places|france"]);
        s.set_library(Path::new("/photos"), lib);
        s.save(&path).unwrap();

        let back = Settings::load(&path);
        assert_eq!(back, s);
        assert_eq!(back.app.get::<f32>("window.width"), Some(1400.5));
        assert_eq!(back.app.get::<bool>("window.maximized"), Some(true));
        assert_eq!(back.app.get::<i32>("review.sort"), Some(2));
        assert_eq!(
            back.library(Path::new("/photos")).get::<Vec<String>>("tags.collapsed"),
            Some(vec!["people".to_owned(), "places|france".to_owned()])
        );
        assert!(back.library(Path::new("/other")).is_empty());
        assert!(back.to_json().contains("\"version\": 1"));
        // No temporary file left behind.
        assert_eq!(fs::read_dir(path.parent().unwrap()).unwrap().count(), 1);
    }

    #[test]
    fn missing_file_is_empty_settings() {
        let dir = tempfile::tempdir().unwrap();
        let s = Settings::load(&dir.path().join("settings.json"));
        assert_eq!(s, Settings::default());
        assert_eq!(s.app.get::<i32>("anything"), None);
    }

    #[test]
    fn a_corrupt_file_is_moved_aside() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        for text in ["{\"version\": 1, \"app\": {", "[1, 2]", "{\"app\": {}}", ""] {
            fs::write(&path, text).unwrap();
            assert_eq!(Settings::load(&path), Settings::default(), "{text:?}");
            assert!(!path.exists());
            assert_eq!(fs::read_to_string(dir.path().join("settings.json.bad")).unwrap(), text);
        }
    }

    #[test]
    fn values_of_the_wrong_type_read_as_absent() {
        let s = Settings::parse(r#"{"version": 1, "app": {"a": "wide", "b": 3, "c": [1, "x"]}, "libraries": {"/x": 5}}"#).unwrap();
        assert_eq!(s.app.get::<f32>("a"), None);
        assert_eq!(s.app.get::<String>("a").as_deref(), Some("wide"));
        assert_eq!(s.app.get::<bool>("b"), None);
        assert_eq!(s.app.get::<u8>("b"), Some(3));
        assert_eq!(s.app.get::<Vec<i32>>("c"), None);
        assert!(s.library(Path::new("/x")).is_empty());
    }

    #[test]
    fn a_newer_files_other_keys_are_kept() {
        let s = Settings::parse(r#"{"version": 7, "app": {"a": 1, "new": {"x": 1}}, "themes": ["dark"]}"#).unwrap();
        assert_eq!(s.app.get::<i32>("a"), Some(1));
        let back = Settings::parse(&s.to_json()).unwrap();
        assert_eq!(back, s);
        assert!(s.to_json().contains("\"themes\""));
    }

    #[test]
    fn empty_library_sections_are_dropped() {
        let mut s = Settings::default();
        let mut lib = Section::default();
        lib.set("k", 1);
        s.set_library(Path::new("/photos"), lib);
        s.set_library(Path::new("/photos"), Section::default());
        assert_eq!(s, Settings::default());
    }
}
