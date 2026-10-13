//! Epson Perfection V850 / GT-X980 flatbed, via the `x980` userspace driver.
//!
//! One thread owns the scanner. It keeps a full-bed scan configured and polls the front
//! button, so a scan starts from either the scanner's button or the panel's Scan button.
//! Image blocks stream straight into a PNG while a box-filtered ~640 px preview is built
//! from them; the preview is shown live and seeds the review thumbnail cache, so the full
//! scan is never held in memory.

use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use nusb::MaybeFuture;
use slint::Rgb8Pixel;
use x980::{ScanParams, Scanner};

use super::{CaptureDevice, DeviceEvent, DeviceKind, EventSink, Frame};

const DEFAULT_DPI: u32 = 600;
const PREVIEW_WIDTH: u32 = 640;
const BUTTON_POLL: Duration = Duration::from_millis(250);
const RECONNECT_DELAY: Duration = Duration::from_secs(2);
const PREVIEW_INTERVAL: Duration = Duration::from_millis(300);
/// Matches `Theme.well`, so the unscanned part of the preview blends with the panel.
const UNSCANNED: Rgb8Pixel = Rgb8Pixel::new(238, 240, 243);

/// True if a V850 is plugged in. Only lists USB devices; doesn't open or claim it.
pub fn present() -> bool {
    nusb::list_devices().wait().is_ok_and(|mut devices| {
        devices.any(|d| d.vendor_id() == x980::usb::VENDOR_ID && d.product_id() == x980::usb::PRODUCT_ID)
    })
}

/// Flags shared between the UI-side handle and the scanner thread.
struct Control {
    alive: AtomicBool,
    start: AtomicBool,
    cancel: AtomicBool,
}

pub struct V850 {
    out_dir: PathBuf,
    dpi: u32,
    control: Arc<Control>,
    started: bool,
}

