//! Locating a photo precisely (centre, size and tilt) and cutting it out upright with a
//! thin strip of mat around it, so every edge of the photo is visibly in the crop.

use std::f64::consts::FRAC_PI_4;

use crate::detect::{self, DETECT_DPI, EDGE_BAND, MAX_SPECK_MM, Rect, Rgb};

/// Resolution of the outline search; ~0.17 mm per pixel.
const FINE_DPI: u32 = 150;
/// How far around the coarse detection to look for the photo's edges.
const SEARCH_MM: f64 = 3.0;
/// Mat kept on every side of the photo.
pub const MARGIN_MM: f64 = 1.5;
/// Below this tilt the photo is cropped as-is rather than resampled.
const MIN_ROTATION_DEG: f64 = 0.1;
/// Fill for parts of the crop beyond the scanned area. Deliberately not white, so it
/// can't be mistaken for mat around a photo that was in fact cut off.
const OUTSIDE: [u8; 3] = [128, 128, 128];
/// Darkness below the mat (0-255) of photo content, as opposed to the soft shadows that
/// the bed's frame and a card's own thickness cast along the edge of the scan (~20-35).
const CLIP_INK: u8 = 64;
/// Share of the scan's outermost strip, alongside a photo, that must be photo content for
/// the photo to count as running off the scan.
const CLIP_FRACTION: f64 = 0.25;
/// Tilt resolution of the Hough search.
const HOUGH_STEP_DEG: f64 = 0.05;
/// Edges this close to the outline, on either side, vote in the Hough search.
const OUTLINE_BAND_MM: f64 = 1.5;
/// Weakest edge (Sobel magnitude of the darkest channel) that votes.
const MIN_EDGE: f64 = 40.0;
/// An edge votes only for lines within this of the direction its gradient gives.
const VOTE_SPREAD_DEG: f64 = 8.0;

/// A photo's exact footprint in full-resolution pixels. `w` runs along the direction
/// `angle` (radians, clockwise on screen since y points down), `h` across it.
#[derive(Debug, Clone, Copy)]
pub struct Placement {
    pub cx: f64,
    pub cy: f64,
    pub w: f64,
    pub h: f64,
    pub angle: f64,
    /// The photo runs off the edge of the scanned area, so part of it is missing.
    pub clipped: bool,
}

impl Placement {
    pub fn describe(&self, dpi: u32) -> String {
        let mm = |v: f64| v * 25.4 / dpi as f64;
        format!("{:.1}x{:.1} mm, tilted {:.1}°", mm(self.w), mm(self.h), self.angle.to_degrees())
    }
}

