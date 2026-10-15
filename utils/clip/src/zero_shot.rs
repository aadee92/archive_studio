//! Zero-shot labels: how likely each of a fixed set of labels is to describe an image.
//!
//! Labels come in groups (scenes, objects, events…). Within a group, labels compete: the
//! probabilities are a softmax over the group's labels at the model's temperature
//! ([`LOGIT_SCALE`]). Each group also has *null* prompts ("people", "a room") standing for
//! "none of these": without them an image showing none of a group's labels would still give
//! one of them most of the probability. Across groups labels don't compete, so a picture can
//! be both a birthday party and a cake.
//!
//! Each label (and null) is embedded as the mean of a few [`TEMPLATES`] filled with its
//! phrase, which is steadier than any one prompt.

use crate::{Embedding, Error, LOGIT_SCALE, TextEncoder};

/// Prompts a label's phrase is put in; `{}` is the phrase.
pub const TEMPLATES: [&str; 3] = ["a photo of {}.", "a picture of {}.", "{}."];

/// Labels that compete with each other, as phrases ("a beach", "a dog").
#[derive(Clone, Copy, Debug)]
pub struct Group<'a> {
    pub labels: &'a [&'a str],
    /// What the image may show instead, when none of the labels apply.
    pub nulls: &'a [&'a str],
}

pub struct ZeroShot {
    /// Per group: the labels' embeddings, then the nulls'.
    groups: Vec<(Vec<Embedding>, Vec<Embedding>)>,
}

impl ZeroShot {
    /// Embeds every label and null, on as many threads as there are cores (each phrase
    /// takes tens of milliseconds on one).
    pub fn new(text: &TextEncoder, groups: &[Group]) -> Result<Self, Error> {
        let phrases: Vec<&str> = groups.iter().flat_map(|g| g.labels.iter().chain(g.nulls)).copied().collect();
        let threads = std::thread::available_parallelism().map_or(1, |n| n.get());
        let chunks: Vec<Result<Vec<Embedding>, Error>> = std::thread::scope(|scope| {
            let workers: Vec<_> = phrases
                .chunks(phrases.len().div_ceil(threads).max(1))
                .map(|chunk| scope.spawn(move || chunk.iter().map(|p| phrase(text, p)).collect()))
                .collect();
            workers.into_iter().map(|w| w.join().expect("embedding thread panicked")).collect()
        });
        let mut embedded = Vec::with_capacity(phrases.len());
        for chunk in chunks {
            embedded.extend(chunk?);
        }
        let mut embedded = embedded.into_iter();
        let mut take = |n: usize| embedded.by_ref().take(n).collect::<Vec<_>>();
        let groups = groups.iter().map(|g| (take(g.labels.len()), take(g.nulls.len()))).collect();
        Ok(Self { groups })
    }

    /// Per group, in the order given to [`new`](Self::new), each label's probability in 0..=1.
    pub fn scores(&self, image: &Embedding) -> Vec<Vec<f32>> {
        self.groups
            .iter()
            .map(|(labels, nulls)| {
                let logits: Vec<f32> = labels.iter().chain(nulls).map(|e| LOGIT_SCALE * e.similarity(image)).collect();
                let max = logits.iter().copied().fold(f32::NEG_INFINITY, f32::max);
                let exp: Vec<f32> = logits.iter().map(|l| (l - max).exp()).collect();
                let sum: f32 = exp.iter().sum();
                exp[..labels.len()].iter().map(|e| e / sum).collect()
            })
            .collect()
    }
}

/// `phrase` in each of the [`TEMPLATES`], averaged.
pub fn phrase(text: &TextEncoder, phrase: &str) -> Result<Embedding, Error> {
    let prompts = TEMPLATES.map(|t| t.replace("{}", phrase));
    let embeddings = prompts.iter().map(|p| text.embed(p)).collect::<Result<Vec<_>, _>>()?;
    Ok(Embedding::mean(&embeddings).expect("templates"))
}
