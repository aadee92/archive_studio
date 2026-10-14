//! Source-file decoding: the slow path, only taken on a cache miss.
//!
//! Each format is decoded no larger than the requested edge needs:
//! - JPEG: IDCT-domain downscaling (`jpeg-decoder`, scale 1/2 – 1/8), so a 600 dpi scan is
//!   never reconstructed at full resolution; then a short resize to the exact edge.
//! - TIFF: the smallest reduced-resolution page (NewSubfileType = 1, or a SubIFD) that still
//!   covers the edge, if the file has one; see [`write_tiff_with_reduced_page`].
//! - Everything else: full decode via `image`, then a SIMD convolution resize (`fast_image_resize`).

use std::fs::File;
use std::io::{self, BufRead, BufReader, BufWriter, Cursor, Read, Seek, SeekFrom};
use std::panic::{self, AssertUnwindSafe};
use std::path::Path;

use fast_image_resize::{FilterType, ResizeAlg, ResizeOptions, Resizer};
use image::metadata::Orientation;
use image::{DynamicImage, ImageDecoder, ImageFormat, ImageReader, RgbImage};
use jpeg_decoder::PixelFormat;
use tiff::decoder::{Decoder as TiffDecoder, DecodingResult};
use tiff::tags::{IfdPointer, Tag};

use archive_core::media::MediaKind;

/// Longest edge of the reduced page written by [`write_tiff_with_reduced_page`]: room for
/// the 384 px tier on HiDPI growth, yet ~2 MB uncompressed.
const REDUCED_PAGE_EDGE: u32 = 1024;
/// Stop walking the page chain of huge multi-page documents.
const MAX_TIFF_PAGES: usize = 64;

/// Decodes `path` to an upright RGB thumbnail no larger than `edge` on its longest side.
///
/// TODO: Video: one frame ~10% in via ffmpeg / libav. Audio: embedded cover art.
pub fn decode(path: &Path, kind: MediaKind, edge: u32) -> Option<RgbImage> {
    if kind != MediaKind::Image {
        return None;
    }
    // Every decoder streams from this one reader; a fallback rewinds it. Large TIFFs are
    // never copied whole into memory (measured faster than both `fs::read` and mmap).
    decode_from(BufReader::with_capacity(64 << 10, File::open(path).ok()?), edge)
}

/// [`decode`] of a file already read into memory (by an I/O thread).
pub fn decode_bytes(bytes: &[u8], kind: MediaKind, edge: u32) -> Option<RgbImage> {
    if kind != MediaKind::Image {
        return None;
    }
    decode_from(Cursor::new(bytes), edge)
}

fn decode_from(mut file: impl BufRead + Seek, edge: u32) -> Option<RgbImage> {
    let format = image::guess_format(file.fill_buf().ok()?).ok();
    // Decoders are fed untrusted files; a panic must not take the worker thread down.
    panic::catch_unwind(AssertUnwindSafe(|| {
        let (image, orientation) = match format {
            Some(ImageFormat::Jpeg) => jpeg(&mut file, edge),
            Some(ImageFormat::Tiff) => tiff(&mut file, edge),
            _ => None,
        }
        .or_else(|| {
            file.rewind().ok()?;
            any_format(&mut file)
        })?;
        Some(upright(fit(&image, edge), orientation))
    }))
    .ok()
    .flatten()
}

/// Whether `head` (the first bytes of a file) starts a TIFF, which [`decode`] streams
/// instead of reading whole: a reduced page may be all it needs of a huge scan.
pub fn is_tiff(head: &[u8]) -> bool {
    head.starts_with(b"II*\0") || head.starts_with(b"MM\0*")
}

/// The EXIF thumbnail a camera embeds in its JPEGs (typically 160×120), upright. Too small
/// for any [`Tier`](super::Tier); meant as an instant placeholder while [`decode`] runs.
/// Reads only the file's metadata segments.
#[allow(dead_code)] // the pipeline uses `preview_from_bytes`; kept for capture backends
pub fn decode_preview(path: &Path) -> Option<RgbImage> {
    preview(&mut BufReader::new(File::open(path).ok()?))
}

/// [`decode_preview`] from the start of a JPEG file (the EXIF segment must be within `head`).
pub fn preview_from_bytes(head: &[u8]) -> Option<RgbImage> {
    preview(&mut Cursor::new(head))
}

