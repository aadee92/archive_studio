//! ESC/I command set as spoken by the Epson GT-X980 (Perfection V850).
//!
//! Byte sequences were taken from a captured SANE `epson2` session against this device;
//! see the README for the transcript.

use std::thread;
use std::time::Duration;

use crate::usb::{Result, Usb};

const ESC: u8 = 0x1b;
const FS: u8 = 0x1c;
const STX: u8 = 0x02;
const ACK: u8 = 0x06;
const NAK: u8 = 0x15;
const CAN: u8 = 0x18;

const CMD_TIMEOUT: Duration = Duration::from_secs(10);
/// Covers carriage travel plus lamp warm-up between FS G and the first data block.
const DATA_TIMEOUT: Duration = Duration::from_secs(180);

// FS F, byte 0
const FSF_FATAL: u8 = 0x80;
const FSF_NOT_READY: u8 = 0x40;
const FSF_WARMING_UP: u8 = 0x02;
// FS G header byte 1 and per-block trailing status byte
const FSG_FATAL: u8 = 0x80;
const FSG_NOT_READY: u8 = 0x40;
const FSG_CANCEL_REQ: u8 = 0x10;

/// Largest block SANE asks for over USB; the scanner sends `block_lines` whole lines per block.
const MAX_BLOCK_BYTES: usize = 128 * 1024;

#[derive(Debug, Clone)]
pub struct Identity {
    pub model: String,
    pub firmware: String,
    /// Resolution the area fields are expressed in.
    pub base_dpi: u32,
    pub max_dpi: u32,
    pub flatbed_width: u32,
    pub flatbed_height: u32,
}

impl Identity {
    /// Full flatbed in pixels at `dpi`, with the width rounded down to a multiple of 8 as
    /// SANE does.
    pub fn flatbed_at(&self, dpi: u32) -> (u32, u32) {
        let w = (self.flatbed_width as u64 * dpi as u64 / self.base_dpi as u64) as u32;
        let h = (self.flatbed_height as u64 * dpi as u64 / self.base_dpi as u64) as u32;
        (w & !7, h)
    }
}

/// 8-bit RGB scan of a rectangle, all values in pixels at `dpi`.
#[derive(Debug, Clone, Copy)]
pub struct ScanParams {
    pub dpi: u32,
    pub left: u32,
    pub top: u32,
    pub width: u32,
    pub height: u32,
}

impl ScanParams {
    pub fn bytes_per_line(&self) -> usize {
        self.width as usize * 3
    }
}

pub struct Scanner {
    usb: Usb,
}

impl Scanner {
    pub fn new(usb: Usb) -> Self {
        Self { usb }
    }

    /// ESC @: reset scanning parameters to their power-on defaults.
    pub fn reset(&mut self) -> Result<()> {
        self.cmd_simple(&[ESC, b'@'])
    }

    /// FS I: extended identity (80 bytes).
    pub fn identity(&mut self) -> Result<Identity> {
        self.usb.write(&[FS, b'I'], CMD_TIMEOUT)?;
        let b = self.usb.read_exact(80, CMD_TIMEOUT)?;
        let text = |r: std::ops::Range<usize>| String::from_utf8_lossy(&b[r]).trim().to_string();
        Ok(Identity {
            model: text(46..62),
            firmware: text(62..66),
            base_dpi: le32(&b[4..]),
            max_dpi: le32(&b[12..]),
            flatbed_width: le32(&b[20..]),
            flatbed_height: le32(&b[24..]),
        })
    }

    /// FS F: 16-byte scanner status; byte 0 is the main status.
    pub fn status(&mut self) -> Result<[u8; 16]> {
        self.usb.write(&[FS, b'F'], CMD_TIMEOUT)?;
        let b = self.usb.read_exact(16, CMD_TIMEOUT)?;
        Ok(b.try_into().unwrap())
    }

    /// ESC !: push-button status. The scanner latches a press until it is read, so
    /// polling never misses one. Returns the raw value (0 = not pressed; the one front
    /// button observed so far reports 1).
    pub fn button(&mut self) -> Result<u8> {
        let payload = self.cmd_info_block(&[ESC, b'!'])?;
        Ok(payload.first().copied().unwrap_or(0))
    }

