//! Raw bulk transport to the scanner.
//!
//! nusb requires IN transfers to be a multiple of the max packet size, while ESC/I
//! replies are arbitrary lengths. `read_exact` therefore requests the remaining length
//! rounded up to a packet multiple and keeps any surplus for the next read. The device
//! only ever sends what the current command asked for, so a transfer always ends either
//! on a short packet or exactly at the requested length.

use std::time::Duration;

use nusb::transfer::{Buffer, Bulk, In, Out};
use nusb::{Endpoint, MaybeFuture};

pub const VENDOR_ID: u16 = 0x04b8;
pub const PRODUCT_ID: u16 = 0x0151;
const INTERFACE: u8 = 0;
const EP_OUT: u8 = 0x01;
const EP_IN: u8 = 0x82;

pub type Error = Box<dyn std::error::Error + Send + Sync>;
pub type Result<T> = std::result::Result<T, Error>;

pub struct Usb {
    out: Endpoint<Bulk, Out>,
    inp: Endpoint<Bulk, In>,
    pending: Vec<u8>,
    /// Set `X980_TRACE=1` to log every transfer to stderr.
    trace: bool,
}

impl Usb {
    /// Opens the first attached GT-X980 / Perfection V850. Returns `Ok(None)` if none is plugged in.
    pub fn open() -> Result<Option<Self>> {
        let Some(info) = nusb::list_devices()
            .wait()?
            .find(|d| d.vendor_id() == VENDOR_ID && d.product_id() == PRODUCT_ID)
        else {
            return Ok(None);
        };
        let device = info.open().wait()?;
        let interface = device.detach_and_claim_interface(INTERFACE).wait()?;
        let mut usb = Self {
            out: interface.endpoint::<Bulk, Out>(EP_OUT)?,
            inp: interface.endpoint::<Bulk, In>(EP_IN)?,
            pending: Vec::new(),
            trace: std::env::var_os("X980_TRACE").is_some(),
        };
        usb.drain();
        Ok(Some(usb))
    }

    /// Throws away whatever the scanner still has queued. A process killed between a
    /// command and its reply leaves that reply behind, which would otherwise shift every
    /// later reply by its length.
    fn drain(&mut self) {
        let packet = self.inp.max_packet_size();
        loop {
            let c = self.inp.transfer_blocking(Buffer::new(packet), Duration::from_millis(200));
            if c.status.is_err() {
                break;
            }
            if self.trace {
                eprintln!("RX (stale) {}", hex(&c.buffer[..c.actual_len]));
            }
        }
    }

    pub fn write(&mut self, data: &[u8], timeout: Duration) -> Result<()> {
        if self.trace {
            eprintln!("TX {}", hex(data));
        }
        let c = self.out.transfer_blocking(data.to_vec().into(), timeout);
        c.status?;
        if c.actual_len != data.len() {
            return Err(format!("short write: {} of {} bytes", c.actual_len, data.len()).into());
        }
        Ok(())
    }

    pub fn read_exact(&mut self, len: usize, timeout: Duration) -> Result<Vec<u8>> {
        let packet = self.inp.max_packet_size();
        while self.pending.len() < len {
            let want = (len - self.pending.len()).div_ceil(packet) * packet;
            let c = self.inp.transfer_blocking(Buffer::new(want), timeout);
            c.status?;
            self.pending.extend_from_slice(&c.buffer[..c.actual_len]);
        }
        let rest = self.pending.split_off(len);
        let data = std::mem::replace(&mut self.pending, rest);
        if self.trace {
            eprintln!("RX {}", hex(&data));
        }
        Ok(data)
    }

    /// Drops buffered bytes, e.g. after a protocol error left a reply half-read.
    pub fn discard_pending(&mut self) {
        self.pending.clear();
    }
}

fn hex(data: &[u8]) -> String {
    let shown: Vec<String> = data.iter().take(32).map(|b| format!("{b:02x}")).collect();
    let more = if data.len() > 32 { format!(" ... ({} bytes)", data.len()) } else { String::new() };
    shown.join(" ") + &more
}