fn preview(file: &mut (impl BufRead + Seek)) -> Option<RgbImage> {
    let exif = read_exif(file)?;
    panic::catch_unwind(|| {
        let exif = Exif::new(&exif)?;
        let (ifd0, next) = exif.ifd(exif.u32(4)? as usize)?;
        let (ifd1, _) = exif.ifd(next.filter(|&n| n != 0)?)?;
        let find = |ifd: &[(u16, u32)], tag| ifd.iter().find(|e| e.0 == tag).map(|e| e.1);
        let start = find(&ifd1, 0x0201)? as usize; // JPEGInterchangeFormat
        let len = find(&ifd1, 0x0202)? as usize; // JPEGInterchangeFormatLength
        let mut decoder = jpeg_decoder::Decoder::new(exif.data.get(start..start.checked_add(len)?)?);
        let pixels = decoder.decode().ok()?;
        let info = decoder.info()?;
        let image = RgbImage::from_raw(info.width.into(), info.height.into(), jpeg_rgb(pixels, info.pixel_format)?)?;
        let orientation = find(&ifd0, 0x0112).and_then(|o| Orientation::from_exif(o as u8));
        Some(upright(image, orientation.unwrap_or(Orientation::NoTransforms)))
    })
    .ok()
    .flatten()
}

/// Mean color of `image`, the tile background shown until its thumbnail arrives.
pub fn average_color(image: &RgbImage) -> [u8; 3] {
    let mut sum = [0u64; 3];
    for p in image.pixels() {
        for (s, &c) in sum.iter_mut().zip(&p.0) {
            *s += c as u64;
        }
    }
    let n = (image.width() as u64 * image.height() as u64).max(1);
    sum.map(|s| (s / n) as u8)
}

/// Writes `image` as an uncompressed RGB TIFF: page 0 at full resolution, then a
/// reduced-resolution page (NewSubfileType = 1, ≤ 1024 px) that [`decode`] reads instead.
/// Other readers still see the full scan as the first page.
#[allow(dead_code)] // called by real capture backends
pub fn write_tiff_with_reduced_page(path: &Path, image: &RgbImage) -> tiff::TiffResult<()> {
    use tiff::encoder::{colortype::RGB8, TiffEncoder};
    let mut encoder = TiffEncoder::new(BufWriter::new(File::create(path)?))?;
    encoder.write_image::<RGB8>(image.width(), image.height(), image.as_raw())?;
    let reduced = fit(image, REDUCED_PAGE_EDGE);
    let mut page = encoder.new_image::<RGB8>(reduced.width(), reduced.height())?;
    page.encoder().write_tag(Tag::NewSubfileType, 1u32)?;
    page.write_data(reduced.as_raw())
}

/// Scales down to fit `edge` on the longest side, keeping the aspect ratio.
pub fn fit(image: &RgbImage, edge: u32) -> RgbImage {
    let (w, h) = image.dimensions();
    if w.max(h) <= edge {
        return image.clone();
    }
    let scale = edge as f64 / w.max(h) as f64;
    let size = |n: u32| ((n as f64 * scale).round() as u32).max(1);
    let mut out = RgbImage::new(size(w), size(h));
    let options = ResizeOptions::new().resize_alg(ResizeAlg::Convolution(FilterType::Bilinear));
    match Resizer::new().resize(image, &mut out, &options) {
        Ok(()) => out,
        Err(_) => image::imageops::thumbnail(image, out.width(), out.height()),
    }
}

fn upright(image: RgbImage, orientation: Orientation) -> RgbImage {
    if orientation == Orientation::NoTransforms {
        return image;
    }
    let mut image = DynamicImage::ImageRgb8(image);
    image.apply_orientation(orientation);
    image.into_rgb8()
}

/// Full decode of any format `image` supports.
fn any_format(reader: impl BufRead + Seek) -> Option<(RgbImage, Orientation)> {
    let mut decoder = ImageReader::new(reader).with_guessed_format().ok()?.into_decoder().ok()?;
    let orientation = decoder.orientation().unwrap_or(Orientation::NoTransforms);
    Some((DynamicImage::from_decoder(decoder).ok()?.into_rgb8(), orientation))
}

/// Decodes at the smallest IDCT scale (1/8, 1/4, 1/2, 1) whose longest edge is still ≥ `edge`.
fn jpeg(reader: impl Read, edge: u32) -> Option<(RgbImage, Orientation)> {
    let mut decoder = jpeg_decoder::Decoder::new(reader);
    let edge = edge.min(u16::MAX.into()) as u16;
    // Satisfied when either axis reaches `edge`, i.e. the longest one does.
    decoder.scale(edge, edge).ok()?;
    let pixels = decoder.decode().ok()?;
    let info = decoder.info()?;
    let image = RgbImage::from_raw(info.width.into(), info.height.into(), jpeg_rgb(pixels, info.pixel_format)?)?;
    let orientation = decoder.exif_data().and_then(Orientation::from_exif_chunk);
    Some((image, orientation.unwrap_or(Orientation::NoTransforms)))
}

