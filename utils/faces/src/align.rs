//! Warping a face onto ArcFace's 112×112 template from its five landmarks.

use image::{Rgb, RgbImage};

/// Side of the aligned crop.
pub const SIZE: u32 = 112;

/// Where ArcFace expects the five landmarks in a 112×112 crop (InsightFace's `arcface_dst`).
pub const TEMPLATE: [[f32; 2]; 5] = [
    [38.2946, 51.6963],
    [73.5318, 51.5014],
    [56.0252, 71.7366],
    [41.5493, 92.3655],
    [70.7299, 92.2041],
];

/// A rotation, uniform scale and translation: `(x, y) -> (a·x − b·y + tx, b·x + a·y + ty)`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Similarity {
    pub a: f32,
    pub b: f32,
    pub tx: f32,
    pub ty: f32,
}

impl Similarity {
    /// The least-squares similarity taking each `from` point onto its `to` point (Umeyama's
    /// method; in 2-D it has this closed form).
    pub fn estimate(from: &[[f32; 2]], to: &[[f32; 2]]) -> Self {
        assert_eq!(from.len(), to.len());
        let n = from.len() as f32;
        let mean = |p: &[[f32; 2]]| {
            let (sx, sy) = p.iter().fold((0.0, 0.0), |(sx, sy), q| (sx + q[0], sy + q[1]));
            [sx / n, sy / n]
        };
        let (mf, mt) = (mean(from), mean(to));
        let (mut dot, mut cross, mut norm) = (0.0, 0.0, 0.0);
        for (f, t) in from.iter().zip(to) {
            let (fx, fy) = (f[0] - mf[0], f[1] - mf[1]);
            let (gx, gy) = (t[0] - mt[0], t[1] - mt[1]);
            dot += fx * gx + fy * gy;
            cross += fx * gy - fy * gx;
            norm += fx * fx + fy * fy;
        }
        let (a, b) = if norm > 0.0 { (dot / norm, cross / norm) } else { (1.0, 0.0) };
        Self { a, b, tx: mt[0] - (a * mf[0] - b * mf[1]), ty: mt[1] - (b * mf[0] + a * mf[1]) }
    }

    pub fn apply(&self, [x, y]: [f32; 2]) -> [f32; 2] {
        [self.a * x - self.b * y + self.tx, self.b * x + self.a * y + self.ty]
    }

    pub fn inverse(&self) -> Self {
        let d = self.a * self.a + self.b * self.b;
        let (a, b) = (self.a / d, -self.b / d);
        Self { a, b, tx: -(a * self.tx - b * self.ty), ty: -(b * self.tx + a * self.ty) }
    }
}

/// The 112×112 crop that puts `landmarks` on [`TEMPLATE`]. Pixels from outside `image` are
/// black.
pub fn align(image: &RgbImage, landmarks: &[[f32; 2]; 5]) -> RgbImage {
    let back = Similarity::estimate(landmarks, &TEMPLATE).inverse();
    RgbImage::from_fn(SIZE, SIZE, |x, y| {
        let [sx, sy] = back.apply([x as f32, y as f32]);
        bilinear(image, sx, sy)
    })
}

fn bilinear(image: &RgbImage, x: f32, y: f32) -> Rgb<u8> {
    let (w, h) = image.dimensions();
    let (x0, y0) = (x.floor(), y.floor());
    let (fx, fy) = (x - x0, y - y0);
    let at = |dx: i64, dy: i64| -> [f32; 3] {
        let (px, py) = (x0 as i64 + dx, y0 as i64 + dy);
        if px < 0 || py < 0 || px >= w as i64 || py >= h as i64 {
            return [0.0; 3];
        }
        image.get_pixel(px as u32, py as u32).0.map(f32::from)
    };
    let (p00, p10, p01, p11) = (at(0, 0), at(1, 0), at(0, 1), at(1, 1));
    Rgb(std::array::from_fn(|c| {
        let top = p00[c] + (p10[c] - p00[c]) * fx;
        let bottom = p01[c] + (p11[c] - p01[c]) * fx;
        (top + (bottom - top) * fy).round().clamp(0.0, 255.0) as u8
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn estimate_recovers_an_exact_similarity() {
        let t = Similarity { a: 0.8, b: -0.3, tx: 12.0, ty: -4.0 };
        let to = TEMPLATE.map(|p| t.apply(p));
        let got = Similarity::estimate(&TEMPLATE, &to);
        for (g, w) in [got.a, got.b, got.tx, got.ty].iter().zip([t.a, t.b, t.tx, t.ty]) {
            assert!((g - w).abs() < 1e-3, "{got:?} != {t:?}");
        }
    }

    #[test]
    fn inverse_round_trips() {
        let t = Similarity { a: 1.7, b: 0.4, tx: -30.0, ty: 55.0 };
        let p = t.inverse().apply(t.apply([10.0, 20.0]));
        assert!((p[0] - 10.0).abs() < 1e-3 && (p[1] - 20.0).abs() < 1e-3, "{p:?}");
    }
}
