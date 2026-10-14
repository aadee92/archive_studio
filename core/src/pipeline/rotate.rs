//! Rotating image files in place, losslessly.
//!
//! A rotation is relative to the image as it is shown, that is with its EXIF orientation
//! applied (the orientation model and the user both see it that way). So the file is
//! rewritten with its pixels in that displayed view, turned, and its orientation set to 1;
//! face and tag regions, kept in displayed coordinates, then turn by the same angle.
//!
//! - JPEG: turned in the DCT domain, as `jpegtran -trim` does (see [`jpeg`]): no sample is
//!   decoded or compressed again. Where the width or height isn't a multiple of the MCU
//!   (8 or 16 pixels), the partial MCUs along an edge that would have to move to the
//!   other side are dropped, so the image may lose up to 15 pixels on that side. Every
//!   APPn and COM segment is kept (EXIF, ICC profile, XMP, comments), with the EXIF
//!   orientation, the EXIF pixel size and an XMP `tiff:Orientation` brought up to date.
//!   The EXIF thumbnail isn't turned.
//! - PNG: decoded to plain samples (palettes and low bit depths expanded, which keeps every
//!   pixel value), turned, and written back with its metadata chunks (DPI swapped for 90° and
//!   270°, ICC profile, gamma, text, EXIF).
//!
//! TIFF isn't rewritten yet: its reduced pages would need rebuilding.

mod exif;
mod jpeg;

use std::fs::File;
use std::io::{self, BufReader, BufWriter};
use std::path::{Path, PathBuf};

/// Whether [`rotate_into`] can rewrite this file.
pub fn can_rotate(path: &Path) -> bool {
    format(path).is_some()
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Format {
    Jpeg,
    Png,
}

fn format(path: &Path) -> Option<Format> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    match ext.as_str() {
        "jpg" | "jpeg" | "jpe" => Some(Format::Jpeg),
        "png" => Some(Format::Png),
        _ => None,
    }
}

/// The hidden file next to `path` that [`rotate_into`] writes before it is renamed over
/// `path`. Dot-prefixed, so library scans and the watcher ignore it.
pub fn temp_path(path: &Path) -> PathBuf {
    let name = path.file_name().map_or_else(Default::default, |n| n.to_string_lossy().into_owned());
    path.with_file_name(format!(".{name}.rotating"))
}

/// Writes `src`, as displayed, turned `cw` degrees clockwise (90, 180 or 270) to `dst`.
pub fn rotate_into(src: &Path, dst: &Path, cw: u16) -> io::Result<()> {
    match format(src) {
        Some(Format::Jpeg) => rotate_jpeg(src, dst, cw),
        Some(Format::Png) => rotate_png(src, dst, cw),
        None => Err(io::Error::new(io::ErrorKind::Unsupported, "only JPEG and PNG files can be rotated losslessly so far")),
    }
}

/// One of the eight ways to turn or mirror an image in 90° steps: transpose it first if
/// `swap`, then mirror it left to right if `flip_x` and top to bottom if `flip_y`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Transform {
    swap: bool,
    flip_x: bool,
    flip_y: bool,
}

impl Transform {
    const IDENTITY: Self = Self::new(false, false, false);

    const fn new(swap: bool, flip_x: bool, flip_y: bool) -> Self {
        Self { swap, flip_x, flip_y }
    }

    /// A clockwise turn by `cw` degrees.
    fn turn(cw: u16) -> Self {
        match cw % 360 {
            90 => Self::new(true, true, false),
            180 => Self::new(false, true, true),
            270 => Self::new(true, false, true),
            _ => Self::IDENTITY,
        }
    }

    /// What displays pixels stored with this EXIF orientation.
    fn exif(orientation: u16) -> Self {
        match orientation {
            2 => Self::new(false, true, false),
            3 => Self::new(false, true, true),
            4 => Self::new(false, false, true),
            5 => Self::new(true, false, false),
            6 => Self::new(true, true, false),
            7 => Self::new(true, true, true),
            8 => Self::new(true, false, true),
            _ => Self::IDENTITY,
        }
    }