/// Finds the exact footprint of the photo that coarse detection put at `coarse` (in
/// `DETECT_DPI` grid cells). `mat` is the mat level from `detect::mat_level`.
pub fn locate(bed: &Rgb, dpi: u32, mat: u8, coarse: Rect) -> Option<Placement> {
    let grid_w = (bed.width as u64 * DETECT_DPI as u64 / dpi as u64) as u32;
    let grid_h = (bed.height as u64 * DETECT_DPI as u64 / dpi as u64) as u32;
    let region = coarse.grow(SEARCH_MM, grid_w, grid_h).to_dpi(dpi);
    let sub = bed.crop(region);
    let (fw, fh) = (sub.width * FINE_DPI / dpi, sub.height * FINE_DPI / dpi);
    if fw < 3 || fh < 3 {
        return None;
    }
    let fine = sub.downsample(dpi, FINE_DPI, fw, fh);

    // Ignore the bed's frame only on the sides where this region reaches the bed's edge.
    let band = EDGE_BAND * (FINE_DPI / DETECT_DPI) as usize;
    let at_edge = [region.x == 0, region.y == 0, region.x + sub.width >= bed.width, region.y + sub.height >= bed.height];
    let edge_band = at_edge.map(|e| if e { band } else { 0 });
    let max_speck = (MAX_SPECK_MM / 25.4 * FINE_DPI as f64) as usize;
    let close = (FINE_DPI / DETECT_DPI) as usize * 3;
    let (w, h) = (fw as usize, fh as usize);
    let mask = detect::photo_mask(&detect::ink(&fine, mat), w, h, edge_band, close, max_speck);

    // The photo is the biggest region; neighbours only reach a few mm into the search area.
    let (labels, rects) = detect::label(&mask, w, h);
    let mut sizes = vec![0usize; rects.len() + 1];
    for &l in &labels {
        sizes[l as usize] += 1;
    }
    let best = (1..sizes.len()).max_by_key(|&l| sizes[l])? as u32;

    // The outline's corner points: each row's leftmost and rightmost pixel edges.
    let mut points = Vec::new();
    for y in 0..h {
        let row = &labels[y * w..(y + 1) * w];
        if let (Some(x0), Some(x1)) = (row.iter().position(|&l| l == best), row.iter().rposition(|&l| l == best)) {
            for (px, py) in [(x0, y), (x0, y + 1), (x1 + 1, y), (x1 + 1, y + 1)] {
                points.push((px as f64, py as f64));
            }
        }
    }
    let hull = convex_hull(points);
    // The tilt comes from the photo's straight edges, so a bump on the outline (a page
    // sticking out, a bent corner, a shadow) can't twist it the way it twists the
    // smallest enclosing rectangle. The extent still covers the whole outline.
    let photo: Vec<bool> = labels.iter().map(|&l| l == best).collect();
    let p = match hough_tilt(&fine, &photo, edge_band) {
        Some(angle) => rect_at_angle(&hull, angle),
        None => min_area_rect(&hull)?,
    };

    let scale = dpi as f64 / FINE_DPI as f64;
    let mut p = Placement {
        cx: region.x as f64 + p.cx * scale,
        cy: region.y as f64 + p.cy * scale,
        w: p.w * scale,
        h: p.h * scale,
        angle: p.angle,
        clipped: false,
    };
    p.clipped = runs_off_scan(bed, dpi, mat, &p);
    Some(p)
}

/// Whether the photo continues past the edge of the scan: along each side of the bed the
/// photo comes close to, the outermost pixels beside it are mostly photo content rather
/// than mat or shadow.
fn runs_off_scan(bed: &Rgb, dpi: u32, mat: u8, p: &Placement) -> bool {
    let (sin, cos) = p.angle.sin_cos();
    let corners = [(-1.0, -1.0), (1.0, -1.0), (-1.0, 1.0), (1.0, 1.0)]
        .map(|(sx, sy): (f64, f64)| (p.cx + sx * p.w / 2.0 * cos - sy * p.h / 2.0 * sin, p.cy + sx * p.w / 2.0 * sin + sy * p.h / 2.0 * cos));
    let (w, h) = (bed.width as f64, bed.height as f64);
    let clamp = |v: f64, max: f64| v.clamp(0.0, max - 1.0) as u32;
    let x0 = clamp(corners.iter().map(|c| c.0).fold(f64::MAX, f64::min), w);
    let x1 = clamp(corners.iter().map(|c| c.0).fold(f64::MIN, f64::max), w);
    let y0 = clamp(corners.iter().map(|c| c.1).fold(f64::MAX, f64::min), h);
    let y1 = clamp(corners.iter().map(|c| c.1).fold(f64::MIN, f64::max), h);

    // Photos that come within twice the frame strip of a side are checked against it.
    let near = 2 * EDGE_BAND as u32 * dpi / DETECT_DPI;
    let photo_like = |pixels: &mut dyn Iterator<Item = (u32, u32)>| {
        let (mut n, mut dark) = (0usize, 0usize);
        for (x, y) in pixels {
            let i = (y as usize * bed.width as usize + x as usize) * 3;
            let darkest = bed.data[i].min(bed.data[i + 1]).min(bed.data[i + 2]);
            n += 1;
            dark += (mat.saturating_sub(darkest) > CLIP_INK) as usize;
        }
        n > 0 && dark as f64 >= n as f64 * CLIP_FRACTION
    };
    (x0 < near && photo_like(&mut (y0..=y1).map(|y| (0, y))))
        || (y0 < near && photo_like(&mut (x0..=x1).map(|x| (x, 0))))
        || (x1 + near >= bed.width && photo_like(&mut (y0..=y1).map(|y| (bed.width - 1, y))))
        || (y1 + near >= bed.height && photo_like(&mut (x0..=x1).map(|x| (x, bed.height - 1))))
}

