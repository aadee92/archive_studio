//! Turning whole-bed scans into saved photos: fronts as new photos, backs paired with the
//! fronts of the last batch. Shared by the command-line tool and the Archive Studio GUI.

use std::fs::File;
use std::io::BufWriter;
use std::path::{Path, PathBuf};

use x980::Result;

use crate::detect::{self, DETECT_DPI, Rect, Rgb};
use crate::straighten;

/// Extra mat kept around a back that is cropped to its front's position.
const BACK_MARGIN_MM: f64 = 3.0;

/// What [`Duplex::process`] reports as it goes.
pub enum Event<'a> {
    /// A line for the log.
    Log(String),
    /// A photo was written to `path`; `image` is what was saved.
    Saved { path: &'a Path, image: &'a Rgb },
}

/// The last batch of fronts, which a back scan pairs with.
struct Batch {
    /// Photo number and position of each front.
    fronts: Vec<(u32, Rect)>,
    /// The front scan at `DETECT_DPI`, to tell photos left face down from flipped ones.
    bed: Rgb,
}

/// Saves photos into one directory, remembering the last batch of fronts. Keep it across
/// USB reconnects so a back scan still pairs with fronts scanned before the reconnect.
pub struct Duplex {
    out: PathBuf,
    pending: Option<Batch>,
}

impl Duplex {
    pub fn new(out: PathBuf) -> Self {
        Self { out, pending: None }
    }

    /// Whether there are fronts for a back scan to pair with.
    pub fn has_pending(&self) -> bool {
        self.pending.is_some()
    }

    /// "photos 0012-0015" (or "photo 0012") for the fronts a back scan would pair with.
    pub fn pending_numbers(&self) -> Option<String> {
        let fronts = &self.pending.as_ref()?.fronts;
        Some(match (fronts.first(), fronts.last()) {
            (Some((a, _)), Some((b, _))) if a != b => format!("photos {a:04}-{b:04}"),
            (Some((a, _)), _) => format!("photo {a:04}"),
            _ => "the photos".into(),
        })
    }

    /// Saves the photos on `bed`, a whole-bed scan at `dpi`. `grid` is the bed's size at
    /// `DETECT_DPI` (`Identity::flatbed_at`). With `backs`, they are saved as the backs of
    /// the last batch of fronts; without, or if there is no such batch, as new fronts.
    /// Returns how many photos were saved.
    pub fn process(
        &mut self,
        bed: &Rgb,
        dpi: u32,
        grid: (u32, u32),
        backs: bool,
        on: &mut dyn FnMut(Event),
    ) -> Result<usize> {
        let small = bed.downsample(dpi, DETECT_DPI, grid.0, grid.1);
        if backs && self.pending.is_none() {
            on(Event::Log("no fronts to add backs to; scanning as fronts".into()));
        }
        // The batch is dropped only once the scan is in, so it survives a reconnect.
        match self.pending.take() {
            Some(batch) if backs => self.save_backs(bed, &small, &batch, dpi, grid, on),
            _ => self.save_fronts(bed, small, dpi, on),
        }
    }

    /// Saves every photo on the bed as a new front and keeps them as the batch for a later
    /// back scan (none if the glass was empty).
    fn save_fronts(&mut self, bed: &Rgb, small: Rgb, dpi: u32, on: &mut dyn FnMut(Event)) -> Result<usize> {
        let photos = detect::find_photos(&small);
        if photos.is_empty() {
            on(Event::Log("no photos found on the glass".into()));
            return Ok(0);
        }
        let first = next_photo_number(&self.out)?;
        let mut fronts = Vec::new();
        for (n, rect) in (first..).zip(photos) {
            let path = self.out.join(format!("photo-{n:04}-front.png"));
            let (img, how) = cut_out(bed, &small, rect, dpi);
            save_png(&path, &img, dpi)?;
            on(Event::Log(format!("  photo {n:04}: {how}")));
            on(Event::Saved { path: &path, image: &img });
            fronts.push((n, rect));
        }
        let saved = fronts.len();
        on(Event::Log(format!("saved {saved} fronts")));
        self.pending = Some(Batch { fronts, bed: small });
        Ok(saved)
    }

