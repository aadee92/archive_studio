//! SCRFD face detection (InsightFace `det_10g.onnx`).
//!
//! The image is scaled to fit a square input and padded at the right and bottom. The model
//! scores two anchors per cell at strides 8, 16 and 32, and regresses a box and five landmarks
//! from each anchor centre in units of the stride.

use std::path::Path;

use image::{RgbImage, imageops::FilterType};
use tract_onnx::prelude::{tract_data::internal::bail, *};

use crate::Error;

const STRIDES: [usize; 3] = [8, 16, 32];
const ANCHORS: usize = 2;
const MEAN: f32 = 127.5;
const STD: f32 = 128.0;

/// One detected face, in the pixel coordinates of the image passed to [`FaceDetector::detect`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Detection {
    /// `[x1, y1, x2, y2]`.
    pub bbox: [f32; 4],
    pub score: f32,
    /// Eye, eye, nose, mouth corner, mouth corner, left to right in the image.
    pub landmarks: [[f32; 2]; 5],
}

impl Detection {
    pub fn width(&self) -> f32 {
        self.bbox[2] - self.bbox[0]
    }

    pub fn height(&self) -> f32 {
        self.bbox[3] - self.bbox[1]
    }

    fn iou(&self, other: &Self) -> f32 {
        let [ax1, ay1, ax2, ay2] = self.bbox;
        let [bx1, by1, bx2, by2] = other.bbox;
        let w = (ax2.min(bx2) - ax1.max(bx1)).max(0.0);
        let h = (ay2.min(by2) - ay1.max(by1)).max(0.0);
        let inter = w * h;
        inter / (self.width() * self.height() + other.width() * other.height() - inter)
    }
}

pub struct FaceDetector {
    model: Arc<TypedRunnableModel>,
    size: usize,
    /// Minimum score to keep a detection.
    pub threshold: f32,
    /// Overlap above which the lower-scoring of two detections is dropped.
    pub nms_iou: f32,
}

impl FaceDetector {
    /// Loads the model with a 640×640 input, InsightFace's default.
    pub fn new(model_path: impl AsRef<Path>) -> Result<Self, Error> {
        Self::with_size(model_path, 640)
    }

    /// Loads the model with a `size`×`size` input; `size` must be a multiple of 32. A larger
    /// input finds smaller faces (group photos, large scans) at quadratic cost.
    pub fn with_size(model_path: impl AsRef<Path>, size: usize) -> Result<Self, Error> {
        assert!(size > 0 && size.is_multiple_of(32), "detector input size {size} is not a multiple of 32");
        let model = tract_onnx::onnx()
            .model_for_path(model_path)?
            .with_input_fact(0, f32::fact([1, 3, size, size]).into())?
            .into_optimized()?
            .into_runnable()?;
        Ok(Self { model, size, threshold: 0.5, nms_iou: 0.4 })
    }

    /// Faces in `image`, highest score first.
    pub fn detect(&self, image: &RgbImage) -> Result<Vec<Detection>, Error> {
        let s = self.size;
        let (w, h) = image.dimensions();
        let scale = (s as f32 / w as f32).min(s as f32 / h as f32);
        let (rw, rh) = (
            ((w as f32 * scale).round() as u32).clamp(1, s as u32),
            ((h as f32 * scale).round() as u32).clamp(1, s as u32),
        );
        let resized = image::imageops::resize(image, rw, rh, FilterType::Triangle);

        // Padding is black, as upstream pads before normalising.
        let mut input = vec![-MEAN / STD; 3 * s * s];
        for (x, y, px) in resized.enumerate_pixels() {
            let i = y as usize * s + x as usize;
            for c in 0..3 {
                input[c * s * s + i] = (px[c] as f32 - MEAN) / STD;
            }
        }

        let input = Tensor::from_shape(&[1, 3, s, s], &input)?;
        let outputs = self.model.run(tvec!(input.into()))?;

        // Nine outputs: scores, boxes and landmarks for each stride. Tell them apart by shape
        // rather than by position, which differs between exports.
        let mut heads: [[Option<&[f32]>; 3]; 3] = Default::default();
        for out in &outputs {
            let shape = out.shape();
            let (rows, cols) = match *shape {
                [rows, cols] | [1, rows, cols] => (rows, cols),
                _ => bail!("unexpected SCRFD output shape {shape:?}"),
            };
            let kind = match cols {
                1 => 0,
                4 => 1,
                10 => 2,
                _ => bail!("unexpected SCRFD output shape {shape:?}"),
            };
            let Some(level) = STRIDES.iter().position(|st| (s / st).pow(2) * ANCHORS == rows)
            else {
                bail!("unexpected SCRFD output shape {shape:?}");
            };
            heads[level][kind] = Some(out.try_as_plain_ram()?.as_slice::<f32>()?);
        }

        let mut found = Vec::new();
        for (level, &stride) in STRIDES.iter().enumerate() {
            let [Some(scores), Some(boxes), Some(kps)] = heads[level] else {
                bail!("SCRFD is missing outputs for stride {stride}");
            };
            let cells = s / stride;
            let st = stride as f32;
            for (i, &score) in scores.iter().enumerate() {
                if score < self.threshold {
                    continue;
                }
                let cell = i / ANCHORS;
                let (cx, cy) = (((cell % cells) * stride) as f32, ((cell / cells) * stride) as f32);
                let b = &boxes[i * 4..i * 4 + 4];
                let k = &kps[i * 10..i * 10 + 10];
                let mut landmarks = [[0.0; 2]; 5];
                for (j, l) in landmarks.iter_mut().enumerate() {
                    *l = [(cx + k[2 * j] * st) / scale, (cy + k[2 * j + 1] * st) / scale];
                }
                found.push(Detection {
                    bbox: [
                        (cx - b[0] * st) / scale,
                        (cy - b[1] * st) / scale,
                        (cx + b[2] * st) / scale,
                        (cy + b[3] * st) / scale,
                    ],
                    score,
                    landmarks,
                });
            }
        }

        found.sort_by(|a, b| b.score.total_cmp(&a.score));
        let mut kept: Vec<Detection> = Vec::new();
        for d in found {
            if kept.iter().all(|k| k.iou(&d) <= self.nms_iou) {
                kept.push(d);
            }
        }
        Ok(kept)
    }
}