/// Cuts the photo out upright, with `MARGIN_MM` of mat on every side. Anything beyond
/// the scanned area is filled with grey (`OUTSIDE`).
pub fn extract(bed: &Rgb, p: &Placement, dpi: u32) -> Rgb {
    let margin = MARGIN_MM / 25.4 * dpi as f64;
    let (out_w, out_h) = ((p.w + 2.0 * margin).round() as u32, (p.h + 2.0 * margin).round() as u32);
    let (sin, cos) = p.angle.sin_cos();

    if p.angle.to_degrees().abs() < MIN_ROTATION_DEG {
        // Straight enough: copy pixels, no resampling.
        let x0 = (p.cx - out_w as f64 / 2.0).round() as i64;
        let y0 = (p.cy - out_h as f64 / 2.0).round() as i64;
        let mut data = Vec::with_capacity(out_w as usize * out_h as usize * 3);
        for y in y0..y0 + out_h as i64 {
            for x in x0..x0 + out_w as i64 {
                data.extend_from_slice(&pixel(bed, x, y));
            }
        }
        return Rgb { width: out_w, height: out_h, data };
    }

    let mut data = Vec::with_capacity(out_w as usize * out_h as usize * 3);
    for v in 0..out_h {
        let dy = v as f64 + 0.5 - out_h as f64 / 2.0;
        for u in 0..out_w {
            let dx = u as f64 + 0.5 - out_w as f64 / 2.0;
            // Pixel centres sit at +0.5, so shift back to index space.
            let sx = p.cx + dx * cos - dy * sin - 0.5;
            let sy = p.cy + dx * sin + dy * cos - 0.5;
            data.extend_from_slice(&bicubic(bed, sx, sy));
        }
    }
    Rgb { width: out_w, height: out_h, data }
}

fn pixel(img: &Rgb, x: i64, y: i64) -> [u8; 3] {
    if x < 0 || y < 0 || x >= img.width as i64 || y >= img.height as i64 {
        return OUTSIDE;
    }
    let i = (y as usize * img.width as usize + x as usize) * 3;
    [img.data[i], img.data[i + 1], img.data[i + 2]]
}

/// Catmull-Rom interpolation at (x, y) in pixel-index coordinates.
fn bicubic(img: &Rgb, x: f64, y: f64) -> [u8; 3] {
    let weights = |t: f64| {
        let (t2, t3) = (t * t, t * t * t);
        [(-t3 + 2.0 * t2 - t) / 2.0, (3.0 * t3 - 5.0 * t2 + 2.0) / 2.0, (-3.0 * t3 + 4.0 * t2 + t) / 2.0, (t3 - t2) / 2.0]
    };
    let (x0, y0) = (x.floor(), y.floor());
    let (wx, wy) = (weights(x - x0), weights(y - y0));
    let (x0, y0) = (x0 as i64, y0 as i64);
    let mut sum = [0.0f64; 3];
    for (j, wyj) in wy.iter().enumerate() {
        for (i, wxi) in wx.iter().enumerate() {
            let px = pixel(img, x0 + i as i64 - 1, y0 + j as i64 - 1);
            for c in 0..3 {
                sum[c] += wxi * wyj * px[c] as f64;
            }
        }
    }
    sum.map(|v| v.round().clamp(0.0, 255.0) as u8)
}

