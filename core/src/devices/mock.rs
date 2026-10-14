//! Simulated devices for UI development. Nothing is written: the paths in their
//! [`DeviceEvent::Saved`] events don't exist.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use image::Rgb;

use crate::device::{CaptureDevice, DeviceEvent, DeviceKind, EventSink, Frame};

pub fn from_spec(spec: &str, out_dir: &Path) -> Vec<Box<dyn CaptureDevice>> {
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
            let dir = out_dir.join(format!("mock-{s}-{}", i + 1));
            Some(Box::new(Mock::new(format!("{name} #{}", i + 1), kind, dir)) as Box<dyn CaptureDevice>)
        })
        .collect()
}

struct Mock {
    name: String,
    kind: DeviceKind,
    dir: PathBuf,
    alive: Arc<AtomicBool>,
    acquiring: Arc<AtomicBool>,
}

impl Mock {
    fn new(name: String, kind: DeviceKind, dir: PathBuf) -> Self {
        Self { name, kind, dir, alive: Arc::new(AtomicBool::new(true)), acquiring: Arc::default() }
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
        let (alive, acquiring, dir) = (self.alive.clone(), self.acquiring.clone(), self.dir.clone());
        let video = self.kind.is_video();
        thread::spawn(move || {
            let (mut tick, mut recorded, mut captured, mut saved) = (0u32, 0u32, 0u32, 0u32);
            let mut was_recording = false;
            let mut save = |tick| {
                saved += 1;
                let ext = if video { "mkv" } else { "png" };
                let path = dir.join(format!("capture-{saved:04}.{ext}"));
                sink(DeviceEvent::Saved { path, thumb: test_pattern(tick), image: None });
            };
            while alive.load(Ordering::Acquire) {
                // Video streams at 10 fps; scanners show the last scanned photo.
                if video || tick % 10 == 0 {
                    sink(DeviceEvent::Preview(test_pattern(tick)));
                }
                if video && was_recording && !acquiring.load(Ordering::Acquire) {
                    save(tick);
                }
                was_recording = acquiring.load(Ordering::Acquire);
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
                        save(tick);
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
    Frame::from_fn(w, h, |x, y| {
        if x.abs_diff(bar) < 4 {
            Rgb([255, 255, 255])
        } else {
            Rgb([(x * 200 / w) as u8 + 40, (y * 200 / h) as u8 + 40, 170])
        }
    })
}
