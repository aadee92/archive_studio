//! cargo run --release --example faces -- <buffalo_l dir> [--crops <dir>] <image>...
//!
//! Detects and embeds the faces in each image, then clusters them all into people.
use std::{path::PathBuf, time::Instant};

use faces::{Faces, cluster};

fn main() {
    let mut args = std::env::args().skip(1);
    if let Ok(n) = std::env::var("FACES_THREADS") {
        faces::set_threads(n.parse().expect("FACES_THREADS is not a number"));
    }
    let t = Instant::now();
    let faces = Faces::from_dir(args.next().expect("model directory")).unwrap();
    println!("loaded models in {:?}", t.elapsed());

    let mut crops = None;
    let mut found = Vec::new();
    while let Some(arg) = args.next() {
        if arg == "--crops" {
            let dir = PathBuf::from(args.next().expect("crop directory"));
            std::fs::create_dir_all(&dir).unwrap();
            crops = Some(dir);
            continue;
        }
        let image = image::open(&arg).unwrap().to_rgb8();
        let t = Instant::now();
        let detections = faces.detector.detect(&image).unwrap();
        let detect = t.elapsed();
        for (i, d) in detections.into_iter().enumerate() {
            let crop = faces::align::align(&image, &d.landmarks);
            let t = Instant::now();
            let embedding = faces.embedder.embed(&crop).unwrap();
            let embed = t.elapsed();
            let [x1, y1, x2, y2] = d.bbox.map(|v| v.round() as i32);
            println!(
                "{arg} #{i}: score {:.2} box ({x1},{y1})-({x2},{y2}) detect {detect:?} embed {embed:?}",
                d.score
            );
            if let Some(dir) = &crops {
                crop.save(dir.join(format!("{}.png", found.len()))).unwrap();
            }
            found.push((format!("{arg} #{i}"), embedding));
        }
    }

    let embeddings: Vec<_> = found.iter().map(|(_, e)| e.clone()).collect();
    let labels = cluster::cluster(&embeddings, &cluster::Params { min_faces: 2, ..Default::default() });
    let people = labels.iter().flatten().max().map_or(0, |m| m + 1);
    for person in 0..people {
        println!("person {person}:");
        for ((name, _), _) in found.iter().zip(&labels).filter(|(_, l)| **l == Some(person)) {
            println!("  {name}");
        }
    }
    println!("unassigned:");
    for ((name, _), _) in found.iter().zip(&labels).filter(|(_, l)| l.is_none()) {
        println!("  {name}");
    }
}
