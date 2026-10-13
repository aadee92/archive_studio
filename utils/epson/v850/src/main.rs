//! Standalone driver for the Epson GT-X980 / Perfection V850: waits for the scanner's
//! front button and saves a full-bed colour scan as PNG each time it is pressed.

mod escil;
mod usb;

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::thread;
use std::time::Duration;

use escil::{Identity, ScanParams, Scanner};
use usb::{Result, Usb};

const BUTTON_POLL: Duration = Duration::from_millis(250);
const RECONNECT_DELAY: Duration = Duration::from_secs(2);

const USAGE: &str = "\
usage: x980 [--out DIR] [--dpi N] [--size WxH] [--once]

  --out DIR    directory for scans (default: current directory)
  --dpi N      resolution (default: 300)
  --size WxH   scan area in pixels from the top-left corner (default: full flatbed)
  --once       scan immediately, without waiting for the button, then exit";

struct Opts {
    out: PathBuf,
    dpi: u32,
    size: Option<(u32, u32)>,
    once: bool,
}

fn parse_args() -> std::result::Result<Opts, String> {
    let mut opts = Opts { out: PathBuf::from("."), dpi: 300, size: None, once: false };
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let mut value = || args.next().ok_or(format!("{arg} needs a value"));
        match arg.as_str() {
            "--out" => opts.out = value()?.into(),
            "--dpi" => opts.dpi = value()?.parse().map_err(|_| "--dpi must be a number")?,
            "--size" => {
                let v = value()?;
                let (w, h) = v.split_once('x').ok_or("--size must look like 1200x800")?;
                let parse = |s: &str| s.parse::<u32>().map_err(|_| "--size must look like 1200x800");
                opts.size = Some((parse(w)?, parse(h)?));
            }
            "--once" => opts.once = true,
            "-h" | "--help" => return Err(USAGE.into()),
            _ => return Err(format!("unknown argument {arg}\n\n{USAGE}")),
        }
    }
    Ok(opts)
}

fn main() -> ExitCode {
    let opts = match parse_args() {
        Ok(o) => o,
        Err(msg) => {
            eprintln!("{msg}");
            return ExitCode::from(2);
        }
    };
    if let Err(e) = std::fs::create_dir_all(&opts.out) {
        eprintln!("cannot create {}: {e}", opts.out.display());
        return ExitCode::FAILURE;
    }

    // Any USB error (unplug, power cycle, protocol hiccup) drops the device and starts over.
    loop {
        match run(&opts) {
            Ok(()) => return ExitCode::SUCCESS,
            Err(e) if opts.once => {
                eprintln!("error: {e}");
                return ExitCode::FAILURE;
            }
            Err(e) => {
                eprintln!("error: {e}; reconnecting");
                thread::sleep(RECONNECT_DELAY);
            }
        }
    }
}

fn run(opts: &Opts) -> Result<()> {
    let mut scanner = Scanner::new(connect()?);
    scanner.reset()?;
    let id = scanner.identity()?;
    eprintln!(
        "connected: {} (firmware {}), flatbed {:.1} x {:.1} mm, max {} dpi",
        id.model,
        id.firmware,
        id.flatbed_width as f64 / id.base_dpi as f64 * 25.4,
        id.flatbed_height as f64 / id.base_dpi as f64 * 25.4,
        id.max_dpi,
    );
    if opts.dpi == 0 || opts.dpi > id.max_dpi {
        return Err(format!("--dpi must be between 1 and {}", id.max_dpi).into());
    }

    let params = scan_params(&id, opts)?;

    if opts.once {
        scanner.configure(&params)?;
        return scan_to_file(&mut scanner, &params, opts);
    }

    // Read once to clear a press latched before we started.
    scanner.button()?;
    loop {
        // Parameters must go in before the wait. After a press the scanner expects FS G
        // directly; resetting and configuring at that point makes FS G report a fatal error.
        scanner.configure(&params)?;
        eprintln!("ready: press the scan button");
        loop {
            let button = scanner.button()?;
            if button != 0 {
                eprintln!("button pressed (value {button})");
                break;
            }
            thread::sleep(BUTTON_POLL);
        }
        scan_to_file(&mut scanner, &params, opts)?;
        // Ignore presses made while the scan was running.
        scanner.button()?;
    }
}

fn scan_params(id: &Identity, opts: &Opts) -> Result<ScanParams> {
    let (bed_w, bed_h) = id.flatbed_at(opts.dpi);
    let (width, height) = match opts.size {
        Some((w, h)) => ((w & !7).min(bed_w), h.min(bed_h)),
        None => (bed_w, bed_h),
    };
    if width == 0 || height == 0 {
        return Err("scan area is empty".into());
    }
    Ok(ScanParams { dpi: opts.dpi, left: 0, top: 0, width, height })
}

fn connect() -> Result<Usb> {
    let mut announced = false;
    loop {
        if let Some(usb) = Usb::open()? {
            return Ok(usb);
        }
        if !announced {
            eprintln!("waiting for scanner {:04x}:{:04x}", usb::VENDOR_ID, usb::PRODUCT_ID);
            announced = true;
        }
        thread::sleep(RECONNECT_DELAY);
    }
}

/// Runs the scan already set up with `Scanner::configure` and writes it out as PNG.
fn scan_to_file(scanner: &mut Scanner, params: &ScanParams, opts: &Opts) -> Result<()> {
    let ScanParams { width, height, .. } = *params;
    let path = next_free_path(&opts.out)?;
    let tmp = path.with_extension("png.part");
    eprintln!("scanning {width}x{height} at {} dpi -> {}", opts.dpi, path.display());

    scanner.wait_ready()?;

    let mut png = png::Encoder::new(BufWriter::new(File::create(&tmp)?), width, height);
    png.set_color(png::ColorType::Rgb);
    png.set_depth(png::BitDepth::Eight);
    png.set_compression(png::Compression::Fast);
    let ppm = (opts.dpi as f64 / 0.0254).round() as u32;
    png.set_pixel_dims(Some(png::PixelDimensions { xppu: ppm, yppu: ppm, unit: png::Unit::Meter }));
    let mut writer = png.write_header()?.into_stream_writer()?;

    let expected = params.bytes_per_line() * height as usize;
    let mut received = 0usize;
    let result = scanner.scan(|data| {
        received += data.len();
        if received > expected {
            return Err(format!("scanner sent more than the expected {expected} bytes").into());
        }
        writer.write_all(data)?;
        Ok(())
    });
    if let Err(e) = result.and_then(|()| {
        if received != expected {
            return Err(format!("scan ended early: {received} of {expected} bytes").into());
        }
        writer.finish()?;
        Ok(())
    }) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    std::fs::rename(&tmp, &path)?;
    eprintln!("saved {}", path.display());
    Ok(())
}

/// `scan-0001.png`, `scan-0002.png`, ... picking the first number after the highest one present.
fn next_free_path(dir: &Path) -> Result<PathBuf> {
    let mut highest = 0u32;
    for entry in std::fs::read_dir(dir)? {
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
