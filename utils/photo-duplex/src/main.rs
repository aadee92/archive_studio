//! Scans both sides of several photos at once on the Epson GT-X980 / Perfection V850.
//!
//! A single press of the scanner button scans the photos on the glass as new, one-sided
//! fronts. A double press scans backs instead: the photos of the last batch that were
//! flipped over in place are paired with the front each one overlaps, and photos that
//! were removed or left face down get no back. Photos are found against the scanner's
//! white document mat, which must be in the lid.

use std::path::PathBuf;
use std::process::ExitCode;
use std::thread;
use std::time::{Duration, Instant};

use photo_duplex::detect::{DETECT_DPI, Rgb};
use photo_duplex::session::{self, Duplex, Event};
use x980::{Result, ScanParams, Scanner};

const RECONNECT_DELAY: Duration = Duration::from_secs(2);
/// How often the button is polled.
const BUTTON_POLL: Duration = Duration::from_millis(40);
/// A second press within this long of the first makes a double press. The scanner ignores
/// a press that comes less than about 0.9 s after the last one, so a quick double tap
/// reads as one press: the two presses need to be about a second apart.
const DOUBLE_PRESS_WINDOW: Duration = Duration::from_secs(2);

const USAGE: &str = "\
usage: photo-duplex [--out DIR] [--dpi N] [--save-bed]

  --out DIR      directory for photos (default: current directory)
  --dpi N        scan resolution (default: 600)
  --save-bed     also keep each whole-bed scan as bed-NNNN.png (for troubleshooting)

The white document mat must be in the lid.

Place photos face down on the glass and press the scanner button once: each is saved
as photo-NNNN-front.png. To add backs, flip the double-sided ones over in place (others
can stay face down or be removed) and press the button twice, about a second apart (a
quick double tap only counts as one press): each back is
saved as photo-NNNN-back.png next to its front. Backs always go with the most recent
single-press batch.
Leave about 1 cm between photos and keep them ~3 mm in from the edges of the glass.";

struct Opts {
    out: PathBuf,
    dpi: u32,
    save_bed: bool,
}

fn parse_args() -> std::result::Result<Opts, String> {
    let mut opts = Opts { out: PathBuf::from("."), dpi: 600, save_bed: false };
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let mut value = || args.next().ok_or(format!("{arg} needs a value"));
        match arg.as_str() {
            "--out" => opts.out = value()?.into(),
            "--dpi" => opts.dpi = value()?.parse().map_err(|_| "--dpi must be a number")?,
            "--save-bed" => opts.save_bed = true,
            "-h" | "--help" => return Err(USAGE.into()),
            _ => return Err(format!("unknown argument {arg}\n\n{USAGE}")),
        }
    }
    if opts.dpi < DETECT_DPI {
        return Err(format!("--dpi must be at least {DETECT_DPI}"));
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

    // Kept across reconnects, so a double press still finds the last batch of fronts.
    let mut duplex = Duplex::new(opts.out.clone());
    loop {
        if let Err(e) = run(&opts, &mut duplex) {
            eprintln!("error: {e}; reconnecting");
            thread::sleep(RECONNECT_DELAY);
        }
    }
}

fn run(opts: &Opts, duplex: &mut Duplex) -> Result<()> {
    let mut scanner = x980::connect()?;
    scanner.reset()?;
    let id = scanner.identity()?;
    eprintln!("connected: {} (firmware {})", id.model, id.firmware);
    if opts.dpi > id.max_dpi {
        return Err(format!("--dpi must be at most {}", id.max_dpi).into());
    }

    let grid = id.flatbed_at(DETECT_DPI);
    // Every scan is of the whole bed, so the parameters can go in before each wait.
    let (width, height) = id.flatbed_at(opts.dpi);
    let params = ScanParams { dpi: opts.dpi, left: 0, top: 0, width, height };

    loop {
        scanner.configure(&params)?;
        match duplex.pending_numbers() {
            None => eprintln!("\n>> place photos FACE DOWN, then press the button"),
            Some(numbers) => eprintln!(
                "\n>> press once to scan new photos (place them FACE DOWN), or flip any of {numbers} over \
                 in place and press twice, about a second apart, to scan their backs"
            ),
        }
        let backs = wait_for_press(&mut scanner)?;
        let bed = scan_bed(&mut scanner, &params, opts)?;
        duplex.process(&bed, opts.dpi, grid, backs, &mut |event| {
            if let Event::Log(line) = event {
                eprintln!("{line}");
            }
        })?;
    }
}

/// Blocks until the button is pressed, then briefly waits for a second press. Returns
/// whether it was a double press.
///
/// `Scanner::configure` must already have been called (see `x980::wait_for_button`).
fn wait_for_press(scanner: &mut Scanner) -> Result<bool> {
    while scanner.button()? == 0 {
        thread::sleep(BUTTON_POLL);
    }
    let first = Instant::now();
    while first.elapsed() < DOUBLE_PRESS_WINDOW {
        thread::sleep(BUTTON_POLL);
        if scanner.button()? != 0 {
            eprintln!("double press ({:.1} s apart): scanning backs", first.elapsed().as_secs_f64());
            return Ok(true);
        }
    }
    eprintln!("single press: scanning fronts");
    Ok(false)
}

/// Scans the whole bed with the configured `params`, once the button has been pressed.
fn scan_bed(scanner: &mut Scanner, params: &ScanParams, opts: &Opts) -> Result<Rgb> {
    eprintln!("scanning at {} dpi...", params.dpi);
    let data = x980::scan_to_vec(scanner, params)?;
    // Ignore presses made while the scan was running.
    scanner.button()?;
    let bed = Rgb { width: params.width, height: params.height, data };
    if opts.save_bed {
        let path = session::next_free(&opts.out, "bed-", ".png")?;
        session::save_png(&path, &bed, params.dpi)?;
        eprintln!("saved whole bed as {}", path.display());
    }
    Ok(bed)
}
