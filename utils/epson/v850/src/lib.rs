//! Userspace driver for the Epson GT-X980 / Perfection V850 (USB `04b8:0151`).

pub mod escil;
pub mod usb;

use std::thread;
use std::time::Duration;

pub use escil::{Identity, ScanParams, Scanner};
pub use usb::{Error, Result};

const BUTTON_POLL: Duration = Duration::from_millis(250);
const RECONNECT_DELAY: Duration = Duration::from_secs(2);

/// Blocks until a scanner is plugged in, then opens it.
pub fn connect() -> Result<Scanner> {
    let mut announced = false;
    loop {
        if let Some(usb) = usb::Usb::open()? {
            return Ok(Scanner::new(usb));
        }
        if !announced {
            eprintln!("waiting for scanner {:04x}:{:04x}", usb::VENDOR_ID, usb::PRODUCT_ID);
            announced = true;
        }
        thread::sleep(RECONNECT_DELAY);
    }
}

/// Blocks until the front button is pressed and returns its value.
///
/// Call `Scanner::configure` first: after a press the scanner expects FS G directly, and
/// resetting or configuring at that point makes FS G report a fatal error.
pub fn wait_for_button(scanner: &mut Scanner) -> Result<u8> {
    loop {
        let button = scanner.button()?;
        if button != 0 {
            return Ok(button);
        }
        thread::sleep(BUTTON_POLL);
    }
}

/// Runs the configured scan to completion and returns the pixel-interleaved RGB data.
pub fn scan_to_vec(scanner: &mut Scanner, params: &ScanParams) -> Result<Vec<u8>> {
    let expected = params.bytes_per_line() * params.height as usize;
    let mut data = Vec::with_capacity(expected);
    scanner.wait_ready()?;
    scanner.scan(|block| {
        data.extend_from_slice(block);
        Ok(())
    })?;
    if data.len() != expected {
        return Err(format!("scan returned {} of {expected} bytes", data.len()).into());
    }
    Ok(data)
}
