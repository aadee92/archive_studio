use std::path::{Path, PathBuf};

use super::query::{parse, Expr};
use super::*;

struct Lib {
    dir: tempfile::TempDir,
    cat: Catalog,
}

impl Lib {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let cat = Catalog::open_in_memory(dir.path());
        Self { dir, cat }
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.dir.path().join(rel)
    }

    fn asset(&self, rel: &str) -> AssetId {
        self.cat.asset(&self.path(rel)).unwrap()
    }

    fn tag(&self, asset: AssetId, tag: &str, kind: TagKind) -> TagId {
        let tag = self.cat.tag(tag, kind).unwrap();
        self.cat.add_tag(asset, tag, None).unwrap();
        tag
    }

    /// Matching file names relative to the library, in order.
    fn search(&self, query: &str) -> Vec<String> {
        self.cat
            .search(query, DEFAULT_MIN_CONFIDENCE)
            .unwrap()
            .iter()
            .map(|p| p.strip_prefix(self.dir.path()).unwrap().to_str().unwrap().to_owned())
            .collect()
    }
}

/// beach.jpg: Scenes|Beach, People|Family|Alice
/// park.jpg: Scenes|Park, Objects|Dog, People|Family|Bob
/// lake.jpg: Scenes|Lake, Objects|Dog
/// blank.jpg: no tags
fn sample() -> Lib {
    let lib = Lib::new();
    let beach = lib.asset("beach.jpg");
    lib.tag(beach, "Scenes|Beach", TagKind::Scene);
    lib.tag(beach, "People|Family|Alice", TagKind::Person);
    let park = lib.asset("park.jpg");
    lib.tag(park, "Scenes|Park", TagKind::Scene);
    lib.tag(park, "Objects|Dog", TagKind::Object);
    lib.tag(park, "People|Family|Bob", TagKind::Person);
    let lake = lib.asset("lake.jpg");
    lib.tag(lake, "Scenes|Lake", TagKind::Scene);
    lib.tag(lake, "Objects|Dog", TagKind::Object);
    lib.asset("blank.jpg");
    lib
}

#[test]
fn search_terms_and_operators() {
    let lib = sample();
    assert_eq!(lib.search(""), ["beach.jpg", "blank.jpg", "lake.jpg", "park.jpg"]);
    assert_eq!(lib.search("dog"), ["lake.jpg", "park.jpg"]);
    assert_eq!(lib.search("DOG park"), ["park.jpg"]);
    assert_eq!(lib.search("dog and park"), ["park.jpg"]);
    assert_eq!(lib.search("(dog and park)"), ["park.jpg"]);
    assert_eq!(lib.search("dog && park"), ["park.jpg"]);
    assert_eq!(lib.search("dog -park"), ["lake.jpg"]);
    assert_eq!(lib.search("dog and not park"), ["lake.jpg"]);
    assert_eq!(lib.search("beach OR lake"), ["beach.jpg", "lake.jpg"]);
    assert_eq!(lib.search("beach OR lake dog"), ["beach.jpg", "lake.jpg"]);
    assert_eq!(lib.search("(beach OR lake) and dog"), ["lake.jpg"]);
    assert_eq!(lib.search("(beach OR lake) AND dog"), ["lake.jpg"]);
    assert_eq!(lib.search("(beach OR park) -alice"), ["park.jpg"]);
    assert_eq!(lib.search("-*"), ["blank.jpg"]);
    assert_eq!(lib.search("nothing"), Vec::<String>::new());
}

#[test]
fn in_memory_query_matches() {
    let tags = vec![
        ("Scenes|Beach".to_string(), Some(TagKind::Scene)),
        ("People|Family|Alice".to_string(), Some(TagKind::Person)),
        ("1999_wedding".to_string(), Some(TagKind::Keyword)),
    ];
    assert!(parse("").unwrap().matches(&tags));
    assert!(parse("beach").unwrap().matches(&tags));
    assert!(!parse("dog").unwrap().matches(&tags));
    assert!(parse("beach OR dog").unwrap().matches(&tags));
    assert!(parse("beach -dog").unwrap().matches(&tags));
    assert!(parse("(beach and alice)").unwrap().matches(&tags));
    assert!(!parse("(beach and dog)").unwrap().matches(&tags));
    assert!(parse("(beach and not dog)").unwrap().matches(&tags));
    assert!(parse("(beach OR dog) and alice").unwrap().matches(&tags));
    assert!(parse("beach && alice").unwrap().matches(&tags));
    assert!(parse("person:alice").unwrap().matches(&tags));
    assert!(!parse("scene:alice").unwrap().matches(&tags));
    assert!(parse("wedding").unwrap().matches(&tags));
    assert!(parse("family|alice").unwrap().matches(&tags));
}

