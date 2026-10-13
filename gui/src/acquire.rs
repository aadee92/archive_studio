//! Acquire mode: device discovery, live previews and capture control.
//!
//! Each backend implements [`CaptureDevice`] and does its I/O on its own threads.
//! Everything it reports flows through an [`EventSink`], which hops onto the UI
//! thread. Preview frames are coalesced per device (latest frame wins, at most one
//! UI update queued), so a busy UI never builds a backlog however many devices stream.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use slint::{ComponentHandle, Image, Model, ModelRc, Rgb8Pixel, SharedPixelBuffer, VecModel};

use crate::{AcquireBridge, AppWindow, DeviceState, DeviceTile};

mod v850;

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
            Self::Camcorder => "Hi8 camcorder",
        }
    }

    pub fn is_video(self) -> bool {
        !matches!(self, Self::PhotoScanner | Self::Flatbed)
    }
}

/// A downscaled preview frame (~640px wide), never a full-resolution capture.
pub type Frame = SharedPixelBuffer<Rgb8Pixel>;

pub enum DeviceEvent {
    Preview(Frame),
    /// Free-form status line, e.g. "600 dpi · feeder loaded" or "00:12:31".
    Status(String),
    /// Acquisition began on the device itself, e.g. from the scanner's own button.
    Started,
    Captured(u32),
    /// Back to idle; also clears an earlier error once the device recovers.
    Finished,
    Error(String),
}

/// Thread-safe callback a backend uses to report to the UI.
pub type EventSink = Arc<dyn Fn(DeviceEvent) + Send + Sync>;

/// Implemented by each device backend. Methods are called on the UI thread and
/// must not block. Dropping a device must stop its threads and release the hardware.
///
/// After writing each still capture to disk, call
/// [`thumbs::store_for_new_file`](crate::thumbs::store_for_new_file) with the in-memory
/// image (from the backend's thread), so the review grid never decodes the full scan.
pub trait CaptureDevice {
    fn name(&self) -> &str;
    fn kind(&self) -> DeviceKind;
    fn start_preview(&mut self, sink: EventSink);
    /// Scanners: run the feeder until empty. Video: record until [`stop`](Self::stop).
    fn start_acquire(&mut self, out_dir: &Path, sink: EventSink);
    /// Stops acquiring; the preview keeps running.
    fn stop(&mut self);
}

/// Probes every backend for connected hardware.
///
/// Implemented: Epson Perfection V850 (`v850.rs`, via `utils/epson/v850`); its scans go to
/// `<out_dir>/epson-v850/`.
///
/// Backends to implement:
/// - FF-680W: SANE `epsonds` (or Epson's SDK), matched by USB VID/PID.
/// - VHS / Hi8 analog: V4L2 capture cards (`/dev/video*`) + ALSA audio, encoded via ffmpeg/GStreamer.
/// - Hi8 / Digital8 over FireWire: `dvgrab`.
/// - Kodak Reels: enumerates as a UVC camera, so V4L2 again.
///
/// Every backend that saves stills must seed thumbnails via `thumbs::store_for_new_file`.
/// Scanner stills saved as TIFF should be written with `thumbs::write_tiff_with_reduced_page`
/// (full scan + a ≤1024 px NewSubfileType=1 page), so a cold thumbnail cache reads only the
/// small page (~2 ms instead of ~80–220 ms for a 600 dpi scan). Compressed or multi-page
/// output would need the same reduced page added via the `tiff` encoder.
///
/// `ARCHIVE_STUDIO_MOCK=ff680,ff680,vhs,reel,hi8` adds simulated devices.
pub fn discover(out_dir: &Path) -> Vec<Box<dyn CaptureDevice>> {
    let mut found: Vec<Box<dyn CaptureDevice>> = Vec::new();
    if v850::present() {
        found.push(Box::new(v850::V850::new(out_dir.join("epson-v850"))));
    }
    if let Ok(spec) = std::env::var("ARCHIVE_STUDIO_MOCK") {
        found.extend(mock::from_spec(&spec));
    }
    found
}

pub struct Acquire {
    ui: slint::Weak<AppWindow>,
    devices: RefCell<Vec<Box<dyn CaptureDevice>>>,
    tiles: Rc<VecModel<DeviceTile>>,
    out_dir: PathBuf,
    /// Bumped on rescan so late events from dropped devices can't land on new tiles.
    epoch: Arc<AtomicU64>,
}

