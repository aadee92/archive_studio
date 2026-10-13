# x980

Standalone userspace driver for the Epson GT-X980 / Perfection V850 (USB `04b8:0151`).
It waits for the scanner's front button and saves a full-flatbed colour scan as PNG on
every press. No SANE, no libusb: it talks to the device through usbfs via `nusb`.

```
cargo build --release
./target/release/x980 --out ~/scans            # wait for button presses
./target/release/x980 --once --dpi 600         # scan now and exit
X980_TRACE=1 ./target/release/x980 ...         # log every USB transfer
```

Options: `--out DIR`, `--dpi N` (default 300), `--size WxH` (pixels from the top-left),
`--once`. Output files are `scan-0001.png`, `scan-0002.png`, ... with the DPI stored in
the PNG. While running, the driver holds the scanner exclusively (SANE cannot use it).

## Protocol

ESC/I (command level B8) over bulk EP 0x01 OUT / EP 0x82 IN, as used by SANE's `epson2`
backend. Commands are answered with ACK (`06`), NAK (`15`), or an info block
`02 <status> <u16 LE length>` followed by the payload.

| Command | Bytes | Reply |
|---|---|---|
| Reset | `1b 40` | ACK |
| Extended identity | `1c 49` | 80 bytes: base dpi @4, max dpi @12, flatbed size @20/@24 (base-dpi pixels), model @46 |
| Scanner status | `1c 46` | 16 bytes; byte 0 bit `02` = warming up, `40` = not ready, `80` = fatal |
| Push button | `1b 21` | info block + 1 byte: `00` idle, `01` pressed. Latched until read. |
| Scan parameters | `1c 57`, then 64-byte block | ACK, ACK |
| Focus | `1b 70`, then `40` | ACK, ACK |
| Colour matrix | `1b 6d`, then `20 00 00 00 20 00 00 00 20` | ACK, ACK |
| Start scan | `1c 47` | 14 bytes: `02 <status> <block len u32> <full blocks u32> <last block len u32>` |

Image data arrives in blocks of `block len` bytes (whole lines, pixel-interleaved RGB),
each followed by one status byte. The host sends ACK (`06`) to request every block after
the first; the last block is not acknowledged. `18` (CAN) aborts a scan.

The parameter block (all u32 little-endian): resolution x/y @0/@4, left/top @8/@12,
width/height in pixels @16/@20; then bytes: colour mode @24 (`13` = RGB),
bits @25, option unit @26, speed @27, lines per block @28, gamma @29, brightness @30,
colour correction @31, threshold @33.

Button sequence (from a captured SANE session): configure (reset, parameters, focus,
colour matrix), poll `1b 21` until it returns non-zero, then status `1c 46` and start
`1c 47` immediately. Resetting or reconfiguring between the press and `1c 47` makes the
scanner answer `1c 47` with the fatal-error bit set.
