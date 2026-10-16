//! Every user operation's undo and redo, round trip: the catalog (and the files) come back
//! exactly as they were.

use std::path::PathBuf;

use faces::Embedding;

use super::*;
use crate::catalog::faces::{FaceFilter, FaceId, FaceOrder, NewFace};
use crate::catalog::{Region, TagKind};
use crate::ops;
use crate::pipeline::{self, ORIENT};

struct Lib {
    _dir: tempfile::TempDir,
    root: PathBuf,
    cat: Catalog,
}

impl Lib {
    /// A library of 40×20 PNGs, each its own colour.
    fn new(files: &[&str]) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        for (i, name) in files.iter().enumerate() {
            let mut img = image::RgbImage::from_pixel(40, 20, image::Rgb([200, 30 * i as u8, 120]));
            img.put_pixel(0, 0, image::Rgb([0, 0, 0]));
            img.save(root.join(name)).unwrap();
        }
        let cat = Catalog::open(&Catalog::default_path(&root), &root).unwrap();
        Self { _dir: dir, root, cat }
    }

    fn asset(&self, name: &str) -> AssetId {
        self.cat.asset(&self.root.join(name)).unwrap()
    }

    /// Detected faces on `asset` (with crops), at the given angles of a 2-d embedding.
    fn scan(&mut self, asset: AssetId, angles: &[f32]) -> Vec<FaceId> {
        let found: Vec<NewFace> = angles
            .iter()
            .enumerate()
            .map(|(i, deg)| NewFace {
                region: Region { x: 0.1 + 0.3 * i as f32, y: 0.1, w: 0.2, h: 0.4 },
                score: 0.9,
                embedding: Embedding::new(vec![deg.to_radians().cos(), deg.to_radians().sin()]),
                crop: vec![0xff, 0xd8, i as u8],
            })
            .collect();
        self.cat.set_faces(asset, "test", &found).unwrap();
        let faces = self.cat.faces(FaceFilter::Asset(asset), FaceOrder::Asset).unwrap();
        faces.into_iter().map(|f| f.id).collect()
    }

    /// Everything a change can touch: the journaled tables, where each asset is, the orient
    /// runs, and the files.
    fn dump(&self) -> Vec<String> {
        let mut out = Vec::new();
        let mut table = |sql: &str| {
            let mut stmt = self.cat.conn.prepare(sql).unwrap();
            let n = stmt.column_count();
            let mut rows: Vec<String> = stmt
                .query_map([], |row| Ok((0..n).map(|i| format!("{:?}", row.get::<_, Value>(i).unwrap())).collect::<Vec<_>>().join(" ")))
                .unwrap()
                .collect::<rusqlite::Result<_>>()
                .unwrap();
            rows.sort();
            out.push(format!("-- {sql}"));
            out.extend(rows);
        };
        for (name, _) in JOURNALED {
            table(&format!("SELECT * FROM {name}"));
        }
        // A file goes to a new place in the trash each time (named by when).
        table("SELECT id, CASE WHEN trashed_from IS NULL THEN path ELSE 'in the trash' END, trashed_from, missing FROM assets");
        table("SELECT asset_id, version, status, value, detail FROM stage_runs WHERE stage = 'orient'");
        let mut files: Vec<String> = walk(&self.root).into_iter().map(|p| p.strip_prefix(&self.root).unwrap().display().to_string()).collect();
        files.retain(|f| !f.starts_with(".archive-studio/catalog") && !f.starts_with(".archive-studio/trash"));
        files.sort();
        out.extend(files);
        out
    }

    /// Runs `op`, then undoes, redoes, undoes and redoes it, checking each time that the
    /// library is as it was before or after. Returns the description it was recorded under.
    fn round_trip<T>(&mut self, op: impl FnOnce(&mut Catalog) -> T) -> String {
        let before = self.dump();
        op(&mut self.cat);
        let after = self.dump();
        assert_ne!(before, after, "the operation changed something");
        let mut description = String::new();
        for _ in 0..2 {
            let step = ops::undo(&mut self.cat, None).unwrap().expect("something to undo");
            assert!(step.change.undone);
            assert_eq!(self.dump(), before, "undo of {:?}", step.change.description);
            let again = ops::redo(&mut self.cat, None).unwrap().expect("something to redo");
            assert_eq!(again.change.id, step.change.id);
            assert_eq!(self.dump(), after, "redo of {:?}", step.change.description);
            description = step.change.description;
        }
        description
    }
}

fn walk(dir: &std::path::Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir).unwrap().flatten() {
        let path = entry.path();
        if path.is_dir() {
            out.extend(walk(&path));
        } else {
            out.push(path);
        }
    }
    out
}