    /// Where the pixel at (`x`, `y`) of a `width` × `height` image ends up.
    fn map(self, (x, y): (usize, usize), (width, height): (usize, usize)) -> (usize, usize) {
        let ((a, b), (w, h)) = if self.swap { ((y, x), (height, width)) } else { ((x, y), (width, height)) };
        (if self.flip_x { w - 1 - a } else { a }, if self.flip_y { h - 1 - b } else { b })
    }

    /// This, then `next`.
    fn then(self, next: Self) -> Self {
        let size = (2, 3);
        let turned = if self.swap { (3, 2) } else { size };
        (0..8)
            .map(|i| Self::new(i & 4 != 0, i & 2 != 0, i & 1 != 0))
            .find(|t| [(0, 0), (1, 0), (0, 1)].iter().all(|&p| t.map(p, size) == next.map(self.map(p, size), turned)))
            .expect("the transforms are a group")
    }
}

fn invalid(e: &dyn std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, e.to_string())
}

fn rotate_jpeg(src: &Path, dst: &Path, cw: u16) -> io::Result<()> {
    let mut jpeg = jpeg::Jpeg::decode(&std::fs::read(src)?)?;
    // The first EXIF segment is the one readers go by.
    let exif_at = jpeg.segments.iter().position(|(marker, p)| *marker == 0xE1 && p.starts_with(b"Exif\0\0"));
    let orientation = exif_at.and_then(|i| exif::orientation(exif::tiff(&mut jpeg.segments[i].1))).unwrap_or(1);
    let t = Transform::exif(orientation).then(Transform::turn(cw));
    jpeg.transform(t)?;

    let (width, height) = (jpeg.width as u32, jpeg.height as u32);
    for (i, (marker, p)) in jpeg.segments.iter_mut().enumerate() {
        if Some(i) == exif_at {
            exif::set_upright(exif::tiff(p), width, height);
        } else if *marker == 0xE1 && p.starts_with(b"http://ns.adobe.com/xap/1.0/\0") {
            exif::set_xmp_upright(p);
        } else if *marker == 0xE0 && p.starts_with(b"JFIF\0") && p.len() >= 12 && t.swap {
            // Pixel density, across then down.
            let (across, down) = p[8..12].split_at_mut(2);
            across.swap_with_slice(down);
        }
    }
    std::fs::write(dst, jpeg.encode())
}

fn rotate_png(src: &Path, dst: &Path, cw: u16) -> io::Result<()> {
    let mut decoder = png::Decoder::new(BufReader::new(File::open(src)?));
    decoder.set_transformations(png::Transformations::EXPAND);
    let mut reader = decoder.read_info().map_err(|e| invalid(&e))?;
    let mut pixels = vec![0; reader.output_buffer_size().ok_or_else(|| invalid(&"image too large"))?];
    let frame = reader.next_frame(&mut pixels).map_err(|e| invalid(&e))?;
    pixels.truncate(frame.buffer_size());
    let (color, depth) = reader.output_color_type();
    let bytes_per_pixel = color.samples() * (depth as usize).div_ceil(8);

    let mut info = reader.info().clone();
    let mut exif = info.exif_metadata.take().map(|e| e.into_owned());
    let orientation = exif.as_mut().and_then(|e| exif::orientation(exif::tiff(e))).unwrap_or(1);
    let t = Transform::exif(orientation).then(Transform::turn(cw));

    let (width, height) = (frame.width, frame.height);
    let turned = apply(&pixels, width as usize, height as usize, bytes_per_pixel, t);

    info.color_type = color;
    info.bit_depth = depth;
    info.interlaced = false;
    // Expanded away; sBIT and bKGD are in the old layout's terms.
    (info.palette, info.trns, info.sbit, info.bkgd) = (None, None, None, None);
    (info.frame_control, info.animation_control) = (None, None);
    if t.swap {
        (info.width, info.height) = (height, width);
        if let Some(dims) = &mut info.pixel_dims {
            (dims.xppu, dims.yppu) = (dims.yppu, dims.xppu);
        }
    }
    if let Some(exif) = &mut exif {
        exif::set_upright(exif::tiff(exif), info.width, info.height);
    }
    info.exif_metadata = exif.map(Into::into);

    let mut encoder = png::Encoder::with_info(BufWriter::new(File::create(dst)?), info).map_err(|e| invalid(&e))?;
    encoder.set_compression(png::Compression::Fast);
    let mut writer = encoder.write_header().map_err(|e| invalid(&e))?;
    writer.write_image_data(&turned).map_err(|e| invalid(&e))?;
    writer.finish().map_err(|e| invalid(&e))?;
    Ok(())
}

