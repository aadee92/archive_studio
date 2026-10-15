//! The basic EXIF fields shown in an asset's properties: camera, dates, exposure.

/// What [`parse`] found; every field is optional.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Exif {
    pub make: Option<String>,
    pub model: Option<String>,
    pub software: Option<String>,
    /// When the picture was taken (`DateTimeOriginal`), as written: `YYYY:MM:DD HH:MM:SS`.
    pub taken: Option<String>,
    /// When it was digitized (`DateTimeDigitized`), e.g. scanned.
    pub digitized: Option<String>,
    /// When the file was last changed by software (`DateTime`).
    pub modified: Option<String>,
    /// 1..=8; 1 is upright (the pipeline bakes rotations in and resets it to 1).
    pub orientation: Option<u16>,
    /// Seconds.
    pub exposure_time: Option<f64>,
    pub f_number: Option<f64>,
    pub iso: Option<u32>,
    /// Millimetres.
    pub focal_length: Option<f64>,
}

const MAKE: u16 = 0x010F;
const MODEL: u16 = 0x0110;
const ORIENTATION: u16 = 0x0112;
const SOFTWARE: u16 = 0x0131;
const DATE_TIME: u16 = 0x0132;
const EXIF_IFD: u16 = 0x8769;
const EXPOSURE_TIME: u16 = 0x829A;
const F_NUMBER: u16 = 0x829D;
const ISO: u16 = 0x8827;
const DATE_TIME_ORIGINAL: u16 = 0x9003;
const DATE_TIME_DIGITIZED: u16 = 0x9004;
const FOCAL_LENGTH: u16 = 0x920A;

struct Tiff<'a> {
    data: &'a [u8],
    big_endian: bool,
}

impl Tiff<'_> {
    fn u16(&self, at: usize) -> Option<u16> {
        let b: [u8; 2] = self.data.get(at..at + 2)?.try_into().ok()?;
        Some(if self.big_endian { u16::from_be_bytes(b) } else { u16::from_le_bytes(b) })
    }

    fn u32(&self, at: usize) -> Option<u32> {
        let b: [u8; 4] = self.data.get(at..at + 4)?.try_into().ok()?;
        Some(if self.big_endian { u32::from_be_bytes(b) } else { u32::from_le_bytes(b) })
    }

    /// The IFD at `ifd`'s entries: (tag, type, count, where the value is).
    fn entries(&self, ifd: usize) -> Vec<(u16, u16, u32, usize)> {
        let Some(count) = self.u16(ifd) else { return Vec::new() };
        (0..usize::from(count).min(512))
            .filter_map(|i| {
                let entry = ifd + 2 + 12 * i;
                let (tag, kind, n) = (self.u16(entry)?, self.u16(entry + 2)?, self.u32(entry + 4)?);
                let size = match kind {
                    1 | 2 | 7 => 1,
                    3 => 2,
                    4 | 9 => 4,
                    5 | 10 => 8,
                    _ => return None,
                } * n as usize;
                let at = if size <= 4 { entry + 8 } else { self.u32(entry + 8)? as usize };
                Some((tag, kind, n, at))
            })
            .collect()
    }

    fn ascii(&self, at: usize, n: u32) -> Option<String> {
        let bytes = self.data.get(at..at + n as usize)?;
        let text = String::from_utf8_lossy(bytes.split(|&b| b == 0).next().unwrap_or_default()).trim().to_owned();
        (!text.is_empty()).then_some(text)
    }

    fn number(&self, kind: u16, at: usize) -> Option<f64> {
        match kind {
            3 => self.u16(at).map(f64::from),
            4 => self.u32(at).map(f64::from),
            5 => {
                let (n, d) = (self.u32(at)?, self.u32(at + 4)?);
                (d != 0).then(|| f64::from(n) / f64::from(d))
            }
            10 => {
                let (n, d) = (self.u32(at)? as i32, self.u32(at + 4)? as i32);
                (d != 0).then(|| f64::from(n) / f64::from(d))
            }
            _ => None,
        }
    }
}

