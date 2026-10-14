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
    /// Every photo is written under its final name and reported before this returns; a
    /// batch of fronts is only remembered for pairing, never held back for a back scan.
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// Full-resolution test scans, twice the detection grid.
    const DPI: u32 = 2 * DETECT_DPI;
    /// The bed's size at `DETECT_DPI`, like a V850's (8.5 x 11.7 in).
    const GRID: (u32, u32) = (425, 585);

    /// An empty directory of its own for each test.
    fn scratch(name: &str) -> PathBuf {
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("photo-duplex-{}-{n}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// The bare white mat at `DPI`.
    fn mat() -> Rgb {
        let (w, h) = (GRID.0 * DPI / DETECT_DPI, GRID.1 * DPI / DETECT_DPI);
        Rgb { width: w, height: h, data: vec![255; (w * h * 3) as usize] }
    }

    fn fill(bed: &mut Rgb, r: Rect, mut colour: impl FnMut(u32, u32) -> [u8; 3]) {
        for y in r.y..r.y + r.h {
            for x in r.x..r.x + r.w {
                let i = (y * bed.width + x) as usize * 3;
                bed.data[i..i + 3].copy_from_slice(&colour(x, y));
            }
        }
    }

    /// A photo's face (at `DPI`): a faint outline, a thin white border, a busy picture.
    fn front(bed: &mut Rgb, r: Rect) {
        fill(bed, r, |x, y| {
            let edge = x == r.x || y == r.y || x == r.x + r.w - 1 || y == r.y + r.h - 1;
            let border = x < r.x + 8 || y < r.y + 8 || x >= r.x + r.w - 8 || y >= r.y + r.h - 8;
            if edge {
                [190, 190, 190]
            } else if border {
                [252, 252, 250]
            } else {
                [(x * 7 % 200) as u8, 90, (y * 3 % 200) as u8]
            }
        });
    }

    /// A photo's plain paper back.
    fn back(bed: &mut Rgb, r: Rect) {
        fill(bed, r, |_, _| [236, 234, 228]);
    }

    /// Two photos side by side, then one below them (in `DPI` pixels).
    const SPOTS: [Rect; 3] = [
        Rect { x: 60, y: 60, w: 300, h: 200 },
        Rect { x: 460, y: 60, w: 300, h: 200 },
        Rect { x: 60, y: 400, w: 200, h: 300 },
    ];

    /// Runs `process`, returning the names of the files reported as saved.
    fn scan(duplex: &mut Duplex, bed: &Rgb, backs: bool) -> Vec<String> {
        let mut saved = Vec::new();
        let n = duplex
            .process(bed, DPI, GRID, backs, &mut |event| {
                if let Event::Saved { path, image } = event {
                    assert!(path.is_file(), "{} reported before it was written", path.display());
                    assert!(image.width > 0 && image.height > 0);
                    saved.push(path.file_name().unwrap().to_string_lossy().into_owned());
                }
            })
            .unwrap();
        assert_eq!(n, saved.len());
        saved
    }

    fn files(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> =
            std::fs::read_dir(dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        names.sort();
        names
    }

    #[test]
    fn a_lone_front_scan_is_saved_and_reported_at_once() {
        let dir = scratch("lone");
        let mut bed = mat();
        front(&mut bed, SPOTS[0]);
        front(&mut bed, SPOTS[1]);
        let mut duplex = Duplex::new(dir.clone());

        let saved = scan(&mut duplex, &bed, false);
        // No back scan, no further scan: the fronts are already on disk under their final
        // names, as if the app quit right here.
        assert_eq!(saved, ["photo-0001-front.png", "photo-0002-front.png"]);
        assert_eq!(files(&dir), saved);
        let decoder = png::Decoder::new(std::io::BufReader::new(File::open(dir.join(&saved[0])).unwrap()));
        let info = decoder.read_info().unwrap();
        assert!(info.info().width < bed.width / 2, "cut out, not the whole bed");
        assert_eq!(duplex.pending_numbers().as_deref(), Some("photos 0001-0002"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_later_back_scan_pairs_with_the_saved_fronts() {
        let dir = scratch("backs");
        let mut bed = mat();
        for spot in SPOTS {
            front(&mut bed, spot);
        }
        let mut duplex = Duplex::new(dir.clone());
        assert_eq!(scan(&mut duplex, &bed, false).len(), 3);

        // The first and last were flipped over in place, the middle one left face down.
        let mut flipped = mat();
        back(&mut flipped, SPOTS[0]);
        front(&mut flipped, SPOTS[1]);
        back(&mut flipped, SPOTS[2]);
        let mut saved = scan(&mut duplex, &flipped, true);
        saved.sort();
        assert_eq!(saved, ["photo-0001-back.png", "photo-0003-back.png"]);
        assert!(!duplex.has_pending(), "the batch is done");
        assert_eq!(files(&dir).len(), 5);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn single_sided_batches_number_on() {
        let dir = scratch("batches");
        let mut duplex = Duplex::new(dir.clone());
        let mut first = mat();
        front(&mut first, SPOTS[0]);
        front(&mut first, SPOTS[2]);
        assert_eq!(scan(&mut duplex, &first, false), ["photo-0001-front.png", "photo-0002-front.png"]);

        let mut second = mat();
        front(&mut second, SPOTS[1]);
        assert_eq!(scan(&mut duplex, &second, false), ["photo-0003-front.png"]);
        // Backs go with the most recent batch only.
        assert_eq!(duplex.pending_numbers().as_deref(), Some("photo 0003"));

        // A new session (app restarted) carries on from the files on disk.
        let mut duplex = Duplex::new(dir.clone());
        assert_eq!(scan(&mut duplex, &first, false), ["photo-0004-front.png", "photo-0005-front.png"]);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_back_scan_without_fronts_saves_fronts() {
        let dir = scratch("nofronts");
        let mut bed = mat();
        front(&mut bed, SPOTS[0]);
        assert_eq!(scan(&mut Duplex::new(dir.clone()), &bed, true), ["photo-0001-front.png"]);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
