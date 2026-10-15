//! Review's search by description, on a worker thread: the text typed in "Description" mode
//! ranked against the images' CLIP embeddings ([`TextSearch::search`]), or "Find similar"
//! from the item menu ranked against one image's ([`describe::similar_images`]). The text
//! model is the engine's if it runs in this window, else loaded here on the first search and
//! kept ([`ops::text_search`]). Requests queued behind a newer one are skipped; each result
//! carries the generation it was asked with, so the grid drops what was superseded.

use std::path::PathBuf;
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::Instant;

use archive_core::catalog::semantic::Coverage;
use archive_core::catalog::Catalog;
use archive_core::media::{classify, MediaKind};
use archive_core::ops;
use archive_core::pipeline::{describe, Engine};

use crate::pipeline::Handle;

/// At most this many results: the cutoffs keep far fewer for most searches.
pub const LIMIT: usize = 1000;

/// What a search looks for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Want {
    /// Pictures that fit this description.
    Text(String),
    /// Pictures that look like this one ("Find similar").
    Like(PathBuf),
}

/// A search's outcome.
#[derive(Debug)]
pub struct Found {
    pub generation: u64,
    pub want: Want,
    pub result: Result<Ranked, String>,
}

#[derive(Debug, Default)]
pub struct Ranked {
    /// Best first; for [`Want::Like`], the picture itself comes first.
    pub paths: Vec<PathBuf>,
    /// How many images could be found at all.
    pub coverage: Coverage,
}

pub struct Searcher {
    tx: mpsc::Sender<(u64, Want)>,
}

impl Searcher {
    /// `deliver` runs on the worker thread with each result.
    pub fn spawn(root: PathBuf, pipeline: Handle, deliver: impl Fn(Found) + Send + 'static) -> Self {
        let (tx, rx) = mpsc::channel::<(u64, Want)>();
        let _ = thread::Builder::new().name("review-describe".into()).spawn(move || {
            let mut catalog = None;
            while let Ok(mut request) = rx.recv() {
                while let Ok(newer) = rx.try_recv() {
                    log::debug!("description search #{} skipped: #{} queued behind it", request.0, newer.0);
                    request = newer;
                }
                let (generation, want) = request;
                let started = Instant::now();
                if catalog.is_none() {
                    catalog = Catalog::open(&Catalog::default_path(&root), &root).map_err(|e| log::warn!("description search: catalog: {e}")).ok();
                }
                let result = match &catalog {
                    Some(catalog) => search(catalog, || pipeline.engine(), &want),
                    None => Err("Can't open the catalog".into()),
                };
                match &result {
                    Ok(r) => log::debug!("description search #{generation} {want:?}: {} found ({:?}) in {:?}", r.paths.len(), r.coverage, started.elapsed()),
                    Err(e) => log::debug!("description search #{generation} {want:?}: {e}"),
                }
                deliver(Found { generation, want, result });
            }
        });
        Self { tx }
    }

    pub fn ask(&self, generation: u64, want: Want) {
        let _ = self.tx.send((generation, want));
    }
}

/// `engine`: the pipeline's, if it runs here, to borrow its text model.
fn search(catalog: &Catalog, engine: impl FnOnce() -> Option<Arc<Engine>>, want: &Want) -> Result<Ranked, String> {
    let coverage = describe::coverage(catalog).map_err(|e| e.to_string())?;
    let paths = match want {
        Want::Text(text) => {
            let search = ops::text_search(engine().as_deref()).map_err(|e| sentence(&e))?;
            search.search(catalog, text, LIMIT)?.into_iter().map(|(path, _)| path).collect()
        }
        Want::Like(path) => {
            let name = path.file_name().map_or_else(|| path.display().to_string(), |n| n.to_string_lossy().into_owned());
            if classify(path) != Some(MediaKind::Image) {
                return Err(format!("{name} isn't a picture: only pictures are described"));
            }
            let asset = catalog.find_asset(path).map_err(|e| e.to_string())?.ok_or_else(|| format!("{name} isn't in the catalog yet"))?;
            let found = describe::similar_images(catalog, asset, LIMIT - 1)
                .map_err(|e| e.to_string())?
                .ok_or_else(|| format!("{name} isn't described yet"))?;
            std::iter::once(path.clone()).chain(found.into_iter().map(|(path, _)| path)).collect()
        }
    };
    Ok(Ranked { paths, coverage })
}

/// `e` with a capital, as the GUI shows messages.
fn sentence(e: &str) -> String {
    let mut chars = e.chars();
    chars.next().map_or_else(String::new, |first| first.to_uppercase().chain(chars).collect())
}

/// The line under the search box once results are in: how much of the library a search can
/// find, while that isn't all of it.
pub fn note(found: &Result<Ranked, String>) -> String {
    match found {
        Err(e) => e.clone(),
        Ok(Ranked { coverage, .. }) if coverage.images > 0 && coverage.described < coverage.images => {
            format!("{} of {} images described so far; only those are searched", coverage.described, coverage.images)
        }
        Ok(_) => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_similar_puts_the_picture_first() {
        use archive_core::catalog::semantic::{Coverage, Embedding};
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let catalog = Catalog::open(&Catalog::default_path(root), root).unwrap();
        let image = |name: &str, v: Option<&[f32]>| {
            let path = root.join(name);
            let (asset, _) = catalog.record_file(&path, MediaKind::Image, 1, 1).unwrap();
            if let Some(v) = v {
                catalog.set_image_embedding(asset, describe::MODEL, &Embedding::new(v.to_vec())).unwrap();
            }
            path
        };
        let beach = image("beach.jpg", Some(&[1.0, 0.0]));
        let dunes = image("dunes.jpg", Some(&[1.0, 0.2]));
        image("dog.jpg", Some(&[0.0, 1.0]));
        let new = image("new.jpg", None);

        let found = search(&catalog, || None, &Want::Like(beach.clone())).unwrap();
        assert_eq!(found.paths, [beach, dunes]);
        assert_eq!(found.coverage, Coverage { described: 3, images: 4 });
        assert_eq!(search(&catalog, || None, &Want::Like(new)).unwrap_err(), "new.jpg isn't described yet");
        let video = search(&catalog, || None, &Want::Like(root.join("clip.mp4"))).unwrap_err();
        assert!(video.contains("only pictures"), "{video}");
    }

    #[test]
    fn notes() {
        let ranked = |described, images| Ok(Ranked { paths: Vec::new(), coverage: Coverage { described, images } });
        assert_eq!(note(&ranked(3, 10)), "3 of 10 images described so far; only those are searched");
        assert_eq!(note(&ranked(10, 10)), "");
        assert_eq!(note(&ranked(0, 0)), "");
        assert_eq!(note(&Err("No model".into())), "No model");
        assert_eq!(sentence("description search needs it"), "Description search needs it");
    }
}