#[test]
fn search_hierarchy_and_kinds() {
    let lib = sample();
    // Any run of whole segments matches, so ancestors find their descendants.
    assert_eq!(lib.search("people"), ["beach.jpg", "park.jpg"]);
    assert_eq!(lib.search("family"), ["beach.jpg", "park.jpg"]);
    assert_eq!(lib.search("family|alice"), ["beach.jpg"]);
    assert_eq!(lib.search("\"People | Family | Bob\""), ["park.jpg"]);
    // Segments match whole, not as substrings.
    assert_eq!(lib.search("ali"), Vec::<String>::new());
    assert_eq!(lib.search("person:*"), ["beach.jpg", "park.jpg"]);
    assert_eq!(lib.search("person:dog"), Vec::<String>::new());
    assert_eq!(lib.search("object:dog"), ["lake.jpg", "park.jpg"]);
    // An unknown prefix is part of the tag name.
    let odd = lib.asset("odd.jpg");
    lib.tag(odd, "Ratio:16x9", TagKind::Keyword);
    assert_eq!(lib.search("ratio:16x9"), ["odd.jpg"]);
}

#[test]
fn like_wildcards_are_literal() {
    let lib = Lib::new();
    let a = lib.asset("a.jpg");
    lib.tag(a, "100% cotton", TagKind::Keyword);
    lib.asset("b.jpg");
    assert_eq!(lib.search("\"100% cotton\""), ["a.jpg"]);
    assert_eq!(lib.search("\"1_0% cotton\""), Vec::<String>::new());
    assert_eq!(lib.search("%"), Vec::<String>::new());
}

#[test]
fn detections_confidence_and_rejection() {
    let mut lib = Lib::new();
    let a = lib.asset("a.jpg");
    let dog = lib.cat.tag("Objects|Dog", TagKind::Object).unwrap();
    let cat = lib.cat.tag("Objects|Cat", TagKind::Object).unwrap();
    let det = |tag, confidence| Detection { tag, confidence, region: None };
    lib.cat.set_detections(a, "objects/v1", &[det(dog, 0.9), det(cat, 0.2)]).unwrap();
    assert_eq!(lib.search("dog"), ["a.jpg"]);
    assert_eq!(lib.search("cat"), Vec::<String>::new());
    assert_eq!(lib.cat.search("cat", 0.1).unwrap().len(), 1);

    // Re-running replaces that detector's rows.
    lib.cat.set_detections(a, "objects/v1", &[det(cat, 0.8)]).unwrap();
    assert_eq!(lib.search("dog"), Vec::<String>::new());
    assert_eq!(lib.search("cat"), ["a.jpg"]);

    // The user removes it; the detector can't bring it back, but the user can.
    lib.cat.remove_tag(a, cat).unwrap();
    assert_eq!(lib.search("cat"), Vec::<String>::new());
    lib.cat.set_detections(a, "objects/v1", &[det(cat, 0.95)]).unwrap();
    assert_eq!(lib.search("cat"), Vec::<String>::new());
    assert!(lib.cat.tags_of(a).unwrap().is_empty());
    lib.cat.add_tag(a, cat, None).unwrap();
    assert_eq!(lib.search("cat"), ["a.jpg"]);
    let tags = lib.cat.tags_of(a).unwrap();
    assert_eq!(tags.len(), 1, "the user's row hides the detector's");
    assert_eq!((tags[0].source.clone(), tags[0].confidence), (Source::User, None));

    // Removing a tag only the user put there deletes it outright.
    let b = lib.asset("b.jpg");
    lib.cat.add_tag(b, dog, None).unwrap();
    lib.cat.remove_tag(b, dog).unwrap();
    lib.cat.set_detections(b, "objects/v1", &[det(dog, 0.9)]).unwrap();
    assert_eq!(lib.search("dog"), ["b.jpg"]);

    assert!(lib.cat.set_detections(a, "user", &[]).is_err());
    assert!(lib.cat.set_detections(a, "x", &[det(dog, 1.5)]).is_err());
}