fn jpeg_rgb(pixels: Vec<u8>, format: PixelFormat) -> Option<Vec<u8>> {
    match format {
        PixelFormat::RGB24 => Some(pixels),
        PixelFormat::L8 => Some(pixels.iter().flat_map(|&g| [g; 3]).collect()),
        // `jpeg-decoder` has already undone Adobe's inverted storage: these are ink amounts.
        PixelFormat::CMYK32 => Some(pixels.chunks_exact(4).flat_map(|p| cmyk_to_rgb(p[0], p[1], p[2], p[3])).collect()),
        PixelFormat::L16 => None, // rare 12/16-bit lossless; `image` handles it
    }
}

fn cmyk_to_rgb(c: u8, m: u8, y: u8, k: u8) -> [u8; 3] {
    let white = 255 - k as u32;
    [c, m, y].map(|ink| ((255 - ink as u32) * white / 255) as u8)
}

/// Where a reduced-resolution TIFF image lives.
enum Page {
    /// Index in the main IFD chain.
    Chained(usize),
    /// A SubIFD of the first image.
    Sub(IfdPointer),
}

/// Decodes the smallest reduced-resolution page whose longest edge is still ≥ `edge`, else
/// the full image. `None` for layouts [`tiff_rgb`] doesn't convert, to fall back to `image`.
fn tiff<R: Read + Seek>(reader: &mut R, edge: u32) -> Option<(RgbImage, Orientation)> {
    let mut decoder = TiffDecoder::new(&mut *reader).ok()?;
    let (w, h) = decoder.dimensions().ok()?;
    let orientation = decoder.find_tag_unsigned::<u8>(Tag::Orientation).ok().flatten().and_then(Orientation::from_exif);

    let mut pages = Vec::new();
    for ifd in decoder.find_tag(Tag::SubIfd).ok().flatten().and_then(|v| v.into_ifd_vec().ok()).unwrap_or_default() {
        let Ok(dir) = decoder.read_directory(ifd) else { continue };
        let mut tags = decoder.read_directory_tags(&dir);
        let size = (tags.find_tag_unsigned(Tag::ImageWidth), tags.find_tag_unsigned(Tag::ImageLength));
        if let (Ok(Some(pw)), Ok(Some(ph))) = size {
            pages.push(((pw, ph), Page::Sub(ifd)));
        }
    }
    let mut index = 0;
    while decoder.more_images() && index < MAX_TIFF_PAGES {
        index += 1;
        if decoder.next_image().is_err() {
            break;
        }
        let reduced = decoder.find_tag_unsigned::<u32>(Tag::NewSubfileType).ok().flatten().unwrap_or(0) & 1 == 1;
        if let (true, Ok(size)) = (reduced, decoder.dimensions()) {
            pages.push((size, Page::Chained(index)));
        }
    }

    let same_aspect = |(pw, ph): (u32, u32)| (pw as f64 / ph as f64 - w as f64 / h as f64).abs() < 0.02 * w as f64 / h as f64;
    let page = pages
        .into_iter()
        .filter(|&(size, _)| size.0.max(size.1) >= edge && size.0.max(size.1) < w.max(h) && same_aspect(size))
        .min_by_key(|&((pw, ph), _)| pw.max(ph))
        .map_or(Page::Chained(0), |(_, page)| page);
    let image = match page {
        Page::Chained(index) => {
            decoder.seek_to_image(index).ok()?;
            tiff_rgb(&mut decoder)?
        }
        Page::Sub(ifd) => {
            drop(decoder);
            tiff_rgb(&mut TiffDecoder::new(Rerooted::new(reader, ifd)?).ok()?)?
        }
    };
    Some((image, orientation.unwrap_or(Orientation::NoTransforms)))
}

/// Converts the decoder's current image; `None` for layouts left to `image`.
fn tiff_rgb<R: Read + Seek>(decoder: &mut TiffDecoder<R>) -> Option<RgbImage> {
    use tiff::ColorType::{Gray, CMYK, RGB, RGBA};
    let (w, h) = decoder.dimensions().ok()?;
    let color = decoder.colortype().ok()?;
    if !matches!(color, RGB(8 | 16) | Gray(8 | 16) | RGBA(8 | 16) | CMYK(8)) {
        return None;
    }
    let high = |v: &[u16]| v.iter().map(|&s| (s >> 8) as u8).collect::<Vec<_>>();
    let rgb = match (color, decoder.read_image().ok()?) {
        (RGB(_), DecodingResult::U8(v)) => v,
        (RGB(_), DecodingResult::U16(v)) => high(&v),
        (Gray(_), DecodingResult::U8(v)) => v.iter().flat_map(|&g| [g; 3]).collect(),
        (Gray(_), DecodingResult::U16(v)) => high(&v).iter().flat_map(|&g| [g; 3]).collect(),
        (RGBA(_), DecodingResult::U8(v)) => v.chunks_exact(4).flat_map(|p| [p[0], p[1], p[2]]).collect(),
        (RGBA(_), DecodingResult::U16(v)) => high(&v).chunks_exact(4).flat_map(|p| [p[0], p[1], p[2]]).collect(),
        (CMYK(_), DecodingResult::U8(v)) => v.chunks_exact(4).flat_map(|p| cmyk_to_rgb(p[0], p[1], p[2], p[3])).collect(),
        _ => return None,
    };
    RgbImage::from_raw(w, h, rgb)
}

