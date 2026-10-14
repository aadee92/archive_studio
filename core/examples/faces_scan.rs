//! cargo run --release --example faces_scan -- <buffalo_l dir> <library root> <image>...
//!
//! Scans images for faces on all cores into the library's catalog, clusters the unreviewed
//! faces and lists the clusters. A stand-in for the CLI's scan command.
use std::{path::PathBuf, sync::Mutex, time::Instant};

use archive_core::catalog::{faces as catalog_faces, Catalog};
use archive_core::faces::{cluster, Faces};

fn main() {
    let mut args = std::env::args().skip(1);
    let models = Faces::from_dir(args.next().expect("model directory")).unwrap();
    let root = PathBuf::from(args.next().expect("library root"));
    let mut catalog = Catalog::open(&Catalog::default_path(&root), &root).unwrap();
    let images: Vec<PathBuf> = args.map(PathBuf::from).collect();

    // One image per thread; results go to the single writer as they come.
    let t = Instant::now();
    let next = Mutex::new(images.iter());
    let (send, receive) = std::sync::mpsc::channel();
    std::thread::scope(|s| {
        for _ in 0..std::thread::available_parallelism().map_or(1, |n| n.get()) {
            let (models, next, send) = (&models, &next, send.clone());
            s.spawn(move || {
                loop {
                    // Its own statement, so the lock is released before the scan.
                    let Some(path) = next.lock().unwrap().next() else { break };
                    send.send((path, catalog_faces::scan_image(models, path))).unwrap();
                }
            });
        }
        drop(send);
        for (path, found) in receive {
            match found {
                Ok(found) => {
                    let asset = catalog.asset(path).unwrap();
                    catalog.set_faces(asset, catalog_faces::MODEL, &found).unwrap();
                    println!("{}: {} faces", path.display(), found.len());
                }
                Err(e) => eprintln!("{e}"),
            }
        }
    });
    println!("scanned {} images in {:?}", images.len(), t.elapsed());

    let params = cluster::Params { min_faces: 2, ..Default::default() };
    println!("{} clusters", catalog.cluster_faces(&params).unwrap());
    for (cluster, size) in catalog.face_clusters().unwrap() {
        println!("cluster {cluster}: {size} faces");
    }
}