#[test]
fn detector_reporting_a_tag_twice_keeps_the_best() {
    let mut lib = Lib::new();
    let a = lib.asset("a.jpg");
    let dog = lib.cat.tag("Dog", TagKind::Object).unwrap();
    let region = |x| Some(Region { x, y: 0.0, w: 0.1, h: 0.1 });
    let found = [
        Detection { tag: dog, confidence: 0.6, region: region(0.1) },
        Detection { tag: dog, confidence: 0.9, region: region(0.5) },
        Detection { tag: dog, confidence: 0.7, region: region(0.8) },
    ];
    lib.cat.set_detections(a, "objects/v1", &found).unwrap();
    let tags = lib.cat.tags_of(a).unwrap();
    assert_eq!(tags.len(), 1);
    assert_eq!(tags[0].confidence, Some(0.9));
    assert_eq!(tags[0].region, region(0.5));
    assert_eq!(tags[0].source, Source::Detector("objects/v1".into()));
}

#[test]
fn tags_are_case_insensitive_and_typed() {
    let lib = Lib::new();
    let alice = lib.cat.tag(" People |Alice", TagKind::Person).unwrap();
    assert_eq!(lib.cat.tag("people|ALICE", TagKind::Person).unwrap(), alice);
    assert_eq!(lib.cat.find_tag("PEOPLE|alice").unwrap(), Some((alice, "People|Alice".into(), TagKind::Person)));
    assert!(matches!(lib.cat.tag("People|Alice", TagKind::Object), Err(Error::Invalid(_))));
    for bad in ["", "|", "People||Alice", "People| "] {
        assert!(matches!(lib.cat.tag(bad, TagKind::Keyword), Err(Error::Invalid(_))), "{bad:?}");
    }
}

#[test]
fn leaf_tags_skip_ancestors_and_other_kinds() {
    let lib = Lib::new();
    lib.cat.tag("People|Family", TagKind::Person).unwrap();
    // Paths match ignoring case, so this is under "People|Family" too.
    let alice = lib.cat.tag("people|FAMILY|Alice", TagKind::Person).unwrap();
    let bob = lib.cat.tag("People|Bob", TagKind::Person).unwrap();
    // A sibling sharing the prefix doesn't make "Family" an ancestor of it.
    let friends = lib.cat.tag("People|Family friends", TagKind::Person).unwrap();
    lib.cat.tag("Scenes|Beach", TagKind::Scene).unwrap();
    assert_eq!(
        lib.cat.leaf_tags(TagKind::Person).unwrap(),
        [(bob, "People|Bob".into()), (friends, "People|Family friends".into()), (alice, "people|FAMILY|Alice".into())]
    );
}

#[test]
fn rename_tag_moves_its_subtree() {
    let mut lib = sample();
    // Ancestors exist as tags of their own.
    let (family, _, kind) = lib.cat.find_tag("people|family").unwrap().unwrap();
    assert_eq!(kind, TagKind::Person);
    // A sibling sharing the prefix must not move.
    let families = lib.asset("other.jpg");
    lib.tag(families, "People|Family friends|Carol", TagKind::Person);
    lib.cat.rename_tag(family, "Relatives").unwrap();
    assert_eq!(lib.search("relatives|alice"), ["beach.jpg"]);
    assert_eq!(lib.search("family"), Vec::<String>::new());
    assert_eq!(lib.search("\"family friends\""), ["other.jpg"]);

    // Taken path: nothing changes.
    let bob = lib.cat.find_tag("Relatives|Bob").unwrap().unwrap().0;
    assert!(lib.cat.rename_tag(bob, "relatives|alice").is_err());
    assert_eq!(lib.search("relatives|bob"), ["park.jpg"]);
}