    /// Saves the backs of `batch`'s photos that were flipped over in place.
    fn save_backs(
        &self,
        bed: &Rgb,
        small: &Rgb,
        batch: &Batch,
        dpi: u32,
        grid: (u32, u32),
        on: &mut dyn FnMut(Event),
    ) -> Result<usize> {
        let fronts = &batch.fronts;
        // Photos left face down look just as they did in the front scan.
        let left_down: Vec<bool> = fronts.iter().map(|(_, r)| detect::unchanged(&batch.bed, small, *r)).collect();
        let backs = detect::find_photos(small);
        let front_rects: Vec<Rect> = fronts.iter().map(|(_, r)| *r).collect();
        let matches = detect::match_backs(&front_rects, &backs);
        let (mut paired, mut saved) = (0, 0);
        for (rect, m) in backs.iter().zip(&matches) {
            let path = match m {
                Some(f) if left_down[*f] => continue,
                Some(f) => {
                    paired += 1;
                    self.out.join(format!("photo-{:04}-back.png", fronts[*f].0))
                }
                None => next_free(&self.out, "unmatched-back-", ".png")?,
            };
            let (img, how) = cut_out(bed, small, *rect, dpi);
            on(Event::Log(match m {
                Some(f) => format!("  photo {:04}: back found, {how}", fronts[*f].0),
                None => format!(
                    "  warning: back at {} doesn't line up with any front; saved as {}",
                    rect.describe(),
                    path.display()
                ),
            }));
            save_png(&path, &img, dpi)?;
            on(Event::Saved { path: &path, image: &img });
            saved += 1;
        }
        // A plain paper back may show no clear outline on the white mat. If the front's spot
        // isn't bare mat, the photo is there: crop it with a margin.
        for (f, (n, front)) in fronts.iter().enumerate() {
            if left_down[f] || matches.contains(&Some(f)) || !detect::occupied(small, *front) {
                continue;
            }
            paired += 1;
            on(Event::Log(format!("  photo {n:04}: back found (no clear outline; cropped with a margin)")));
            let rect = front.grow(BACK_MARGIN_MM, grid.0, grid.1).to_dpi(dpi);
            let (path, img) = (self.out.join(format!("photo-{n:04}-back.png")), bed.crop(rect));
            save_png(&path, &img, dpi)?;
            on(Event::Saved { path: &path, image: &img });
            saved += 1;
        }
        let kept = left_down.iter().filter(|d| **d).count();
        if kept > 0 {
            on(Event::Log(format!("  {kept} left face down; no back saved for them")));
        }
        on(Event::Log(format!("saved {paired} of {} backs; batch done", fronts.len())));
        Ok(saved)
    }
}

/// Cuts a detected photo out of the full-resolution bed: straightened, with a thin strip
/// of mat on every side. Falls back to the coarse box if the outline can't be traced.
/// Also returns a description for the log.
fn cut_out(bed: &Rgb, small: &Rgb, rect: Rect, dpi: u32) -> (Rgb, String) {
    let Some(p) = straighten::locate(bed, dpi, detect::mat_level(small), rect) else {
        return (bed.crop(rect.to_dpi(dpi)), format!("{} (outline unclear; not straightened)", rect.describe()));
    };
    let image = straighten::extract(bed, &p, dpi);
    let mut how = format!("{} at {}", p.describe(dpi), rect.describe_position());
    if p.clipped {
        how += "\n    WARNING: runs off the edge of the scan area and may be cut off (grey in the \
                image); move it ~3 mm in from the edge and rescan";
    }
    (image, how)
}

/// One past the highest photo number already in `dir`, so reruns never overwrite.
fn next_photo_number(dir: &Path) -> Result<u32> {
    let mut highest = 0;
    for entry in std::fs::read_dir(dir)? {
        let name = entry?.file_name();
        let n = name.to_str().and_then(|s| s.strip_prefix("photo-")).and_then(|s| s.get(..4)).and_then(|s| s.parse().ok());
        highest = highest.max(n.unwrap_or(0));
    }
    Ok(highest + 1)
}

/// `<prefix>NNNN<suffix>` with the first number not yet used in `dir`.
pub fn next_free(dir: &Path, prefix: &str, suffix: &str) -> Result<PathBuf> {
    for n in 1.. {
        let path = dir.join(format!("{prefix}{n:04}{suffix}"));
        if !path.exists() {
            return Ok(path);
        }
    }
    unreachable!()
}

/// Writes `img` as an 8-bit RGB PNG tagged with `dpi`, via a `.part` file so a partly
/// written photo never appears under its final name.
pub fn save_png(path: &Path, img: &Rgb, dpi: u32) -> Result<()> {
    let tmp = path.with_extension("png.part");
    let mut png = png::Encoder::new(BufWriter::new(File::create(&tmp)?), img.width, img.height);
    png.set_color(png::ColorType::Rgb);
    png.set_depth(png::BitDepth::Eight);
    png.set_compression(png::Compression::Fast);
    let ppm = (dpi as f64 / 0.0254).round() as u32;
    png.set_pixel_dims(Some(png::PixelDimensions { xppu: ppm, yppu: ppm, unit: png::Unit::Meter }));
    let mut writer = png.write_header()?;
    writer.write_image_data(&img.data)?;
    writer.finish()?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}