/// The fields of EXIF data (TIFF structure, with or without JPEG's `Exif\0\0` header).
/// Anything malformed is skipped.
pub fn parse(exif: &[u8]) -> Exif {
    let data = exif.strip_prefix(b"Exif\0\0").unwrap_or(exif);
    let big_endian = match data.get(..4) {
        Some(b"MM\0*") => true,
        Some(b"II*\0") => false,
        _ => return Exif::default(),
    };
    let tiff = Tiff { data, big_endian };
    let mut out = Exif::default();
    let Some(first) = tiff.u32(4) else { return out };
    let mut ifds = vec![first as usize];
    let mut seen = Vec::new();
    while let Some(ifd) = ifds.pop() {
        if seen.contains(&ifd) || seen.len() > 4 {
            continue;
        }
        seen.push(ifd);
        for (tag, kind, n, at) in tiff.entries(ifd) {
            match tag {
                MAKE => out.make = tiff.ascii(at, n),
                MODEL => out.model = tiff.ascii(at, n),
                SOFTWARE => out.software = tiff.ascii(at, n),
                DATE_TIME => out.modified = tiff.ascii(at, n),
                DATE_TIME_ORIGINAL => out.taken = tiff.ascii(at, n),
                DATE_TIME_DIGITIZED => out.digitized = tiff.ascii(at, n),
                ORIENTATION => out.orientation = tiff.number(kind, at).map(|v| v as u16),
                EXPOSURE_TIME => out.exposure_time = tiff.number(kind, at),
                F_NUMBER => out.f_number = tiff.number(kind, at),
                ISO => out.iso = tiff.number(kind, at).map(|v| v as u32),
                FOCAL_LENGTH => out.focal_length = tiff.number(kind, at),
                EXIF_IFD => ifds.extend(tiff.u32(at).map(|o| o as usize)),
                _ => {}
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Little-endian TIFF: IFD0 with Make, Orientation and a pointer to an EXIF IFD holding
    /// DateTimeOriginal and FNumber.
    fn sample() -> Vec<u8> {
        let mut d = b"Exif\0\0II*\0".to_vec();
        let base = 6;
        let push16 = |d: &mut Vec<u8>, v: u16| d.extend(v.to_le_bytes());
        let push32 = |d: &mut Vec<u8>, v: u32| d.extend(v.to_le_bytes());
        push32(&mut d, 8);
        // IFD0 at 8: 3 entries, then next-IFD 0. Data after it at 8 + 2 + 36 + 4 = 50.
        push16(&mut d, 3);
        for (tag, kind, n, value) in [(MAKE, 2u16, 6u32, 50u32), (ORIENTATION, 3, 1, 6), (EXIF_IFD, 4, 1, 56)] {
            push16(&mut d, tag);
            push16(&mut d, kind);
            push32(&mut d, n);
            push32(&mut d, value);
        }
        push32(&mut d, 0);
        d.extend(b"Epson\0");
        // EXIF IFD at 56: 2 entries; data at 56 + 2 + 24 + 4 = 86.
        assert_eq!(d.len() - base, 56);
        push16(&mut d, 2);
        for (tag, kind, n, value) in [(DATE_TIME_ORIGINAL, 2u16, 20u32, 86u32), (F_NUMBER, 5, 1, 106)] {
            push16(&mut d, tag);
            push16(&mut d, kind);
            push32(&mut d, n);
            push32(&mut d, value);
        }
        push32(&mut d, 0);
        d.extend(b"1987:06:05 04:03:02\0");
        push32(&mut d, 28);
        push32(&mut d, 10);
        d
    }

    #[test]
    fn reads_the_basic_fields() {
        let exif = parse(&sample());
        assert_eq!(exif.make.as_deref(), Some("Epson"));
        assert_eq!(exif.orientation, Some(6));
        assert_eq!(exif.taken.as_deref(), Some("1987:06:05 04:03:02"));
        assert_eq!(exif.f_number, Some(2.8));
        assert_eq!(exif.model, None);
        assert_eq!(parse(b"junk"), Exif::default());
        // Truncated anywhere: no panic.
        let data = sample();
        for end in 0..data.len() {
            parse(&data[..end]);
        }
    }
}
