//! cargo run --release --example clip -- <model dir> <image>... -- <text>...
//!
//! Embeds each image and each text, and prints how well every text fits every image.
use std::time::Instant;

use clip::Clip;

fn main() {
    let mut args = std::env::args().skip(1);
    let t = Instant::now();
    let clip = Clip::from_dir(args.next().expect("model directory")).unwrap();
    println!("loaded models in {:?}", t.elapsed());

    let args: Vec<String> = args.collect();
    let split = args.iter().position(|a| a == "--").unwrap_or(args.len());
    let (images, texts) = (&args[..split], args.get(split + 1..).unwrap_or_default());

    let texts: Vec<_> = texts
        .iter()
        .map(|text| {
            let t = Instant::now();
            let e = clip.text.embed(text).unwrap();
            println!("text {text:?}: {:?}", t.elapsed());
            (text, e)
        })
        .collect();
    for path in images {
        let image = image::open(path).unwrap();
        let t = Instant::now();
        let e = clip.image.embed(&image).unwrap();
        println!("{path}: embedded in {:?}, starts {:?}", t.elapsed(), &e.as_slice()[..4]);
        let mut fits: Vec<_> = texts.iter().map(|(text, te)| (te.similarity(&e), text)).collect();
        fits.sort_by(|a, b| b.0.total_cmp(&a.0));
        for (similarity, text) in fits {
            println!("  {similarity:.4}  {text}");
        }
    }
}