#[test]
fn tags_on_pictures() {
    let mut lib = Lib::new(&["a.png", "b.png", "c.png"]);
    let (a, b, c) = (lib.asset("a.png"), lib.asset("b.png"), lib.asset("c.png"));

    // A new tag and its ancestors are made, and dropped again by the undo.
    assert_eq!(lib.round_trip(|cat| ops::add_tag(cat, a, "Holidays|1998").unwrap()), "Tag a.png Holidays|1998");
    assert_eq!(lib.round_trip(|cat| ops::add_tag_all(cat, &[a, b, c], "place:Home").unwrap()), "Tag 3 items Home");
    lib.round_trip(|cat| ops::add_tag_all(cat, &[b, c], "People|Ann").unwrap());

    // A detector's tag taken off is kept as a rejection; undo lets it apply again.
    let dog = lib.cat.tag("Objects|Dog", TagKind::Object).unwrap();
    lib.cat.set_detections(b, "objects/v1", &[crate::catalog::Detection { tag: dog, confidence: 0.9, region: None }]).unwrap();
    assert_eq!(lib.round_trip(|cat| assert!(ops::remove_tag(cat, b, "objects|dog").unwrap())), "Untag b.png objects|dog");
    lib.round_trip(|cat| assert!(ops::remove_tag(cat, a, "Holidays|1998").unwrap()));

    // Taking a person's tag off un-names their face there: undo names it again.
    let faces = lib.scan(c, &[0.0]);
    ops::name_faces(&mut lib.cat, &faces, "Ann").unwrap();
    lib.round_trip(|cat| assert!(ops::remove_tag(cat, c, "ann").unwrap()));

    // Changes mark sidecars for sync, undoing them too.
    lib.cat.conn.execute("UPDATE assets SET needs_sync = 0", []).unwrap();
    ops::undo(&mut lib.cat, None).unwrap().unwrap();
    assert!(lib.cat.needing_sync().unwrap().contains(&c));
}

#[test]
fn tags_as_a_whole() {
    let mut lib = Lib::new(&["a.png", "b.png"]);
    let (a, b) = (lib.asset("a.png"), lib.asset("b.png"));
    ops::add_tag(&mut lib.cat, a, "Holidays|1998").unwrap();
    ops::add_tag(&mut lib.cat, b, "Trips|1998").unwrap();
    ops::add_tag(&mut lib.cat, b, "Trips|1999").unwrap();
    let faces = lib.scan(a, &[0.0, 90.0]);
    ops::name_faces(&mut lib.cat, &faces[..1], "Family|Al").unwrap();
    ops::name_faces(&mut lib.cat, &faces[1..], "Bob").unwrap();
    let al = lib.cat.find_tag("People|Family|Al").unwrap().unwrap().0;
    ops::set_person_cover(&mut lib.cat, al, Some(faces[0])).unwrap();

    assert_eq!(lib.round_trip(|cat| ops::rename_tag(cat, "Trips", "Journeys", false).unwrap()), "Rename tag Trips to Journeys");
    assert_eq!(lib.round_trip(|cat| ops::rename_tag(cat, "Journeys", "holidays", true).unwrap()), "Merge tag Journeys into holidays");
    // A person: renamed, merged into another (faces, tags and cover move), deleted.
    lib.round_trip(|cat| ops::rename_tag(cat, "People|Family|Al", "Alan", false).unwrap());
    lib.round_trip(|cat| ops::merge_tags(cat, "People|Family|Alan", "Bob").unwrap());
    lib.round_trip(|cat| ops::delete_tag(cat, "People|Bob").unwrap());
    assert_eq!(lib.round_trip(|cat| ops::delete_tag(cat, "Holidays").unwrap()), "Delete tag Holidays");
}