/// `pixels` (`width` × `height`, `bpp` bytes each, rows packed) with `t` applied.
fn apply(pixels: &[u8], width: usize, height: usize, bpp: usize, t: Transform) -> Vec<u8> {
    let mut out = vec![0; pixels.len()];
    let out_width = if t.swap { height } else { width };
    for y in 0..height {
        for x in 0..width {
            let (nx, ny) = t.map((x, y), (width, height));
            let from = (y * width + x) * bpp;
            let to = (ny * out_width + nx) * bpp;
            out[to..to + bpp].copy_from_slice(&pixels[from..from + bpp]);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_png(path: &Path, width: u32, height: u32, data: &[u8], dpi_x: u32, dpi_y: u32) {
        let mut png = png::Encoder::new(BufWriter::new(File::create(path).unwrap()), width, height);
        png.set_color(png::ColorType::Rgb);
        png.set_depth(png::BitDepth::Eight);
        png.set_pixel_dims(Some(png::PixelDimensions { xppu: dpi_x, yppu: dpi_y, unit: png::Unit::Meter }));
        let mut writer = png.write_header().unwrap();
        writer.write_image_data(data).unwrap();
        writer.finish().unwrap();
    }

    fn read_png(path: &Path) -> (u32, u32, Vec<u8>, Option<png::PixelDimensions>) {
        let mut reader = png::Decoder::new(BufReader::new(File::open(path).unwrap())).read_info().unwrap();
        let mut buf = vec![0; reader.output_buffer_size().unwrap()];
        let frame = reader.next_frame(&mut buf).unwrap();
        buf.truncate(frame.buffer_size());
        (frame.width, frame.height, buf, reader.info().pixel_dims)
    }

    #[test]
    fn turns_pixels_and_dpi() {
        let dir = tempfile::tempdir().unwrap();
        let (src, dst) = (dir.path().join("a.png"), dir.path().join("b.png"));
        // 3×2, pixel value = its index.
        let data: Vec<u8> = (0..6u8).flat_map(|i| [i, i, i]).collect();
        write_png(&src, 3, 2, &data, 100, 200);

        rotate_into(&src, &dst, 90).unwrap();
        let (w, h, px, dims) = read_png(&dst);
        assert_eq!((w, h), (2, 3));
        // Row 0 of the turned image is the old left column, bottom to top: 3, 0.
        let firsts: Vec<u8> = px.chunks(3).map(|p| p[0]).collect();
        assert_eq!(firsts, [3, 0, 4, 1, 5, 2]);
        let dims = dims.unwrap();
        assert_eq!((dims.xppu, dims.yppu), (200, 100));

        // Four quarter turns are the identity.
        for _ in 0..3 {
            rotate_into(&dst, &src, 90).unwrap();
            std::fs::rename(&src, &dst).unwrap();
        }
        assert_eq!(read_png(&dst).2, data);
    }

    #[test]
    fn half_turn_and_three_quarters() {
        let data: Vec<u8> = (0..6u8).collect();
        assert_eq!(apply(&data, 3, 2, 1, Transform::turn(180)), [5, 4, 3, 2, 1, 0]);
        assert_eq!(apply(&data, 3, 2, 1, Transform::turn(270)), [2, 5, 1, 4, 0, 3]);
    }

    #[test]
    fn refuses_other_formats() {
        let err = rotate_into(Path::new("scan.tif"), Path::new("out.tif"), 90).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::Unsupported);
    }

    #[test]
    fn transforms_compose() {
        for cw in [90, 180, 270] {
            assert_eq!(Transform::turn(cw).then(Transform::turn(360 - cw)), Transform::IDENTITY);
        }
        assert_eq!(Transform::turn(90).then(Transform::turn(90)), Transform::turn(180));
        // Orientation 6 is shown turned 90° clockwise; turning that back is the identity.
        assert_eq!(Transform::exif(6).then(Transform::turn(270)), Transform::IDENTITY);
        // 5 (transpose) turned 90° is a left-right mirror.
        assert_eq!(Transform::exif(5).then(Transform::turn(90)), Transform::exif(2));
    }

    // -----------------------------------------------------------------------------------
    // JPEG.

    const TESTDATA: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/src/pipeline/testdata");

    /// A `width` × `height` picture with detail in every block.
    fn picture(width: u32, height: u32) -> image::RgbImage {
        image::RgbImage::from_fn(width, height, |x, y| {
            let wave = 127.0 + 120.0 * (x as f32 / 5.0).sin() * (y as f32 / 7.0).cos();
            image::Rgb([wave as u8, ((x * 255 / width) ^ ((y * 3) & 0x3F)) as u8, ((y * 255 / height + x * y % 17) % 256) as u8])
        })
    }

    /// A baseline JPEG (4:2:2, so 16 × 8 MCUs) with optional EXIF and ICC profile, and a
    /// comment.
    fn write_jpeg(path: &Path, image: &image::RgbImage, exif: Option<Vec<u8>>, icc: Option<Vec<u8>>) {
        use image::ImageEncoder;
        let mut data = Vec::new();
        let mut encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut data, 90);
        if let Some(exif) = exif {
            encoder.set_exif_metadata(exif).unwrap();
        }
        if let Some(icc) = icc {
            encoder.set_icc_profile(icc).unwrap();
        }
        encoder.write_image(image, image.width(), image.height(), image::ExtendedColorType::Rgb8).unwrap();
        let comment = b"\xFF\xFE\x00\x0Ascanned!";
        data.splice(2..2, comment.iter().copied());
        std::fs::write(path, data).unwrap();
    }

    /// The image as displayed (EXIF orientation applied), as the pipeline decodes it.
    fn displayed(path: &Path) -> image::RgbImage {
        use image::ImageDecoder;
        let mut decoder = image::ImageReader::open(path).unwrap().with_guessed_format().unwrap().into_decoder().unwrap();
        let orientation = decoder.orientation().unwrap();
        let mut image = image::DynamicImage::from_decoder(decoder).unwrap();
        image.apply_orientation(orientation);
        image.to_rgb8()
    }

    /// `image` turned `cw` degrees, cut to `width` × `height` the way the JPEG trim does.
    fn expected(image: &image::RgbImage, cw: u16, (width, height): (u32, u32)) -> image::RgbImage {
        let turned = match cw {
            90 => image::imageops::rotate90(image),
            180 => image::imageops::rotate180(image),
            _ => image::imageops::rotate270(image),
        };
        // The trimmed edge is the one a mirrored axis brings to the start.
        let t = Transform::turn(cw);
        let x = if t.flip_x { turned.width() - width } else { 0 };
        let y = if t.flip_y { turned.height() - height } else { 0 };
        image::imageops::crop_imm(&turned, x, y, width, height).to_image()
    }

    /// The largest difference between two images' samples, at least `margin` pixels in
    /// from the edges; panics if the sizes differ.
    fn difference(a: &image::RgbImage, b: &image::RgbImage, margin: u32) -> u8 {
        assert_eq!(a.dimensions(), b.dimensions());
        let (width, height) = a.dimensions();
        let inside = |x: u32, y: u32| x >= margin && y >= margin && x + margin < width && y + margin < height;
        let pairs = a.enumerate_pixels().zip(b.pixels()).filter(|((x, y, _), _)| inside(*x, *y));
        pairs.flat_map(|((_, _, p), q)| p.0.into_iter().zip(q.0).map(|(x, y)| x.abs_diff(y))).max().unwrap_or(0)
    }

    fn coefficients(path: &Path) -> Vec<Vec<[i16; 64]>> {
        jpeg::Jpeg::decode(&std::fs::read(path).unwrap()).unwrap().coefficients()
    }

    /// Turns `path` by 90° four times, checking each turn's pixels, and returns the last
    /// file.
    fn turn_around(dir: &Path, path: &Path, tolerance: u8) -> PathBuf {
        let mut current = path.to_owned();
        for step in 0..4 {
            let next = dir.join(format!("turn{step}.jpg"));
            rotate_into(&current, &next, 90).unwrap();
            let (before, after) = (displayed(&current), displayed(&next));
            let diff = difference(&expected(&before, 90, after.dimensions()), &after, 0);
            assert!(diff <= tolerance, "turn {step}: off by {diff}");
            current = next;
        }
        current
    }

    #[test]
    fn jpeg_turns_losslessly() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("a.jpg");
        write_jpeg(&src, &picture(64, 48), None, None);
        let original = displayed(&src);
        for cw in [90, 180, 270] {
            let dst = dir.path().join(format!("{cw}.jpg"));
            rotate_into(&src, &dst, cw).unwrap();
            let turned = displayed(&dst);
            let size = if cw == 180 { (64, 48) } else { (48, 64) };
            assert_eq!(turned.dimensions(), size);
            // Same coefficients, so only the decoder's rounding can differ.
            let diff = difference(&expected(&original, cw, size), &turned, 0);
            assert!(diff <= 2, "{cw}°: off by {diff}");
        }
        // Four quarter turns give back the very same coefficients.
        let last = turn_around(dir.path(), &src, 2);
        assert_eq!(coefficients(&last), coefficients(&src));
    }

    #[test]
    fn jpeg_progressive_and_restarts() {
        let dir = tempfile::tempdir().unwrap();
        // 64 × 48, progressive 4:2:0 with restart markers.
        let src = Path::new(TESTDATA).join("progressive.jpg");
        // 4:2:0 chroma is upsampled with a bias that doesn't turn with the image.
        let last = turn_around(dir.path(), &src, 3);
        assert_eq!(coefficients(&last), coefficients(&src));
        // The sequential copy reads the same as the progressive original.
        let copy = dir.path().join("copy.jpg");
        let mut jpeg = jpeg::Jpeg::decode(&std::fs::read(&src).unwrap()).unwrap();
        jpeg.transform(Transform::IDENTITY).unwrap();
        std::fs::write(&copy, jpeg.encode()).unwrap();
        assert_eq!(displayed(&copy), displayed(&src));
    }

    #[test]
    fn jpeg_partial_mcus_are_trimmed() {
        let dir = tempfile::tempdir().unwrap();
        // 70 × 45 with 16 × 16 MCUs (4:2:0) and restart markers.
        let src = Path::new(TESTDATA).join("restart_420.jpg");
        let original = displayed(&src);
        // 90° mirrors the old height into the new width: 45 → 32. 180° trims both, 270° the
        // old width.
        for (cw, size) in [(90, (32, 70)), (180, (64, 32)), (270, (45, 64))] {
            let dst = dir.path().join(format!("{cw}.jpg"));
            rotate_into(&src, &dst, cw).unwrap();
            let turned = displayed(&dst);
            assert_eq!(turned.dimensions(), size, "{cw}°");
            // Along a trimmed edge, chroma is upsampled without the neighbours that were cut.
            let diff = difference(&expected(&original, cw, size), &turned, 2);
            assert!(diff <= 3, "{cw}°: off by {diff}");
        }
        // Turning back and forth trims only once: the height is cut to 32 by the first 90°,
        // and the width, never mirrored, keeps its partial blocks.
        let mut current = src.clone();
        for step in 0..4 {
            let next = dir.path().join(format!("transpose{step}.jpg"));
            rotate_into(&current, &next, if step % 2 == 0 { 90 } else { 270 }).unwrap();
            current = next;
        }
        assert_eq!(displayed(&current).dimensions(), (70, 32));
    }

    #[test]
    fn jpeg_exif_orientation_is_baked_in() {
        let dir = tempfile::tempdir().unwrap();
        let icc = (0..300).map(|i| i as u8).collect::<Vec<u8>>();
        for orientation in 1..=8 {
            let src = dir.path().join(format!("o{orientation}.jpg"));
            // 64 × 48 stored; displayed sideways for 5 to 8.
            write_jpeg(&src, &picture(64, 48), Some(exif::tests::sample(orientation, 64, 48)), Some(icc.clone()));
            let shown = displayed(&src);
            let dst = dir.path().join(format!("o{orientation}-turned.jpg"));
            rotate_into(&src, &dst, 90).unwrap();

            let turned = displayed(&dst);
            let diff = difference(&expected(&shown, 90, turned.dimensions()), &turned, 0);
            assert!(diff <= 2, "orientation {orientation}: off by {diff}");
            assert_eq!(turned.dimensions(), (shown.height(), shown.width()));

            let segments = jpeg::Jpeg::decode(&std::fs::read(&dst).unwrap()).unwrap().segments;
            let mut tiff = segments.iter().find(|(m, p)| *m == 0xE1 && p.starts_with(b"Exif")).unwrap().1.clone();
            let tiff = exif::tiff(&mut tiff);
            assert_eq!(exif::orientation(tiff), Some(1));
            assert_eq!(exif::tests::dimensions(tiff), turned.dimensions());
            let original = jpeg::Jpeg::decode(&std::fs::read(&src).unwrap()).unwrap().segments;
            let others = |s: &[(u8, Vec<u8>)]| s.iter().filter(|(m, _)| *m != 0xE1).cloned().collect::<Vec<_>>();
            // The comment, ICC profile and JFIF header, as they were.
            assert_eq!(others(&segments), others(&original));
            assert!(segments.iter().any(|(m, p)| *m == 0xE2 && p.starts_with(b"ICC_PROFILE")));
            assert!(segments.iter().any(|(m, p)| *m == 0xFE && p == b"scanned!"));
        }
    }

    #[test]
    fn png_exif_orientation_is_baked_in() {
        let dir = tempfile::tempdir().unwrap();
        let (src, dst) = (dir.path().join("a.png"), dir.path().join("b.png"));
        let image = picture(5, 3);
        let mut png = png::Encoder::new(BufWriter::new(File::create(&src).unwrap()), 5, 3);
        png.set_color(png::ColorType::Rgb);
        png.set_depth(png::BitDepth::Eight);
        let mut writer = png.write_header().unwrap();
        writer.write_chunk(png::chunk::eXIf, &exif::tests::sample(6, 5, 3)).unwrap();
        writer.write_image_data(image.as_raw()).unwrap();
        writer.finish().unwrap();

        let shown = displayed(&src);
        assert_eq!(shown.dimensions(), (3, 5));
        rotate_into(&src, &dst, 90).unwrap();
        assert_eq!(displayed(&dst), image::imageops::rotate90(&shown));
        let reader = png::Decoder::new(BufReader::new(File::open(&dst).unwrap())).read_info().unwrap();
        let mut exif = reader.info().exif_metadata.as_ref().unwrap().to_vec();
        assert_eq!(exif::orientation(exif::tiff(&mut exif)), Some(1));
    }
}