pub fn install(ui: &AppWindow, out_dir: &Path) -> Rc<Acquire> {
    let this = Rc::new(Acquire {
        ui: ui.as_weak(),
        devices: RefCell::default(),
        tiles: Rc::new(VecModel::default()),
        out_dir: out_dir.to_path_buf(),
        epoch: Arc::default(),
    });

    let bridge = ui.global::<AcquireBridge>();
    bridge.set_devices(ModelRc::from(this.tiles.clone()));
    bridge.set_output_dir(out_dir.display().to_string().into());

    let t = this.clone();
    bridge.on_rescan(move || t.rescan());
    let t = this.clone();
    bridge.on_start(move |row| t.start(row as usize));
    let t = this.clone();
    bridge.on_stop(move |row| t.stop(row as usize));
    let t = this.clone();
    bridge.on_start_all(move || (0..t.tiles.row_count()).for_each(|row| t.start(row)));

    this.rescan();
    this
}

impl Acquire {
    fn rescan(&self) {
        self.epoch.fetch_add(1, Ordering::AcqRel);
        let mut devices = self.devices.borrow_mut();
        *devices = discover(&self.out_dir);

        let tiles: Vec<DeviceTile> = devices
            .iter()
            .map(|d| DeviceTile {
                name: d.name().into(),
                kind_label: d.kind().label().into(),
                is_video: d.kind().is_video(),
                state: DeviceState::Previewing,
                ..Default::default()
            })
            .collect();
        self.tiles.set_vec(tiles);

        for (row, device) in devices.iter_mut().enumerate() {
            device.start_preview(self.sink_for(row));
        }
    }

    fn start(&self, row: usize) {
        let mut devices = self.devices.borrow_mut();
        let (Some(device), Some(mut tile)) = (devices.get_mut(row), self.tiles.row_data(row)) else {
            return;
        };
        if tile.state != DeviceState::Previewing {
            return;
        }
        // TODO: output layout / naming, e.g. <out>/<date>/<device>/<seq>.tif
        device.start_acquire(&self.out_dir, self.sink_for(row));
        tile.state = DeviceState::Acquiring;
        self.tiles.set_row_data(row, tile);
    }

    fn stop(&self, row: usize) {
        if let Some(device) = self.devices.borrow_mut().get_mut(row) {
            device.stop();
        }
        if let Some(mut tile) = self.tiles.row_data(row) {
            tile.state = DeviceState::Previewing;
            self.tiles.set_row_data(row, tile);
        }
    }

    fn sink_for(&self, row: usize) -> EventSink {
        let ui = self.ui.clone();
        let epoch = self.epoch.clone();
        let mine = epoch.load(Ordering::Acquire);
        let latest: Arc<Mutex<Option<Frame>>> = Arc::default();
        let queued = Arc::new(AtomicBool::new(false));

        Arc::new(move |event| {
            let epoch = epoch.clone();
            let deliver = move |ui: AppWindow, event| {
                if epoch.load(Ordering::Acquire) == mine {
                    apply(&ui, row, event);
                }
            };
            match event {
                DeviceEvent::Preview(frame) => {
                    *latest.lock().unwrap() = Some(frame);
                    if !queued.swap(true, Ordering::AcqRel) {
                        let (latest, queued) = (latest.clone(), queued.clone());
                        let _ = ui.upgrade_in_event_loop(move |ui| {
                            queued.store(false, Ordering::Release);
                            if let Some(frame) = latest.lock().unwrap().take() {
                                deliver(ui, DeviceEvent::Preview(frame));
                            }
                        });
                    }
                }
                other => {
                    let _ = ui.upgrade_in_event_loop(move |ui| deliver(ui, other));
                }
            }
        })
    }
}