#[test]
fn sync_flag_follows_tag_changes() {
    let mut lib = Lib::new();
    let a = lib.asset("a.jpg");
    let b = lib.asset("b.jpg");
    assert!(lib.cat.needing_sync().unwrap().is_empty());
    let dog = lib.tag(a, "Dog", TagKind::Object);
    assert_eq!(lib.cat.needing_sync().unwrap(), [a]);
    lib.cat.mark_synced(a);
    assert!(lib.cat.needing_sync().unwrap().is_empty());

    lib.cat.set_detections(b, "objects/v1", &[Detection { tag: dog, confidence: 0.9, region: None }]).unwrap();
    assert_eq!(lib.cat.needing_sync().unwrap(), [b]);
    lib.cat.mark_synced(b);

    lib.cat.rename_tag(dog, "Animals|Dog").unwrap();
    assert_eq!(lib.cat.needing_sync().unwrap(), [a, b]);
    lib.cat.mark_synced(a);
    lib.cat.mark_synced(b);

    lib.cat.remove_tag(a, dog).unwrap();
    assert_eq!(lib.cat.needing_sync().unwrap(), [a]);
}

#[test]
fn asset_paths() {
    let lib = Lib::new();
    std::fs::create_dir(lib.path("sub")).unwrap();
    let a = lib.asset("sub/a.jpg");
    assert_eq!(lib.cat.asset(&lib.path("sub/./a.jpg")).unwrap(), a);
    assert_eq!(lib.cat.asset(&lib.path("sub/../sub/a.jpg")).unwrap(), a);
    assert_eq!(lib.cat.asset_path(a).unwrap(), lib.path("sub/a.jpg"));
    assert_eq!(lib.cat.find_asset(&lib.path("nope.jpg")).unwrap(), None);

    // Through a symlink to the library.
    let outside = tempfile::tempdir().unwrap();
    let link = outside.path().join("link");
    std::os::unix::fs::symlink(lib.dir.path(), &link).unwrap();
    assert_eq!(lib.cat.asset(&link.join("sub/a.jpg")).unwrap(), a);

    assert!(matches!(lib.cat.asset(&outside.path().join("x.jpg")), Err(Error::Path(_))));
    assert!(matches!(lib.cat.asset(lib.dir.path()), Err(Error::Path(_))));

    // Moves keep tags; removal drops them.
    lib.tag(a, "Dog", TagKind::Object);
    lib.cat.move_asset(&lib.path("sub/a.jpg"), &lib.path("b.jpg")).unwrap();
    assert_eq!(lib.search("dog"), ["b.jpg"]);
    lib.cat.remove_asset(&lib.path("b.jpg")).unwrap();
    assert_eq!(lib.search(""), Vec::<String>::new());
}

#[test]
fn on_disk_reopen_and_shared_use() {
    let dir = tempfile::tempdir().unwrap();
    let db = Catalog::default_path(dir.path());
    let a = {
        let cat = Catalog::open(&db, dir.path()).unwrap();
        let a = cat.asset(&dir.path().join("a.jpg")).unwrap();
        cat.add_tag(a, cat.tag("Dog", TagKind::Object).unwrap(), None).unwrap();
        a
    };
    let reader = Catalog::open(&db, dir.path()).unwrap();
    let writer = Catalog::open(&db, dir.path()).unwrap();
    writer.add_tag(a, writer.tag("Cat", TagKind::Object).unwrap(), None).unwrap();
    assert_eq!(reader.search("dog cat", 0.5).unwrap(), [dir.path().join("a.jpg")]);

    let conn = Connection::open(&db).unwrap();
    conn.pragma_update(None, "user_version", 99).unwrap();
    assert!(matches!(Catalog::open(&db, Path::new(dir.path())), Err(Error::NewerSchema(99))));
}

#[test]
fn exact_terms_match_a_path_and_its_subtree() {
    let lib = sample();
    let ann = lib.asset("ann.jpg");
    lib.tag(ann, "People|Family|Alicia", TagKind::Person);
    lib.tag(ann, "Trips|People|Family|Alice", TagKind::Keyword);
    assert_eq!(lib.search("=people|family"), ["ann.jpg", "beach.jpg", "park.jpg"]);
    assert_eq!(lib.search("=People|Family|Alice"), ["beach.jpg"], "not Alicia, nor under Trips");
    assert_eq!(lib.search("alice"), ["ann.jpg", "beach.jpg"], "a plain term matches segments anywhere");
    assert_eq!(lib.search("=keyword:people"), Vec::<String>::new());
    assert_eq!(lib.search("=family"), Vec::<String>::new(), "from the top");
    // In memory, as front ends filter.
    let expr = parse("=People|Family|Alice").unwrap();
    let tags = |paths: &[&str]| paths.iter().map(|p| (p.to_string(), Some(TagKind::Person))).collect::<Vec<_>>();
    assert!(expr.matches(&tags(&["people | family|alice|Kid"])));
    assert!(!expr.matches(&tags(&["People|Family|Alicia", "Trips|People|Family|Alice"])));
}

