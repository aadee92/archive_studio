//! Coarse (90°-step) orientation detection using the
//! `DuarteBarbosa/deep-image-orientation-detection` EfficientNetV2 ONNX model.
//!
//! Model: https://huggingface.co/DuarteBarbosa/deep-image-orientation-detection
//! (`orientation_model_v2_0.9882.onnx`, MIT).

use std::path::Path;

use image::{DynamicImage, imageops::FilterType};
use tract_onnx::prelude::*;

pub use tract_onnx::prelude::TractError as Error;

const MEAN: [f32; 3] = [0.485, 0.456, 0.406];
const STD: [f32; 3] = [0.229, 0.224, 0.225];
/// Upstream resizes to `SIZE + 32` square, then center-crops to `SIZE`.
const SIZE: u32 = 384;
const CROP_MARGIN: u32 = 32;

pub struct Prediction {
    /// Clockwise rotation (0, 90, 180 or 270) that makes the image upright.
    pub rotate_cw: u16,
    /// Softmax probability of the chosen class.
    pub confidence: f32,
}

impl Prediction {
    pub fn apply(&self, img: &DynamicImage) -> DynamicImage {
        match self.rotate_cw {
            90 => img.rotate90(),
            180 => img.rotate180(),
            270 => img.rotate270(),
            _ => img.clone(),
        }
    }
}

/// Load once and share; `detect` takes `&self` and is safe to call from multiple threads.
pub struct OrientationDetector {
    model: Arc<TypedRunnableModel>,
}

impl OrientationDetector {
    pub fn new(model_path: impl AsRef<Path>) -> Result<Self, Error> {
        let s = SIZE as usize;
        let model = tract_onnx::onnx()
            .model_for_path(model_path)?
            .with_input_fact(0, f32::fact([1, 3, s, s]).into())?
            .into_optimized()?
            .into_runnable()?;
        Ok(Self { model })
    }

    pub fn detect(&self, img: &DynamicImage) -> Result<Prediction, Error> {
        let s = SIZE as usize;
        let resized = img
            .resize_exact(SIZE + CROP_MARGIN, SIZE + CROP_MARGIN, FilterType::Triangle)
            .to_rgb8();
        let off = CROP_MARGIN / 2;

        let mut input = vec![0f32; 3 * s * s];
        for y in 0..SIZE {
            for x in 0..SIZE {
                let px = resized.get_pixel(x + off, y + off).0;
                let i = y as usize * s + x as usize;
                for c in 0..3 {
                    input[c * s * s + i] = (px[c] as f32 / 255.0 - MEAN[c]) / STD[c];
                }
            }
        }

        let input = Tensor::from_shape(&[1, 3, s, s], &input)?;
        let outputs = self.model.run(tvec!(input.into()))?;
        let logits = outputs[0].try_as_plain_ram()?.as_slice::<f32>()?;

        let (class, &max) = logits
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.total_cmp(b.1))
            .expect("model returned no logits");
        let sum: f32 = logits.iter().map(|l| (l - max).exp()).sum();

        // Class k means the image was rotated by k*90° counter-clockwise (PIL
        // `Transpose.ROTATE_90`), so undoing it takes k*90° clockwise.
        Ok(Prediction { rotate_cw: class as u16 * 90, confidence: 1.0 / sum })
    }
}
