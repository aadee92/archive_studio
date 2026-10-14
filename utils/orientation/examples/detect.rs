//! cargo run --release --example detect -- <model.onnx> <image>...
fn main() {
    let mut args = std::env::args().skip(1);
    let det = orientation::OrientationDetector::new(args.next().expect("model path")).unwrap();
    for path in args {
        let img = image::open(&path).unwrap();
        let t = std::time::Instant::now();
        let p = det.detect(&img).unwrap();
        println!("{path}: rotate {}° CW ({:.1}%, {:?})", p.rotate_cw, p.confidence * 100.0, t.elapsed());
    }
}
