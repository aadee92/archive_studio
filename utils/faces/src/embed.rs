//! ArcFace embeddings (InsightFace `w600k_r50.onnx`).

use std::path::Path;

use image::RgbImage;
use tract_onnx::prelude::*;

use crate::{Error, align};

const MEAN: f32 = 127.5;
const STD: f32 = 127.5;

/// A unit-length face embedding. Two faces of one person have a high cosine similarity.
#[derive(Clone, Debug, PartialEq)]
pub struct Embedding(Vec<f32>);

impl Embedding {
    /// Normalises `v` to unit length.
    pub fn new(mut v: Vec<f32>) -> Self {
        let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        if norm > 0.0 {
            v.iter_mut().for_each(|x| *x /= norm);
        }
        Self(v)
    }

    pub fn as_slice(&self) -> &[f32] {
        &self.0
    }

    /// Cosine similarity, in [-1, 1].
    pub fn similarity(&self, other: &Self) -> f32 {
        self.0.iter().zip(&other.0).map(|(a, b)| a * b).sum()
    }

    /// Cosine distance, `1 − similarity`, in [0, 2].
    pub fn distance(&self, other: &Self) -> f32 {
        1.0 - self.similarity(other)
    }

    /// Little-endian `f32`s, for storing as a blob.
    pub fn to_bytes(&self) -> Vec<u8> {
        self.0.iter().flat_map(|x| x.to_le_bytes()).collect()
    }

    /// The inverse of [`to_bytes`](Self::to_bytes); `None` if the length is not a multiple of 4.
    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        if !bytes.len().is_multiple_of(4) {
            return None;
        }
        let v = bytes.as_chunks::<4>().0.iter().map(|&c| f32::from_le_bytes(c));
        Some(Self::new(v.collect()))
    }

    /// The normalised mean of `embeddings`: a single embedding standing for a group.
    pub fn mean<'a>(embeddings: impl IntoIterator<Item = &'a Embedding>) -> Option<Self> {
        let mut it = embeddings.into_iter();
        let mut sum = it.next()?.0.clone();
        for e in it {
            sum.iter_mut().zip(&e.0).for_each(|(s, x)| *s += x);
        }
        Some(Self::new(sum))
    }
}

pub struct FaceEmbedder {
    model: Arc<TypedRunnableModel>,
}

impl FaceEmbedder {
    pub fn new(model_path: impl AsRef<Path>) -> Result<Self, Error> {
        let s = align::SIZE as usize;
        let model = tract_onnx::onnx()
            .model_for_path(model_path)?
            .with_input_fact(0, f32::fact([1, 3, s, s]).into())?
            .into_optimized()?
            .into_runnable()?;
        Ok(Self { model })
    }

    /// Embeds a face already aligned by [`align::align`].
    pub fn embed(&self, aligned: &RgbImage) -> Result<Embedding, Error> {
        let s = align::SIZE as usize;
        assert_eq!(aligned.dimensions(), (align::SIZE, align::SIZE), "face is not aligned");
        let mut input = vec![0f32; 3 * s * s];
        for (x, y, px) in aligned.enumerate_pixels() {
            let i = y as usize * s + x as usize;
            for c in 0..3 {
                input[c * s * s + i] = (px[c] as f32 - MEAN) / STD;
            }
        }
        let input = Tensor::from_shape(&[1, 3, s, s], &input)?;
        let outputs = self.model.run(tvec!(input.into()))?;
        Ok(Embedding::new(outputs[0].try_as_plain_ram()?.as_slice::<f32>()?.to_vec()))
    }
}