fn apply(ui: &AppWindow, row: usize, event: DeviceEvent) {
    let tiles = ui.global::<AcquireBridge>().get_devices();
    let Some(mut tile) = tiles.row_data(row) else { return };
    match event {
        DeviceEvent::Preview(frame) => {
            tile.preview = Image::from_rgb8(frame);
            tile.has_preview = true;
        }
        DeviceEvent::Status(text) => tile.detail = text.into(),
        DeviceEvent::Captured(n) => {
            tile.captured = n as i32;
            tile.detail = format!("{n} captured").into();
        }
        DeviceEvent::Started => tile.state = DeviceState::Acquiring,
        DeviceEvent::Finished => tile.state = DeviceState::Previewing,
        DeviceEvent::Error(text) => {
            tile.state = DeviceState::Error;
            tile.detail = text.into();
        }
    }
    tiles.set_row_data(row, tile);
}

/// Simulated devices for UI development.
mod mock {
    use super::*;
    use std::thread;
    use std::time::Duration;

    pub fn from_spec(spec: &str) -> Vec<Box<dyn CaptureDevice>> {
        spec.split(',')
            .map(str::trim)
            .enumerate()
            .filter_map(|(i, s)| {
                let (kind, name) = match s {
                    "ff680" => (DeviceKind::PhotoScanner, "Epson FastFoto FF-680W"),
                    "vhs" => (DeviceKind::VhsCapture, "USB Video Capture"),
                    "reel" => (DeviceKind::FilmReel, "Kodak Reels 8mm"),
                    "hi8" => (DeviceKind::Camcorder, "Sony Hi8 Handycam"),
                    _ => return None,
                };
                Some(Box::new(Mock::new(format!("{name} #{}", i + 1), kind)) as Box<dyn CaptureDevice>)
            })
            .collect()
    }

    struct Mock {
        name: String,
        kind: DeviceKind,
        alive: Arc<AtomicBool>,
        acquiring: Arc<AtomicBool>,
    }

    impl Mock {
        fn new(name: String, kind: DeviceKind) -> Self {
            Self { name, kind, alive: Arc::new(AtomicBool::new(true)), acquiring: Arc::default() }
        }
    }

    impl Drop for Mock {
        fn drop(&mut self) {
            self.alive.store(false, Ordering::Release);
        }
    }

    impl CaptureDevice for Mock {
        fn name(&self) -> &str {
            &self.name
        }

        fn kind(&self) -> DeviceKind {
            self.kind
        }

        fn start_preview(&mut self, sink: EventSink) {
            let (alive, acquiring) = (self.alive.clone(), self.acquiring.clone());
            let video = self.kind.is_video();
            thread::spawn(move || {
                let (mut tick, mut recorded, mut captured) = (0u32, 0u32, 0u32);
                while alive.load(Ordering::Acquire) {
                    // Video streams at 10 fps; scanners show the last scanned photo.
                    if video || tick % 10 == 0 {
                        sink(DeviceEvent::Preview(test_pattern(tick)));
                    }
                    if tick % 10 == 0 {
                        if !acquiring.load(Ordering::Acquire) {
                            recorded = 0;
                        } else if video {
                            recorded += 1;
                            let (h, m, s) = (recorded / 3600, recorded / 60 % 60, recorded % 60);
                            sink(DeviceEvent::Status(format!("{h:02}:{m:02}:{s:02} · 720×480")));
                        } else {
                            captured += 1;
                            sink(DeviceEvent::Captured(captured));
                            if captured % 24 == 0 {
                                acquiring.store(false, Ordering::Release);
                                sink(DeviceEvent::Finished);
                            }
                        }
                    }
                    tick += 1;
                    thread::sleep(Duration::from_millis(100));
                }
            });
        }

        fn start_acquire(&mut self, _out_dir: &Path, _sink: EventSink) {
            self.acquiring.store(true, Ordering::Release);
        }

        fn stop(&mut self) {
            self.acquiring.store(false, Ordering::Release);
        }
    }

    fn test_pattern(tick: u32) -> Frame {
        let (w, h) = (320u32, 240u32);
        let bar = (tick * 8) % w;
        let mut frame = Frame::new(w, h);
        for (i, px) in frame.make_mut_slice().iter_mut().enumerate() {
            let (x, y) = (i as u32 % w, i as u32 / w);
            *px = if x.abs_diff(bar) < 4 {
                Rgb8Pixel::new(255, 255, 255)
            } else {
                Rgb8Pixel::new((x * 200 / w) as u8 + 40, (y * 200 / h) as u8 + 40, 170)
            };
        }
        frame
    }
}