/// The tilt (radians, in (-45°, 45°]) of the rectangle whose four sides best match the
/// straight edges along the outline of `photo`, a mask over `img`: a Hough transform in
/// which each side contributes its strongest line. Edges vote by contrast, so the photo's
/// real edges outweigh a faint shadow or a strip of paper beyond them. Pixels within
/// `ignore` (left, top, right, bottom) of the image's border, where the bed's frame is,
/// don't vote. `None` if there are no edges to go on.
fn hough_tilt(img: &Rgb, photo: &[bool], ignore: [usize; 4]) -> Option<f64> {
    let (w, h) = (img.width as usize, img.height as usize);
    let r = (OUTLINE_BAND_MM / 25.4 * FINE_DPI as f64).round() as usize;
    let (outer, inner) = (detect::dilate(photo, w, h, r), detect::erode(photo, w, h, r));
    // Smoothed first, so the gradient along a jagged (pixel-stepped) edge points the way
    // the edge runs rather than the way each step does.
    let dark = blur(&img.data.as_chunks::<3>().0.iter().map(|p| p[0].min(p[1]).min(p[2]) as f64).collect::<Vec<_>>(), w, h);
    let [left, top, right, bottom] = ignore.map(|b| b + 1);

    // Edge pixels as (x, y, strength, angle bin of the gradient's direction modulo 90°).
    // Modulo 90°, all four sides of a rectangle tilted by θ point the same way.
    let bins = (90.0 / HOUGH_STEP_DEG).round() as usize;
    let mut edges = Vec::new();
    for y in top.max(1)..h.saturating_sub(bottom.max(1)) {
        for x in left.max(1)..w.saturating_sub(right.max(1)) {
            let i = y * w + x;
            if !outer[i] || inner[i] {
                continue;
            }
            let at = |dx: isize, dy: isize| dark[(y as isize + dy) as usize * w + (x as isize + dx) as usize];
            let gx = at(1, -1) + 2.0 * at(1, 0) + at(1, 1) - at(-1, -1) - 2.0 * at(-1, 0) - at(-1, 1);
            let gy = at(-1, 1) + 2.0 * at(0, 1) + at(1, 1) - at(-1, -1) - 2.0 * at(0, -1) - at(1, -1);
            let strength = gx.hypot(gy);
            if strength < MIN_EDGE {
                continue;
            }
            let bin = (gy.atan2(gx).to_degrees().rem_euclid(90.0) / HOUGH_STEP_DEG).round() as usize % bins;
            edges.push((x as f64, y as f64, strength, bin));
        }
    }
    if edges.is_empty() {
        return None;
    }
    let mut by_bin = vec![Vec::new(); bins];
    for (i, e) in edges.iter().enumerate() {
        by_bin[e.3].push(i);
    }
    let (n, mut sx, mut sy) = (photo.iter().filter(|&&p| p).count() as f64, 0.0, 0.0);
    for (i, _) in photo.iter().enumerate().filter(|(_, p)| **p) {
        (sx, sy) = (sx + (i % w) as f64, sy + (i / w) as f64);
    }
    let (cx, cy) = (sx / n, sy / n);

    // For each tilt, project the edges that point that way onto the normals of both pairs
    // of sides. Each side shows up as a peak on its side of the centre; the tilt's score
    // is the sum of the four peaks.
    let spread = (VOTE_SPREAD_DEG / HOUGH_STEP_DEG).round() as isize;
    let span = w + h + 2;
    let mut hist = vec![0.0f64; 2 * span];
    let mut scores = vec![0.0f64; bins];
    for (bin, score) in scores.iter_mut().enumerate() {
        let theta = (bin as f64 * HOUGH_STEP_DEG).to_radians();
        let normals = [theta.sin_cos(), (theta + std::f64::consts::FRAC_PI_2).sin_cos()];
        hist.fill(0.0);
        for d in -spread..=spread {
            for &i in &by_bin[(bin as isize + d).rem_euclid(bins as isize) as usize] {
                let (x, y, strength, _) = edges[i];
                // Which pair of sides: whichever normal the edge lies closer to.
                let k = ((x - cx) * normals[1].1 + (y - cy) * normals[1].0).abs()
                    > ((x - cx) * normals[0].1 + (y - cy) * normals[0].0).abs();
                let (sin, cos) = normals[k as usize];
                let rho = ((x - cx) * cos + (y - cy) * sin).round() as isize + (span / 2) as isize;
                hist[k as usize * span + rho.clamp(0, span as isize - 1) as usize] += strength;
            }
        }
        *score = hist
            .chunks(span)
            .flat_map(|side| {
                let peak = |range: std::ops::Range<usize>| {
                    range.map(|j| side[j.saturating_sub(1)] + side[j] + side[(j + 1).min(span - 1)]).fold(0.0, f64::max)
                };
                [peak(0..span / 2), peak(span / 2..span)]
            })
            .sum();
    }
    let (best, _) = scores.iter().enumerate().max_by(|a, b| a.1.total_cmp(b.1))?;
    // Refine between bins with a parabola through the peak and its neighbours.
    let (l, c, r) = (scores[(best + bins - 1) % bins], scores[best], scores[(best + 1) % bins]);
    let offset = if l + r - 2.0 * c != 0.0 { (0.5 * (l - r) / (l + r - 2.0 * c)).clamp(-0.5, 0.5) } else { 0.0 };
    let deg = (best as f64 + offset) * HOUGH_STEP_DEG;
    Some(if deg > 45.0 { deg - 90.0 } else { deg }.to_radians())
}

