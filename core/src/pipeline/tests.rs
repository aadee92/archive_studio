use std::sync::mpsc;
use std::time::{Duration, Instant};

use super::*;
use crate::catalog::{Region, TagKind};

fn write_png(path: &Path, width: u32, height: u32) {
    let file = std::io::BufWriter::new(File::create(path).unwrap());
    let mut png = png::Encoder::new(file, width, height);
    png.set_color(png::ColorType::Rgb);
    let mut writer = png.write_header().unwrap();
    writer.write_image_data(&vec![128; (width * height * 3) as usize]).unwrap();
    writer.finish().unwrap();
}

fn png_size(path: &Path) -> (u32, u32) {
    let reader = png::Decoder::new(std::io::BufReader::new(File::open(path).unwrap())).read_info().unwrap();
    (reader.info().width, reader.info().height)
}

fn open(root: &Path) -> Catalog {
    Catalog::open(&Catalog::default_path(root), root).unwrap()
}

#[test]
fn catch_up_finds_new_changed_and_missing_files() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::create_dir(root.join("box")).unwrap();
    write_png(&root.join("a.png"), 4, 2);
    write_png(&root.join("box/b.png"), 4, 2);
    std::fs::write(root.join("notes.txt"), "not media").unwrap();
    std::fs::write(root.join(".hidden.png"), "ignored").unwrap();
    let catalog = open(root);

    assert_eq!(catch_up(&catalog).unwrap(), (2, 0, 0));
    assert_eq!(catch_up(&catalog).unwrap(), (0, 0, 0));

    write_png(&root.join("a.png"), 8, 2);
    std::fs::remove_file(root.join("box/b.png")).unwrap();
    std::fs::write(root.join(".a.png.rotating"), "left over").unwrap();
    assert_eq!(catch_up(&catalog).unwrap(), (0, 1, 1));
    assert!(!root.join(".a.png.rotating").exists());

    // Back again: changed, not added (the asset was kept).
    write_png(&root.join("box/b.png"), 4, 2);
    assert_eq!(catch_up(&catalog).unwrap(), (0, 1, 0));
}

#[test]
fn stages_are_due_until_run_on_the_current_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.png");
    write_png(&path, 4, 2);
    let catalog = open(dir.path());
    catch_up(&catalog).unwrap();

    let both = Availability { orient: true, faces: true };
    let due = catalog.due(&orient_query(), 10).unwrap();
    assert_eq!(due.len(), 1);
    // Faces wait for orient.
    assert!(catalog.due(&faces_query(both), 10).unwrap().is_empty());
    // ... unless there is no orientation model.
    assert_eq!(catalog.due(&faces_query(Availability { orient: false, faces: true }), 10).unwrap().len(), 1);

    let Due { asset, input, .. } = due[0].clone();
    let done = Run { stage: ORIENT, version: ORIENT_VERSION, input: &input, status: RunStatus::Done, value: Some(0), detail: "" };
    catalog.set_run(asset, &done).unwrap();
    assert!(catalog.due(&orient_query(), 10).unwrap().is_empty());
    assert_eq!(catalog.due(&faces_query(both), 10).unwrap().len(), 1);
    let counts = catalog.run_counts(&orient_query()).unwrap();
    assert_eq!((counts.done, counts.pending), (1, 0));

    // A newer model makes it due again, but never over the user's decision.
    let newer = StageQuery { version: "newer", ..orient_query() };
    assert_eq!(catalog.due(&newer, 10).unwrap().len(), 1);
    catalog.set_run(asset, &Run { status: RunStatus::User, ..done.clone() }).unwrap();
    assert!(catalog.due(&newer, 10).unwrap().is_empty());

    // A failure isn't retried on the same file...
    catalog.set_run(asset, &Run { status: RunStatus::Failed, ..done.clone() }).unwrap();
    assert!(catalog.due(&orient_query(), 10).unwrap().is_empty());
    assert_eq!(catalog.run_counts(&orient_query()).unwrap().failed, 1);
    // ... but is after a change to it, or when asked.
    write_png(&path, 6, 2);
    catch_up(&catalog).unwrap();
    assert_eq!(catalog.due(&orient_query(), 10).unwrap().len(), 1);
    catalog.set_run(asset, &Run { status: RunStatus::Failed, input: &catalog.current_input(asset).unwrap().unwrap(), ..done }).unwrap();
    assert_eq!(catalog.clear_failed(None).unwrap(), 1);
    assert_eq!(catalog.due(&orient_query(), 10).unwrap().len(), 1);

    // Missing files aren't due.
    std::fs::remove_file(&path).unwrap();
    catch_up(&catalog).unwrap();
    assert!(catalog.due(&orient_query(), 10).unwrap().is_empty());
}

