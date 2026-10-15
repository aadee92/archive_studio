//! Epson Perfection V850 / GT-X980 flatbed, via the `x980` userspace driver, scanning
//! several photos at a time with `photo-duplex`.
//!
//! One thread owns the scanner. It keeps a full-bed scan configured and polls the front
//! button. Scan (or one press of the button) saves each photo on the glass as a new front,
//! `photo-NNNN-front.png`; Scan backs (or two presses about a second apart) pairs the
//! photos of that batch that were flipped over in place with their fronts,
//! `photo-NNNN-back.png`. The whole bed is shown only as the live preview, built from the
//! image blocks as they stream in; it is held in memory for the cut-out (~110 MB at
//! 600 dpi) but never saved. Each saved photo is reported with its full image, so the
//! host can seed its thumbnails from memory.
//!
//! Fronts are cut out, saved and reported as soon as their scan is in, whether or not a
//! back scan ever follows: a batch of one-sided photos needs nothing more. Dropping the
//! device lets a scan that is already in finish saving, so quitting right after a scan
//! loses nothing.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use image::{Rgb as Pixel, RgbImage};
use nusb::MaybeFuture;
use photo_duplex::detect::{DETECT_DPI, Rgb};
use photo_duplex::session::{Duplex, Event};
use x980::{ScanParams, Scanner};

use crate::device::{CaptureDevice, DeviceEvent, DeviceKind, EventSink, Frame, RECENT};
use crate::resize;

const DEFAULT_DPI: u32 = 600;
const PREVIEW_WIDTH: u32 = 640;
const BUTTON_POLL: Duration = Duration::from_millis(250);
/// A second press within this long of the first makes a double press. The scanner ignores
/// a press that comes less than about 0.9 s after the last one, so the two presses need
/// to be about a second apart.
const DOUBLE_PRESS_WINDOW: Duration = Duration::from_secs(2);
const RECONNECT_DELAY: Duration = Duration::from_secs(2);
/// Longest a drop waits for the photos of a finished scan to be saved.
const SAVE_GRACE: Duration = Duration::from_secs(60);
const PREVIEW_INTERVAL: Duration = Duration::from_millis(300);
/// Matches `Theme.well`, so the unscanned part of the preview blends with the panel.
const UNSCANNED: Pixel<u8> = Pixel([238, 240, 243]);
const SCAN_BACKS: &str = "Scan backs";
/// Longest edge of the thumbnail sent with each saved photo.
const THUMB_EDGE: u32 = 192;

/// True if a V850 is plugged in. Only lists USB devices; doesn't open or claim it.
pub fn present() -> bool {
    nusb::list_devices().wait().is_ok_and(|mut devices| {
        devices.any(|d| d.vendor_id() == x980::usb::VENDOR_ID && d.product_id() == x980::usb::PRODUCT_ID)
    })
}

/// Flags shared between the host-side handle and the scanner thread.
struct Control {
    alive: AtomicBool,
    start: AtomicBool,
    start_backs: AtomicBool,
    cancel: AtomicBool,
    /// From the start of a scan's image data until its photos are saved: the bed is only
    /// in memory, so a drop waits for this to clear.
    busy: AtomicBool,
}

pub struct V850 {
    out_dir: PathBuf,
    dpi: u32,
    control: Arc<Control>,
    started: bool,
}

impl V850 {
    /// Photos are saved in `out_dir`. `ARCHIVE_STUDIO_V850_DPI` overrides the default
    /// 600 dpi (clamped to the scanner's maximum).
    pub fn new(out_dir: PathBuf) -> Self {
        let dpi = std::env::var("ARCHIVE_STUDIO_V850_DPI").ok().and_then(|v| v.parse().ok()).unwrap_or(DEFAULT_DPI);
        Self {
            out_dir,
            dpi,
            control: Arc::new(Control {
                alive: AtomicBool::new(true),
                start: AtomicBool::new(false),
                start_backs: AtomicBool::new(false),
                cancel: AtomicBool::new(false),
                busy: AtomicBool::new(false),
            }),
            started: false,
        }
    }
}

