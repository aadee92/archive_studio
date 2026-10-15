//! The text half (`textual.onnx`) and CLIP's tokenizer.

use std::path::Path;

use instant_clip_tokenizer::{Token, Tokenizer};
use tract_onnx::prelude::*;

use crate::{Embedding, Error};

/// Tokens the model reads, start and end of text included; longer text is cut short.
pub const CONTEXT: usize = 77;

pub struct TextEncoder {
    model: Arc<TypedRunnableModel>,
    tokenizer: Tokenizer,
}

impl TextEncoder {
    pub fn new(model_path: impl AsRef<Path>) -> Result<Self, Error> {
        let model = tract_onnx::onnx()
            .model_for_path(model_path)?
            .with_input_fact(0, i64::fact([1, CONTEXT]).into())?
            .into_optimized()?
            .into_runnable()?;
        Ok(Self { model, tokenizer: Tokenizer::new() })
    }

    /// The model's input for `text`: start of text, its tokens, end of text, then zeros.
    pub fn tokens(&self, text: &str) -> Vec<i64> {
        let mut tokens = vec![self.tokenizer.start_of_text()];
        self.tokenizer.encode(text, &mut tokens);
        tokens.truncate(CONTEXT - 1);
        tokens.push(self.tokenizer.end_of_text());
        let mut ids: Vec<i64> = tokens.into_iter().map(|t| i64::from(Token::to_u16(t))).collect();
        // Padding after the end is never looked at (the model is causal and reads the
        // embedding off the end-of-text token), so any value would do.
        ids.resize(CONTEXT, 0);
        ids
    }

    pub fn embed(&self, text: &str) -> Result<Embedding, Error> {
        let input = Tensor::from_shape(&[1, CONTEXT], &self.tokens(text))?;
        let outputs = self.model.run(tvec!(input.into()))?;
        Ok(Embedding::new(outputs[0].try_as_plain_ram()?.as_slice::<f32>()?.to_vec()))
    }
}

#[cfg(test)]
mod tests {
    use instant_clip_tokenizer::{Token, Tokenizer};

    /// The same ids as Hugging Face's `CLIPTokenizer` for TinyCLIP.
    #[test]
    fn tokenizes_like_clip() {
        let tokenizer = Tokenizer::new();
        let mut tokens = vec![tokenizer.start_of_text()];
        tokenizer.encode("A photo of a dog.", &mut tokens);
        tokens.push(tokenizer.end_of_text());
        let ids: Vec<u16> = tokens.into_iter().map(Token::to_u16).collect();
        assert_eq!(ids, [49406, 320, 1125, 539, 320, 1929, 269, 49407]);
    }
}
