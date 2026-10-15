//! The image half (`visual.onnx`).

use std::path::Path;

use image::{DynamicImage, imageops::FilterType};
use tract_onnx::prelude::*;

use crate::{Embedding, Error};

/// OpenAI CLIP's normalisation, which TinyCLIP keeps.
const MEAN: [f32; 3] = [0.481_454_66, 0.457_827_5, 0.408_210_73];
const STD: [f32; 3] = [0.268_629_5, 0.261_302_6, 0.275_777_1];
/// The model's input is `SIZE`×`SIZE`.
pub const SIZE: u32 = 224;

pub struct ImageEncoder {
    model: Arc<TypedRunnableModel>,
}

impl ImageEncoder {
    pub fn new(model_path: impl AsRef<Path>) -> Result<Self, Error> {
        let s = SIZE as usize;
        let model = tract_onnx::onnx()
            .model_for_path(model_path)?
            .with_input_fact(0, f32::fact([1, 3, s, s]).into())?
            .into_optimized()?
            .into_runnable()?;
        Ok(Self { model })
    }

    /// Embeds `image` as displayed (apply its EXIF orientation first). Like CLIP's own
    /// preprocessing, only the centre square is seen: the short side scaled to [`SIZE`].
    pub fn embed(&self, image: &DynamicImage) -> Result<Embedding, Error> {
        let s = SIZE as usize;
        let square = center_square(image);
        let input = square.to_rgb8();
        let mut data = vec![0f32; 3 * s * s];
        for (x, y, px) in input.enumerate_pixels() {
            let i = y as usize * s + x as usize;
            for c in 0..3 {
                data[c * s * s + i] = (px[c] as f32 / 255.0 - MEAN[c]) / STD[c];
            }
        }
        let input = Tensor::from_shape(&[1, 3, s, s], &data)?;
        let outputs = self.model.run(tvec!(input.into()))?;
        Ok(Embedding::new(outputs[0].try_as_plain_ram()?.as_slice::<f32>()?.to_vec()))
    }
}

/// The centre square of `image`, scaled to `SIZE`×`SIZE`.
fn center_square(image: &DynamicImage) -> DynamicImage {
    let (w, h) = (image.width(), image.height());
    let side = w.min(h).max(1);
    let square = image.crop_imm((w - side.min(w)) / 2, (h - side.min(h)) / 2, side, side);
    if side > 2 * SIZE {
        // Area averaging first: an antialiased bicubic from a full-size photo is slow, and
        // over such a reduction the two come out alike.
        square.thumbnail_exact(2 * SIZE, 2 * SIZE).resize_exact(SIZE, SIZE, FilterType::CatmullRom)
    } else {
        square.resize_exact(SIZE, SIZE, FilterType::CatmullRom)
    }
}
