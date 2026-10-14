//! Device backends and discovery.

use std::path::Path;

use crate::device::{CaptureDevice, DeviceKind};

mod dv;
mod mock;
mod v850;

/// Probes every backend for connected hardware.
///
/// Implemented:
/// - Epson Perfection V850 (`v850.rs`, via `utils/epson/v850` and `utils/photo-duplex`); the
///   photos it cuts out of each scan go to `<out_dir>/epson-v850/`.
/// - DV / Digital8 camcorders over FireWire (`dv.rs`, via `dvgrab` and `dvcont`); tapes go to
///   `<out_dir>/<model>/tape-NNNN.dv`, e.g. `sony-dcr-trv240/`.
///
/// Backends to implement:
/// - FF-680W: SANE `epsonds` (or Epson's SDK), matched by USB VID/PID.
/// - VHS / Hi8 analog: V4L2 capture cards (`/dev/video*`) + ALSA audio, encoded via ffmpeg/GStreamer.
/// - Kodak Reels: enumerates as a UVC camera, so V4L2 again.
///
/// Every backend that saves stills reports each with [`DeviceEvent::Saved`], carrying the
/// in-memory image, so hosts can seed thumbnails without decoding the file. Scanner stills
/// saved as TIFF should include a ≤1024 px NewSubfileType=1 page (see the GUI's
/// `thumbs::write_tiff_with_reduced_page`), so a cold thumbnail cache reads only the small
/// page (~2 ms instead of ~80–220 ms for a 600 dpi scan).
///
/// `ARCHIVE_STUDIO_MOCK=ff680,ff680,vhs,reel,hi8` adds simulated devices.
///
/// [`DeviceEvent::Saved`]: crate::device::DeviceEvent::Saved
pub fn discover(out_dir: &Path) -> Vec<Box<dyn CaptureDevice>> {
    let mut found: Vec<Box<dyn CaptureDevice>> = Vec::new();
    if v850::present() {
        found.push(Box::new(v850::V850::new(out_dir.join("epson-v850"))));
    }
    if let Some(unit) = dv::find() {
        found.push(camcorder(unit, out_dir));
    }
    if let Ok(spec) = std::env::var("ARCHIVE_STUDIO_MOCK") {
        found.extend(mock::from_spec(&spec, out_dir));
    }
    found
}

/// Devices switched on or plugged in since `known` was discovered; cheap enough to poll
/// every few seconds (only reads sysfs). For now just DV camcorders: once added, a
/// camcorder's own controller copes with it going away and coming back.
pub fn hotplug(out_dir: &Path, known: &[Box<dyn CaptureDevice>]) -> Vec<Box<dyn CaptureDevice>> {
    let Some(unit) = dv::find() else { return Vec::new() };
    if known.iter().any(|d| d.kind() == DeviceKind::Camcorder && d.name() == unit.name()) {
        return Vec::new();
    }
    vec![camcorder(unit, out_dir)]
}

fn camcorder(unit: dv::Unit, out_dir: &Path) -> Box<dyn CaptureDevice> {
    let dir = out_dir.join(unit.slug());
    Box::new(dv::Camcorder::new(unit, dir))
}
