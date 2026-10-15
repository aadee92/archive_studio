//! The describe stage: a CLIP embedding of each image, for search by description
//! ([`crate::catalog::semantic`]), and the tags it suggests from a fixed vocabulary.
//!
//! Tags are scored zero-shot ([`clip::zero_shot`]) in groups that each become a branch of
//! the tag tree: `Scenes|Beach`, `Objects|Dog`, `Events|Birthday party`, `Photo|Selfie`.
//! Labels in a group compete; groups don't. Tags from [`MIN_STORED`] up are stored as the
//! detector [`DETECTOR`]'s, with their probability as confidence, so as usual only those from
//! [`crate::catalog::DEFAULT_MIN_CONFIDENCE`] up match searches and reach the sidecars, and
//! the user can reject any of them.

use std::path::{Path, PathBuf};

use clip::zero_shot::{self, ZeroShot};
use clip::{Clip, Embedding, TextEncoder};

use crate::catalog::{Catalog, TagKind};

/// The source name of the tags this stage proposes.
pub const DETECTOR: &str = "clip/tinyclip-8m";
/// The model and the vocabulary: changing either makes every describe run due again.
pub const VERSION: &str = "tinyclip-8m-16-yfcc15m/tags-v1";
/// Tags less likely than this aren't stored at all: shown with their confidence, they'd
/// mostly be wrong guesses.
pub const MIN_STORED: f32 = 0.35;
/// Text searches don't return images less similar than this to the text…
pub const MIN_SEARCH_SIMILARITY: f32 = 0.2;
/// …or than the best match's similarity less this: fitting images stand out by a few
/// hundredths over the rest, whose similarities are all alike.
pub const SEARCH_WINDOW: f32 = 0.08;

/// Labels that compete, under the tag `branch`.
struct Group {
    branch: &'static str,
    kind: TagKind,
    /// Tag name, and the phrase the model is shown ("a photo of {phrase}.").
    labels: &'static [(&'static str, &'static str)],
    /// What a picture may show instead when none of the labels apply.
    nulls: &'static [&'static str],
}

/// Tuned by eye on family photos: what the model gets right often enough to be worth
/// proposing. Changing it means bumping [`VERSION`].
const VOCABULARY: &[Group] = &[
    Group {
        branch: "Scenes",
        kind: TagKind::Scene,
        labels: &[
            ("Beach", "a beach"),
            ("Mountains", "mountains"),
            ("Snow", "snow"),
            ("Forest", "a forest"),
            ("Lake", "a lake"),
            ("Ocean", "the ocean"),
            ("River", "a river"),
            ("Desert", "a desert"),
            ("Garden", "a garden"),
            ("Park", "a park"),
            ("City", "a city"),
            ("Street", "a street"),
            ("Night", "night"),
            ("Sunset", "a sunset"),
            ("Kitchen", "a kitchen"),
            ("Living room", "a living room"),
            ("Bedroom", "a bedroom"),
            ("Restaurant", "a restaurant"),
            ("Church", "a church"),
            ("Classroom", "a classroom"),
            ("Stadium", "a stadium"),
            ("Swimming pool", "a swimming pool"),
            ("Playground", "a playground"),
            ("Farm", "a farm"),
            ("Waterfall", "a waterfall"),
            ("Harbor", "a harbor"),
            ("Airport", "an airport"),
            ("Museum", "a museum"),
            ("Stage", "a stage"),
            ("Office", "an office"),
            ("Campsite", "a campsite"),
            ("Backyard", "a backyard"),
            ("Bar", "a bar"),
            ("Hospital", "a hospital"),
            ("Store", "a store"),
        ],
        nulls: &["people", "a room", "outdoors", "a house"],
    },
    Group {
        branch: "Objects",
        kind: TagKind::Object,
        labels: &[
            ("Dog", "a dog"),
            ("Cat", "a cat"),
            ("Horse", "a horse"),
            ("Bird", "a bird"),
            ("Car", "a car"),
            ("Boat", "a boat"),
            ("Airplane", "an airplane"),
            ("Train", "a train"),
            ("Bicycle", "a bicycle"),
            ("Motorcycle", "a motorcycle"),
            ("Flowers", "flowers"),
            ("Christmas tree", "a Christmas tree"),
            ("Cake", "a cake"),
            ("Food", "food"),
            ("Drinks", "drinks"),
            ("Baby", "a baby"),
            ("Child", "a child"),
            ("Teddy bear", "a teddy bear"),
            ("Guitar", "a guitar"),
            ("Piano", "a piano"),
            ("Book", "a book"),
            ("Computer", "a computer"),
            ("Phone", "a phone"),
            ("Television", "a television"),
            ("Ball", "a ball"),
            ("Balloons", "balloons"),
            ("Gifts", "gifts"),
            ("Hat", "a hat"),
            ("Sunglasses", "sunglasses"),
            ("Uniform", "a uniform"),
        ],
        nulls: &["people", "a man", "a woman", "a room", "outdoors"],
    },
    Group {
        branch: "Events",
        kind: TagKind::Event,
        labels: &[
            ("Birthday party", "a birthday party"),
            ("Wedding", "a wedding"),
            ("Christmas", "Christmas"),
            ("Halloween", "Halloween"),
            ("Graduation", "a graduation"),
            ("Concert", "a concert"),
            ("Sports game", "a sports game"),
            ("Camping", "camping"),
            ("Easter", "Easter"),
            ("Thanksgiving", "Thanksgiving dinner"),
            ("Picnic", "a picnic"),
            ("Fireworks", "fireworks"),
        ],
        nulls: &["people", "a family", "friends", "a vacation", "an ordinary day", "dinner"],
    },
    Group {
        branch: "Photo",
        kind: TagKind::Keyword,
        labels: &[
            ("Selfie", "a selfie"),
            ("Group photo", "a group photo"),
            ("Portrait", "a portrait"),
            ("Black and white", "a black and white photo"),
            ("Document", "a document"),
            ("Screenshot", "a screenshot"),
            ("Old photo", "an old photograph"),
            ("Landscape", "a landscape"),
        ],
        nulls: &["a photo", "people", "a snapshot"],
    },
];