#[test]
fn query_parsing() {
    let term = |text: &str| Expr::Term { kind: None, text: text.into() };
    assert_eq!(parse("  ").unwrap(), Expr::All);
    assert_eq!(parse("a b OR c").unwrap(), Expr::Or(vec![Expr::And(vec![term("a"), term("b")]), term("c")]));
    assert_eq!(parse("-(a OR b)").unwrap(), Expr::Not(Box::new(Expr::Or(vec![term("a"), term("b")]))));
    assert_eq!(parse("x-ray").unwrap(), term("x-ray"));
    assert_eq!(parse("\"OR\" or").unwrap(), Expr::And(vec![term("OR"), term("or")]));
    assert_eq!(
        parse("Person:\"Alice Smith\"").unwrap(),
        Expr::Term { kind: Some(TagKind::Person), text: "Alice Smith".into() }
    );
    assert_eq!(parse("(a and b)").unwrap(), Expr::And(vec![term("a"), term("b")]));
    assert_eq!(parse("a AND b").unwrap(), Expr::And(vec![term("a"), term("b")]));
    assert_eq!(parse("a && b").unwrap(), Expr::And(vec![term("a"), term("b")]));
    assert_eq!(parse("(a OR b) and c").unwrap(), Expr::And(vec![Expr::Or(vec![term("a"), term("b")]), term("c")]));
    assert_eq!(parse("a and not b").unwrap(), Expr::And(vec![term("a"), Expr::Not(Box::new(term("b")))]));
    assert_eq!(parse("=\"People | Ann Lee\"").unwrap(), Expr::Under { kind: None, path: "People|Ann Lee".into() });
    assert_eq!(parse("= person:Family -x").unwrap(), Expr::And(vec![
        Expr::Under { kind: Some(TagKind::Person), path: "Family".into() },
        Expr::Not(Box::new(term("x")))
    ]));
    for bad in ["(a", "a)", "a OR", "OR a", "a AND", "AND a", "(a and)", "-", "\"a", "a\"b\"", "person:", "()", "a||b", "=", "=-a", "=(a b)"] {
        assert!(parse(bad).is_err(), "{bad:?}");
    }
}


