//! The capture device API: commands in, events out.
//!
//! A host (GUI, CLI) holds each device as a `Box<dyn CaptureDevice>` and calls its
//! methods from whatever thread it likes; they only signal the backend and return at
//! once. Each backend does its I/O on its own threads and reports everything through the
//! host's [`EventSink`], from those threads. Backends write captures to disk themselves
//! and know nothing about how a host displays or caches them.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use image::RgbImage;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeviceKind {
    /// Epson FastFoto FF-680W and similar sheet-fed photo scanners.
    PhotoScanner,
    /// Flatbed scanner, e.g. Epson Perfection V850.
    Flatbed,
    /// Composite / S-Video USB capture card fed by a VCR.
    VhsCapture,
    /// Kodak Reels 8mm / Super 8 film digitizer.
    FilmReel,
    /// Hi8 / Digital8 camcorder (analog out or FireWire).
    Camcorder,
}

impl DeviceKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::PhotoScanner => "Photo scanner",
            Self::Flatbed => "Flatbed scanner",
            Self::VhsCapture => "VHS capture",
            Self::FilmReel => "8mm film reel",
            Self::Camcorder => "DV / Hi8 camcorder",
        }
    }

    pub fn is_video(self) -> bool {
        !matches!(self, Self::PhotoScanner | Self::Flatbed)
    }
}

/// A downscaled preview frame or thumbnail, never a full-resolution capture.
pub type Frame = RgbImage;

/// How many recent captures a device reports as [`DeviceEvent::Recent`] when it starts.
pub const RECENT: usize = 5;

pub enum DeviceEvent {
    /// Live picture, ~640 px wide at most. May come at video rate: hosts that can't keep
    /// up should keep only the latest.
    Preview(Frame),
    /// Free-form status line, e.g. "600 dpi · feeder loaded" or "00:12:31".
    Status(String),
    /// Acquisition began on the device itself, e.g. from the scanner's own button.
    Started,
    Captured(u32),
    /// A capture was just saved at `path` (closed, so its size and mtime are final): for a
    /// flatbed, each photo cut out of the bed, never the whole bed.
    Saved {
        path: PathBuf,
        /// Small thumbnail (≤192 px) for a strip of recent captures.
        thumb: Frame,
        /// The capture's picture, upright, still in memory (a still at full resolution, a
        /// video's poster frame), so a host can make its own thumbnails without decoding
        /// the file. `None` when there is nothing trustworthy to seed from.
        image: Option<Arc<RgbImage>>,
    },
    /// A capture saved earlier (before this session), sent oldest first when the device
    /// starts, at most [`RECENT`]. The host finds its own thumbnail, e.g. from a cache.
    Recent(PathBuf),
    /// Label of the device's secondary action (e.g. "Scan backs") and whether it can run
    /// now. Devices that never send this have none.
    AltAction(String, bool),
    /// Back to idle; also clears an earlier error once the device recovers.
    Finished,
    Error(String),
}

/// The host's callback for a device's events. Invoked on the backend's own threads,
/// never the host's: it must be cheap or hand off (e.g. post to a UI event loop), since
/// a slow sink stalls the device.
pub type EventSink = Arc<dyn Fn(DeviceEvent) + Send + Sync>;

/// Implemented by each device backend. Methods must not block, whichever thread the host
/// calls them on: they signal the backend's threads and return. Dropping a device must
/// stop its threads and release the hardware.
pub trait CaptureDevice: Send {
    fn name(&self) -> &str;
    fn kind(&self) -> DeviceKind;
    fn start_preview(&mut self, sink: EventSink);
    /// Scanners: run the feeder until empty. Video: record until [`stop`](Self::stop).
    fn start_acquire(&mut self, out_dir: &Path, sink: EventSink);
    /// The secondary action announced with [`DeviceEvent::AltAction`].
    fn start_alt(&mut self, _out_dir: &Path, _sink: EventSink) {}
    /// Stops acquiring; the preview keeps running.
    fn stop(&mut self);
}