/// Two passes of a [1 2 1] / 4 filter in each direction (roughly a Gaussian, σ ≈ 1 px).
fn blur(v: &[f64], w: usize, h: usize) -> Vec<f64> {
    let pass = |src: &[f64], horizontal: bool| {
        let mut out = vec![0.0; w * h];
        for y in 0..h {
            for x in 0..w {
                let at = |d: isize| {
                    let (x, y) = if horizontal {
                        ((x as isize + d).clamp(0, w as isize - 1) as usize, y)
                    } else {
                        (x, (y as isize + d).clamp(0, h as isize - 1) as usize)
                    };
                    src[y * w + x]
                };
                out[y * w + x] = (at(-1) + 2.0 * at(0) + at(1)) / 4.0;
            }
        }
        out
    };
    let mut v = v.to_vec();
    for _ in 0..2 {
        v = pass(&pass(&v, true), false);
    }
    v
}

/// The rectangle tilted by `angle` (in (-45°, 45°]) that just encloses `hull`.
fn rect_at_angle(hull: &[(f64, f64)], angle: f64) -> Placement {
    let (sin, cos) = angle.sin_cos();
    let (mut u0, mut u1, mut v0, mut v1) = (f64::MAX, f64::MIN, f64::MAX, f64::MIN);
    for p in hull {
        let (pu, pv) = (p.0 * cos + p.1 * sin, -p.0 * sin + p.1 * cos);
        (u0, u1, v0, v1) = (u0.min(pu), u1.max(pu), v0.min(pv), v1.max(pv));
    }
    let (mu, mv) = ((u0 + u1) / 2.0, (v0 + v1) / 2.0);
    Placement { cx: mu * cos - mv * sin, cy: mu * sin + mv * cos, w: u1 - u0, h: v1 - v0, angle, clipped: false }
}

/// Andrew's monotone chain; returns the hull counter-clockwise without repeating the start.
fn convex_hull(mut points: Vec<(f64, f64)>) -> Vec<(f64, f64)> {
    points.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.total_cmp(&b.1)));
    points.dedup();
    if points.len() < 3 {
        return points;
    }
    let cross = |o: (f64, f64), a: (f64, f64), b: (f64, f64)| (a.0 - o.0) * (b.1 - o.1) - (a.1 - o.1) * (b.0 - o.0);
    let mut hull: Vec<(f64, f64)> = Vec::new();
    for pass in 0..2 {
        let start = hull.len();
        let iter: Box<dyn Iterator<Item = &(f64, f64)>> =
            if pass == 0 { Box::new(points.iter()) } else { Box::new(points.iter().rev()) };
        for &p in iter {
            while hull.len() >= start + 2 && cross(hull[hull.len() - 2], hull[hull.len() - 1], p) <= 0.0 {
                hull.pop();
            }
            hull.push(p);
        }
        hull.pop();
    }
    hull
}