#[test]
fn people() {
    let mut lib = Lib::new(&["a.png", "b.png"]);
    let (a, b) = (lib.asset("a.png"), lib.asset("b.png"));
    let fa = lib.scan(a, &[0.0, 90.0]);
    let fb = lib.scan(b, &[5.0]);
    ops::name_faces(&mut lib.cat, &fa[..1], "Ann").unwrap();
    ops::name_faces(&mut lib.cat, &fa[1..], "Bea").unwrap();
    let ann = lib.cat.person("Ann").unwrap();
    let bea = lib.cat.person("Bea").unwrap();
    ops::reject_person(&mut lib.cat, &fb, bea).unwrap();

    assert_eq!(lib.round_trip(|cat| ops::set_person_cover(cat, ann, Some(fa[0])).unwrap()), "Set the cover of Ann");
    assert_eq!(lib.round_trip(|cat| ops::set_person_cover(cat, ann, None).unwrap()), "Cover of Ann back to automatic");
    ops::set_person_cover(&mut lib.cat, ann, Some(fa[0])).unwrap();
    assert_eq!(lib.round_trip(|cat| ops::rename_person(cat, ann, "Anna").unwrap()), "Rename Ann to Anna");
    // Onto someone who exists: a merge, with the rejections of the one merged.
    lib.round_trip(|cat| ops::rename_person(cat, bea, "Anna").unwrap());
    ops::undo(&mut lib.cat, None).unwrap();
    assert_eq!(lib.round_trip(|cat| ops::merge_people(cat, bea, ann).unwrap()), "Merge Bea into Anna");
    assert_eq!(lib.round_trip(|cat| ops::delete_person(cat, ann).unwrap()), "Delete person Anna");
    // Nothing to put back: not recorded.
    let latest = lib.cat.history(1).unwrap();
    assert_eq!(ops::sync_person_tags(&mut lib.cat).unwrap(), 0);
    assert_eq!(lib.cat.history(1).unwrap(), latest);
}

#[test]
fn faces() {
    let mut lib = Lib::new(&["a.png", "b.png"]);
    let (a, b) = (lib.asset("a.png"), lib.asset("b.png"));
    let fa = lib.scan(a, &[0.0, 90.0]);
    let fb = lib.scan(b, &[3.0, 180.0]);

    assert_eq!(lib.round_trip(|cat| ops::name_faces(cat, &fa[..1], "Ann").unwrap()), "Name a face Ann");
    let ann = lib.cat.person("Ann").unwrap();
    // Suggestions come from the pipeline, outside the journal.
    lib.cat.refresh_suggestions(0.4).unwrap();
    assert_eq!(lib.round_trip(|cat| ops::confirm_faces(cat, &fb[..1], ann).unwrap()), "Confirm a face as Ann");
    ops::undo(&mut lib.cat, None).unwrap();
    assert_eq!(lib.round_trip(|cat| ops::reject_person(cat, &fb[..1], ann).unwrap()), "Say a face is not Ann");
    assert_eq!(lib.round_trip(|cat| ops::clear_rejections(cat, &fb[..1], ann).unwrap()), "Suggest a face as Ann again");
    lib.round_trip(|cat| ops::not_person(cat, &fa[..1]).unwrap());
    assert_eq!(lib.round_trip(|cat| ops::ignore_faces(cat, &fb).unwrap()), "Ignore 2 faces");
    assert_eq!(lib.round_trip(|cat| ops::unignore_faces(cat, &fb).unwrap()), "Un-ignore 2 faces");
    // Deleted with crops, rejections, the name and the tag it carried, and a cover.
    ops::name_faces(&mut lib.cat, &fa[..1], "Ann").unwrap();
    ops::set_person_cover(&mut lib.cat, ann, Some(fa[0])).unwrap();
    assert_eq!(lib.round_trip(|cat| ops::delete_faces(cat, &[fa[0], fb[0]]).unwrap()), "Delete 2 faces");
    assert_eq!(lib.cat.face_crop(fa[0]).unwrap(), None, "redone: gone");
    ops::undo(&mut lib.cat, None).unwrap();
    assert_eq!(lib.cat.face_crop(fa[0]).unwrap(), Some(vec![0xff, 0xd8, 0]));
    assert_eq!(lib.cat.person_cover(ann).unwrap(), Some(fa[0]));
    ops::redo(&mut lib.cat, None).unwrap();
    // A face boxed by hand (its crop cut from the file).
    let region = Region { x: 0.5, y: 0.0, w: 0.25, h: 0.5 };
    assert_eq!(lib.round_trip(|cat| ops::add_face(cat, b, region, None).unwrap()), "Add a face on b.png");
}

#[test]
fn front_and_back() {
    let mut lib = Lib::new(&["photo-0001-front.png", "photo-0001-back.png", "x.png", "y.png"]);
    let [f1, b1, x, y] = ["photo-0001-front.png", "photo-0001-back.png", "x.png", "y.png"].map(|n| lib.asset(n));
    assert_eq!(lib.cat.back_of(f1).unwrap(), Some(b1));
    // Re-pairing replaces two pairs; undo puts both back.
    lib.round_trip(|cat| ops::pair(cat, x, y).unwrap());
    assert_eq!(lib.round_trip(|cat| ops::pair(cat, f1, y).unwrap()), "Pair y.png as the back of photo-0001-front.png");
    assert_eq!(lib.round_trip(|cat| ops::swap_sides(cat, y).unwrap()), "Swap the sides of y.png");
    assert_eq!(lib.round_trip(|cat| ops::unpair(cat, f1).unwrap()), "Unpair photo-0001-front.png");
    let _ = (b1, x);
}