#[test]
fn sync_sidecars_writes_and_tracks() {
    let mut lib = Lib::new();
    // A real 40×20 PNG, so regions get the image size.
    image::RgbImage::new(40, 20).save(lib.path("a.png")).unwrap();
    let a = lib.asset("a.png");
    let sidecar = xmp::sidecar_path(&lib.path("a.png"));
    let alice = lib.cat.tag("People|Alice", TagKind::Person).unwrap();
    lib.cat.add_tag(a, alice, Some(Region { x: 0.5, y: 0.5, w: 0.25, h: 0.5 })).unwrap();
    let dog = lib.cat.tag("Dog", TagKind::Object).unwrap();
    let cat = lib.cat.tag("Cat", TagKind::Object).unwrap();
    let det = |tag, confidence| Detection { tag, confidence, region: None };
    lib.cat.set_detections(a, "objects/v1", &[det(dog, 0.9), det(cat, 0.3)]).unwrap();

    let report = lib.cat.sync_sidecars(DEFAULT_MIN_CONFIDENCE).unwrap();
    assert_eq!((report.written, report.failed.len()), (1, 0));
    assert!(lib.cat.needing_sync().unwrap().is_empty());
    let text = std::fs::read_to_string(&sidecar).unwrap();
    assert!(text.contains("<rdf:li>People|Alice</rdf:li>") && text.contains("<rdf:li>Dog</rdf:li>"));
    assert!(!text.contains("Cat"), "below the confidence threshold");
    assert!(text.contains(r#"stDim:w="40" stDim:h="20""#));
    assert!(text.contains(r#"stArea:x="0.625000" stArea:y="0.750000""#));

    assert_eq!(lib.cat.sidecar_tags(a).unwrap(), ["Dog", "People|Alice"], "what the sidecar has of ours");

    // Nothing pending: nothing written.
    assert_eq!(lib.cat.sync_sidecars(DEFAULT_MIN_CONFIDENCE).unwrap().written, 0);

    // Another program adds a keyword; we remove Dog. Theirs stays, Dog goes.
    std::fs::write(&sidecar, text.replace("<rdf:li>Dog</rdf:li>", "<rdf:li>Dog</rdf:li><rdf:li>Holiday</rdf:li>")).unwrap();
    lib.cat.remove_tag(a, dog).unwrap();
    lib.cat.sync_sidecars(DEFAULT_MIN_CONFIDENCE).unwrap();
    let text = std::fs::read_to_string(&sidecar).unwrap();
    assert!(text.contains("Holiday") && !text.contains("Dog"));

    // Once only the other program's keyword is left, the sidecar stays for it...
    lib.cat.remove_tag(a, alice).unwrap();
    lib.cat.sync_sidecars(DEFAULT_MIN_CONFIDENCE).unwrap();
    let text = std::fs::read_to_string(&sidecar).unwrap();
    assert!(text.contains("Holiday") && !text.contains("Alice"));
    // ...and a sidecar of ours alone is deleted with its last tag.
    std::fs::remove_file(&sidecar).unwrap();
    lib.cat.add_tag(a, alice, None).unwrap();
    lib.cat.sync_sidecars(DEFAULT_MIN_CONFIDENCE).unwrap();
    assert!(sidecar.exists());
    lib.cat.remove_tag(a, alice).unwrap();
    lib.cat.sync_sidecars(DEFAULT_MIN_CONFIDENCE).unwrap();
    assert!(!sidecar.exists());

    // A missing file is reported and stays pending; no sidecar appears.
    let gone = lib.asset("gone.jpg");
    lib.cat.add_tag(gone, dog, None).unwrap();
    let report = lib.cat.sync_sidecars(DEFAULT_MIN_CONFIDENCE).unwrap();
    assert_eq!(report.failed.len(), 1);
    assert_eq!(lib.cat.needing_sync().unwrap(), [gone]);
    assert!(!xmp::sidecar_path(&lib.path("gone.jpg")).exists());
}

#[test]
fn tag_paths_reject_control_characters() {
    let lib = Lib::new();
    assert!(matches!(lib.cat.tag("a\nb", TagKind::Keyword), Err(Error::Invalid(_))));
}

#[test]
fn a_new_catalog_opens_from_many_threads_at_once() {
    // A host opening a new library opens several connections at once (indexer, people
    // list, pipeline); turning the fresh database to WAL must not fail any of them.
    for _ in 0..20 {
        let dir = tempfile::tempdir().unwrap();
        let db = Catalog::default_path(dir.path());
        let barrier = std::sync::Barrier::new(8);
        std::thread::scope(|s| {
            for _ in 0..8 {
                s.spawn(|| {
                    barrier.wait();
                    Catalog::open(&db, dir.path()).unwrap();
                });
            }
        });
    }
}

#[test]
fn dates_taken() {
    let lib = Lib::new();
    let (a, b, c) = (lib.asset("a.jpg"), lib.asset("b.jpg"), lib.asset("c.jpg"));
    assert_eq!(lib.cat.taken(a).unwrap(), None);
    assert!(lib.cat.set_taken(a, Some(549_864_182)).unwrap());
    assert!(!lib.cat.set_taken(a, Some(549_864_182)).unwrap(), "the same date: nothing changed");
    assert!(lib.cat.set_taken(b, Some(-126_165_600)).unwrap(), "before 1970");
    assert!(!lib.cat.set_taken(c, None).unwrap());
    assert_eq!(lib.cat.taken(a).unwrap(), Some(549_864_182));
    let found = lib.cat.taken_of(&[a, b, c]).unwrap();
    assert_eq!(found, HashMap::from([(a, 549_864_182), (b, -126_165_600)]));
    assert!(lib.cat.set_taken(a, None).unwrap());
    assert_eq!(lib.cat.taken_of(&[a]).unwrap(), HashMap::new());
}