/// The smallest-area rectangle enclosing the convex polygon `hull`. One of its sides is
/// always collinear with a hull edge, so trying each edge's direction is exhaustive. The
/// angle is normalised to (-45°, 45°] so a photo keeps the orientation it had on the bed.
fn min_area_rect(hull: &[(f64, f64)]) -> Option<Placement> {
    if hull.len() < 3 {
        return None;
    }
    let mut best: Option<(f64, Placement)> = None;
    for i in 0..hull.len() {
        let (a, b) = (hull[i], hull[(i + 1) % hull.len()]);
        let len = ((b.0 - a.0).powi(2) + (b.1 - a.1).powi(2)).sqrt();
        if len == 0.0 {
            continue;
        }
        let u = ((b.0 - a.0) / len, (b.1 - a.1) / len);
        let v = (-u.1, u.0);
        let (mut u0, mut u1, mut v0, mut v1) = (f64::MAX, f64::MIN, f64::MAX, f64::MIN);
        for p in hull {
            let (pu, pv) = (p.0 * u.0 + p.1 * u.1, p.0 * v.0 + p.1 * v.1);
            (u0, u1, v0, v1) = (u0.min(pu), u1.max(pu), v0.min(pv), v1.max(pv));
        }
        let area = (u1 - u0) * (v1 - v0);
        if best.as_ref().is_none_or(|(a, _)| area < *a) {
            let (mu, mv) = ((u0 + u1) / 2.0, (v0 + v1) / 2.0);
            let placement = Placement {
                cx: mu * u.0 + mv * v.0,
                cy: mu * u.1 + mv * v.1,
                w: u1 - u0,
                h: v1 - v0,
                angle: u.1.atan2(u.0),
                clipped: false,
            };
            best = Some((area, placement));
        }
    }
    let mut p = best?.1;
    // Turning the reference direction by 90° swaps which side counts as the width.
    while p.angle > FRAC_PI_4 {
        p.angle -= 2.0 * FRAC_PI_4;
        (p.w, p.h) = (p.h, p.w);
    }
    while p.angle <= -FRAC_PI_4 {
        p.angle += 2.0 * FRAC_PI_4;
        (p.w, p.h) = (p.h, p.w);
    }
    Some(p)
}

#[cfg(test)]
mod tests {
    use super::*;

    const DPI: u32 = 300;

    /// White mat with a rotated card: dark blue body, 2 mm white border, faint outline.
    fn bed_with_card(cx: f64, cy: f64, w: f64, h: f64, angle_deg: f64) -> Rgb {
        let (width, height) = (1500u32, 1200u32);
        let (sin, cos) = angle_deg.to_radians().sin_cos();
        let border = 2.0 / 25.4 * DPI as f64;
        let mut data = Vec::with_capacity((width * height * 3) as usize);
        for y in 0..height {
            for x in 0..width {
                let (dx, dy) = (x as f64 + 0.5 - cx, y as f64 + 0.5 - cy);
                // Into the card's own axes.
                let (u, v) = (dx * cos + dy * sin, -dx * sin + dy * cos);
                let (eu, ev) = (w / 2.0 - u.abs(), h / 2.0 - v.abs());
                let px = if eu < 0.0 || ev < 0.0 {
                    [255, 255, 255]
                } else if eu < 1.5 || ev < 1.5 {
                    [190, 190, 190]
                } else if eu < border || ev < border {
                    [252, 252, 250]
                } else {
                    [30, 60, 140]
                };
                data.extend_from_slice(&px);
            }
        }
        Rgb { width, height, data }
    }

    fn coarse_for(bed: &Rgb) -> Rect {
        let small = bed.downsample(DPI, DETECT_DPI, bed.width * DETECT_DPI / DPI, bed.height * DETECT_DPI / DPI);
        let found = detect::find_photos(&small);
        assert_eq!(found.len(), 1, "{found:?}");
        found[0]
    }

    #[test]
    fn measures_a_tilted_card() {
        // An ID-1 card (85.6 x 54 mm) at 300 dpi, tilted 7°.
        let (w, h) = (85.6 / 25.4 * DPI as f64, 54.0 / 25.4 * DPI as f64);
        let bed = bed_with_card(700.0, 600.0, w, h, 7.0);
        let p = locate(&bed, DPI, 255, coarse_for(&bed)).unwrap();
        assert!((p.angle.to_degrees() - 7.0).abs() < 0.3, "{p:?}");
        assert!((p.w - w).abs() < 4.0 && (p.h - h).abs() < 4.0, "{p:?}");
        assert!((p.cx - 700.0).abs() < 3.0 && (p.cy - 600.0).abs() < 3.0, "{p:?}");
    }

