//! Handing files to the desktop (freedesktop.org: Linux and the BSDs): [`open`] a file in
//! its default application, [`show_in_folder`] in the file manager with the file selected,
//! and what copying files puts on the clipboard ([`CopiedFiles`]).
//!
//! Both run external tools (`xdg-open`, `gdbus` / `dbus-send`); [`show_in_folder`] waits for
//! the file manager to answer (at most a few seconds), so front ends call it on a worker.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;

/// Seconds a file manager has to answer `ShowItems`.
const DBUS_TIMEOUT: u32 = 3;

/// How [`show_in_folder`] showed the file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shown {
    /// The file manager opened its folder with the file selected (`FileManager1.ShowItems`).
    Selected,
    /// Only its folder could be opened (`xdg-open`): no file manager on D-Bus.
    Folder,
}

/// Opens `path` in the desktop's default application for it (`xdg-open`), without waiting
/// for it.
pub fn open(path: &Path) -> Result<(), String> {
    if !path.exists() {
        return Err(format!("{} doesn't exist", path.display()));
    }
    let result = spawn("xdg-open", [path.as_os_str().to_owned()]);
    match &result {
        Ok(()) => log::debug!("open {}: handed to xdg-open", path.display()),
        Err(e) => log::warn!("open {}: {e}", path.display()),
    }
    result
}

/// Shows `path` in the file manager: its folder, with the file selected where the file
/// manager offers that over D-Bus (`org.freedesktop.FileManager1.ShowItems`, asked with
/// `gdbus`, else `dbus-send`), else just the folder (`xdg-open`). Blocks until the file
/// manager answers (at most a few seconds per try): call it on a worker.
pub fn show_in_folder(path: &Path) -> Result<Shown, String> {
    let path = std::path::absolute(path).map_err(|e| format!("{}: {e}", path.display()))?;
    if !path.exists() {
        return Err(format!("{} doesn't exist", path.display()));
    }
    let uri = file_uri(&path);
    for (program, args) in show_items_commands(&uri) {
        let status = Command::new(program).args(&args).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).status();
        match status {
            Ok(s) if s.success() => {
                log::debug!("show in folder {}: ShowItems via {program}", path.display());
                return Ok(Shown::Selected);
            }
            Ok(s) => log::debug!("show in folder {}: {program} ShowItems failed ({s})", path.display()),
            Err(e) => log::debug!("show in folder {}: no {program} ({e})", path.display()),
        }
    }
    let dir = folder(&path);
    let result = spawn("xdg-open", [dir.as_os_str().to_owned()]).map(|()| Shown::Folder);
    match &result {
        Ok(_) => log::debug!("show in folder {}: opened {} with xdg-open", path.display(), dir.display()),
        Err(e) => log::warn!("show in folder {}: {e}", path.display()),
    }
    result
}

/// The folder to open for `path`: its parent (the root itself for a root).
fn folder(path: &Path) -> PathBuf {
    path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(path).to_path_buf()
}