impl Drop for V850 {
    fn drop(&mut self) {
        // The thread notices within one button poll, or at the next block mid-scan.
        self.control.alive.store(false, Ordering::Release);
        self.control.cancel.store(true, Ordering::Release);
        // A scan still coming in stops at its next block; one that is in gets its photos
        // saved first.
        let deadline = Instant::now() + SAVE_GRACE;
        while self.control.busy.load(Ordering::Acquire) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(20));
        }
    }
}

impl CaptureDevice for V850 {
    fn name(&self) -> &str {
        "Epson Perfection V850"
    }

    fn kind(&self) -> DeviceKind {
        DeviceKind::Flatbed
    }

    fn start_preview(&mut self, sink: EventSink) {
        if std::mem::replace(&mut self.started, true) {
            return;
        }
        let (control, out_dir, dpi) = (self.control.clone(), self.out_dir.clone(), self.dpi);
        thread::Builder::new()
            .name("v850".into())
            .spawn(move || run(&control, &out_dir, dpi, &sink))
            .expect("spawn V850 thread");
    }

    fn start_acquire(&mut self, _out_dir: &Path, _sink: EventSink) {
        self.control.start.store(true, Ordering::Release);
    }

    fn start_alt(&mut self, _out_dir: &Path, _sink: EventSink) {
        self.control.start_backs.store(true, Ordering::Release);
    }

    fn stop(&mut self) {
        self.control.cancel.store(true, Ordering::Release);
    }
}

/// Reconnects after any USB or protocol error (unplug, power cycle, scanner busy).
fn run(control: &Control, out_dir: &Path, dpi: u32, sink: &EventSink) {
    show_recent(out_dir, sink);
    sink(DeviceEvent::AltAction(SCAN_BACKS.into(), false));
    // Kept across reconnects, so a back scan still pairs with the last batch of fronts.
    let mut duplex = Duplex::new(out_dir.to_path_buf());
    let mut last_error = None;
    let mut captured = 0;
    while control.alive.load(Ordering::Acquire) {
        if let Err(e) = session(control, out_dir, dpi, sink, last_error.take(), &mut duplex, &mut captured) {
            if !control.alive.load(Ordering::Acquire) {
                break;
            }
            sink(DeviceEvent::Error(e.to_string()));
            last_error = Some(e.to_string());
            thread::sleep(RECONNECT_DELAY);
        }
    }
}

fn session(
    control: &Control,
    out_dir: &Path,
    dpi: u32,
    sink: &EventSink,
    last_error: Option<String>,
    duplex: &mut Duplex,
    captured: &mut u32,
) -> x980::Result<()> {
    let usb = x980::usb::Usb::open()?.ok_or("scanner not connected")?;
    let mut scanner = Scanner::new(usb);
    scanner.reset()?;
    let id = scanner.identity()?;
    let dpi = dpi.clamp(DETECT_DPI, id.max_dpi);
    let grid = id.flatbed_at(DETECT_DPI);
    let (width, height) = id.flatbed_at(dpi);
    let params = ScanParams { dpi, left: 0, top: 0, width, height };
    fs::create_dir_all(out_dir)?;

    sink(DeviceEvent::Finished);
    let mut status = match last_error {
        Some(e) => format!("Reconnected after: {e}"),
        None => format!("{} · {dpi} dpi · white mat in the lid", id.model),
    };
    // Drop a press latched before we connected.
    scanner.button()?;

    while control.alive.load(Ordering::Acquire) {
        sink(DeviceEvent::Status(format!("{status} · {}", prompt(duplex))));
        sink(DeviceEvent::AltAction(SCAN_BACKS.into(), duplex.has_pending()));
        // Parameters must go in before the wait: after a button press the scanner expects
        // FS G directly (see `x980::wait_for_button`).
        scanner.configure(&params)?;
        let Some(backs) = wait_for_trigger(&mut scanner, control, duplex.has_pending(), sink)? else {
            break;
        };
        sink(DeviceEvent::Started);
        let busy = Busy(&control.busy);
        status = match scan_bed(&mut scanner, &params, control, sink)? {
            Some(bed) => {
                sink(DeviceEvent::Status(format!("Finding photos at {dpi} dpi…")));
                save_photos(duplex, &bed, dpi, grid, backs, sink, captured)?
            }
            None => "Scan cancelled".into(),
        };
        drop(busy);
        sink(DeviceEvent::Finished);
        // Ignore presses made while the scan was running.
        scanner.button()?;
    }
    Ok(())
}