#[test]
fn failures_are_listed_and_retried() {
    use crate::catalog::runs::{Failure, Retry};
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write_png(&root.join("a.png"), 4, 2);
    write_png(&root.join("b.png"), 4, 2);
    let mut catalog = open(root);
    catch_up(&catalog).unwrap();
    let id = |name: &str| catalog.find_asset(&root.join(name)).unwrap().unwrap();
    let (a, b) = (id("a.png"), id("b.png"));
    let fail = |asset, stage: &str, input: &str| {
        let run = Run { stage, version: ORIENT_VERSION, input, status: RunStatus::Failed, value: None, detail: "broken" };
        catalog.set_run(asset, &run).unwrap();
    };
    let input = |asset| catalog.current_input(asset).unwrap().unwrap();
    fail(a, FACES, &input(a));
    fail(b, FACES, &input(b));
    fail(a, ORIENT, &input(a));
    fail(b, ORIENT, "1:2"); // on an older version of the file: doesn't count
    let listed = |catalog: &Catalog| -> Vec<(AssetId, String)> {
        catalog.failures().unwrap().into_iter().map(|f: Failure| (f.asset, f.stage)).collect()
    };
    assert_eq!(listed(&catalog), [(a, FACES.into()), (b, FACES.into()), (a, ORIENT.into())], "by stage, then path");
    let first = &catalog.failures().unwrap()[0];
    assert_eq!((first.path.clone(), first.error.as_str()), (root.join("a.png"), "broken"));
    assert!(first.at > 0);

    assert_eq!(catalog.retry(&Retry::Runs(vec![(a, FACES.into()), (a, SIGNATURE.into())])).unwrap(), 1);
    assert_eq!(listed(&catalog), [(b, FACES.into()), (a, ORIENT.into())]);
    assert_eq!(catalog.due(&orient_query(), 10).unwrap().len(), 1, "only the one failed on an older file");
    assert_eq!(catalog.retry(&Retry::Stage(ORIENT.into())).unwrap(), 2, "the stale one goes too");
    assert_eq!(catalog.due(&orient_query(), 10).unwrap().len(), 2);
    assert_eq!(listed(&catalog), [(b, FACES.into())]);

    // Trashed files aren't listed.
    catalog.trash(b).unwrap();
    assert!(catalog.failures().unwrap().is_empty());
    assert_eq!(catalog.retry(&Retry::All).unwrap(), 1);
}

#[test]
fn regions_turn_with_the_image() {
    let dir = tempfile::tempdir().unwrap();
    let catalog = open(dir.path());
    let asset = catalog.asset(&dir.path().join("a.png")).unwrap();
    let tag = catalog.tag("People|Alice", TagKind::Person).unwrap();
    let region = Region { x: 0.1, y: 0.2, w: 0.3, h: 0.4 };
    catalog.add_tag(asset, tag, Some(region)).unwrap();
    let turned = |catalog: &Catalog| catalog.tags_of(asset).unwrap()[0].region.unwrap();
    let close = |a: Region, b: Region| (a.x - b.x).abs() + (a.y - b.y).abs() + (a.w - b.w).abs() + (a.h - b.h).abs() < 1e-5;

    catalog.rotate_regions(asset, 90).unwrap();
    assert!(close(turned(&catalog), Region { x: 0.4, y: 0.1, w: 0.4, h: 0.3 }));
    catalog.rotate_regions(asset, 270).unwrap();
    assert!(close(turned(&catalog), region));
    catalog.rotate_regions(asset, 180).unwrap();
    assert!(close(turned(&catalog), Region { x: 0.6, y: 0.4, w: 0.3, h: 0.4 }));
}

/// Waits for an event `matches` accepts, failing after a few seconds.
fn expect(events: &mpsc::Receiver<Event>, what: &str, matches: impl Fn(&Event) -> bool) -> Event {
    let deadline = Instant::now() + Duration::from_secs(10);
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        match events.recv_timeout(left) {
            Ok(event) if matches(&event) => return event,
            Ok(_) => {}
            Err(_) => break,
        }
    }
    panic!("no {what} event");
}

