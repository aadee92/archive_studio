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
use std::sync::Arc;

use clip::zero_shot::{self, ZeroShot};
use clip::{Clip, Embedding, ImageEncoder, TextEncoder};

use crate::catalog::semantic::{self, Coverage, Cutoff};
use crate::catalog::{AssetId, Catalog, TagKind};

/// The model whose embeddings the stage stores and searches compare against.
pub const MODEL: &str = clip::MODEL;
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
/// What a text search keeps.
pub const TEXT_CUTOFF: Cutoff = Cutoff { min: MIN_SEARCH_SIMILARITY, window: SEARCH_WINDOW };
/// What [`similar_images`] keeps. Measured on family photos: pictures of one event are
/// mostly 0.8 up to each other, bursts and near-duplicates 0.95 up, unrelated ones around
/// 0.55; the window keeps a burst's look-alikes from drowning in the rest of the event.
pub const IMAGE_CUTOFF: Cutoff = Cutoff { min: MIN_SIMILAR_IMAGE, window: 0.2 };
/// See [`IMAGE_CUTOFF`].
pub const MIN_SIMILAR_IMAGE: f32 = 0.75;

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
    image: ImageEncoder,
    /// The text half, shared with hosts ([`text_search`](Self::text_search)).
    search: Arc<TextSearch>,
    zero_shot: ZeroShot,
}

impl Describer {
    /// Loads the model from `dir` (`visual.onnx`, `textual.onnx`) and embeds the vocabulary.
    pub fn from_dir(dir: &Path) -> Result<Self, clip::Error> {
        let Clip { image, text } = Clip::from_dir(dir)?;
        let groups: Vec<(Vec<&str>, &[&str])> =
            VOCABULARY.iter().map(|g| (g.labels.iter().map(|(_, phrase)| *phrase).collect(), g.nulls)).collect();
        let groups: Vec<zero_shot::Group> = groups.iter().map(|(labels, nulls)| zero_shot::Group { labels, nulls }).collect();
        let zero_shot = ZeroShot::new(&text, &groups)?;
        Ok(Self { image, search: Arc::new(TextSearch { text }), zero_shot })
    }

    /// Search by description with the text model loaded here, rather than a second copy.
    pub fn text_search(&self) -> Arc<TextSearch> {
        self.search.clone()
    }

    /// Describes the image file at `path`, upright (EXIF orientation applied).
    pub fn describe_file(&self, path: &Path) -> Result<Description, String> {
        let image = super::decode_upright(path).map_err(|e| format!("decode: {e}"))?;
        self.describe(&image)
    }

    pub fn describe(&self, image: &image::DynamicImage) -> Result<Description, String> {
        let embedding = self.image.embed(image).map_err(|e| format!("clip model: {e}"))?;
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
    /// (cut off by [`TEXT_CUTOFF`]). Only images the describe stage has done are found
    /// ([`Catalog::embedding_coverage`] says how many).
    pub fn search(&self, catalog: &Catalog, text: &str, limit: usize) -> Result<Vec<(PathBuf, f32)>, String> {
        let query = self.text.embed(text).map_err(|e| format!("clip model: {e}"))?;
        let found = catalog.search_embedding(&query, clip::MODEL, usize::MAX).map_err(|e| e.to_string())?;
        let found = semantic::rank(found, TEXT_CUTOFF, limit);
        log::debug!("text search {text:?}: {} found (best {:?})", found.len(), found.first().map(|(_, s)| *s));
        Ok(found)
    }
}

/// How much of the library searches by description can find: images with an embedding of
/// the current model ([`Catalog::embedding_coverage`]).
pub fn coverage(catalog: &Catalog) -> crate::catalog::Result<Coverage> {
    catalog.embedding_coverage(MODEL)
}

/// The images that look most like `asset`'s by their CLIP embeddings (made by the describe
/// stage), best first, with their similarity, at most `limit`, cut off by [`IMAGE_CUTOFF`];
/// not `asset` itself. `None` while the describe stage hasn't done `asset`.
pub fn similar_images(catalog: &Catalog, asset: AssetId, limit: usize) -> crate::catalog::Result<Option<Vec<(PathBuf, f32)>>> {
    let Some((model, embedding)) = catalog.image_embedding(asset)? else { return Ok(None) };
    let own = catalog.asset_path(asset)?;
    let mut found = catalog.search_embedding(&embedding, &model, usize::MAX)?;
    found.retain(|(path, _)| *path != own);
    let found = semantic::rank(found, IMAGE_CUTOFF, limit);
    log::debug!("images like asset {} ({}): {} found", asset.0, own.display(), found.len());
    Ok(Some(found))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn similar_images_leave_out_the_image_and_cut_off() {
        let dir = tempfile::tempdir().unwrap();
        let cat = Catalog::open(&dir.path().join("catalog.sqlite"), dir.path()).unwrap();
        let asset = |name: &str| cat.asset(&dir.path().join(name)).unwrap();
        let (beach, dunes, dog, plain) = (asset("beach.jpg"), asset("dunes.jpg"), asset("dog.jpg"), asset("plain.jpg"));
        let unit = |v: &[f32]| Embedding::new(v.to_vec());
        cat.set_image_embedding(beach, clip::MODEL, &unit(&[1.0, 0.0])).unwrap();
        cat.set_image_embedding(dunes, clip::MODEL, &unit(&[1.0, 0.3])).unwrap();
        cat.set_image_embedding(dog, clip::MODEL, &unit(&[0.0, 1.0])).unwrap();

        let found = similar_images(&cat, beach, 10).unwrap().unwrap();
        assert_eq!(found.iter().map(|(p, _)| p.file_name().unwrap().to_str().unwrap()).collect::<Vec<_>>(), ["dunes.jpg"]);
        assert!(found[0].1 > MIN_SIMILAR_IMAGE);
        assert_eq!(similar_images(&cat, plain, 10).unwrap(), None, "not described");
    }

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
