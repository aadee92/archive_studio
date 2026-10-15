//! Acquire mode: one tile per capture device, hosting the backends in
//! [`archive_core::devices`].
//!
//! Each backend does its I/O on its own threads and reports through an [`EventSink`].
//! The sink does the work that mustn't touch the UI thread there (seeding the thumbnail
//! cache from a saved capture, loading thumbnails of earlier ones, converting pictures),
//! then hops onto the UI thread. Preview frames are coalesced per device (latest frame
//! wins, at most one UI update queued), so a busy UI never builds a backlog however many
//! devices stream.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use slint::{ComponentHandle, Image, Model, ModelRc, Rgb8Pixel, SharedPixelBuffer, SharedString, Timer, TimerMode, VecModel};

use archive_core::device::{CaptureDevice, DeviceEvent, EventSink, Frame, RECENT};
use archive_core::devices;
use archive_core::image::DynamicImage;

use crate::pipeline::Handle;
use crate::thumbs::{self, Tier};
use crate::nav::{self, Scope, Target};
use crate::{AcquireBridge, AppWindow, DeviceState, DeviceTile};

/// How often to look for hardware plugged in after launch (see [`Acquire::hotplug`]).
const HOTPLUG_POLL: Duration = Duration::from_secs(2);

type Buffer = SharedPixelBuffer<Rgb8Pixel>;

/// What reaches the UI thread: a [`DeviceEvent`] with its pictures already converted.
enum Update {
    Preview(Buffer),
    /// A thumbnail for the strip of recent captures, and its file.
    Recent(Buffer, PathBuf),
    Event(DeviceEvent),
}

pub struct Acquire {
    ui: slint::Weak<AppWindow>,
    devices: RefCell<Vec<Box<dyn CaptureDevice>>>,
    tiles: Rc<VecModel<DeviceTile>>,
    out_dir: PathBuf,
    /// Saved captures are handed to the pipeline.
    pipeline: Handle,
    /// Bumped on rescan so late events from dropped devices can't land on new tiles.
    epoch: Arc<AtomicU64>,
    hotplug: Timer,
}

pub fn install(ui: &AppWindow, out_dir: &Path, pipeline: Handle) -> Rc<Acquire> {
    let this = Rc::new(Acquire {
        ui: ui.as_weak(),
        devices: RefCell::default(),
        tiles: Rc::new(VecModel::default()),
        out_dir: out_dir.to_path_buf(),
        pipeline,
        epoch: Arc::default(),
        hotplug: Timer::default(),
    });

    let bridge = ui.global::<AcquireBridge>();
    bridge.set_devices(ModelRc::from(this.tiles.clone()));
    bridge.set_output_dir(out_dir.display().to_string().into());

    let t = this.clone();
    bridge.on_rescan(move || t.rescan());
    let t = this.clone();
    bridge.on_start(move |row| t.start(row as usize));
    let t = this.clone();
    bridge.on_start_alt(move |row| t.start_alt(row as usize));
    let t = this.clone();
    bridge.on_stop(move |row| t.stop(row as usize));
    let t = this.clone();
    bridge.on_start_all(move || (0..t.tiles.row_count()).for_each(|row| t.start(row)));
    let t = this.clone();
    bridge.on_open_recent(move |row, slot| {
        if let (Some(ui), Some(target)) = (t.ui.upgrade(), t.recent_target(row as usize, slot as usize)) {
            nav::open(&ui, target);
        }
    });
    let t = this.clone();
    bridge.on_context_menu(move |row, slot| {
        if let (Some(ui), Some(target)) = (t.ui.upgrade(), t.recent_target(row as usize, slot as usize)) {
            nav::context_menu(&ui, target);
        }
    });

    this.rescan();
    let weak: Weak<Acquire> = Rc::downgrade(&this);
    this.hotplug.start(TimerMode::Repeated, HOTPLUG_POLL, move || {
        if let Some(t) = weak.upgrade() {
            t.hotplug();
        }
    });
    this
}

impl Acquire {
    /// Recent capture `slot` of device `row`, as an item to open; its scope is the device's
    /// recent captures, newest first.
    fn recent_target(&self, row: usize, slot: usize) -> Option<Target> {
        let tile = self.tiles.row_data(row)?;
        let paths: Vec<PathBuf> = tile.recent_paths.iter().map(|p| PathBuf::from(p.as_str())).collect();
        let path = paths.get(slot)?.clone();
        Some(Target::new(path, Scope::of(format!("Acquire · {}", tile.name), paths, Some(slot))))
    }

    fn rescan(&self) {
        self.epoch.fetch_add(1, Ordering::AcqRel);
        let mut devices = self.devices.borrow_mut();
        *devices = devices::discover(&self.out_dir);

        let tiles: Vec<DeviceTile> = devices.iter().map(|d| tile_for(d.as_ref())).collect();
        self.tiles.set_vec(tiles);

        for (row, device) in devices.iter_mut().enumerate() {
            device.start_preview(self.sink_for(row));
        }
        report_capturing(&*self.tiles);
    }