    #[test]
    fn steep_tilt_keeps_landscape_orientation() {
        let (w, h) = (85.6 / 25.4 * DPI as f64, 54.0 / 25.4 * DPI as f64);
        let bed = bed_with_card(700.0, 600.0, w, h, -30.0);
        let p = locate(&bed, DPI, 255, coarse_for(&bed)).unwrap();
        assert!((p.angle.to_degrees() + 30.0).abs() < 0.3, "{p:?}");
        assert!(p.w > p.h, "{p:?}");
    }

    #[test]
    fn upright_crop_shows_the_whole_card_with_a_thin_margin() {
        let (w, h) = (85.6 / 25.4 * DPI as f64, 54.0 / 25.4 * DPI as f64);
        let bed = bed_with_card(700.0, 600.0, w, h, 4.0);
        let p = locate(&bed, DPI, 255, coarse_for(&bed)).unwrap();
        assert!(!p.clipped);
        let out = extract(&bed, &p, DPI);
        let margin = MARGIN_MM / 25.4 * DPI as f64;
        assert!((out.width as f64 - (w + 2.0 * margin)).abs() < 5.0, "{}x{}", out.width, out.height);

        let at = |x: u32, y: u32| {
            let i = ((y * out.width + x) * 3) as usize;
            [out.data[i], out.data[i + 1], out.data[i + 2]]
        };
        // Every side's outermost pixels are bare mat...
        for x in 0..out.width {
            assert_eq!(at(x, 0), [255; 3]);
            assert_eq!(at(x, out.height - 1), [255; 3]);
        }
        for y in 0..out.height {
            assert_eq!(at(0, y), [255; 3]);
            assert_eq!(at(out.width - 1, y), [255; 3]);
        }
        // ...and the card's body fills the middle, square to the crop: the blue starts at
        // the same column at the top and bottom of the card.
        let first_blue = |y: u32| (0..out.width).position(|x| at(x, y)[2] > 100 && at(x, y)[0] < 100).unwrap();
        let (top, bottom) = (out.height / 4, out.height * 3 / 4);
        assert!(first_blue(top).abs_diff(first_blue(bottom)) <= 2, "{} vs {}", first_blue(top), first_blue(bottom));
    }

    #[test]
    fn square_photo_is_copied_without_resampling() {
        let (w, h) = (85.6 / 25.4 * DPI as f64, 54.0 / 25.4 * DPI as f64);
        let bed = bed_with_card(700.0, 600.0, w, h, 0.0);
        let p = locate(&bed, DPI, 255, coarse_for(&bed)).unwrap();
        assert!(p.angle.to_degrees().abs() < MIN_ROTATION_DEG, "{p:?}");
        let out = extract(&bed, &p, DPI);
        // Only colours present in the source appear: nothing was interpolated.
        for px in out.data.as_chunks::<3>().0 {
            assert!([[255; 3], [190; 3], [252, 252, 250], [30, 60, 140]].contains(px), "{px:?}");
        }
    }

    #[test]
    fn card_past_the_scan_area_is_flagged_and_filled_grey() {
        // Pushed into the top-left corner, 5 mm beyond what the scanner can see. (A photo
        // cut off only within its white border can't be told from mat at the edge.)
        let (w, h) = (85.6 / 25.4 * DPI as f64, 54.0 / 25.4 * DPI as f64);
        let past = 5.0 / 25.4 * DPI as f64;
        let bed = bed_with_card(w / 2.0 - past, h / 2.0 - past, w, h, 0.0);
        let p = locate(&bed, DPI, 255, coarse_for(&bed)).unwrap();
        assert!(p.clipped);
        assert_eq!(extract(&bed, &p, DPI).data[..3], OUTSIDE);
    }
}