#[test]
fn trash() {
    let mut lib = Lib::new(&["photo-0001-front.png", "photo-0001-back.png", "x.png", "y.png"]);
    let [front, back, x, y] = ["photo-0001-front.png", "photo-0001-back.png", "x.png", "y.png"].map(|n| lib.asset(n));
    ops::add_tag(&mut lib.cat, x, "Holidays").unwrap();

    // The front takes its back; undo restores both, files and all.
    assert_eq!(lib.round_trip(|cat| ops::delete(cat, front).unwrap()), "Delete photo-0001-front.png");
    assert!(lib.cat.is_trashed(back).unwrap());
    assert_eq!(lib.round_trip(|cat| ops::delete_all(cat, &[x, y])), "Delete 2 items");
    ops::delete_all(&mut lib.cat, &[x, y]);
    ops::undo(&mut lib.cat, None).unwrap();
    ops::redo(&mut lib.cat, None).unwrap();
    assert!(lib.cat.is_trashed(x).unwrap() && lib.cat.is_trashed(y).unwrap() && !lib.root.join("x.png").exists());
    assert_eq!(lib.round_trip(|cat| ops::restore_all(cat, &[x])), "Restore x.png");

    // Deleted for good: not undoable, and the changes that touched it go.
    ops::purge(&mut lib.cat, &[y]).unwrap();
    let history: Vec<String> = lib.cat.history(50).unwrap().into_iter().map(|c| c.description).collect();
    assert_eq!(history, ["Restore x.png", "Delete photo-0001-front.png", "Tag x.png Holidays"]);
    assert_eq!(ops::undo(&mut lib.cat, None).unwrap().unwrap().change.description, "Restore x.png");

    // Something else at a trashed file's place: that change can't be undone, and goes.
    image::RgbImage::new(4, 4).save(lib.root.join("photo-0001-front.png")).ok();
    let err = ops::undo(&mut lib.cat, None).unwrap_err();
    assert!(err.contains("Delete photo-0001-front.png") && err.contains("left out of the history"), "{err}");
    assert_eq!(lib.cat.next_undo().unwrap().unwrap().0.description, "Tag x.png Holidays");
}

#[test]
fn rotations() {
    let mut lib = Lib::new(&["a.png", "b.png"]);
    pipeline::catch_up(&lib.cat).unwrap();
    let a = lib.asset("a.png");
    let face = ops::add_face(&mut lib.cat, a, Region { x: 0.5, y: 0.0, w: 0.25, h: 0.5 }, None).unwrap();
    ops::name_faces(&mut lib.cat, &[face], "Ann").unwrap();
    let pixels = || image::open(lib.root.join("a.png")).unwrap().to_rgb8();
    let original = pixels();

    // Turned back losslessly, with the face box and tag region, and the orient run as it was.
    let before = lib.dump();
    ops::rotate(&lib.cat, None, a, 90).unwrap();
    assert_eq!(pixels().dimensions(), (20, 40));
    let step = ops::undo(&mut lib.cat, None).unwrap().unwrap();
    assert_eq!((step.change.description.as_str(), step.pixels), ("Rotate a.png 90°", true));
    assert_eq!(pixels(), original);
    assert_eq!(lib.dump(), before);
    ops::redo(&mut lib.cat, None).unwrap();
    assert_eq!(pixels().dimensions(), (20, 40));
    assert_eq!(lib.cat.face_boxes(a).unwrap()[0].region, Region { x: 0.5, y: 0.5, w: 0.5, h: 0.25 });
    ops::undo(&mut lib.cat, None).unwrap();
    // Many at once: one change.
    let b = lib.asset("b.png");
    let done = ops::rotate_all(&lib.cat, None, &[a, b], 180);
    assert!(done.iter().all(|(_, r)| r.is_ok()));
    let step = ops::undo(&mut lib.cat, None).unwrap().unwrap();
    assert_eq!((step.change.description.as_str(), step.paths.len()), ("Rotate 2 items 180°", 2));
    assert_eq!((lib.dump(), pixels()), (before.clone(), original.clone()));

    // A proposed rotation: accepted or kept, and back to review.
    let input = lib.cat.current_input(a).unwrap().unwrap();
    let review = runs::Run { stage: ORIENT, version: "test", input: &input, status: RunStatus::Review, value: Some(270), detail: "proposed" };
    lib.cat.set_run(a, &review).unwrap();
    let proposed = lib.dump();
    assert_eq!(ops::accept(&lib.cat, None, a).unwrap(), 270);
    let accepted = lib.dump();
    ops::undo(&mut lib.cat, None).unwrap();
    assert_eq!((lib.dump(), pixels()), (proposed.clone(), original.clone()));
    ops::redo(&mut lib.cat, None).unwrap();
    assert_eq!(lib.dump(), accepted);
    ops::undo(&mut lib.cat, None).unwrap();
    assert_eq!(lib.round_trip(|cat| ops::keep(cat, None, a).unwrap()), "Keep a.png as it is");
    ops::undo(&mut lib.cat, None).unwrap();
    assert_eq!(lib.cat.run_of(a, ORIENT).unwrap().map(|r| r.0), Some(RunStatus::Review));
}

