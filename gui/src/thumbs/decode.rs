//! Source-file decoding: the slow path, only taken on a cache miss. Done by the `picture`
//! crate (`utils/picture`, optimized in debug builds too): each format decoded no larger than
//! the requested edge needs, upright.
//!
//! TODO: Video: one frame ~10% in via ffmpeg / libav. Audio: embedded cover art.

use std::path::Path;

use image::RgbImage;

use archive_core::media::MediaKind;

#[allow(unused_imports)] // `decode_preview`, `write_tiff_with_reduced_page`: for capture backends
pub use picture::{average_color, decode_preview, dimensions, fit, is_tiff, preview_from_bytes, upright_dimensions, write_tiff_with_reduced_page};

/// Decodes `path` to an upright RGB picture no larger than `edge` on its longest side.
pub fn decode(path: &Path, kind: MediaKind, edge: u32) -> Option<RgbImage> {
    if kind != MediaKind::Image {
        return None;
    }
    picture::decode(path, edge)
}

/// [`decode`] at full resolution, for looking closely (zoom), unless that is more than
/// `max_pixels`: then as large as fits them.
pub fn decode_full(path: &Path, kind: MediaKind, max_pixels: u64) -> Option<RgbImage> {
    if kind != MediaKind::Image {
        return None;
    }
    picture::decode_full(path, max_pixels)
}

/// [`decode`] of a file already read into memory (by an I/O thread).
pub fn decode_bytes(bytes: &[u8], kind: MediaKind, edge: u32) -> Option<RgbImage> {
    if kind != MediaKind::Image {
        return None;
    }
    picture::decode_bytes(bytes, edge)
}
