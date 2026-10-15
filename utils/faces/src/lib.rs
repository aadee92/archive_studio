//! Face detection, alignment, embedding and clustering for the photo archive.
//!
//! The pipeline is InsightFace's: SCRFD finds faces and five landmarks, each face is warped
//! to ArcFace's 112×112 template, ArcFace turns it into a unit-length 512-d embedding, and
//! embeddings are grouped into people by cosine distance.
//!
//! - [`detect`]: [`FaceDetector`] (SCRFD).
//! - [`align`]: [`align::align`], landmarks to a 112×112 crop.
//! - [`embed`]: [`FaceEmbedder`] (ArcFace) and [`Embedding`].
//! - [`cluster`]: [`cluster::cluster`], DBSCAN over cosine distance.
//! - [`order`]: [`order::similarity_path`], a review order where each face resembles the last.
//! - [`Faces`]: all of the above for one image, or ([`Faces::embed_region`]) for a face
//!   someone boxed by hand.
//!
//! Models, from InsightFace's `buffalo_l` pack
//! (https://github.com/deepinsight/insightface/releases/download/v0.7/buffalo_l.zip):
//! `det_10g.onnx` (SCRFD-10GF) and `w600k_r50.onnx` (ArcFace R50, WebFace600K). The code is
//! ours; those weights are licensed for non-commercial research only.
//!
//! Everything runs on tract (pure Rust). Models load once; every method takes `&self` and is
//! safe to call from several threads.

pub mod align;
pub mod cluster;
pub mod detect;
pub mod embed;
pub mod order;

use std::path::Path;

use image::RgbImage;

pub use detect::{Detection, FaceDetector};
pub use embed::{Embedding, FaceEmbedder};
pub use tract_onnx::prelude::TractError as Error;

/// Runs model inference on a pool of `threads` threads, process-wide; `0` or `1` runs it on
/// the calling thread (the default). Set it once, before analysing, from the host that knows
/// how many cores it can spare.
pub fn set_threads(threads: usize) {
    use tract_linalg::multithread::{Executor, set_default_executor};
    set_default_executor(if threads > 1 { Executor::multithread(threads) } else { Executor::SingleThread });
}

/// One face found in an image.
#[derive(Clone, Debug)]
pub struct Face {
    pub detection: Detection,
    pub embedding: Embedding,
}

/// The detector and embedder together.
pub struct Faces {
    pub detector: FaceDetector,
    pub embedder: FaceEmbedder,
}

impl Faces {
    /// Loads `det_10g.onnx` and `w600k_r50.onnx` from `dir`.
    pub fn from_dir(dir: impl AsRef<Path>) -> Result<Self, Error> {
        let dir = dir.as_ref();
        Ok(Self {
            detector: FaceDetector::new(dir.join("det_10g.onnx"))?,
            embedder: FaceEmbedder::new(dir.join("w600k_r50.onnx"))?,
        })
    }

    /// Detects every face in `image` and embeds each one, highest detection score first.
    pub fn analyze(&self, image: &RgbImage) -> Result<Vec<Face>, Error> {
        self.detector
            .detect(image)?
            .into_iter()
            .map(|detection| {
                let crop = align::align(image, &detection.landmarks);
                Ok(Face { embedding: self.embedder.embed(&crop)?, detection })
            })
            .collect()
    }

    /// Embeds the face someone boxed by hand at `bbox` (`[x1, y1, x2, y2]` in `image`'s
    /// pixels), typically one the detector missed in the whole picture. The detector looks
    /// again at just that part of the image (twice the box each way), where the face fills
    /// much more of its input; the best detection centred inside the box gives the landmarks
    /// to align it with. If there is none, ArcFace's template is laid over the box, which
    /// embeds a roughly upright, frontal face boxed from brows to chin well enough.
    ///
    /// The returned detection has `bbox` as given; its score and landmarks are the
    /// detector's, or 0 and the template's if it found nothing there.
    pub fn embed_region(&self, image: &RgbImage, bbox: [f32; 4]) -> Result<Face, Error> {
        let detection = self.find_in(image, bbox)?.unwrap_or_else(|| template_detection(bbox));
        let crop = align::align(image, &detection.landmarks);
        Ok(Face { embedding: self.embedder.embed(&crop)?, detection: Detection { bbox, ..detection } })
    }

    /// The best detection whose centre is inside `bbox`, looking at that part of `image` only.
    fn find_in(&self, image: &RgbImage, [x1, y1, x2, y2]: [f32; 4]) -> Result<Option<Detection>, Error> {
        let (iw, ih) = image.dimensions();
        let (w, h) = ((x2 - x1).max(1.0), (y2 - y1).max(1.0));
        let left = (x1 - w / 2.0).clamp(0.0, iw.saturating_sub(1) as f32) as u32;
        let top = (y1 - h / 2.0).clamp(0.0, ih.saturating_sub(1) as f32) as u32;
        let right = ((x2 + w / 2.0).ceil() as u32).clamp(left + 1, iw.max(left + 1));
        let bottom = ((y2 + h / 2.0).ceil() as u32).clamp(top + 1, ih.max(top + 1));
        let part = image::imageops::crop_imm(image, left, top, right - left, bottom - top).to_image();
        let (dx, dy) = (left as f32, top as f32);
        let inside = |d: &Detection| {
            let (cx, cy) = ((d.bbox[0] + d.bbox[2]) / 2.0, (d.bbox[1] + d.bbox[3]) / 2.0);
            (x1..=x2).contains(&cx) && (y1..=y2).contains(&cy)
        };
        Ok(self
            .detector
            .detect(&part)?
            .into_iter()
            .map(|d| Detection {
                bbox: [d.bbox[0] + dx, d.bbox[1] + dy, d.bbox[2] + dx, d.bbox[3] + dy],
                landmarks: d.landmarks.map(|[x, y]| [x + dx, y + dy]),
                ..d
            })
            .filter(inside)
            .max_by(|a, b| a.score.total_cmp(&b.score)))
    }
}

/// A detection with ArcFace's template landmarks scaled onto `bbox`, score 0.
fn template_detection(bbox: [f32; 4]) -> Detection {
    let [x1, y1, x2, y2] = bbox;
    let s = align::SIZE as f32;
    let landmarks = align::TEMPLATE.map(|[x, y]| [x1 + x / s * (x2 - x1), y1 + y / s * (y2 - y1)]);
    Detection { bbox, score: 0.0, landmarks }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_template_fills_the_box() {
        let d = template_detection([100.0, 50.0, 212.0, 274.0]);
        assert_eq!(d.score, 0.0);
        // The template's 112×112 square stretched onto 112×224: x as is, y doubled.
        let [lx, ly] = d.landmarks[0];
        assert!((lx - (100.0 + align::TEMPLATE[0][0])).abs() < 1e-3 && (ly - (50.0 + 2.0 * align::TEMPLATE[0][1])).abs() < 1e-3);
        assert!(d.landmarks.iter().all(|[x, y]| (100.0..=212.0).contains(x) && (50.0..=274.0).contains(y)));
    }
}