/// A TIFF stream with its header's first-IFD pointer redirected to `ifd`. The `tiff`
/// decoder only walks the main IFD chain, so this is how it opens a SubIFD as an image.
struct Rerooted<R> {
    header: [u8; 16],
    inner: R,
    pos: u64,
}

impl<R: Read + Seek> Rerooted<R> {
    fn new(mut inner: R, ifd: IfdPointer) -> Option<Self> {
        let mut header = [0; 16];
        inner.rewind().ok()?;
        inner.read_exact(&mut header).ok()?;
        let little = &header[..2] == b"II";
        let put = |dst: &mut [u8], src: &[u8]| {
            dst.copy_from_slice(src);
            if !little {
                dst.reverse();
            }
        };
        match header[2..4] {
            [42, 0] | [0, 42] => put(&mut header[4..8], &u32::try_from(ifd.0).ok()?.to_le_bytes()),
            [43, 0] | [0, 43] => put(&mut header[8..16], &ifd.0.to_le_bytes()),
            _ => return None,
        }
        inner.rewind().ok()?;
        Some(Self { header, inner, pos: 0 })
    }
}

impl<R: Read> Read for Rerooted<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        if let Some(patch) = self.header.get(self.pos as usize..) {
            let len = patch.len().min(n);
            buf[..len].copy_from_slice(&patch[..len]);
        }
        self.pos += n as u64;
        Ok(n)
    }
}

impl<R: Seek> Seek for Rerooted<R> {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        self.pos = self.inner.seek(pos)?;
        Ok(self.pos)
    }
}

/// The EXIF APP1 payload (from its TIFF header on), scanning only the JPEG's metadata segments.
fn read_exif(file: &mut (impl BufRead + Seek)) -> Option<Vec<u8>> {
    let mut soi = [0; 2];
    file.read_exact(&mut soi).ok()?;
    if soi != [0xff, 0xd8] {
        return None;
    }
    loop {
        let mut segment = [0; 4];
        file.read_exact(&mut segment).ok()?;
        let len = (u16::from_be_bytes([segment[2], segment[3]]) as usize).checked_sub(2)?;
        match segment[..2] {
            [0xff, 0xe1] => {
                let mut body = vec![0; len];
                file.read_exact(&mut body).ok()?;
                if let Some(exif) = body.strip_prefix(b"Exif\0\0") {
                    return Some(exif.to_vec());
                }
            }
            // Start of scan / image data: no metadata past here.
            [0xff, 0xda] | [0xff, 0xd9] => return None,
            [0xff, _] => file.seek_relative(len as i64).ok()?,
            _ => return None,
        }
    }
}

/// Minimal reader of the TIFF structure inside an EXIF payload.
struct Exif<'a> {
    data: &'a [u8],
    little: bool,
}

impl<'a> Exif<'a> {
    fn new(data: &'a [u8]) -> Option<Self> {
        let little = match data.get(..4)? {
            b"II*\0" => true,
            b"MM\0*" => false,
            _ => return None,
        };
        Some(Self { data, little })
    }

    fn u16(&self, at: usize) -> Option<u16> {
        let b = self.data.get(at..at.checked_add(2)?)?.try_into().ok()?;
        Some(if self.little { u16::from_le_bytes(b) } else { u16::from_be_bytes(b) })
    }

    fn u32(&self, at: usize) -> Option<u32> {
        let b = self.data.get(at..at.checked_add(4)?)?.try_into().ok()?;
        Some(if self.little { u32::from_le_bytes(b) } else { u32::from_be_bytes(b) })
    }