/// What to do next, for the status line.
fn prompt(duplex: &Duplex) -> String {
    match duplex.pending_numbers() {
        None => "place photos face down, then Scan".into(),
        Some(numbers) => format!("Scan new photos, or flip {numbers} over in place and Scan backs"),
    }
}

/// Waits for the panel's Scan / Scan backs buttons or the scanner's front button: one
/// press scans fronts, two about a second apart scan backs (only waited for when there
/// are fronts to pair with). `Some(backs)`, or `None` on shutdown.
fn wait_for_trigger(scanner: &mut Scanner, control: &Control, pending: bool, sink: &EventSink) -> x980::Result<Option<bool>> {
    let backs = loop {
        if !control.alive.load(Ordering::Acquire) {
            return Ok(None);
        }
        if control.start.swap(false, Ordering::AcqRel) {
            break false;
        }
        if control.start_backs.swap(false, Ordering::AcqRel) {
            break true;
        }
        if scanner.button()? != 0 {
            break pending && second_press(scanner, sink)?;
        }
        thread::sleep(BUTTON_POLL);
    };
    control.cancel.store(false, Ordering::Release);
    Ok(Some(backs))
}

/// After a first press of the scanner button, whether a second one follows in time.
fn second_press(scanner: &mut Scanner, sink: &EventSink) -> x980::Result<bool> {
    sink(DeviceEvent::Status("Press the button again to scan backs…".into()));
    let first = Instant::now();
    while first.elapsed() < DOUBLE_PRESS_WINDOW {
        thread::sleep(Duration::from_millis(40));
        if scanner.button()? != 0 {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Runs the configured whole-bed scan into memory, showing it as the live preview.
/// `None` if cancelled.
fn scan_bed(scanner: &mut Scanner, params: &ScanParams, control: &Control, sink: &EventSink) -> x980::Result<Option<Rgb>> {
    sink(DeviceEvent::Status("Warming up…".into()));
    scanner.wait_ready()?;
    // Set here rather than at the trigger, so a drop doesn't wait out the warm-up.
    control.busy.store(true, Ordering::Release);

    let expected = params.bytes_per_line() * params.height as usize;
    let mut data = Vec::with_capacity(expected);
    let mut preview = Preview::new(params.width, params.height);
    let mut last_update = Instant::now();
    let result = scanner.scan(|block| {
        if control.cancel.load(Ordering::Acquire) {
            return Err("cancelled".into());
        }
        if data.len() + block.len() > expected {
            return Err(format!("scanner sent more than the expected {expected} bytes").into());
        }
        data.extend_from_slice(block);
        preview.feed(block);
        if last_update.elapsed() >= PREVIEW_INTERVAL {
            last_update = Instant::now();
            sink(DeviceEvent::Preview(preview.frame.clone()));
            sink(DeviceEvent::Status(format!("Scanning {}% · {} dpi", data.len() * 100 / expected, params.dpi)));
        }
        Ok(())
    });
    if let Err(e) = result {
        return if control.cancel.load(Ordering::Acquire) { Ok(None) } else { Err(e) };
    }
    if data.len() != expected {
        return Err(format!("scan ended early: {} of {expected} bytes", data.len()).into());
    }
    sink(DeviceEvent::Preview(preview.frame));
    Ok(Some(Rgb { width: params.width, height: params.height, data }))
}

/// Clears [`Control::busy`] however the scan ends (saved, cancelled or failed).
struct Busy<'a>(&'a AtomicBool);

impl Drop for Busy<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

/// Cuts the photos out of `bed` and saves them, reporting each as a recent capture the
/// moment its file is in place; nothing waits for a later scan. Returns the outcome for
/// the status line.
fn save_photos(
    duplex: &mut Duplex,
    bed: &Rgb,
    dpi: u32,
    grid: (u32, u32),
    backs: bool,
    sink: &EventSink,
    captured: &mut u32,
) -> x980::Result<String> {
    let (mut summary, mut warning) = (String::new(), None);
    duplex.process(bed, dpi, grid, backs, &mut |event| match event {
        Event::Log(line) => {
            log::info!("photo-duplex: {line}");
            if warning.is_none() {
                warning = line.lines().find_map(|l| {
                    let l = l.trim();
                    l.strip_prefix("WARNING: ").or_else(|| l.strip_prefix("warning: ")).map(str::to_owned)
                });
            }
            if !line.starts_with(' ') {
                summary = line;
            }
        }
        Event::Saved { path, image } => {
            *captured += 1;
            sink(DeviceEvent::Captured(*captured));
            if let Some(name) = path.file_name() {
                sink(DeviceEvent::Status(format!("Saved {}", name.to_string_lossy())));
            }
            let Some(full) = RgbImage::from_raw(image.width, image.height, image.data.clone()) else { return };
            let thumb = resize::fit(&full, THUMB_EDGE);
            sink(DeviceEvent::Saved { path: path.to_path_buf(), thumb, image: Some(Arc::new(full)) });
        }
    })?;
    let mut status = capitalise(&summary);
    if let Some(w) = warning {
        status += &format!(" · {}", capitalise(&w));
    }
    Ok(status)
}

/// Reports the last few photos already in `out_dir` as recent captures, oldest first.
fn show_recent(out_dir: &Path, sink: &EventSink) {
    let Ok(entries) = fs::read_dir(out_dir) else { return };
    let mut photos: Vec<(SystemTime, PathBuf)> = entries
        .filter_map(|e| {
            let e = e.ok()?;
            let name = e.file_name();
            let name = name.to_str()?;
            let ours = name.starts_with("photo-") || name.starts_with("unmatched-back-");
            (ours && name.ends_with(".png")).then_some(())?;
            Some((e.metadata().ok()?.modified().ok()?, e.path()))
        })
        .collect();
    photos.sort();
    for (_, path) in photos.into_iter().rev().take(RECENT).rev() {
        sink(DeviceEvent::Recent(path));
    }
}

fn capitalise(s: &str) -> String {
    let mut chars = s.chars();
    chars.next().map_or_else(String::new, |c| c.to_uppercase().chain(chars).collect())
}

/// Box-filtered downscale of the scan, filled in as lines arrive. Blocks may split lines.
struct Preview {
    frame: Frame,
    step: usize,
    line_bytes: usize,
    /// Partial line left over from the previous block.
    carry: Vec<u8>,
    /// Per-channel sums for the preview row being accumulated.
    sums: Vec<u32>,
    lines_in_row: usize,
    row: usize,
}

impl Preview {
    fn new(width: u32, height: u32) -> Self {
        let step = width.div_ceil(PREVIEW_WIDTH).max(1);
        let (w, h) = ((width / step).max(1), (height / step).max(1));
        Self {
            frame: Frame::from_pixel(w, h, UNSCANNED),
            step: step as usize,
            line_bytes: width as usize * 3,
            carry: Vec::new(),
            sums: vec![0; w as usize * 3],
            lines_in_row: 0,
            row: 0,
        }
    }

    fn feed(&mut self, mut data: &[u8]) {
        if !self.carry.is_empty() {
            let take = (self.line_bytes - self.carry.len()).min(data.len());
            self.carry.extend_from_slice(&data[..take]);
            data = &data[take..];
            if self.carry.len() < self.line_bytes {
                return;
            }
            let line = std::mem::take(&mut self.carry);
            self.add_line(&line);
        }
        let mut lines = data.chunks_exact(self.line_bytes);
        for line in &mut lines {
            self.add_line(line);
        }
        self.carry.extend_from_slice(lines.remainder());
    }

    fn add_line(&mut self, line: &[u8]) {
        let step = self.step;
        if self.row >= self.frame.height() as usize {
            return;
        }
        for (x, sum) in self.sums.chunks_exact_mut(3).enumerate() {
            for px in line[x * step * 3..(x + 1) * step * 3].chunks_exact(3) {
                sum[0] += px[0] as u32;
                sum[1] += px[1] as u32;
                sum[2] += px[2] as u32;
            }
        }
        self.lines_in_row += 1;
        if self.lines_in_row < step {
            return;
        }
        let n = (step * step) as u32;
        for (x, sum) in self.sums.chunks_exact(3).enumerate() {
            let px = Pixel([(sum[0] / n) as u8, (sum[1] / n) as u8, (sum[2] / n) as u8]);
            self.frame.put_pixel(x as u32, self.row as u32, px);
        }
        self.sums.fill(0);
        self.lines_in_row = 0;
        self.row += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[test]
    fn preview_box_filters_lines_split_across_blocks() {
        // 2000 px wide → step 4, 500×3 preview from 12 lines; odd block size splits lines.
        let (width, height) = (2000u32, 12u32);
        let mut scan = Vec::new();
        for y in 0..height {
            for x in 0..width {
                let v = if x % 2 == 0 { 100 } else { 200 }; // averages to 150
                scan.extend_from_slice(&[v, y as u8 * 10, 7]);
            }
        }
        let mut preview = Preview::new(width, height);
        for block in scan.chunks(7001) {
            preview.feed(block);
        }
        let image = preview.frame;
        assert_eq!(image.dimensions(), (500, 3));
        for (y, expected_g) in [(0, 15), (1, 55), (2, 95)] {
            assert_eq!(image.get_pixel(123, y).0, [150, expected_g, 7]);
        }
    }

    #[test]
    fn preview_shows_unscanned_rows_until_they_arrive() {
        let mut preview = Preview::new(640, 4);
        preview.feed(&vec![9u8; 640 * 3]);
        let image = preview.frame;
        assert_eq!(image.get_pixel(0, 0).0, [9, 9, 9]);
        assert_eq!(*image.get_pixel(0, 3), UNSCANNED);
    }

    /// The bed's size at `DETECT_DPI`, like a V850's, and test scans at twice that.
    const GRID: (u32, u32) = (425, 585);
    const DPI: u32 = 2 * DETECT_DPI;

    /// White mat at `DPI` with a photo face (outline, white border, picture) at each spot.
    fn bed_with_fronts(spots: &[(u32, u32, u32, u32)]) -> Rgb {
        let (w, h) = (GRID.0 * 2, GRID.1 * 2);
        let mut data = vec![255u8; (w * h * 3) as usize];
        for &(x0, y0, pw, ph) in spots {
            for y in y0..y0 + ph {
                for x in x0..x0 + pw {
                    let edge = x == x0 || y == y0 || x == x0 + pw - 1 || y == y0 + ph - 1;
                    let border = x < x0 + 8 || y < y0 + 8 || x >= x0 + pw - 8 || y >= y0 + ph - 8;
                    let px = if edge {
                        [190, 190, 190]
                    } else if border {
                        [252, 252, 250]
                    } else {
                        [(x * 7 % 200) as u8, 90, (y * 3 % 200) as u8]
                    };
                    let i = (y * w + x) as usize * 3;
                    data[i..i + 3].copy_from_slice(&px);
                }
            }
        }
        Rgb { width: w, height: h, data }
    }

    /// Runs `save_photos`, returning the file names reported with `Saved`.
    fn saved_by(duplex: &mut Duplex, bed: &Rgb, backs: bool, captured: &mut u32) -> Vec<String> {
        let names: Arc<Mutex<Vec<String>>> = Arc::default();
        let log = names.clone();
        let sink: EventSink = Arc::new(move |event| {
            if let DeviceEvent::Saved { path, thumb, image } = event {
                assert!(path.is_file(), "{} reported before it was written", path.display());
                assert!(thumb.width() <= THUMB_EDGE && image.is_some());
                log.lock().unwrap().push(path.file_name().unwrap().to_string_lossy().into_owned());
            }
        });
        save_photos(duplex, bed, DPI, GRID, backs, &sink, captured).unwrap();
        let names = names.lock().unwrap().clone();
        names
    }

    #[test]
    fn fronts_are_saved_and_reported_without_a_back_scan() {
        let dir = tempfile::tempdir().unwrap();
        let mut duplex = Duplex::new(dir.path().to_path_buf());
        let mut captured = 0;
        let bed = bed_with_fronts(&[(60, 60, 300, 200), (460, 60, 300, 200)]);
        // One-sided photos: a scan, and nothing after it.
        assert_eq!(saved_by(&mut duplex, &bed, false, &mut captured), ["photo-0001-front.png", "photo-0002-front.png"]);
        assert_eq!(captured, 2);

        // Another batch of one-sided photos numbers on.
        let bed = bed_with_fronts(&[(60, 400, 200, 300)]);
        assert_eq!(saved_by(&mut duplex, &bed, false, &mut captured), ["photo-0003-front.png"]);

        // Scan backs pairs with that latest batch, already on disk.
        let mut flipped = bed_with_fronts(&[]);
        for y in 400..700 {
            for x in 60..260 {
                let i = (y * flipped.width + x) as usize * 3;
                flipped.data[i..i + 3].copy_from_slice(&[236, 234, 228]);
            }
        }
        assert_eq!(saved_by(&mut duplex, &flipped, true, &mut captured), ["photo-0003-back.png"]);
        assert!(!duplex.has_pending());
        assert_eq!(captured, 4);
    }

    #[test]
    fn drop_waits_for_a_scan_being_saved() {
        let device = V850::new(PathBuf::from("/nonexistent"));
        let control = device.control.clone();
        control.busy.store(true, Ordering::Release);
        let saver = thread::spawn(move || {
            // Stands in for the scanner thread: sees the drop, finishes saving.
            while control.alive.load(Ordering::Acquire) {
                thread::sleep(Duration::from_millis(5));
            }
            thread::sleep(Duration::from_millis(200));
            control.busy.store(false, Ordering::Release);
            Instant::now()
        });
        drop(device);
        let dropped = Instant::now();
        assert!(saver.join().unwrap() <= dropped, "drop returned before the photos were saved");
    }
}

/// Scans the real bed once at 75 dpi through the same path the panel uses; put a photo or
/// two on the glass (white mat in the lid) to see them cut out.
/// Needs a connected V850: `V850_OUT=/tmp/x cargo test --release -- --ignored --nocapture v850_hardware_scan`
#[cfg(test)]
mod hardware {
    use super::*;
    use std::sync::Mutex;

    #[test]
    #[ignore]
    fn v850_hardware_scan() {
        assert!(present(), "no V850 connected");
        let out = PathBuf::from(std::env::var("V850_OUT").expect("set V850_OUT"));
        let events: Arc<Mutex<Vec<String>>> = Arc::default();
        let log = events.clone();
        let sink: EventSink = Arc::new(move |event| {
            let line = match event {
                DeviceEvent::Preview(f) => format!("preview {}x{}", f.width(), f.height()),
                DeviceEvent::Status(s) => format!("status {s}"),
                DeviceEvent::Started => "started".into(),
                DeviceEvent::Captured(n) => format!("captured {n}"),
                DeviceEvent::Saved { path, thumb, .. } => format!("saved {} {}x{}", path.display(), thumb.width(), thumb.height()),
                DeviceEvent::Recent(path) => format!("recent {}", path.display()),
                DeviceEvent::AltAction(label, enabled) => format!("alt {label} {enabled}"),
                DeviceEvent::Finished => "finished".into(),
                DeviceEvent::Error(e) => format!("error {e}"),
            };
            eprintln!("{line}");
            log.lock().unwrap().push(line);
        });

        let mut device = V850::new(out.clone());
        device.dpi = 75;
        device.start_preview(sink.clone());
        let deadline = Instant::now() + Duration::from_secs(240);
        let has = |prefix: &str| events.lock().unwrap().iter().any(|e| e.starts_with(prefix));
        while !has("status Epson") && !has("status GT-") && !has("status Perfection") {
            assert!(Instant::now() < deadline && !has("error"), "never became ready: {:?}", events.lock().unwrap());
            thread::sleep(Duration::from_millis(100));
        }
        device.start_acquire(&out, sink);
        let done = || events.lock().unwrap().iter().skip_while(|e| *e != "started").any(|e| e == "finished");
        while !done() {
            assert!(Instant::now() < deadline && !has("error"), "scan failed: {:?}", events.lock().unwrap());
            thread::sleep(Duration::from_millis(200));
        }
        drop(device);
        for entry in fs::read_dir(&out).unwrap() {
            let path = entry.unwrap().path();
            let image = image::open(&path).expect("saved PNG decodes");
            eprintln!("saved {} {}x{}", path.display(), image.width(), image.height());
        }
    }
}
