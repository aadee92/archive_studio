//! The few EXIF and XMP fields a rotation changes, edited in place: everything else in the
//! metadata keeps its bytes and offsets.

const ORIENTATION: u16 = 0x0112;
const EXIF_IFD: u16 = 0x8769;
const PIXEL_X_DIMENSION: u16 = 0xA002;
const PIXEL_Y_DIMENSION: u16 = 0xA003;
const SHORT: u16 = 3;
const LONG: u16 = 4;

/// The TIFF structure in EXIF data, without the `Exif\0\0` header JPEG's APP1 (and some PNG
/// writers) put before it.
pub fn tiff(exif: &mut [u8]) -> &mut [u8] {
    if exif.starts_with(b"Exif\0\0") {
        &mut exif[6..]
    } else {
        exif
    }
}

struct Tiff<'a> {
    data: &'a [u8],
    big_endian: bool,
}

impl<'a> Tiff<'a> {
    fn new(data: &'a [u8]) -> Option<Self> {
        let big_endian = match data.get(..4)? {
            b"MM\0*" => true,
            b"II*\0" => false,
            _ => return None,
        };
        Some(Self { data, big_endian })
    }

    fn u16(&self, at: usize) -> Option<u16> {
        let b: [u8; 2] = self.data.get(at..at + 2)?.try_into().ok()?;
        Some(if self.big_endian { u16::from_be_bytes(b) } else { u16::from_le_bytes(b) })
    }

    fn u32(&self, at: usize) -> Option<u32> {
        let b: [u8; 4] = self.data.get(at..at + 4)?.try_into().ok()?;
        Some(if self.big_endian { u32::from_be_bytes(b) } else { u32::from_le_bytes(b) })
    }

    /// Where `tag`'s single SHORT or LONG value is stored in the IFD at `ifd`, with its type.
    fn find(&self, ifd: usize, tag: u16) -> Option<(usize, u16)> {
        let count = usize::from(self.u16(ifd)?);
        (0..count).map(|i| ifd + 2 + 12 * i).find_map(|entry| {
            let kind = self.u16(entry + 2)?;
            (self.u16(entry)? == tag && matches!(kind, SHORT | LONG) && self.u32(entry + 4)? == 1).then_some((entry + 8, kind))
        })
    }

    fn value(&self, (at, kind): (usize, u16)) -> Option<u32> {
        if kind == SHORT {
            self.u16(at).map(u32::from)
        } else {
            self.u32(at)
        }
    }

    fn ifd0(&self) -> Option<usize> {
        self.u32(4).map(|o| o as usize)
    }

    fn exif_ifd(&self) -> Option<usize> {
        let at = self.find(self.ifd0()?, EXIF_IFD)?;
        self.value(at).map(|o| o as usize)
    }
}

/// Writes `value` over the SHORT or LONG at `at` in `data`.
fn put(data: &mut [u8], big_endian: bool, (at, kind): (usize, u16), value: u32) {
    let bytes = match (kind, big_endian) {
        (SHORT, true) => (value as u16).to_be_bytes().to_vec(),
        (SHORT, false) => (value as u16).to_le_bytes().to_vec(),
        (_, true) => value.to_be_bytes().to_vec(),
        (_, false) => value.to_le_bytes().to_vec(),
    };
    data[at..at + bytes.len()].copy_from_slice(&bytes);
}

/// The image's EXIF orientation (1 to 8), if it has one.
pub fn orientation(tiff: &[u8]) -> Option<u16> {
    let t = Tiff::new(tiff)?;
    let value = t.value(t.find(t.ifd0()?, ORIENTATION)?)?;
    (1..=8).contains(&value).then_some(value as u16)
}