#[test]
fn the_journal() {
    let mut lib = Lib::new(&["a.png"]);
    let a = lib.asset("a.png");
    assert!(ops::undo(&mut lib.cat, None).unwrap().is_none() && ops::redo(&mut lib.cat, None).unwrap().is_none());
    for tag in ["One", "Two", "Three"] {
        ops::add_tag(&mut lib.cat, a, tag).unwrap();
    }
    // Something that changed nothing isn't a change; a failed one neither.
    assert!(!ops::remove_tag(&mut lib.cat, a, "Nope").unwrap());
    assert!(ops::rename_tag(&mut lib.cat, "Nope", "x", false).is_err());
    let history = |cat: &Catalog| cat.history(10).unwrap().into_iter().map(|c| (c.description, c.undone)).collect::<Vec<_>>();
    assert_eq!(history(&lib.cat), [("Tag a.png Three".into(), false), ("Tag a.png Two".into(), false), ("Tag a.png One".into(), false)]);

    // Undone changes stay for redo, until a new change.
    ops::undo(&mut lib.cat, None).unwrap();
    ops::undo(&mut lib.cat, None).unwrap();
    assert_eq!(history(&lib.cat)[..2], [("Tag a.png Three".into(), true), ("Tag a.png Two".into(), true)]);
    assert_eq!(lib.cat.next_redo().unwrap().unwrap().0.description, "Tag a.png Two");
    // Kept in the catalog: another connection (or a restart) sees it.
    let other = Catalog::open(&Catalog::default_path(&lib.root), &lib.root).unwrap();
    assert_eq!(history(&other), history(&lib.cat));
    let mut other = other;
    assert_eq!(ops::redo(&mut other, None).unwrap().unwrap().change.description, "Tag a.png Two");
    ops::add_tag(&mut lib.cat, a, "Four").unwrap();
    assert!(lib.cat.next_redo().unwrap().is_none());
    let tags: Vec<String> = lib.cat.tags_of(a).unwrap().into_iter().map(|t| t.path).collect();
    assert_eq!(tags, ["Four", "One", "Two"]);
    // Only the change that is next can be applied.
    let oldest = lib.cat.history(10).unwrap().last().unwrap().id;
    assert!(lib.cat.apply_change(oldest, true).is_err());

    // Bounded.
    for i in 0..KEEP + 5 {
        ops::add_tag(&mut lib.cat, a, &format!("Many|{i}")).unwrap();
    }
    assert_eq!(lib.cat.history(KEEP * 2).unwrap().len(), KEEP);
    let count: i64 = lib.cat.conn.query_row("SELECT count(*) FROM undo_rows WHERE change_id NOT IN (SELECT id FROM undo_changes)", [], |r| r.get(0)).unwrap();
    assert_eq!(count, 0, "rows go with their change");
}

#[test]
fn a_tag_in_use_elsewhere_stays() {
    let mut lib = Lib::new(&["a.png", "b.png"]);
    let (a, b) = (lib.asset("a.png"), lib.asset("b.png"));
    ops::add_tag(&mut lib.cat, a, "Scenes|Beach").unwrap();
    // The pipeline puts the tag the user made on another picture meanwhile.
    let beach = lib.cat.find_tag("Scenes|Beach").unwrap().unwrap().0;
    lib.cat.set_detections(b, "clip", &[crate::catalog::Detection { tag: beach, confidence: 0.8, region: None }]).unwrap();
    ops::undo(&mut lib.cat, None).unwrap();
    assert!(lib.cat.tags_of(a).unwrap().is_empty());
    assert_eq!(lib.cat.tags_of(b).unwrap().len(), 1, "the detection is kept");
}