    /// The (tag, value) of each SHORT / LONG entry of the IFD at `at`, and the next IFD's offset.
    fn ifd(&self, at: usize) -> Option<(Vec<(u16, u32)>, Option<usize>)> {
        let count = self.u16(at)? as usize;
        let entries = (0..count)
            .filter_map(|i| {
                let entry = at + 2 + i * 12;
                let value = match self.u16(entry + 2)? {
                    3 => self.u16(entry + 8)? as u32,
                    4 => self.u32(entry + 8)?,
                    _ => return None,
                };
                Some((self.u16(entry)?, value))
            })
            .collect();
        Some((entries, self.u32(at + 2 + count * 12).map(|n| n as usize)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::codecs::jpeg::JpegEncoder;
    use image::Rgb;
    use std::path::PathBuf;
    use std::time::Instant;
    use tiff::encoder::{colortype::RGB8, TiffEncoder};

    const RED: Rgb<u8> = Rgb([220, 30, 30]);
    const BLUE: Rgb<u8> = Rgb([30, 30, 220]);
    const GREEN: Rgb<u8> = Rgb([30, 200, 30]);

    /// The pre-optimization path: full `image` decode, `thumbnail()`, EXIF orientation.
    fn reference(path: &Path, edge: u32) -> Option<RgbImage> {
        let mut decoder = ImageReader::open(path).ok()?.with_guessed_format().ok()?.into_decoder().ok()?;
        let orientation = decoder.orientation().unwrap_or(Orientation::NoTransforms);
        let mut image = DynamicImage::from_decoder(decoder).ok()?.thumbnail(edge, edge);
        image.apply_orientation(orientation);
        Some(image.into_rgb8())
    }

    fn near(a: Rgb<u8>, b: Rgb<u8>) -> bool {
        a.0.iter().zip(b.0).all(|(x, y)| x.abs_diff(y) <= 24)
    }

    /// Top half `top`, bottom half `bottom`.
    fn halves(w: u32, h: u32, top: Rgb<u8>, bottom: Rgb<u8>) -> RgbImage {
        RgbImage::from_fn(w, h, |_, y| if y < h / 2 { top } else { bottom })
    }

    fn jpeg_bytes(image: &RgbImage) -> Vec<u8> {
        let mut out = Vec::new();
        JpegEncoder::new_with_quality(&mut out, 90).encode_image(image).unwrap();
        out
    }

    /// A little-endian EXIF payload with an orientation and optionally an IFD1 JPEG thumbnail.
    fn exif(orientation: u16, thumbnail: Option<&[u8]>) -> Vec<u8> {
        let mut t = b"II*\0".to_vec();
        t.extend(8u32.to_le_bytes());
        t.extend(1u16.to_le_bytes()); // IFD0: one entry
        t.extend([0x12, 0x01, 3, 0, 1, 0, 0, 0]);
        t.extend((orientation as u32).to_le_bytes());
        t.extend((if thumbnail.is_some() { 26u32 } else { 0 }).to_le_bytes());
        if let Some(thumb) = thumbnail {
            t.extend(2u16.to_le_bytes()); // IFD1 at 26: offset + length, data at 56
            t.extend([0x01, 0x02, 4, 0, 1, 0, 0, 0]);
            t.extend(56u32.to_le_bytes());
            t.extend([0x02, 0x02, 4, 0, 1, 0, 0, 0]);
            t.extend((thumb.len() as u32).to_le_bytes());
            t.extend(0u32.to_le_bytes());
            t.extend(thumb);
        }
        t
    }

    /// `jpeg` with an APP1 EXIF segment inserted after SOI.
    fn with_exif(jpeg: &[u8], exif: &[u8]) -> Vec<u8> {
        let mut out = jpeg[..2].to_vec();
        out.extend([0xff, 0xe1]);
        out.extend(((exif.len() + 8) as u16).to_be_bytes());
        out.extend(b"Exif\0\0");
        out.extend(exif);
        out.extend(&jpeg[2..]);
        out
    }

    fn write(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, bytes).unwrap();
        path
    }

    #[test]
    fn jpeg_decodes_at_reduced_dct_scale() {
        for ((w, h), scaled, thumb) in [
            ((3200, 2000), (400, 250), (384, 240)), // 1/8
            ((3000, 2000), (750, 500), (384, 256)), // 1/8 would be 375: 1/4
            ((1000, 1600), (250, 400), (240, 384)), // 1/4
            ((1000, 700), (500, 350), (384, 269)),  // 1/2
            ((500, 300), (500, 300), (384, 230)),   // full scale
            ((300, 200), (300, 200), (300, 200)),   // never upscaled
        ] {
            let bytes = jpeg_bytes(&halves(w, h, RED, BLUE));
            let (image, _) = jpeg(&bytes[..], 384).unwrap();
            assert_eq!(image.dimensions(), scaled, "{w}x{h}");

            let dir = tempfile::tempdir().unwrap();
            let thumb_image = decode(&write(dir.path(), "a.jpg", &bytes), MediaKind::Image, 384).unwrap();
            assert_eq!(thumb_image.dimensions(), thumb, "{w}x{h}");
            assert!(near(*thumb_image.get_pixel(thumb.0 / 2, 2), RED) && near(*thumb_image.get_pixel(thumb.0 / 2, thumb.1 - 3), BLUE));
        }
    }

    #[test]
    fn jpeg_exif_orientation_is_applied() {
        let dir = tempfile::tempdir().unwrap();
        // Stored landscape, top red / bottom blue; orientation 6 = display rotated 90° clockwise,
        // so the stored top ends up on the right.
        let bytes = with_exif(&jpeg_bytes(&halves(2400, 1200, RED, BLUE)), &exif(6, None));
        let path = write(dir.path(), "rotated.jpg", &bytes);
        let thumb = decode(&path, MediaKind::Image, 384).unwrap();
        assert_eq!(thumb.dimensions(), (192, 384));
        assert!(near(*thumb.get_pixel(180, 192), RED) && near(*thumb.get_pixel(10, 192), BLUE));
        assert_eq!(reference(&path, 384).unwrap().dimensions(), thumb.dimensions(), "agrees with the old path");
    }

    #[test]
    fn jpeg_grayscale_cmyk_and_progressive() {
        let dir = tempfile::tempdir().unwrap();
        let gray = image::GrayImage::from_fn(1600, 800, |x, _| image::Luma([if x < 800 { 40 } else { 210 }]));
        let gray_path = dir.path().join("gray.jpg");
        gray.save(&gray_path).unwrap();
        let thumb = decode(&gray_path, MediaKind::Image, 384).unwrap();
        assert_eq!(thumb.dimensions(), (384, 192));
        assert!(near(*thumb.get_pixel(10, 96), Rgb([40; 3])) && near(*thumb.get_pixel(370, 96), Rgb([210; 3])));

        // Adobe CMYK (as Photoshop / PIL write it): left 100% M+Y = red, right 50% K = gray.
        let bytes = include_bytes!("testdata/cmyk.jpg");
        let (cmyk, _) = jpeg(&bytes[..], 384).expect("CMYK via jpeg-decoder");
        assert!(near(*cmyk.get_pixel(5, 16), Rgb([255, 0, 0])), "{:?}", cmyk.get_pixel(5, 16));
        assert!(near(*cmyk.get_pixel(58, 16), Rgb([127; 3])), "{:?}", cmyk.get_pixel(58, 16));
        let (zune, _) = any_format(Cursor::new(bytes)).unwrap();
        assert!(near(*cmyk.get_pixel(5, 16), *zune.get_pixel(5, 16)) && near(*cmyk.get_pixel(58, 16), *zune.get_pixel(58, 16)));

        let bytes = include_bytes!("testdata/progressive.jpg");
        let (scaled, _) = jpeg(&bytes[..], 384).expect("progressive");
        assert_eq!(scaled.dimensions(), (400, 300), "1/4 scale");
        let thumb = decode(&write(dir.path(), "p.jpg", bytes), MediaKind::Image, 384).unwrap();
        assert_eq!(thumb.dimensions(), (384, 288));
        assert!(near(*thumb.get_pixel(2, 144), Rgb([5, 40, 202])) && near(*thumb.get_pixel(381, 144), Rgb([255, 40, 202])));
    }

    #[test]
    fn exif_preview_is_extracted() {
        let dir = tempfile::tempdir().unwrap();
        let embedded = jpeg_bytes(&halves(160, 80, RED, BLUE));
        let full = jpeg_bytes(&halves(1600, 800, RED, BLUE));
        let path = write(dir.path(), "camera.jpg", &with_exif(&full, &exif(6, Some(&embedded))));

        let preview = decode_preview(&path).unwrap();
        assert_eq!(preview.dimensions(), (80, 160), "rotated like the main image");
        assert!(near(*preview.get_pixel(75, 80), RED) && near(*preview.get_pixel(4, 80), BLUE));
        assert_eq!(decode(&path, MediaKind::Image, 384).unwrap().dimensions(), (192, 384), "not the tiny preview");

        let no_thumb = write(dir.path(), "plain.jpg", &with_exif(&full, &exif(1, None)));
        assert!(decode_preview(&no_thumb).is_none());
        assert!(decode_preview(&write(dir.path(), "bare.jpg", &full)).is_none());
        let png = dir.path().join("a.png");
        halves(10, 10, RED, BLUE).save(&png).unwrap();
        assert!(decode_preview(&png).is_none());
    }

    /// Page 0: 2400×1600 red. Reduced pages: 600×400 green, 300×200 blue (below 384).
    fn write_pyramid(path: &Path) {
        let mut encoder = TiffEncoder::new(BufWriter::new(File::create(path).unwrap())).unwrap();
        let full = RgbImage::from_pixel(2400, 1600, RED);
        encoder.write_image::<RGB8>(2400, 1600, full.as_raw()).unwrap();
        for (w, h, color) in [(600, 400, GREEN), (300, 200, BLUE)] {
            let mut page = encoder.new_image::<RGB8>(w, h).unwrap();
            page.encoder().write_tag(Tag::NewSubfileType, 1u32).unwrap();
            page.write_data(RgbImage::from_pixel(w, h, color).as_raw()).unwrap();
        }
    }

    /// Page 0: 2400×1600 red, with SubIFDs 1200×800 green and 600×400 blue.
    fn write_sub_ifds(path: &Path) {
        let mut encoder = TiffEncoder::new(BufWriter::new(File::create(path).unwrap())).unwrap();
        let mut offsets = Vec::new();
        for (w, h, color) in [(1200u32, 800u32, GREEN), (600, 400, BLUE)] {
            let mut dir = encoder.extra_directory().unwrap();
            let pixels = RgbImage::from_pixel(w, h, color).into_raw();
            let data = dir.write_data(&pixels[..]).unwrap() as u32;
            dir.write_tag(Tag::NewSubfileType, 1u32).unwrap();
            dir.write_tag(Tag::ImageWidth, w).unwrap();
            dir.write_tag(Tag::ImageLength, h).unwrap();
            dir.write_tag(Tag::BitsPerSample, &[8u16, 8, 8][..]).unwrap();
            dir.write_tag(Tag::Compression, 1u16).unwrap();
            dir.write_tag(Tag::PhotometricInterpretation, 2u16).unwrap();
            dir.write_tag(Tag::SamplesPerPixel, 3u16).unwrap();
            dir.write_tag(Tag::RowsPerStrip, h).unwrap();
            dir.write_tag(Tag::StripOffsets, data).unwrap();
            dir.write_tag(Tag::StripByteCounts, pixels.len() as u32).unwrap();
            offsets.push(dir.finish_with_offsets().unwrap().pointer.0 as u32);
        }
        let mut page = encoder.new_image::<RGB8>(2400, 1600).unwrap();
        page.encoder().write_tag(Tag::SubIfd, &offsets[..]).unwrap();
        page.write_data(RgbImage::from_pixel(2400, 1600, RED).as_raw()).unwrap();
    }

    #[test]
    fn tiff_reads_smallest_sufficient_reduced_page() {
        let dir = tempfile::tempdir().unwrap();
        for (name, write, page) in [
            ("chained.tif", write_pyramid as fn(&Path), (600, 400)),
            ("subifd.tif", write_sub_ifds, (600, 400)),
        ] {
            let path = dir.path().join(name);
            write(&path);
            let bytes = std::fs::read(&path).unwrap();
            let (reduced, _) = tiff(&mut Cursor::new(&bytes), 384).expect(name);
            assert_eq!(reduced.dimensions(), page, "{name}");
            let thumb = decode(&path, MediaKind::Image, 384).unwrap();
            assert_eq!(thumb.dimensions(), (384, 256));
            let wanted = if name == "chained.tif" { GREEN } else { BLUE };
            assert!(near(*thumb.get_pixel(192, 128), wanted), "{name}: {:?}", thumb.get_pixel(192, 128));
            assert_eq!(tiff(&mut Cursor::new(&bytes), 1600).unwrap().0.dimensions(), (2400, 1600), "{name}: no page covers 1600 px");
            assert_eq!(any_format(Cursor::new(&bytes)).unwrap().0.dimensions(), (2400, 1600), "{name}: others see page 0");
        }

        // Single-page TIFF: full decode.
        let plain = dir.path().join("plain.tif");
        RgbImage::from_pixel(1200, 800, RED).save(&plain).unwrap();
        assert_eq!(tiff(&mut Cursor::new(std::fs::read(&plain).unwrap()), 384).unwrap().0.dimensions(), (1200, 800));
        assert_eq!(decode(&plain, MediaKind::Image, 384).unwrap().dimensions(), (384, 256));
    }

    #[test]
    fn tiff_writer_adds_a_reduced_page() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("scan.tif");
        let scan = halves(2000, 3000, RED, BLUE);
        write_tiff_with_reduced_page(&path, &scan).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(tiff(&mut Cursor::new(&bytes), 384).unwrap().0.dimensions(), (683, 1024));
        assert_eq!(any_format(Cursor::new(&bytes)).unwrap().0, scan, "page 0 is the lossless full scan");
        let thumb = decode(&path, MediaKind::Image, 384).unwrap();
        assert_eq!(thumb.dimensions(), (256, 384));
        assert!(near(*thumb.get_pixel(128, 5), RED) && near(*thumb.get_pixel(128, 378), BLUE));
    }

    #[test]
    fn corrupt_files_return_none() {
        let dir = tempfile::tempdir().unwrap();
        let jpeg = jpeg_bytes(&halves(1600, 1200, RED, BLUE));
        let mut tiff = Vec::new();
        RgbImage::from_pixel(600, 400, RED).write_to(&mut Cursor::new(&mut tiff), ImageFormat::Tiff).unwrap();
        let mut garbage_tiff = tiff[..8].to_vec();
        garbage_tiff.extend([0xff; 64]);
        for (name, bytes) in [
            ("garbage.jpg", vec![0x5a; 4096]),
            ("empty.jpg", Vec::new()),
            ("soi-only.jpg", jpeg[..2].to_vec()),
            ("header-only.jpg", jpeg[..200].to_vec()),
            ("bad-ifd.tif", garbage_tiff),
            ("header-only.tif", tiff[..16].to_vec()),
        ] {
            let path = write(dir.path(), name, &bytes);
            assert!(decode(&path, MediaKind::Image, 384).is_none(), "{name}");
            assert!(decode_preview(&path).is_none(), "{name}");
        }
        // Truncated data: whatever the decoders salvage, they must not panic.
        decode(&write(dir.path(), "half.jpg", &jpeg[..jpeg.len() / 2]), MediaKind::Image, 384);
        decode(&write(dir.path(), "half.tif", &tiff[..tiff.len() / 2]), MediaKind::Image, 384);
        let bad_exif = with_exif(&jpeg, &[b"II*\0".as_slice(), &[0xff; 40]].concat());
        assert!(decode_preview(&write(dir.path(), "bad-exif.jpg", &bad_exif)).is_none());
        assert!(decode(&write(dir.path(), "bad-exif.jpg", &bad_exif), MediaKind::Image, 384).is_some());
    }

    /// Old vs new decode time per file.
    /// `THUMB_BENCH_DIR=<dir of big files> [THUMB_BENCH_OUT=<dir for PNGs>]
    ///  cargo test --release -- --ignored --nocapture bench_decode`
    #[test]
    #[ignore]
    fn bench_decode() {
        let time = |f: &dyn Fn() -> Option<RgbImage>, runs: u32| {
            let mut best = f64::MAX;
            for _ in 0..runs {
                let start = Instant::now();
                f().expect("decodes");
                best = best.min(start.elapsed().as_secs_f64() * 1e3);
            }
            best
        };
        let out = std::env::var_os("THUMB_BENCH_OUT").map(PathBuf::from);
        if let Some(dir) = std::env::var_os("THUMB_BENCH_DIR") {
            let mut files: Vec<_> = std::fs::read_dir(dir).unwrap().flatten().map(|e| e.path()).collect();
            files.sort();
            // Each TIFF again as written by `write_tiff_with_reduced_page`.
            let reduced = tempfile::tempdir().unwrap();
            for tif in files.clone().iter().filter(|p| p.extension().is_some_and(|e| e == "tif")) {
                let full = ImageReader::open(tif).unwrap().decode().unwrap().into_rgb8();
                let path = reduced.path().join(tif.file_stem().unwrap()).with_extension("reduced.tif");
                write_tiff_with_reduced_page(&path, &full).unwrap();
                files.push(path);
            }
            println!("{:<28} {:>10} {:>10} {:>7}", "file (best of 5)", "old ms", "new ms", "speedup");
            for path in files {
                let old = time(&|| reference(&path, 384), 5);
                let new = time(&|| decode(&path, MediaKind::Image, 384), 5);
                let name = path.file_name().unwrap().to_string_lossy().into_owned();
                println!("{name:<28} {old:>10.1} {new:>10.1} {:>6.1}x", old / new);
                if let Some(out) = &out {
                    reference(&path, 384).unwrap().save(out.join(format!("{name}.old.png"))).unwrap();
                    decode(&path, MediaKind::Image, 384).unwrap().save(out.join(format!("{name}.new.png"))).unwrap();
                }
            }
        }

        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("test_images");
        let mut files = Vec::new();
        let mut dirs = vec![root];
        while let Some(dir) = dirs.pop() {
            for path in std::fs::read_dir(dir).unwrap().flatten().map(|e| e.path()) {
                if path.is_dir() {
                    dirs.push(path);
                } else if image::ImageFormat::from_path(&path).is_ok() {
                    files.push(path);
                }
            }
        }
        let total = |f: &dyn Fn(&Path) -> Option<RgbImage>| {
            let start = Instant::now();
            let decoded = files.iter().filter(|p| f(p).is_some()).count();
            (start.elapsed().as_secs_f64() * 1e3, decoded)
        };
        let (old, n_old) = total(&|p| reference(p, 384));
        let (new, n_new) = total(&|p| decode(p, MediaKind::Image, 384));
        println!("test_images: {} files, old {old:.0} ms ({n_old} ok), new {new:.0} ms ({n_new} ok), {:.2}x", files.len(), old / new);
    }
}