#[test]
fn engine_watches_rotates_and_holds_the_library() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    write_png(&root.join("old.png"), 4, 2);
    let (send, events) = mpsc::channel();
    let send = Mutex::new(send);
    let config = Config { threads: 2, models: Models::default() };
    let engine = Engine::start(&root, config.clone(), move |event| send.lock().unwrap().send(event).unwrap()).unwrap();

    // Models load first; without any, only registering and sidecars run.
    assert_eq!(expect(&events, "ready", |e| matches!(e, Event::Ready(_))).clone_ready(), Availability::default());
    expect(&events, "catch-up", |e| matches!(e, Event::CatchUp { added: 1, .. }));
    assert!(Engine::start(&root, config, |_| {}).is_err(), "a second engine on the same library");

    // A file copied in is taken in once it settles.
    let new = root.join("new.png");
    write_png(&new, 6, 2);
    expect(&events, "new file", |e| matches!(e, Event::Updated(p) if *p == new));

    // The user's rotation rewrites the file and isn't reported back as a change.
    engine.rotate(&new, 90).unwrap();
    assert_eq!(png_size(&new), (2, 6));
    expect(&events, "rotation", |e| matches!(e, Event::Rotated { by_user: true, cw: 90, .. }));
    let catalog = open(&root);
    let asset = catalog.find_asset(&new).unwrap().unwrap();
    assert!(matches!(catalog.run_of(asset, ORIENT).unwrap(), Some((RunStatus::User, Some(90), _))));
    assert!(catalog.current_input(asset).unwrap().is_some());
    assert!(engine.rotate(root.join("nope.png"), 90).is_err());

    // Gone files are marked missing.
    std::fs::remove_file(root.join("old.png")).unwrap();
    expect(&events, "removal", |e| matches!(e, Event::Updated(p) if p.ends_with("old.png")));
    let old = catalog.file_state(&root.join("old.png")).unwrap().unwrap();
    assert!(old.missing);

    drop(engine);
    // The lock went with it.
    let again = Engine::start(&root, Config { threads: 1, models: Models::default() }, |_| {});
    assert!(again.is_ok());
}

#[test]
fn the_engine_signs_images_and_finds_near_duplicates() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let picture = |w: u32, h: u32, flip: bool| {
        image::RgbImage::from_fn(w, h, |x, y| {
            let x = if flip { w - 1 - x } else { x };
            let v = (x * 200 / w + y * 50 / h) as u8;
            image::Rgb([v, v / 2, 255 - v])
        })
    };
    picture(160, 120, false).save(root.join("a.png")).unwrap();
    picture(80, 60, false).save(root.join("a-small.png")).unwrap();
    picture(160, 120, true).save(root.join("b.png")).unwrap();
    let (send, events) = mpsc::channel();
    let send = Mutex::new(send);
    let engine =
        Engine::start(&root, Config { threads: 2, models: Models::default() }, move |e| send.lock().unwrap().send(e).unwrap()).unwrap();
    expect(&events, "idle", |e| matches!(e, Event::Idle));
    drop(engine);

    let catalog = open(&root);
    let id = |name: &str| catalog.find_asset(&root.join(name)).unwrap().unwrap();
    let all = [id("a.png"), id("a-small.png"), id("b.png")];
    let counts = signature_status(&catalog, Availability::default()).unwrap();
    assert_eq!((counts.done, counts.pending), (3, 0));
    let found = catalog.similar(all[0], &all, 5).unwrap();
    assert_eq!(found.iter().map(|f| f.0).collect::<Vec<_>>(), [all[1]]);
}

#[test]
fn the_engine_reads_when_pictures_were_taken() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    // A PNG whose eXIf chunk says 1987:06:05 04:03:02, and one without.
    let dated = root.join("dated.png");
    let mut png = png::Encoder::new(std::io::BufWriter::new(File::create(&dated).unwrap()), 4, 2);
    png.set_color(png::ColorType::Rgb);
    let mut writer = png.write_header().unwrap();
    writer.write_chunk(png::chunk::eXIf, &crate::exif::tests::sample()[6..]).unwrap();
    writer.write_image_data(&[128; 4 * 2 * 3]).unwrap();
    writer.finish().unwrap();
    write_png(&root.join("plain.png"), 4, 2);
    assert_eq!(read_taken(&dated).unwrap(), Some(549_864_182));
    assert_eq!(read_taken(&root.join("plain.png")).unwrap(), None);

    let (send, events) = mpsc::channel();
    let send = Mutex::new(send);
    let engine =
        Engine::start(&root, Config { threads: 2, models: Models::default() }, move |e| send.lock().unwrap().send(e).unwrap()).unwrap();
    // Front ends hear of the date (an update of the file).
    expect(&events, "dated", |e| matches!(e, Event::Updated(p) if *p == dated));
    expect(&events, "idle", |e| matches!(e, Event::Idle));
    drop(engine);

    let catalog = open(&root);
    let id = |name: &str| catalog.find_asset(&root.join(name)).unwrap().unwrap();
    assert_eq!(catalog.taken(id("dated.png")).unwrap(), Some(549_864_182));
    assert_eq!(catalog.taken(id("plain.png")).unwrap(), None);
    let counts = date_status(&catalog).unwrap();
    assert_eq!((counts.done, counts.pending, counts.failed), (2, 0, 0));
    let run = catalog.run_of(id("dated.png"), DATE).unwrap().unwrap();
    assert_eq!((run.0, run.1, run.2.as_str()), (RunStatus::Done, Some(549_864_182), "taken 1987-06-05 04:03"));
}

impl Event {
    fn clone_ready(&self) -> Availability {
        match self {
            Event::Ready(a) => *a,
            _ => unreachable!(),
        }
    }
}
