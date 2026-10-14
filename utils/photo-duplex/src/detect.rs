//! Finding photos on the white document mat, and pairing the photos of a back scan with
//! those of the front scan.
//!
//! Everything here runs on a coarse grid (`DETECT_DPI`); the caller scales the resulting
//! rectangles up to the full-resolution scan.

/// Resolution of all detection work.
pub const DETECT_DPI: u32 = 50;

/// How much darker than the mat (0-255, darkest channel) a pixel must be to count as photo.
const INK_THRESHOLD: u8 = 16;
/// Looser threshold for "anything but bare mat", used to tell a flipped photo whose back
/// has no clear outline from an empty spot.
const PRESENT_THRESHOLD: u8 = 4;
/// Share of a front's area that must be non-mat for its back to count as present.
const PRESENT_FRACTION: f64 = 0.5;
/// How far (0-255, any channel) a pixel must move between two scans to count as changed.
const CHANGE_THRESHOLD: u8 = 24;
/// Share of a photo's area that must change for it to count as flipped over.
const CHANGED_FRACTION: f64 = 0.1;
/// Width in pixels of the strip along the bed's edge that detection ignores.
pub(crate) const EDGE_BAND: usize = 2;
/// Blobs no bigger than this in both directions are dust, removed before anything else.
pub(crate) const MAX_SPECK_MM: f64 = 3.0;
/// Photos narrower than this in either direction are treated as noise (dust, hairs, edges).
const MIN_SIDE_MM: f64 = 20.0;
/// A back must overlap its front by at least this intersection-over-union to be paired.
const MIN_MATCH_IOU: f64 = 0.3;

pub struct Rgb {
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>,
}

impl Rgb {
    fn px(&self, x: u32, y: u32) -> &[u8] {
        let i = (y as usize * self.width as usize + x as usize) * 3;
        &self.data[i..i + 3]
    }

    /// Copies out `r`, clipped to the image.
    pub fn crop(&self, r: Rect) -> Rgb {
        let x1 = (r.x + r.w).min(self.width);
        let y1 = (r.y + r.h).min(self.height);
        let (x0, y0) = (r.x.min(x1), r.y.min(y1));
        let row = self.width as usize * 3;
        let mut data = Vec::with_capacity((x1 - x0) as usize * (y1 - y0) as usize * 3);
        for y in y0..y1 {
            let start = y as usize * row + x0 as usize * 3;
            data.extend_from_slice(&self.data[start..start + (x1 - x0) as usize * 3]);
        }
        Rgb { width: x1 - x0, height: y1 - y0, data }
    }