/// The ways to ask the session's file manager to show `uri` selected, first choice first.
fn show_items_commands(uri: &str) -> [(&'static str, Vec<String>); 2] {
    [
        (
            "gdbus",
            vec![
                "call".into(),
                "--session".into(),
                "--timeout".into(),
                DBUS_TIMEOUT.to_string(),
                "--dest".into(),
                "org.freedesktop.FileManager1".into(),
                "--object-path".into(),
                "/org/freedesktop/FileManager1".into(),
                "--method".into(),
                "org.freedesktop.FileManager1.ShowItems".into(),
                // GVariant text: an array of one string, and an empty startup id.
                format!("['{uri}']"),
                "".into(),
            ],
        ),
        (
            "dbus-send",
            vec![
                "--session".into(),
                "--print-reply".into(),
                format!("--reply-timeout={}", DBUS_TIMEOUT * 1000),
                "--dest=org.freedesktop.FileManager1".into(),
                "--type=method_call".into(),
                "/org/freedesktop/FileManager1".into(),
                "org.freedesktop.FileManager1.ShowItems".into(),
                format!("array:string:{uri}"),
                "string:".into(),
            ],
        ),
    ]
}

/// `file://` URI of an absolute path: every byte but unreserved characters and `/`
/// percent-encoded (so it holds no quote or comma either, which the D-Bus tools' argument
/// syntax would take for its own).
pub fn file_uri(path: &Path) -> String {
    use std::os::unix::ffi::OsStrExt;
    let mut uri = String::from("file://");
    for &b in path.as_os_str().as_bytes() {
        if b.is_ascii_alphanumeric() || b"-._~/".contains(&b) {
            uri.push(b as char);
        } else {
            uri.push_str(&format!("%{b:02X}"));
        }
    }
    uri
}

/// Files copied to the clipboard, in the formats other programs paste: a file manager takes
/// `text/uri-list` (or GNOME's `x-special/gnome-copied-files`) and copies the files, a text
/// field takes the paths as text. A front end offers all of them at once.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CopiedFiles {
    /// `text/uri-list` (RFC 2483): a `file://` URI per line, CRLF-terminated.
    pub uri_list: String,
    /// `x-special/gnome-copied-files`: `copy`, then a URI per line.
    pub gnome: String,
    /// `text/plain`: the paths, one per line.
    pub text: String,
}

impl CopiedFiles {
    /// The formats for `paths` (absolute, as the catalog has them).
    pub fn new(paths: &[impl AsRef<Path>]) -> Self {
        let uris: Vec<String> = paths.iter().map(|p| file_uri(p.as_ref())).collect();
        Self {
            uri_list: uris.iter().map(|u| format!("{u}\r\n")).collect(),
            gnome: std::iter::once("copy").chain(uris.iter().map(String::as_str)).collect::<Vec<_>>().join("\n"),
            text: paths.iter().map(|p| p.as_ref().to_string_lossy()).collect::<Vec<_>>().join("\n"),
        }
    }
}

/// Starts `program` detached (its exit is reaped on a thread), or says why it couldn't.
fn spawn<const N: usize>(program: &str, args: [OsString; N]) -> Result<(), String> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("can't run {program}: {e}"))?;
    thread::spawn(move || child.wait());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_uris_are_percent_encoded() {
        assert_eq!(file_uri(Path::new("/lib/a-1_b.~x.jpg")), "file:///lib/a-1_b.~x.jpg");
        assert_eq!(file_uri(Path::new("/lib/Mom's, 1980/ü #2.jpg")), "file:///lib/Mom%27s%2C%201980/%C3%BC%20%232.jpg");
    }

    #[test]
    fn copied_files_in_every_format() {
        let copied = CopiedFiles::new(&[Path::new("/lib/a b.jpg"), Path::new("/lib/c.png")]);
        assert_eq!(copied.uri_list, "file:///lib/a%20b.jpg\r\nfile:///lib/c.png\r\n");
        assert_eq!(copied.gnome, "copy\nfile:///lib/a%20b.jpg\nfile:///lib/c.png");
        assert_eq!(copied.text, "/lib/a b.jpg\n/lib/c.png");
    }

    #[test]
    fn show_items_asks_gdbus_then_dbus_send() {
        let [(first, gdbus), (second, dbus_send)] = show_items_commands("file:///a%20b.jpg");
        assert_eq!((first, second), ("gdbus", "dbus-send"));
        assert_eq!(gdbus[gdbus.len() - 2], "['file:///a%20b.jpg']");
        assert!(gdbus.contains(&"org.freedesktop.FileManager1.ShowItems".to_owned()));
        assert_eq!(dbus_send[dbus_send.len() - 2], "array:string:file:///a%20b.jpg");
    }

    #[test]
    fn folder_of_a_file_is_its_parent() {
        assert_eq!(folder(Path::new("/lib/a.jpg")), Path::new("/lib"));
        assert_eq!(folder(Path::new("/")), Path::new("/"));
    }

    #[test]
    fn missing_files_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("gone.jpg");
        assert!(show_in_folder(&missing).unwrap_err().contains("doesn't exist"));
        assert!(open(&missing).unwrap_err().contains("doesn't exist"));
    }
}