impl V850 {
    /// Scans are saved as `<out_dir>/scan-NNNN.png`. `ARCHIVE_STUDIO_V850_DPI` overrides the
    /// default 600 dpi (clamped to the scanner's maximum).
    pub fn new(out_dir: PathBuf) -> Self {
        let dpi = std::env::var("ARCHIVE_STUDIO_V850_DPI").ok().and_then(|v| v.parse().ok()).unwrap_or(DEFAULT_DPI);
        Self {
            out_dir,
            dpi,
            control: Arc::new(Control {
                alive: AtomicBool::new(true),
                start: AtomicBool::new(false),
                cancel: AtomicBool::new(false),
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

    fn stop(&mut self) {
        self.control.cancel.store(true, Ordering::Release);
    }
}

/// Reconnects after any USB or protocol error (unplug, power cycle, scanner busy).
fn run(control: &Control, out_dir: &Path, dpi: u32, sink: &EventSink) {
    let mut last_error = None;
    let mut captured = 0;
    while control.alive.load(Ordering::Acquire) {
        if let Err(e) = session(control, out_dir, dpi, sink, last_error.take(), &mut captured) {
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
    captured: &mut u32,
) -> x980::Result<()> {
    let usb = x980::usb::Usb::open()?.ok_or("scanner not connected")?;
    let mut scanner = Scanner::new(usb);
    scanner.reset()?;
    let id = scanner.identity()?;
    let dpi = dpi.clamp(1, id.max_dpi);
    let (width, height) = id.flatbed_at(dpi);
    let params = ScanParams { dpi, left: 0, top: 0, width, height };

    let ready = format!("{} · {dpi} dpi · press Scan or the scanner button", id.model);
    sink(DeviceEvent::Finished);
    sink(DeviceEvent::Status(match last_error {
        Some(e) => format!("Reconnected after: {e}"),
        None => ready.clone(),
    }));
    // Drop a press latched before we connected.
    scanner.button()?;

    while control.alive.load(Ordering::Acquire) {
        // Parameters must go in before the wait: after a button press the scanner expects
        // FS G directly (see `x980::wait_for_button`).
        scanner.configure(&params)?;
        if !wait_for_trigger(&mut scanner, control)? {
            break;
        }
        sink(DeviceEvent::Started);
        match scan_to_png(&mut scanner, &params, out_dir, control, sink)? {
            Some(path) => {
                *captured += 1;
                sink(DeviceEvent::Captured(*captured));
                let name = path.file_name().unwrap_or_default().to_string_lossy();
                sink(DeviceEvent::Status(format!("Saved {name} · {captured} this session")));
            }
            None => sink(DeviceEvent::Status(format!("Scan cancelled · {ready}"))),
        }
        sink(DeviceEvent::Finished);
        // Ignore presses made while the scan was running.
        scanner.button()?;
    }
    Ok(())
}

/// Waits for the panel's Scan button or the scanner's front button. `false` on shutdown.
fn wait_for_trigger(scanner: &mut Scanner, control: &Control) -> x980::Result<bool> {
    loop {
        if !control.alive.load(Ordering::Acquire) {
            return Ok(false);
        }
        if control.start.swap(false, Ordering::AcqRel) || scanner.button()? != 0 {
            control.cancel.store(false, Ordering::Release);
            return Ok(true);
        }
        thread::sleep(BUTTON_POLL);
    }
}

/// Runs the configured scan into the next free `scan-NNNN.png`. `None` if cancelled.
fn scan_to_png(
    scanner: &mut Scanner,
    params: &ScanParams,
    out_dir: &Path,
    control: &Control,
    sink: &EventSink,
) -> x980::Result<Option<PathBuf>> {
    fs::create_dir_all(out_dir)?;
    let path = next_free_path(out_dir)?;
    let tmp = path.with_extension("png.part");

    sink(DeviceEvent::Status("Warming up…".into()));
    scanner.wait_ready()?;

    let mut png = png::Encoder::new(BufWriter::new(File::create(&tmp)?), params.width, params.height);
    png.set_color(png::ColorType::Rgb);
    png.set_depth(png::BitDepth::Eight);
    png.set_compression(png::Compression::Fast);
    let ppm = (params.dpi as f64 / 0.0254).round() as u32;
    png.set_pixel_dims(Some(png::PixelDimensions { xppu: ppm, yppu: ppm, unit: png::Unit::Meter }));
    let mut writer = png.write_header()?.into_stream_writer()?;

    let mut preview = Preview::new(params.width, params.height);
    let expected = params.bytes_per_line() * params.height as usize;
    let mut received = 0usize;
    let mut last_update = Instant::now();
    let result = scanner.scan(|data| {
        if control.cancel.load(Ordering::Acquire) {
            return Err("cancelled".into());
        }
        received += data.len();
        if received > expected {
            return Err(format!("scanner sent more than the expected {expected} bytes").into());
        }
        writer.write_all(data)?;
        preview.feed(data);
        if last_update.elapsed() >= PREVIEW_INTERVAL {
            last_update = Instant::now();
            sink(DeviceEvent::Preview(preview.frame.clone()));
            sink(DeviceEvent::Status(format!("Scanning {}% · {} dpi", received * 100 / expected, params.dpi)));
        }
        Ok(())
    });
    let result = result.and_then(|()| {
        if received != expected {
            return Err(format!("scan ended early: {received} of {expected} bytes").into());
        }
        writer.finish()?;
        Ok(())
    });
    if let Err(e) = result {
        let _ = fs::remove_file(&tmp);
        return if control.cancel.load(Ordering::Acquire) { Ok(None) } else { Err(e) };
    }
    fs::rename(&tmp, &path)?;

    sink(DeviceEvent::Preview(preview.frame.clone()));
    crate::thumbs::store_for_new_file(&path, &image::DynamicImage::ImageRgb8(preview.into_image()));
    Ok(Some(path))
}

/// `scan-0001.png`, `scan-0002.png`, ... picking the first number after the highest present.
fn next_free_path(dir: &Path) -> x980::Result<PathBuf> {
    let mut highest = 0u32;
    for entry in fs::read_dir(dir)? {
        let name = entry?.file_name();
        let n = name
            .to_str()
            .and_then(|s| s.strip_prefix("scan-"))
            .and_then(|s| s.strip_suffix(".png"))
            .and_then(|s| s.parse::<u32>().ok());
        highest = highest.max(n.unwrap_or(0));
    }
    Ok(dir.join(format!("scan-{:04}.png", highest + 1)))
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
        let mut frame = Frame::new(w, h);
        frame.make_mut_slice().fill(UNSCANNED);
        Self {
            frame,
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
        let (w, step) = (self.frame.width() as usize, self.step);
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
        let row = &mut self.frame.make_mut_slice()[self.row * w..(self.row + 1) * w];
        for (px, sum) in row.iter_mut().zip(self.sums.chunks_exact(3)) {
            *px = Rgb8Pixel::new((sum[0] / n) as u8, (sum[1] / n) as u8, (sum[2] / n) as u8);
        }
        self.sums.fill(0);
        self.lines_in_row = 0;
        self.row += 1;
    }

    fn into_image(self) -> image::RgbImage {
        let (w, h) = (self.frame.width(), self.frame.height());
        image::RgbImage::from_raw(w, h, self.frame.as_bytes().to_vec()).expect("preview buffer size")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let image = preview.into_image();
        assert_eq!(image.dimensions(), (500, 3));
        for (y, expected_g) in [(0, 15), (1, 55), (2, 95)] {
            assert_eq!(image.get_pixel(123, y).0, [150, expected_g, 7]);
        }
    }

    #[test]
    fn preview_shows_unscanned_rows_until_they_arrive() {
        let mut preview = Preview::new(640, 4);
        preview.feed(&vec![9u8; 640 * 3]);
        let image = preview.into_image();
        assert_eq!(image.get_pixel(0, 0).0, [9, 9, 9]);
        assert_eq!(image.get_pixel(0, 3).0, [UNSCANNED.r, UNSCANNED.g, UNSCANNED.b]);
    }

    #[test]
    fn next_free_path_follows_the_highest_number() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(next_free_path(dir.path()).unwrap(), dir.path().join("scan-0001.png"));
        fs::write(dir.path().join("scan-0007.png"), b"").unwrap();
        fs::write(dir.path().join("scan-0002.png.part"), b"").unwrap();
        assert_eq!(next_free_path(dir.path()).unwrap(), dir.path().join("scan-0008.png"));
    }
}

/// Scans the real bed once at 75 dpi through the same path the panel uses.
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
        while !has("captured 1") {
            assert!(Instant::now() < deadline && !has("error"), "scan failed: {:?}", events.lock().unwrap());
            thread::sleep(Duration::from_millis(200));
        }
        drop(device);
        let saved = out.join("scan-0001.png");
        let image = image::open(&saved).expect("saved PNG decodes");
        eprintln!("saved {} {}x{}", saved.display(), image.width(), image.height());
    }
}
