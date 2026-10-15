//! CLIP image and text embeddings, for tagging photos and searching them by description.
//!
//! CLIP maps images and text into one space: a photo and a sentence describing it have a
//! high cosine similarity. That gives two uses:
//!
//! - **zero-shot tags** ([`zero_shot`]): score an image against prompts for a fixed list of
//!   labels ("a photo of a beach.") and keep the likely ones;
//! - **search by description**: embed every image once, then rank them against the
//!   embedding of whatever the user types.
//!
//! - [`visual`]: [`ImageEncoder`], an image to an [`Embedding`].
//! - [`text`]: [`TextEncoder`], text to an [`Embedding`] (CLIP's tokenizer included).
//! - [`zero_shot`]: [`ZeroShot`], labels in groups, scored per group against "nothing in
//!   particular" prompts.
//!
//! Model: TinyCLIP ViT-8M/16, Text-3M, trained on YFCC-15M (Microsoft, MIT licence;
//! https://huggingface.co/wkcn/TinyCLIP-ViT-8M-16-Text-3M-YFCC15M), the smallest CLIP we know
//! of: 8M parameters for images, 3M for text, 512-d embeddings. Its two halves are exported
//! to ONNX by `export_onnx.py` as `visual.onnx` and `textual.onnx`.
//!
//! Everything runs on tract (pure Rust). Models load once; every method takes `&self` and is
//! safe to call from several threads. Inference uses tract's process-wide executor: a host
//! that also uses the `faces` crate sets its thread count there (`faces::set_threads`).

pub mod text;
pub mod visual;
pub mod zero_shot;

use std::path::Path;

pub use text::TextEncoder;
pub use tract_onnx::prelude::TractError as Error;
pub use visual::ImageEncoder;
pub use zero_shot::ZeroShot;

/// The model's name, for recording which model an embedding or tag came from: embeddings
/// from different models can't be compared.
pub const MODEL: &str = "tinyclip-8m-16-yfcc15m";

/// The model's learned temperature: cosine similarities × this are its logits.
pub const LOGIT_SCALE: f32 = 50.0;

/// A unit-length CLIP embedding, of an image or of text. The more alike two things are, the
/// higher the cosine similarity of their embeddings; for an image and a text that fits it,
/// typically 0.3–0.45 with this model.
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

    /// The normalised mean of `embeddings`: one embedding standing for several (e.g. one
    /// label's prompts).
    pub fn mean<'a>(embeddings: impl IntoIterator<Item = &'a Embedding>) -> Option<Self> {
        let mut it = embeddings.into_iter();
        let mut sum = it.next()?.0.clone();
        for e in it {
            sum.iter_mut().zip(&e.0).for_each(|(s, x)| *s += x);
        }
        Some(Self::new(sum))
    }
}

/// Both encoders.
pub struct Clip {
    pub image: ImageEncoder,
    pub text: TextEncoder,
}

impl Clip {
    /// Loads `visual.onnx` and `textual.onnx` from `dir`.
    pub fn from_dir(dir: impl AsRef<Path>) -> Result<Self, Error> {
        let dir = dir.as_ref();
        Ok(Self { image: ImageEncoder::new(dir.join("visual.onnx"))?, text: TextEncoder::new(dir.join("textual.onnx"))? })
    }
}
