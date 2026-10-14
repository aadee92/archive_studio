//! Media files as the library sees them: what kind each file is, decided by extension.

use std::path::Path;

/// Declaration order is the "Type" sort order.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum MediaKind {
    #[default]
    Image,
    Video,
    Audio,
}

/// The kind of media file `path` is, or `None` for anything the library ignores.
pub fn classify(path: &Path) -> Option<MediaKind> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    Some(match ext.as_str() {
        "jpg" | "jpeg" | "png" | "tif" | "tiff" | "webp" | "heic" | "bmp" | "dng" => MediaKind::Image,
        "mp4" | "mkv" | "mov" | "avi" | "m4v" | "mts" | "dv" | "mpg" | "mpeg" | "webm" => MediaKind::Video,
        "wav" | "flac" | "mp3" | "m4a" | "aac" | "ogg" => MediaKind::Audio,
        _ => return None,
    })
}