    /// Adds devices switched on or plugged in after the last scan, leaving the others (and
    /// anything they're capturing) alone.
    fn hotplug(&self) {
        let mut devices = self.devices.borrow_mut();
        for mut device in devices::hotplug(&self.out_dir, &devices) {
            let row = devices.len();
            self.tiles.push(tile_for(device.as_ref()));
            device.start_preview(self.sink_for(row));
            devices.push(device);
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
        report_capturing(&*self.tiles);
    }

    fn start_alt(&self, row: usize) {
        let mut devices = self.devices.borrow_mut();
        let (Some(device), Some(mut tile)) = (devices.get_mut(row), self.tiles.row_data(row)) else {
            return;
        };
        if tile.state != DeviceState::Previewing || !tile.alt_enabled {
            return;
        }
        device.start_alt(&self.out_dir, self.sink_for(row));
        tile.state = DeviceState::Acquiring;
        self.tiles.set_row_data(row, tile);
        report_capturing(&*self.tiles);
    }

    fn stop(&self, row: usize) {
        if let Some(device) = self.devices.borrow_mut().get_mut(row) {
            device.stop();
        }
        if let Some(mut tile) = self.tiles.row_data(row) {
            tile.state = DeviceState::Previewing;
            self.tiles.set_row_data(row, tile);
        }
        report_capturing(&*self.tiles);
    }

    fn sink_for(&self, row: usize) -> EventSink {
        let (ui, pipeline) = (self.ui.clone(), self.pipeline.clone());
        let epoch = self.epoch.clone();
        let mine = epoch.load(Ordering::Acquire);
        let latest: Arc<Mutex<Option<Buffer>>> = Arc::default();
        let queued = Arc::new(AtomicBool::new(false));

        // Runs on the backend's threads.
        Arc::new(move |event| {
            let epoch = epoch.clone();
            let deliver = move |ui: AppWindow, update| {
                if epoch.load(Ordering::Acquire) == mine {
                    apply(&ui, row, update);
                }
            };
            let update = match event {
                DeviceEvent::Preview(frame) => {
                    *latest.lock().unwrap() = Some(buffer(&frame));
                    if !queued.swap(true, Ordering::AcqRel) {
                        let (latest, queued) = (latest.clone(), queued.clone());
                        let _ = ui.upgrade_in_event_loop(move |ui| {
                            queued.store(false, Ordering::Release);
                            if let Some(frame) = latest.lock().unwrap().take() {
                                deliver(ui, Update::Preview(frame));
                            }
                        });
                    }
                    return;
                }
                DeviceEvent::Saved { path, thumb, image } => {
                    // Seeds the review grid from memory, so it never decodes the file.
                    if let Some(image) = image {
                        let large = thumbs::fit(&image, Tier::Large.edge());
                        thumbs::store_for_new_file(&path, &DynamicImage::ImageRgb8(large));
                    }
                    // After seeding, so the grid finds the thumbnail when the file arrives.
                    pipeline.saved(&path);
                    Update::Recent(buffer(&thumb), path)
                }
                DeviceEvent::Recent(path) => match thumbs::small_for_file(&path) {
                    Some(thumb) => Update::Recent(buffer(&thumb), path),
                    None => return,
                },
                other => Update::Event(other),
            };
            let _ = ui.upgrade_in_event_loop(move |ui| deliver(ui, update));
        })
    }
}

fn tile_for(device: &dyn CaptureDevice) -> DeviceTile {
    DeviceTile {
        name: device.name().into(),
        kind_label: device.kind().label().into(),
        is_video: device.kind().is_video(),
        state: DeviceState::Previewing,
        ..Default::default()
    }
}

fn buffer(frame: &Frame) -> Buffer {
    Buffer::clone_from_slice(frame.as_raw(), frame.width(), frame.height())
}

/// Tells the pipeline whether any device is capturing, so it can step back meanwhile.
fn report_capturing(tiles: &impl Model<Data = DeviceTile>) {
    crate::pipeline::set_capturing(tiles.iter().any(|tile| tile.state == DeviceState::Acquiring));
}

fn apply(ui: &AppWindow, row: usize, update: Update) {
    let tiles = ui.global::<AcquireBridge>().get_devices();
    let Some(mut tile) = tiles.row_data(row) else { return };
    let event = matches!(update, Update::Event(_));
    match update {
        Update::Preview(frame) => {
            tile.preview = Image::from_rgb8(frame);
            tile.has_preview = true;
        }
        Update::Recent(thumb, path) => {
            let mut recent: Vec<Image> = tile.recent.iter().take(RECENT - 1).collect();
            recent.insert(0, Image::from_rgb8(thumb));
            tile.recent = ModelRc::new(VecModel::from(recent));
            let mut paths: Vec<SharedString> = tile.recent_paths.iter().take(RECENT - 1).collect();
            paths.insert(0, path.display().to_string().into());
            tile.recent_paths = ModelRc::new(VecModel::from(paths));
        }
        Update::Event(event) => match event {
            DeviceEvent::Status(text) => tile.detail = text.into(),
            DeviceEvent::Captured(n) => {
                tile.captured = n as i32;
                tile.detail = format!("{n} captured").into();
            }
            DeviceEvent::AltAction(label, enabled) => {
                tile.alt_action = label.into();
                tile.alt_enabled = enabled;
            }
            DeviceEvent::Started => tile.state = DeviceState::Acquiring,
            DeviceEvent::Finished => tile.state = DeviceState::Previewing,
            DeviceEvent::Error(text) => {
                tile.state = DeviceState::Error;
                tile.detail = text.into();
            }
            // Converted to the variants above in the sink.
            DeviceEvent::Preview(_) | DeviceEvent::Saved { .. } | DeviceEvent::Recent(_) => return,
        },
    }
    tiles.set_row_data(row, tile);
    if event {
        report_capturing(&tiles);
    }
}
