//! Downscaling for the small images backends report (previews, strip thumbnails).

use fast_image_resize::{FilterType, ResizeAlg, ResizeOptions, Resizer};
use image::RgbImage;

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