    /// Box-filters a scan taken at `src_dpi` onto a `width` x `height` grid at `dst_dpi`.
    /// Both are anchored at the top-left of the bed, so the grids line up physically.
    pub fn downsample(&self, src_dpi: u32, dst_dpi: u32, width: u32, height: u32) -> Rgb {
        let span = |i: u32, limit: u32| {
            let a = (i as u64 * src_dpi as u64 / dst_dpi as u64) as u32;
            let b = ((i as u64 + 1) * src_dpi as u64 / dst_dpi as u64) as u32;
            (a.min(limit), b.max(a + 1).min(limit))
        };
        let mut data = Vec::with_capacity(width as usize * height as usize * 3);
        for y in 0..height {
            let (y0, y1) = span(y, self.height);
            for x in 0..width {
                let (x0, x1) = span(x, self.width);
                let mut sum = [0u64; 3];
                for sy in y0..y1 {
                    for sx in x0..x1 {
                        for (s, v) in sum.iter_mut().zip(self.px(sx, sy)) {
                            *s += *v as u64;
                        }
                    }
                }
                let n = ((y1 - y0) as u64 * (x1 - x0) as u64).max(1);
                data.extend(sum.iter().map(|s| (s / n) as u8));
            }
        }
        Rgb { width, height, data }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

impl Rect {
    fn area(&self) -> u64 {
        self.w as u64 * self.h as u64
    }

    fn iou(&self, o: &Rect) -> f64 {
        let ix = (self.x + self.w).min(o.x + o.w).saturating_sub(self.x.max(o.x));
        let iy = (self.y + self.h).min(o.y + o.h).saturating_sub(self.y.max(o.y));
        let inter = ix as u64 * iy as u64;
        inter as f64 / (self.area() + o.area() - inter) as f64
    }

    /// Maps a detection-grid rectangle onto a scan at `dpi`, growing it by one grid cell on
    /// every side so that rounding never trims the photo's edge.
    pub fn to_dpi(self, dpi: u32) -> Rect {
        let s = |v: u32| (v as u64 * dpi as u64 / DETECT_DPI as u64) as u32;
        let (x0, y0) = (self.x.saturating_sub(1), self.y.saturating_sub(1));
        Rect { x: s(x0), y: s(y0), w: s(self.x + self.w + 1) - s(x0), h: s(self.y + self.h + 1) - s(y0) }
    }

    /// Grows the rectangle by `mm` on every side, clipped to a `width` x `height` grid.
    pub fn grow(self, mm: f64, width: u32, height: u32) -> Rect {
        let d = (mm / 25.4 * DETECT_DPI as f64).round() as u32;
        let (x0, y0) = (self.x.saturating_sub(d), self.y.saturating_sub(d));
        let (x1, y1) = ((self.x + self.w + d).min(width), (self.y + self.h + d).min(height));
        Rect { x: x0, y: y0, w: x1 - x0, h: y1 - y0 }
    }

    /// Top-left corner on the bed in millimetres, for messages.
    pub fn describe_position(&self) -> String {
        let mm = |v: u32| v as f64 * 25.4 / DETECT_DPI as f64;
        format!("({:.0}, {:.0}) mm", mm(self.x), mm(self.y))
    }

    /// Position and size in millimetres, for messages.
    pub fn describe(&self) -> String {
        let mm = |v: u32| v as f64 * 25.4 / DETECT_DPI as f64;
        format!("{:.0}x{:.0} mm at ({:.0}, {:.0})", mm(self.w), mm(self.h), mm(self.x), mm(self.y))
    }
}

/// Finds the photos in `bed` (a scan downsampled to `DETECT_DPI`), in reading order:
/// top to bottom by row, left to right within a row.
pub fn find_photos(bed: &Rgb) -> Vec<Rect> {
    let (w, h) = (bed.width as usize, bed.height as usize);
    let max_speck = (MAX_SPECK_MM / 25.4 * DETECT_DPI as f64) as usize;
    let mask = photo_mask(&ink(bed, mat_level(bed)), w, h, [EDGE_BAND; 4], 3, max_speck);

    let min_side = (MIN_SIDE_MM / 25.4 * DETECT_DPI as f64) as u32;
    let mut photos: Vec<Rect> = components(&mask, w, h)
        .into_iter()
        .filter(|r| r.w >= min_side && r.h >= min_side)
        .map(|r| snap_to_edges(r, w as u32, h as u32))
        .collect();

    // Group into rows: a photo whose centre lies above the bottom of the current row joins it.
    photos.sort_by_key(|r| r.y + r.h / 2);
    let mut rows: Vec<Vec<Rect>> = Vec::new();
    for r in photos {
        match rows.last_mut() {
            Some(row) if r.y + r.h / 2 < row.iter().map(|o| o.y + o.h).max().unwrap() => row.push(r),
            _ => rows.push(vec![r]),
        }
    }
    rows.into_iter()
        .flat_map(|mut row| {
            row.sort_by_key(|r| r.x);
            row
        })
        .collect()
}

/// Whether the bed inside `r` is mostly something other than bare mat.
pub fn occupied(bed: &Rgb, r: Rect) -> bool {
    let ink = ink(bed, mat_level(bed));
    let (x1, y1) = ((r.x + r.w).min(bed.width), (r.y + r.h).min(bed.height));
    let (mut total, mut covered) = (0u64, 0u64);
    for y in r.y..y1 {
        for x in r.x..x1 {
            total += 1;
            covered += (ink[(y * bed.width + x) as usize] > PRESENT_THRESHOLD) as u64;
        }
    }
    total > 0 && covered as f64 >= total as f64 * PRESENT_FRACTION
}

/// Whether the spot `r` looks the same in `before` and `after` (two scans on the same
/// grid), i.e. the photo there was left as it was rather than flipped over. A one-cell
/// strip around the edge is ignored so that a slight shift of the outline doesn't count.
pub fn unchanged(before: &Rgb, after: &Rgb, r: Rect) -> bool {
    let (x0, y0) = (r.x + 1, r.y + 1);
    let x1 = (r.x + r.w).saturating_sub(1).min(before.width).min(after.width);
    let y1 = (r.y + r.h).saturating_sub(1).min(before.height).min(after.height);
    let (mut total, mut differ) = (0u64, 0u64);
    for y in y0..y1 {
        for x in x0..x1 {
            total += 1;
            let d = before.px(x, y).iter().zip(after.px(x, y)).map(|(a, b)| a.abs_diff(*b)).max().unwrap();
            differ += (d > CHANGE_THRESHOLD) as u64;
        }
    }
    total > 0 && (differ as f64) < total as f64 * CHANGED_FRACTION
}

/// The mat's level (darkest channel, 0-255), taken from the scan itself: the 99th
/// percentile, since dust and photos only ever make pixels darker.
pub fn mat_level(bed: &Rgb) -> u8 {
    let mut histogram = [0usize; 256];
    for p in bed.data.as_chunks::<3>().0 {
        histogram[p[0].min(p[1]).min(p[2]) as usize] += 1;
    }
    let total = bed.data.len() / 3;
    let mut seen = 0;
    for (v, &n) in histogram.iter().enumerate() {
        seen += n;
        if seen * 100 >= total * 99 {
            return v as u8;
        }
    }
    255
}

/// Per pixel, how much darker its darkest channel is than the mat.
pub(crate) fn ink(bed: &Rgb, mat: u8) -> Vec<u8> {
    bed.data.as_chunks::<3>().0.iter().map(|p| mat.saturating_sub(p[0].min(p[1]).min(p[2]))).collect()
}

/// Turns `ink` into a mask of solid photo areas.
///
/// `edge_band` (left, top, right, bottom) is how many pixels to ignore along each side:
/// the outermost pixels of the bed see its frame, which reads darker than the mat, and a
/// photo pushed against the edge must not merge with that strip. Blobs no larger than
/// `max_speck` in both directions are dust. `close` is the radius used to bridge gaps in
/// faint outlines.
pub(crate) fn photo_mask(ink: &[u8], w: usize, h: usize, edge_band: [usize; 4], close: usize, max_speck: usize) -> Vec<bool> {
    let [left, top, right, bottom] = edge_band;
    let inside = |x: usize, y: usize| x >= left && y >= top && x + right < w && y + bottom < h;
    let mut mask: Vec<bool> = ink.iter().enumerate().map(|(i, &v)| v > INK_THRESHOLD && inside(i % w, i / w)).collect();

    // Remove specks first so that closing can't bridge them onto a nearby photo. This is
    // done by blob size rather than by morphological opening, which would also erase the
    // one-pixel shadow outlines.
    let max_speck = max_speck as u32;
    for r in components(&mask, w, h).into_iter().filter(|r| r.w <= max_speck && r.h <= max_speck) {
        for y in r.y..r.y + r.h {
            let row = y as usize * w;
            mask[row + r.x as usize..row + (r.x + r.w) as usize].fill(false);
        }
    }

    // Bridge small gaps in outlines, then fill everything an outline encloses: white
    // borders and pale areas are part of the photo even though they look like the mat.
    fill_holes(&closing(&mask, w, h, close), w, h)
}

/// Morphological closing with everything outside the image counted as unset. Without the
/// padding, erosion never sees past the border and closing would stretch shapes that end
/// within `r` of it out to the border.
fn closing(mask: &[bool], w: usize, h: usize, r: usize) -> Vec<bool> {
    let (pw, ph) = (w + 2 * r, h + 2 * r);
    let mut padded = vec![false; pw * ph];
    for y in 0..h {
        padded[(y + r) * pw + r..(y + r) * pw + r + w].copy_from_slice(&mask[y * w..(y + 1) * w]);
    }
    let closed = erode(&dilate(&padded, pw, ph, r), pw, ph, r);
    (0..h).flat_map(|y| closed[(y + r) * pw + r..(y + r) * pw + r + w].iter().copied()).collect()
}

/// For each back, the index of the front it was flipped from, if any. Each front gets at
/// most one back; the pairs with the largest overlap win.
pub fn match_backs(fronts: &[Rect], backs: &[Rect]) -> Vec<Option<usize>> {
    let mut pairs: Vec<(f64, usize, usize)> = backs
        .iter()
        .enumerate()
        .flat_map(|(b, back)| fronts.iter().enumerate().map(move |(f, front)| (back.iou(front), b, f)))
        .filter(|(iou, _, _)| *iou >= MIN_MATCH_IOU)
        .collect();
    pairs.sort_by(|a, b| b.0.total_cmp(&a.0));

    let mut result = vec![None; backs.len()];
    let mut taken = vec![false; fronts.len()];
    for (_, b, f) in pairs {
        if result[b].is_none() && !taken[f] {
            result[b] = Some(f);
            taken[f] = true;
        }
    }
    result
}

/// Square min (erode) / max (dilate) filter of radius `r`, done as two 1-D passes.
fn morph(mask: &[bool], w: usize, h: usize, r: usize, grow: bool) -> Vec<bool> {
    let pass = |src: &[bool], horizontal: bool| {
        let mut out = vec![false; w * h];
        for y in 0..h {
            for x in 0..w {
                let (pos, len) = if horizontal { (x, w) } else { (y, h) };
                let mut window = (pos.saturating_sub(r)..(pos + r + 1).min(len))
                    .map(|i| if horizontal { src[y * w + i] } else { src[i * w + x] });
                out[y * w + x] = if grow { window.any(|v| v) } else { window.all(|v| v) };
            }
        }
        out
    };
    pass(&pass(mask, true), false)
}

pub(crate) fn erode(mask: &[bool], w: usize, h: usize, r: usize) -> Vec<bool> {
    morph(mask, w, h, r, false)
}

pub(crate) fn dilate(mask: &[bool], w: usize, h: usize, r: usize) -> Vec<bool> {
    morph(mask, w, h, r, true)
}

/// Extends sides that reach the ignored edge band out to the edge of the bed.
fn snap_to_edges(r: Rect, w: u32, h: u32) -> Rect {
    let band = EDGE_BAND as u32;
    let (mut x0, mut y0, mut x1, mut y1) = (r.x, r.y, r.x + r.w, r.y + r.h);
    if x0 <= band {
        x0 = 0;
    }
    if y0 <= band {
        y0 = 0;
    }
    if x1 + band >= w {
        x1 = w;
    }
    if y1 + band >= h {
        y1 = h;
    }
    Rect { x: x0, y: y0, w: x1 - x0, h: y1 - y0 }
}

/// Sets every unset pixel that can't reach the image border through unset pixels.
fn fill_holes(mask: &[bool], w: usize, h: usize) -> Vec<bool> {
    let mut outside = vec![false; w * h];
    let mut stack: Vec<usize> = (0..w * h).filter(|&i| i % w == 0 || i % w == w - 1 || i / w == 0 || i / w == h - 1).collect();
    stack.retain(|&i| !mask[i]);
    for &i in &stack {
        outside[i] = true;
    }
    while let Some(i) = stack.pop() {
        let (x, y) = (i % w, i / w);
        let neighbours = [(x > 0).then(|| i - 1), (x + 1 < w).then(|| i + 1), (y > 0).then(|| i - w), (y + 1 < h).then(|| i + w)];
        for j in neighbours.into_iter().flatten() {
            if !mask[j] && !outside[j] {
                outside[j] = true;
                stack.push(j);
            }
        }
    }
    outside.iter().map(|o| !o).collect()
}

/// Bounding boxes of the 8-connected regions of `mask`.
fn components(mask: &[bool], w: usize, h: usize) -> Vec<Rect> {
    label(mask, w, h).1
}

/// Labels the 8-connected regions of `mask`: per pixel the region number (0 = not in
/// `mask`, regions count from 1), and each region's bounding box at index `label - 1`.
pub(crate) fn label(mask: &[bool], w: usize, h: usize) -> (Vec<u32>, Vec<Rect>) {
    let mut labels = vec![0u32; w * h];
    let mut rects = Vec::new();
    let mut stack = Vec::new();
    for start in 0..w * h {
        if !mask[start] || labels[start] != 0 {
            continue;
        }
        let id = rects.len() as u32 + 1;
        labels[start] = id;
        stack.push(start);
        let (mut x0, mut y0, mut x1, mut y1) = (w, h, 0, 0);
        while let Some(i) = stack.pop() {
            let (x, y) = (i % w, i / w);
            (x0, y0, x1, y1) = (x0.min(x), y0.min(y), x1.max(x), y1.max(y));
            for ny in y.saturating_sub(1)..(y + 2).min(h) {
                for nx in x.saturating_sub(1)..(x + 2).min(w) {
                    let j = ny * w + nx;
                    if mask[j] && labels[j] == 0 {
                        labels[j] = id;
                        stack.push(j);
                    }
                }
            }
        }
        rects.push(Rect { x: x0 as u32, y: y0 as u32, w: (x1 - x0 + 1) as u32, h: (y1 - y0 + 1) as u32 });
    }
    (labels, rects)
}

#[cfg(test)]
mod tests {
    use super::*;

    const W: u32 = 424;
    const H: u32 = 585;

    /// The white mat as scanned: saturated, with a darker strip at the left edge of the
    /// bed and a few dust specks.
    fn mat() -> Rgb {
        let mut img = Rgb { width: W, height: H, data: vec![255; (W * H * 3) as usize] };
        for y in 0..H {
            set(&mut img, 0, y, [235, 235, 235]);
            set(&mut img, 1, y, [243, 243, 243]);
        }
        for (x, y) in [(224, 37), (290, 112), (237, 281), (411, 315)] {
            set(&mut img, x, y, [90, 90, 90]);
        }
        img
    }

    fn set(img: &mut Rgb, x: u32, y: u32, v: [u8; 3]) {
        let i = (y * img.width + x) as usize * 3;
        img.data[i..i + 3].copy_from_slice(&v);
    }

    /// A photo front with a white border and a faint shadow outline, so only the outline
    /// separates the border from the mat. Against the bed's edge there is no shadow.
    fn paint_front(img: &mut Rgb, r: Rect) {
        for y in r.y..r.y + r.h {
            for x in r.x..r.x + r.w {
                let edge = (x == r.x || y == r.y || x == r.x + r.w - 1 || y == r.y + r.h - 1) && x > 0 && y > 0;
                let border = x < r.x + 4 || y < r.y + 4 || x >= r.x + r.w - 4 || y >= r.y + r.h - 4;
                let v = if edge {
                    [200, 200, 200]
                } else if border {
                    [253, 253, 253]
                } else {
                    [(x * 7 % 200) as u8, 120, (y * 3 % 200) as u8]
                };
                set(img, x, y, v);
            }
        }
    }

    fn close_to(a: Rect, b: Rect) -> bool {
        a.x.abs_diff(b.x) <= 1 && a.y.abs_diff(b.y) <= 1 && a.w.abs_diff(b.w) <= 2 && a.h.abs_diff(b.h) <= 2
    }

    #[test]
    fn finds_photos_in_reading_order() {
        let mut bed = mat();
        let photos = [
            Rect { x: 230, y: 25, w: 150, h: 100 }, // top right, slightly lower than top left
            Rect { x: 0, y: 0, w: 150, h: 110 },    // pushed into the top-left corner
            Rect { x: 60, y: 300, w: 100, h: 150 }, // second row
        ];
        for p in photos {
            paint_front(&mut bed, p);
        }

        let found = find_photos(&bed);
        assert_eq!(found.len(), 3, "{found:?}");
        assert!(close_to(found[0], photos[1]), "{found:?}");
        assert!(close_to(found[1], photos[0]), "{found:?}");
        assert!(close_to(found[2], photos[2]), "{found:?}");
    }

    #[test]
    fn empty_mat_has_no_photos() {
        assert!(find_photos(&mat()).is_empty());
    }

    #[test]
    fn plain_paper_back_counts_as_present() {
        let mut bed = mat();
        let back = Rect { x: 50, y: 50, w: 150, h: 100 };
        for y in back.y..back.y + back.h {
            for x in back.x..back.x + back.w {
                set(&mut bed, x, y, [246, 246, 244]);
            }
        }
        assert!(occupied(&bed, back));
        assert!(!occupied(&bed, Rect { x: 220, y: 300, w: 150, h: 100 }));
    }

    #[test]
    fn a_photo_left_face_down_is_unchanged() {
        let a = Rect { x: 20, y: 20, w: 150, h: 100 };
        let b = Rect { x: 230, y: 25, w: 150, h: 100 };
        let mut before = mat();
        paint_front(&mut before, a);
        paint_front(&mut before, b);
        // `a` stays put (with a little sensor noise); `b` is flipped to a plain back.
        let mut after = mat();
        paint_front(&mut after, a);
        for (i, v) in after.data.iter_mut().enumerate() {
            *v = v.saturating_sub((i % 5) as u8);
        }
        for y in b.y..b.y + b.h {
            for x in b.x..b.x + b.w {
                set(&mut after, x, y, [246, 246, 244]);
            }
        }
        assert!(unchanged(&before, &after, a));
        assert!(!unchanged(&before, &after, b));
    }

    #[test]
    fn backs_pair_with_the_overlapping_front() {
        let fronts = [
            Rect { x: 20, y: 20, w: 150, h: 100 },
            Rect { x: 230, y: 25, w: 150, h: 100 },
            Rect { x: 60, y: 300, w: 100, h: 150 },
        ];
        // The middle photo was removed; the others were flipped and nudged a little.
        let backs = [Rect { x: 75, y: 290, w: 100, h: 150 }, Rect { x: 12, y: 30, w: 150, h: 100 }];
        assert_eq!(match_backs(&fronts, &backs), vec![Some(2), Some(0)]);
    }

    #[test]
    fn a_back_far_from_every_front_is_unmatched() {
        let fronts = [Rect { x: 20, y: 20, w: 150, h: 100 }];
        let backs = [Rect { x: 200, y: 400, w: 150, h: 100 }];
        assert_eq!(match_backs(&fronts, &backs), vec![None]);
    }

    #[test]
    fn downsample_averages_and_rect_maps_back_up() {
        // 4x2 grey image: 10 20 30 40 / 50 60 70 80.
        let data = [10u8, 20, 30, 40, 50, 60, 70, 80].iter().flat_map(|v| [*v; 3]).collect();
        let img = Rgb { width: 4, height: 2, data };
        let small = img.downsample(100, 50, 2, 1);
        assert_eq!(small.data, vec![35, 35, 35, 55, 55, 55]);

        let r = Rect { x: 10, y: 10, w: 20, h: 30 }.to_dpi(600);
        assert_eq!(r, Rect { x: 108, y: 108, w: 264, h: 384 });
    }
}