/// A tag the model proposes for an image.
#[derive(Clone, Debug, PartialEq)]
pub struct SuggestedTag {
    /// `Scenes|Beach`.
    pub path: String,
    pub kind: TagKind,
    /// In [`MIN_STORED`]..=1.
    pub confidence: f32,
}

/// What the describe stage found in an image.
#[derive(Clone, Debug)]
pub struct Description {
    pub embedding: Embedding,
    /// Most likely first.
    pub tags: Vec<SuggestedTag>,
}

/// The model and the vocabulary's prompts, embedded once.
pub struct Describer {
    clip: Clip,
    zero_shot: ZeroShot,
}

impl Describer {
    /// Loads the model from `dir` (`visual.onnx`, `textual.onnx`) and embeds the vocabulary.
    pub fn from_dir(dir: &Path) -> Result<Self, clip::Error> {
        let clip = Clip::from_dir(dir)?;
        let groups: Vec<(Vec<&str>, &[&str])> =
            VOCABULARY.iter().map(|g| (g.labels.iter().map(|(_, phrase)| *phrase).collect(), g.nulls)).collect();
        let groups: Vec<zero_shot::Group> = groups.iter().map(|(labels, nulls)| zero_shot::Group { labels, nulls }).collect();
        let zero_shot = ZeroShot::new(&clip.text, &groups)?;
        Ok(Self { clip, zero_shot })
    }

    /// Describes the image file at `path`, upright (EXIF orientation applied).
    pub fn describe_file(&self, path: &Path) -> Result<Description, String> {
        let image = super::decode_upright(path).map_err(|e| format!("decode: {e}"))?;
        self.describe(&image)
    }

    pub fn describe(&self, image: &image::DynamicImage) -> Result<Description, String> {
        let embedding = self.clip.image.embed(image).map_err(|e| format!("clip model: {e}"))?;
        let mut tags = Vec::new();
        for (group, scores) in VOCABULARY.iter().zip(self.zero_shot.scores(&embedding)) {
            for ((name, _), confidence) in group.labels.iter().zip(scores) {
                if confidence >= MIN_STORED {
                    tags.push(SuggestedTag { path: format!("{}|{name}", group.branch), kind: group.kind, confidence });
                }
            }
        }
        tags.sort_by(|a, b| b.confidence.total_cmp(&a.confidence));
        Ok(Description { embedding, tags })
    }
}

/// Finds images by description. Needs only the text half of the model, so a host can load
/// it to search without running the pipeline.
pub struct TextSearch {
    text: TextEncoder,
}

impl TextSearch {
    /// Loads `textual.onnx` from the model directory ([`super::Models::describe`]).
    pub fn from_dir(dir: &Path) -> Result<Self, clip::Error> {
        Ok(Self { text: TextEncoder::new(dir.join("textual.onnx"))? })
    }

    /// The images that fit `text` best, best first, with their similarity, at most `limit`
    /// (see [`MIN_SEARCH_SIMILARITY`], [`SEARCH_WINDOW`]). Only images the describe stage
    /// has done are found.
    pub fn search(&self, catalog: &Catalog, text: &str, limit: usize) -> Result<Vec<(PathBuf, f32)>, String> {
        let query = self.text.embed(text).map_err(|e| format!("clip model: {e}"))?;
        let mut found = catalog.search_embedding(&query, clip::MODEL, limit).map_err(|e| e.to_string())?;
        let best = found.first().map_or(0.0, |(_, similarity)| *similarity);
        found.retain(|(_, similarity)| *similarity >= MIN_SEARCH_SIMILARITY.max(best - SEARCH_WINDOW));
        Ok(found)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_names_the_model() {
        assert!(VERSION.starts_with(clip::MODEL));
    }

    #[test]
    fn vocabulary_paths_are_unique() {
        let mut paths: Vec<String> =
            VOCABULARY.iter().flat_map(|g| g.labels.iter().map(move |(name, _)| format!("{}|{name}", g.branch).to_lowercase())).collect();
        let n = paths.len();
        paths.sort();
        paths.dedup();
        assert_eq!(paths.len(), n);
    }
}