    /// Configures a scan the way SANE's epson2 backend does for Color/8-bit/flatbed.
    pub fn configure(&mut self, p: &ScanParams) -> Result<()> {
        self.reset()?;

        // FS W: 64-byte extended scanning parameter block.
        let block_lines = (MAX_BLOCK_BYTES / p.bytes_per_line()).clamp(21, 255) as u8;
        let mut b = [0u8; 64];
        b[0..4].copy_from_slice(&p.dpi.to_le_bytes()); // main-scan resolution
        b[4..8].copy_from_slice(&p.dpi.to_le_bytes()); // sub-scan resolution
        b[8..12].copy_from_slice(&p.left.to_le_bytes());
        b[12..16].copy_from_slice(&p.top.to_le_bytes());
        b[16..20].copy_from_slice(&p.width.to_le_bytes());
        b[20..24].copy_from_slice(&p.height.to_le_bytes());
        b[24] = 0x13; // colour, pixel-interleaved RGB
        b[25] = 8; // bits per sample
        b[26] = 0; // option unit: none (flatbed)
        b[27] = 0; // normal (not preview) speed
        b[28] = block_lines;
        b[29] = 2; // gamma: default for multi-bit
        b[30] = 0; // brightness
        b[31] = 1; // colour correction: built-in
        b[33] = 0x80; // threshold
        self.cmd_simple(&[FS, b'W'])?;
        self.cmd_simple(&b)?;

        // ESC p: focus position (64 = glass surface).
        self.cmd_simple(&[ESC, b'p'])?;
        self.cmd_simple(&[0x40])?;

        // ESC m: colour-correction matrix, identity in 1/32 units.
        self.cmd_simple(&[ESC, b'm'])?;
        self.cmd_simple(&[0x20, 0, 0, 0, 0x20, 0, 0, 0, 0x20])?;
        Ok(())
    }

    /// Polls FS F until the lamp has warmed up.
    pub fn wait_ready(&mut self) -> Result<()> {
        for _ in 0..120 {
            let s = self.status()?[0];
            if s & FSF_FATAL != 0 {
                return Err(format!("scanner reports a fatal error (status {s:#04x})").into());
            }
            if s & (FSF_WARMING_UP | FSF_NOT_READY) == 0 {
                return Ok(());
            }
            thread::sleep(Duration::from_secs(1));
        }
        Err("scanner did not become ready within 2 minutes".into())
    }

    /// FS G: starts the configured scan and feeds the image data to `sink` in order,
    /// block by block. On any failure mid-scan the scan is cancelled on the device.
    pub fn scan(&mut self, mut sink: impl FnMut(&[u8]) -> Result<()>) -> Result<()> {
        self.usb.write(&[FS, b'G'], CMD_TIMEOUT)?;
        let h = self.usb.read_exact(14, DATA_TIMEOUT)?;
        if h[0] != STX {
            return Err(format!("FS G: expected STX, got {:#04x}", h[0]).into());
        }
        if h[1] & FSG_FATAL != 0 {
            return Err("FS G: scanner reports a fatal error".into());
        }
        if h[1] & FSG_NOT_READY != 0 {
            return Err("FS G: scanner not ready (in use elsewhere?)".into());
        }
        let block_len = le32(&h[2..]) as usize;
        let full_blocks = le32(&h[6..]) as usize;
        let last_len = le32(&h[10..]) as usize;
        let mut lens = vec![block_len; full_blocks];
        if last_len > 0 {
            lens.push(last_len);
        }

        let result = self.read_blocks(&lens, &mut sink);
        if result.is_err() {
            self.cancel();
        }
        result
    }

    fn read_blocks(&mut self, lens: &[usize], sink: &mut impl FnMut(&[u8]) -> Result<()>) -> Result<()> {
        for (i, &len) in lens.iter().enumerate() {
            // Each block is followed by one status byte.
            let mut block = self.usb.read_exact(len + 1, DATA_TIMEOUT)?;
            let status = block.pop().unwrap();
            if status & FSG_CANCEL_REQ != 0 {
                return Err("scan cancelled from the scanner".into());
            }
            if status & (FSG_FATAL | FSG_NOT_READY) != 0 {
                return Err(format!("scanner error during block {i} (status {status:#04x})").into());
            }
            sink(&block)?;
            // ACK requests the next block; the last one is not acknowledged.
            if i + 1 < lens.len() {
                self.usb.write(&[ACK], CMD_TIMEOUT)?;
            }
        }
        Ok(())
    }

    /// CAN: aborts a running scan. Best effort; used only on the error path.
    fn cancel(&mut self) {
        self.usb.discard_pending();
        let _ = self.cmd_simple(&[CAN]);
        self.usb.discard_pending();
    }

    /// Sends a command that is answered with a single ACK or NAK.
    fn cmd_simple(&mut self, cmd: &[u8]) -> Result<()> {
        self.usb.write(cmd, CMD_TIMEOUT)?;
        match self.usb.read_exact(1, CMD_TIMEOUT)?[0] {
            ACK => Ok(()),
            NAK => Err(format!("scanner rejected command {:02x?}", &cmd[..cmd.len().min(2)]).into()),
            other => Err(format!("expected ACK, got {other:#04x}").into()),
        }
    }

    /// Sends a command answered by a 4-byte info block (STX, status, u16 length) + payload.
    fn cmd_info_block(&mut self, cmd: &[u8]) -> Result<Vec<u8>> {
        self.usb.write(cmd, CMD_TIMEOUT)?;
        let h = self.usb.read_exact(4, CMD_TIMEOUT)?;
        match h[0] {
            STX => {}
            NAK => return Err(format!("scanner does not support command {cmd:02x?}").into()),
            other => return Err(format!("expected STX, got {other:#04x}").into()),
        }
        let len = u16::from_le_bytes([h[2], h[3]]) as usize;
        self.usb.read_exact(len, CMD_TIMEOUT)
    }
}

fn le32(b: &[u8]) -> u32 {
    u32::from_le_bytes(b[..4].try_into().unwrap())
}
