//! Copying files to the clipboard (Ctrl+C, and Copy in the item menu), so they can be pasted
//! in a file manager, a mail or chat window, or as paths in a text field.
//!
//! Dragging items out of the window to other programs isn't possible: Slint (1.18, over winit
//! 0.30) only drags within the window and has no way to start a drag the desktop sees. Copy
//! and paste stands in for it.
//!
//! On Wayland the files go on the clipboard in every format of
//! [`CopiedFiles`](archive_core::desktop::CopiedFiles) (`text/uri-list`,
//! `x-special/gnome-copied-files` and text), through the compositor's data-control protocol
//! (`wl-clipboard-rs`, served from its own thread until something else is copied). Where that
//! protocol isn't offered (X11, or a compositor without it), `arboard` puts the
//! `text/uri-list` alone on the X11 clipboard (XWayland under Wayland): a file manager
//! pastes the files, a text field nothing.

use std::path::PathBuf;
use std::sync::Mutex;

use archive_core::desktop::CopiedFiles;

/// Kept for the process: on X11, the clipboard is served while it lives.
static ARBOARD: Mutex<Option<arboard::Clipboard>> = Mutex::new(None);

/// Puts `paths` on the clipboard; blocks briefly (a round trip to the display server), so
/// call it on a worker.
pub fn copy_files(paths: &[PathBuf]) -> Result<(), String> {
    let copied = CopiedFiles::new(paths);
    let wayland = std::env::var_os("WAYLAND_DISPLAY").is_some();
    if wayland {
        match copy_wayland(&copied) {
            Ok(()) => {
                log::debug!("clipboard: {} files (uri-list, gnome-copied-files, text) via data-control", paths.len());
                return Ok(());
            }
            Err(e) => log::debug!("clipboard: Wayland data-control unavailable ({e}); trying arboard"),
        }
    }
    let mut guard = ARBOARD.lock().unwrap_or_else(|e| e.into_inner());
    if guard.is_none() {
        *guard = Some(arboard::Clipboard::new().map_err(|e| format!("no clipboard: {e}"))?);
    }
    let clipboard = guard.as_mut().expect("just set");
    clipboard.set().file_list(paths).map_err(|e| format!("can't copy: {e}"))?;
    log::debug!("clipboard: {} files (uri-list) via arboard", paths.len());
    Ok(())
}

fn copy_wayland(copied: &CopiedFiles) -> Result<(), String> {
    use wl_clipboard_rs::copy::{MimeSource, MimeType, Options, Source};
    let source = |text: &str, mime: MimeType| MimeSource { source: Source::Bytes(text.as_bytes().into()), mime_type: mime };
    Options::new()
        .copy_multi(vec![
            source(&copied.uri_list, MimeType::Specific("text/uri-list".into())),
            source(&copied.gnome, MimeType::Specific("x-special/gnome-copied-files".into())),
            source(&copied.text, MimeType::Text),
        ])
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Needs a display: `cargo test -- --ignored copy_files_to_the_desktop`.
    #[test]
    #[ignore]
    fn copy_files_to_the_desktop() {
        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path().canonicalize().unwrap();
        let paths = vec![dir.join("a b.jpg"), dir.join("c.png")];
        for p in &paths {
            std::fs::write(p, b"x").unwrap();
        }
        copy_files(&paths).unwrap();
        let back = arboard::Clipboard::new().unwrap().get().file_list().unwrap();
        assert_eq!(back, paths);
    }
}
