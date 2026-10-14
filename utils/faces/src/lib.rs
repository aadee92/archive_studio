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
//! - [`Faces`]: all of the above for one image.
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
}