/// Marks the pixels as stored upright (orientation 1), and records the new size where
/// the EXIF says one.
pub fn set_upright(tiff: &mut [u8], width: u32, height: u32) {
    let Some(t) = Tiff::new(tiff) else { return };
    let big_endian = t.big_endian;
    let orientation = t.ifd0().and_then(|ifd| t.find(ifd, ORIENTATION));
    let exif = t.exif_ifd();
    let x = exif.and_then(|ifd| t.find(ifd, PIXEL_X_DIMENSION));
    let y = exif.and_then(|ifd| t.find(ifd, PIXEL_Y_DIMENSION));
    if let Some(at) = orientation {
        put(tiff, big_endian, at, 1);
    }
    // A SHORT can't hold every size; then the field is left for readers to distrust.
    for (at, value) in [(x, width), (y, height)] {
        if let Some(at) = at.filter(|&(_, kind)| kind == LONG || value <= u32::from(u16::MAX)) {
            put(tiff, big_endian, at, value);
        }
    }
}

/// Sets `tiff:Orientation` in an XMP packet to 1, as an attribute or an element (a digit
/// for a digit, so the packet keeps its length).
pub fn set_xmp_upright(xmp: &mut [u8]) {
    for pattern in [&b"tiff:Orientation=\""[..], b"tiff:Orientation='", b"<tiff:Orientation>"] {
        let mut from = 0;
        while let Some(found) = xmp[from..].windows(pattern.len()).position(|w| w == pattern) {
            let digit = from + found + pattern.len();
            if xmp.get(digit).is_some_and(|d| (b'1'..=b'8').contains(d)) && !xmp.get(digit + 1).is_some_and(u8::is_ascii_digit) {
                xmp[digit] = b'1';
            }
            from = digit;
        }
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;

    /// Little-endian TIFF with IFD0 = {orientation}, and an EXIF IFD with pixel dimensions.
    pub fn sample(orientation: u16, width: u32, height: u32) -> Vec<u8> {
        let mut t = b"II*\0".to_vec();
        t.extend_from_slice(&8u32.to_le_bytes());
        // IFD0 at 8: two entries.
        t.extend_from_slice(&2u16.to_le_bytes());
        t.extend_from_slice(&ORIENTATION.to_le_bytes());
        t.extend_from_slice(&SHORT.to_le_bytes());
        t.extend_from_slice(&1u32.to_le_bytes());
        t.extend_from_slice(&u32::from(orientation).to_le_bytes());
        t.extend_from_slice(&EXIF_IFD.to_le_bytes());
        t.extend_from_slice(&LONG.to_le_bytes());
        t.extend_from_slice(&1u32.to_le_bytes());
        t.extend_from_slice(&38u32.to_le_bytes());
        t.extend_from_slice(&0u32.to_le_bytes());
        // EXIF IFD at 38.
        t.extend_from_slice(&2u16.to_le_bytes());
        for (tag, kind, value) in [(PIXEL_X_DIMENSION, LONG, width), (PIXEL_Y_DIMENSION, SHORT, height)] {
            t.extend_from_slice(&tag.to_le_bytes());
            t.extend_from_slice(&kind.to_le_bytes());
            t.extend_from_slice(&1u32.to_le_bytes());
            t.extend_from_slice(&value.to_le_bytes());
        }
        t.extend_from_slice(&0u32.to_le_bytes());
        t
    }

    /// The pixel dimensions in `tiff`'s EXIF IFD.
    pub fn dimensions(tiff: &[u8]) -> (u32, u32) {
        let t = Tiff::new(tiff).unwrap();
        let exif = t.exif_ifd().unwrap();
        (t.value(t.find(exif, PIXEL_X_DIMENSION).unwrap()).unwrap(), t.value(t.find(exif, PIXEL_Y_DIMENSION).unwrap()).unwrap())
    }

    #[test]
    fn edits_in_place() {
        let mut t = sample(6, 40, 30);
        let length = t.len();
        assert_eq!(orientation(&t), Some(6));
        set_upright(&mut t, 30, 40);
        assert_eq!((orientation(&t), dimensions(&t), t.len()), (Some(1), (30, 40), length));
        assert_eq!(orientation(b"nonsense"), None);
    }

    #[test]
    fn xmp_orientation() {
        let mut xmp = b"<rdf:Description tiff:Orientation=\"6\"/><tiff:Orientation>8</tiff:Orientation>".to_vec();
        set_xmp_upright(&mut xmp);
        assert_eq!(xmp, b"<rdf:Description tiff:Orientation=\"1\"/><tiff:Orientation>1</tiff:Orientation>");
    }
}
