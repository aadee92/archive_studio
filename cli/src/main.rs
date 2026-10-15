//! `archive [ROOT]`: Archive Studio's interactive shell. While it is open, the pipeline
//! engine runs on its own thread (see `archive_core::pipeline`): it catches up with the
//! library, watches it, and its reports are printed above the prompt.
//!
//! `-c COMMAND` runs a command and exits (repeatable); `--until-idle` exits once the
//! pipeline has nothing left to do.

use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc, Mutex};

use archive_core::catalog::runs::{RunCounts, RunStatus};
use archive_core::catalog::{AssetId, Catalog, TagKind};
use archive_core::ops;
use archive_core::pipeline::{self, Availability, Config, Engine, Event, Mode, Models, FACES, ORIENT, SIGNATURE};
use rustyline::error::ReadlineError;
use rustyline::{DefaultEditor, ExternalPrinter};

const HELP: &str = "\
status                     what the pipeline has done and has left
review                     rotations waiting for you
accept PATH|all            apply the proposed rotation
keep PATH|all              the file is upright as it is
rotate PATH 90|180|270     turn the file clockwise (rewrites it)
failed                     files a stage failed on
retry                      run the failed stages again
add PATH...                take in files now, without waiting for them to settle
faces [PATH]               people and unknown faces, or the faces in one file
name FACE_ID... NAME       name faces (ids from faces PATH); tags their files People|NAME
person rename NAME NEW     rename a person (onto an existing one: merge)
person merge NAME INTO     NAME's faces and tags become INTO's
person delete NAME         un-name their faces and drop their tag
sync-people                put missing People| tags on files with named faces
tags PATH                  the file's tags
tag PATH [KIND:]TAG        add a tag (KIND: keyword, scene, object, person, place, event)
untag PATH TAG             remove a tag (a person tag also un-names their faces there)
props PATH                 everything known about a file
pair PATH                  the file's front or back
pair PATH BACK             pair a back with its front by hand
similar PATH [N]           highly similar files in the library
delete PATH                move the file (and its back) to the library's trash
trash                      what is in the trash
restore PATH               put a trashed file back (its original path)
empty-trash [DAYS]         delete for good what was trashed DAYS (default 0) ago
pause | resume | background   stop starting work / full speed / one thread
help | quit";

struct Args {
    root: PathBuf,
    commands: Vec<String>,
    until_idle: bool,
}

fn parse_args() -> Result<Args, String> {
    let mut args = Args { root: PathBuf::from("."), commands: Vec::new(), until_idle: false };
    let mut it = std::env::args().skip(1);
    let mut root = None;
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "-c" => args.commands.push(it.next().ok_or("-c needs a command")?),
            "--until-idle" => args.until_idle = true,
            "-h" | "--help" => return Err(format!("usage: archive [ROOT] [-c COMMAND]... [--until-idle]\n\n{HELP}")),
            _ if root.is_none() => root = Some(PathBuf::from(arg)),
            _ => return Err(format!("unexpected argument {arg}")),
        }
    }
    if let Some(root) = root {
        args.root = root;
    }
    args.root = args.root.canonicalize().map_err(|e| format!("{}: {e}", args.root.display()))?;
    Ok(args)
}

fn main() {
    let args = match parse_args() {
        Ok(args) => args,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(2);
        }
    };
    if let Err(e) = run(args) {
        eprintln!("{e}");
        std::process::exit(1);
    }
}

/// Where engine reports go: above the prompt while the shell is interactive, else stdout.
type Printer = Arc<Mutex<Option<Box<dyn ExternalPrinter + Send>>>>;

fn say(printer: &Printer, line: String) {
    match printer.lock().unwrap().as_mut() {
        Some(p) => {
            let _ = p.print(line);
        }
        None => println!("{line}"),
    }
}

fn run(args: Args) -> Result<(), String> {
    let root = args.root.clone();
    let catalog = Catalog::open(&Catalog::default_path(&root), &root).map_err(|e| e.to_string())?;
    let interactive = args.commands.is_empty() && !args.until_idle;

    let mut editor = DefaultEditor::new().map_err(|e| e.to_string())?;
    let printer: Printer = Arc::new(Mutex::new(None));
    if interactive {
        println!("Archive Studio — {}", root.display());
        if let Ok(p) = editor.create_external_printer() {
            *printer.lock().unwrap() = Some(Box::new(p));
        }
    }

    let (idle_send, idle) = mpsc::channel();
    let idle_send = Mutex::new(idle_send);
    let (sink_root, sink_printer) = (root.clone(), printer.clone());
    let engine = match Engine::start(&root, Config::default(), move |event| {
        if matches!(event, Event::Idle) {
            let _ = idle_send.lock().unwrap().send(());
        }
        if let Some(line) = describe(&sink_root, &event) {
            say(&sink_printer, line);
        }
    }) {
        Ok(engine) => Some(engine),
        Err(e) => {
            println!("pipeline not started: {e} (status and listings still work)");
            None
        }
    };

    let mut shell = Shell { root: root.clone(), catalog, engine };
    for command in &args.commands {
        if !shell.execute(command) {
            return Ok(());
        }
    }
    if args.until_idle {
        if shell.engine.is_some() {
            let _ = idle.recv();
        }
        shell.execute("status");
        return Ok(());
    }
    if !interactive {
        return Ok(());
    }

    let history = root.join(".archive-studio").join("cli_history");
    let _ = editor.load_history(&history);
    loop {
        match editor.readline("archive> ") {
            Ok(line) => {
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                let _ = editor.add_history_entry(line);
                if !shell.execute(line) {
                    break;
                }
            }
            Err(ReadlineError::Interrupted) => continue,
            Err(ReadlineError::Eof) => break,
            Err(e) => return Err(e.to_string()),
        }
    }
    let _ = editor.save_history(&history);
    // Reports from here on would land after the prompt is gone.
    *printer.lock().unwrap() = None;
    Ok(())
}

/// `path` relative to the library root, for display.
fn show(root: &Path, path: &Path) -> String {
    path.strip_prefix(root).unwrap_or(path).display().to_string()
}

fn describe(root: &Path, event: &Event) -> Option<String> {
    Some(match event {
        Event::CatchUp { added, changed, missing } => {
            if added + changed + missing == 0 {
                return None;
            }
            format!("catch-up: {added} new, {changed} changed, {missing} missing")
        }
        Event::Ready(available) => {
            let dir = Models::dir().map_or_else(|| "the models directory".to_owned(), |d| d.display().to_string());
            let mut missing = Vec::new();
            if !available.orient {
                missing.push("orient (orientation.onnx)");
            }
            if !available.faces {
                missing.push("faces (buffalo_l/)");
            }
            if missing.is_empty() {
                return None;
            }
            format!("no model for {} in {dir}; skipping", missing.join(", "))
        }
        Event::Rotated { path, cw, by_user } => {
            format!("rotated {} {cw}°{}", show(root, path), if *by_user { "" } else { " (auto)" })
        }
        Event::Review { path, detail, .. } => format!("review: {}: {detail}", show(root, path)),
        Event::Failed { path, stage, error } => format!("{stage} failed: {}: {error}", show(root, path)),
        Event::Identified { suggested, clusters } => format!("identify: {suggested} suggestions, {clusters} unknown-face clusters"),
        Event::Synced { written } => format!("sidecars: wrote {written}"),
        Event::Warning(message) => format!("warning: {message}"),
        Event::Idle => "all caught up".to_owned(),
        Event::Updated(_) | Event::Busy => return None,
    })
}

struct Shell {
    root: PathBuf,
    catalog: Catalog,
    engine: Option<Engine>,
}

impl Shell {
    /// Runs one command line; false to quit.
    fn execute(&mut self, line: &str) -> bool {
        let words: Vec<&str> = line.split_whitespace().collect();
        let Some((&command, rest)) = words.split_first() else { return true };
        let result = match command {
            "quit" | "exit" | "q" => return false,
            "help" | "?" => {
                println!("{HELP}");
                Ok(())
            }
            "status" => self.status(),
            "review" => self.review(),
            "accept" => self.each_review(rest, |engine, path| engine.accept(path)),
            "keep" => self.each_review(rest, |engine, path| engine.keep(path)),
            "rotate" => self.rotate(rest),
            "failed" => self.failed(),
            "retry" => self.engine().map(|e| e.retry_failed()),
            "add" => self.engine().map(|engine| {
                for arg in rest {
                    engine.add(self.resolve(arg));
                }
            }),
            "faces" if rest.is_empty() => self.faces(),
            "faces" => self.faces_in(rest),
            "name" => self.name(rest),
            "person" => self.person(rest),
            "sync-people" => self.catalog.sync_person_tags().map(|n| println!("added {n} person tags")).map_err(|e| e.to_string()),
            "tags" => self.tags(rest),
            "tag" => self.tag(rest),
            "untag" => self.untag(rest),
            "props" => self.props(rest),
            "pair" => self.pair(rest),
            "similar" => self.similar(rest),
            "delete" => self.delete(rest),
            "trash" => self.trash(),
            "restore" => self.restore(rest),
            "empty-trash" => self.empty_trash(rest),
            "pause" => self.engine().map(|e| e.set_mode(Mode::Paused)),
            "resume" => self.engine().map(|e| e.set_mode(Mode::Normal)),
            "background" => self.engine().map(|e| e.set_mode(Mode::Background)),
            _ => Err(format!("unknown command {command}; try help")),
        };
        if let Err(e) = result {
            println!("{e}");
        }
        true
    }

    fn engine(&self) -> Result<&Engine, String> {
        self.engine.as_ref().ok_or_else(|| "the pipeline isn't running in this shell".to_owned())
    }

    /// A path as typed: relative to the current directory if it exists there, else to the
    /// library root.
    fn resolve(&self, arg: &str) -> PathBuf {
        let path = PathBuf::from(arg);
        if path.is_absolute() || path.exists() {
            std::path::absolute(&path).unwrap_or(path)
        } else {
            self.root.join(path)
        }
    }

    fn status(&self) -> Result<(), String> {
        // Without an engine here, assume the models another one would have.
        let models = Models::locate();
        let available = self
            .engine
            .as_ref()
            .and_then(Engine::availability)
            .unwrap_or(Availability { orient: models.orientation.is_some(), faces: models.faces.is_some() });
        let status = pipeline::status(&self.catalog, available).map_err(|e| e.to_string())?;
        let line = |name: &str, available: bool, c: RunCounts| {
            if available {
                println!(
                    "{name:<9} {:>7} done  {:>6} pending  {:>5} to review  {:>5} failed",
                    c.done, c.pending, c.review, c.failed
                );
            } else {
                println!("{name:<9} no model");
            }
        };
        line(ORIENT, available.orient, status.orient);
        line(FACES, available.faces, status.faces);
        match status.faces_since_identify {
            0 => println!("identify  up to date"),
            n => println!("identify  {n} new faces since the last run"),
        }
        let sig = pipeline::signature_status(&self.catalog, available).map_err(|e| e.to_string())?;
        line(SIGNATURE, true, sig);
        println!("sidecars  {} to write", status.needs_sync);
        if self.engine.is_none() {
            println!("(pipeline not running in this shell)");
        }
        Ok(())
    }

    fn review(&self) -> Result<(), String> {
        let items = self.catalog.runs_with_status(ORIENT, RunStatus::Review).map_err(|e| e.to_string())?;
        if items.is_empty() {
            println!("nothing to review");
        }
        for (_, path, _, detail) in items {
            println!("{}: {detail}", show(&self.root, &path));
        }
        Ok(())
    }

    /// Runs `action` on the file named in `args`, or on every file in review with `all`.
    fn each_review(&self, args: &[&str], action: impl Fn(&Engine, &Path) -> Result<(), String>) -> Result<(), String> {
        let engine = self.engine()?;
        let paths: Vec<PathBuf> = match args {
            ["all"] => self
                .catalog
                .runs_with_status(ORIENT, RunStatus::Review)
                .map_err(|e| e.to_string())?
                .into_iter()
                .map(|(_, path, _, _)| path)
                .collect(),
            [path] => vec![self.resolve(path)],
            _ => return Err("give one path, or all".into()),
        };
        for path in paths {
            if let Err(e) = action(engine, &path) {
                println!("{}: {e}", show(&self.root, &path));
            }
        }
        Ok(())
    }

    fn rotate(&self, args: &[&str]) -> Result<(), String> {
        let [path, degrees] = args else { return Err("rotate PATH 90|180|270".into()) };
        let cw: u16 = degrees.parse().map_err(|_| "rotate by 90, 180 or 270".to_owned())?;
        match &self.engine {
            Some(engine) => engine.rotate(self.resolve(path), cw),
            None => ops::rotate(&self.catalog, self.asset(path)?, cw),
        }
    }

    fn failed(&self) -> Result<(), String> {
        let mut any = false;
        for stage in [ORIENT, FACES, SIGNATURE] {
            for (_, path, _, detail) in self.catalog.runs_with_status(stage, RunStatus::Failed).map_err(|e| e.to_string())? {
                println!("{stage}: {}: {detail}", show(&self.root, &path));
                any = true;
            }
        }
        if !any {
            println!("no failures");
        }
        Ok(())
    }

    fn faces(&self) -> Result<(), String> {
        use archive_core::catalog::faces::{FaceFilter, FaceOrder};
        let err = |e: archive_core::catalog::Error| e.to_string();
        let unknown = self.catalog.faces(FaceFilter::Unknown, FaceOrder::Asset).map_err(err)?.len();
        let clusters = self.catalog.face_clusters().map_err(err)?;
        println!("unknown   {unknown} faces, {} clusters", clusters.len());
        for person in self.catalog.people_with_faces().map_err(err)? {
            println!("{:<30} {:>5} confirmed  {:>5} suggested", person.path, person.confirmed, person.suggested);
        }
        Ok(())
    }

    /// The asset a typed path names.
    fn asset(&self, arg: &str) -> Result<AssetId, String> {
        let path = self.resolve(arg);
        self.catalog.find_asset(&path).map_err(err)?.ok_or_else(|| format!("{}: not in the library", show(&self.root, &path)))
    }

    fn shown(&self, asset: AssetId) -> String {
        self.catalog.asset_path(asset).map_or_else(|_| format!("#{}", asset.0), |p| show(&self.root, &p))
    }

    fn faces_in(&self, args: &[&str]) -> Result<(), String> {
        use archive_core::catalog::faces::FaceStatus;
        let [path] = args else { return Err("faces [PATH]".into()) };
        let boxes = self.catalog.face_boxes(self.asset(path)?).map_err(err)?;
        if boxes.is_empty() {
            println!("no faces");
        }
        for b in boxes {
            let r = b.region;
            let who = match (&b.person, &b.suggestion, b.status) {
                (Some((_, name)), _, _) => name.clone(),
                (None, _, FaceStatus::Ignored) => "(ignored)".to_owned(),
                (None, Some((_, name, similarity)), _) => format!("{name}? ({:.0}%)", similarity * 100.0),
                _ => "?".to_owned(),
            };
            println!("face {:<6} x {:.3}  y {:.3}  w {:.3}  h {:.3}  {who}", b.face.0, r.x, r.y, r.w, r.h);
        }
        Ok(())
    }

    /// `name FACE_ID... NAME WORDS`.
    fn name(&mut self, args: &[&str]) -> Result<(), String> {
        use archive_core::catalog::faces::FaceId;
        let ids: Vec<FaceId> = args.iter().map_while(|a| a.parse().ok().map(FaceId)).collect();
        let name = args[ids.len()..].join(" ");
        if ids.is_empty() || name.is_empty() {
            return Err("name FACE_ID... NAME".into());
        }
        let person = self.catalog.name_faces(&ids, &name).map_err(err)?;
        let path = self.catalog.people_with_faces().map_err(err)?.into_iter().find(|p| p.tag == person).map(|p| p.path);
        println!("named {} faces {}", ids.len(), path.unwrap_or(name));
        Ok(())
    }

    /// `person rename|merge|delete ...`; two names are separated by `->` (or are one word each).
    fn person(&mut self, args: &[&str]) -> Result<(), String> {
        let usage = "person rename NAME -> NEW | person merge NAME -> INTO | person delete NAME";
        let Some((&action, rest)) = args.split_first() else { return Err(usage.into()) };
        let two = || -> Result<(String, String), String> {
            match rest.iter().position(|w| *w == "->") {
                Some(i) => Ok((rest[..i].join(" "), rest[i + 1..].join(" "))),
                None if rest.len() == 2 => Ok((rest[0].to_owned(), rest[1].to_owned())),
                None => Err(usage.to_owned()),
            }
        };
        let existing = |catalog: &Catalog, name: &str| -> Result<archive_core::catalog::TagId, String> {
            let known = catalog.leaf_tags(TagKind::Person).map_err(err)?;
            let lower = name.to_lowercase();
            let matches: Vec<_> = known
                .iter()
                .filter(|(_, p)| p.eq_ignore_ascii_case(name) || archive_core::catalog::people::display_name(p).to_lowercase() == lower)
                .collect();
            match matches[..] {
                [(id, _)] => Ok(*id),
                [] => Err(format!("no person {name}")),
                _ => Err(format!("{name} is ambiguous; give the full path")),
            }
        };
        match action {
            "rename" => {
                let (old, new) = two()?;
                let person = existing(&self.catalog, &old)?;
                self.catalog.rename_person(person, &new).map_err(err)?;
            }
            "merge" => {
                let (from, into) = two()?;
                let (from, into) = (existing(&self.catalog, &from)?, existing(&self.catalog, &into)?);
                self.catalog.merge_people(from, into).map_err(err)?;
            }
            "delete" => {
                let person = existing(&self.catalog, &rest.join(" "))?;
                self.catalog.delete_person(person).map_err(err)?;
            }
            _ => return Err(usage.into()),
        }
        println!("done");
        Ok(())
    }

    fn tags(&self, args: &[&str]) -> Result<(), String> {
        let [path] = args else { return Err("tags PATH".into()) };
        for tag in self.catalog.tags_of(self.asset(path)?).map_err(err)? {
            let source = match (&tag.source, tag.confidence) {
                (archive_core::catalog::Source::User, _) => String::new(),
                (archive_core::catalog::Source::Detector(d), Some(c)) => format!("  ({d}, {:.0}%)", c * 100.0),
                (archive_core::catalog::Source::Detector(d), None) => format!("  ({d})"),
            };
            let region = tag.region.map_or_else(String::new, |r| format!("  [{:.2} {:.2} {:.2} {:.2}]", r.x, r.y, r.w, r.h));
            println!("{:<8} {}{source}{region}", tag.kind.name(), tag.path);
        }
        Ok(())
    }

    /// `tag PATH [KIND:]TAG WORDS`.
    fn tag(&self, args: &[&str]) -> Result<(), String> {
        let Some((path, words)) = args.split_first().filter(|(_, w)| !w.is_empty()) else {
            return Err("tag PATH [KIND:]TAG".into());
        };
        let text = words.join(" ");
        let (kind, tag) = match text.split_once(':').and_then(|(k, rest)| TagKind::from_name(k).map(|k| (k, rest.trim()))) {
            Some((kind, tag)) => (kind, tag.to_owned()),
            None => (TagKind::Keyword, text.clone()),
        };
        self.catalog.add_tag_path(self.asset(path)?, &tag, kind).map_err(err)?;
        Ok(())
    }

    fn untag(&mut self, args: &[&str]) -> Result<(), String> {
        let Some((path, words)) = args.split_first().filter(|(_, w)| !w.is_empty()) else { return Err("untag PATH TAG".into()) };
        let asset = self.asset(path)?;
        if !self.catalog.remove_tag_path(asset, &words.join(" ")).map_err(err)? {
            return Err("no such tag".into());
        }
        Ok(())
    }

    fn props(&self, args: &[&str]) -> Result<(), String> {
        let [path] = args else { return Err("props PATH".into()) };
        let asset = self.asset(path)?;
        let p = ops::properties(&self.catalog, asset).map_err(err)?;
        let time = |t: Option<std::time::SystemTime>| {
            t.and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map_or_else(|| "-".to_owned(), |d| format!("{} (unix)", d.as_secs()))
        };
        println!("path       {}", show(&self.root, &p.path));
        println!("asset      #{}{}{}", p.asset.0, if p.trashed { "  (in the trash)" } else { "" }, if p.missing { "  (missing)" } else { "" });
        println!("size       {}", p.size.map_or_else(|| "-".to_owned(), |s| format!("{s} bytes")));
        println!("format     {}", p.format.as_deref().unwrap_or("-"));
        println!("dimensions {}", p.dimensions.map_or_else(|| "-".to_owned(), |(w, h)| format!("{w} × {h}")));
        println!("modified   {}", time(p.modified));
        println!("created    {}", time(p.created));
        let e = &p.exif;
        for (label, value) in [
            ("camera", [e.make.as_deref(), e.model.as_deref()].into_iter().flatten().collect::<Vec<_>>().join(" ")),
            ("taken", e.taken.clone().unwrap_or_default()),
            ("digitized", e.digitized.clone().unwrap_or_default()),
            ("software", e.software.clone().unwrap_or_default()),
            ("exposure", e.exposure_time.map_or_else(String::new, |t| format!("{t} s"))),
            ("aperture", e.f_number.map_or_else(String::new, |f| format!("f/{f:.1}"))),
            ("iso", e.iso.map_or_else(String::new, |i| i.to_string())),
        ] {
            if !value.is_empty() {
                println!("{label:<10} {value}");
            }
        }
        let r = &p.rotation;
        if !r.detail.is_empty() {
            println!("rotation   {}{}", r.detail, if r.by_user { " (yours; final)" } else { "" });
        }
        for tag in &p.tags {
            println!("tag        {}", tag.path);
        }
        for face in &p.faces {
            println!("face       #{} {}", face.face.0, face.person.as_ref().map_or("?", |(_, n)| n.as_str()));
        }
        if let Some(back) = p.back {
            println!("back       {}", self.shown(back));
        }
        if let Some(front) = p.front {
            println!("front      {}", self.shown(front));
        }
        for s in &p.stages {
            println!("stage      {:<9} {:?}{} {}", s.stage, s.status, if s.current { "" } else { " (older file)" }, s.detail);
        }
        Ok(())
    }

    fn pair(&mut self, args: &[&str]) -> Result<(), String> {
        match args {
            [path] => {
                let asset = self.asset(path)?;
                match (self.catalog.back_of(asset).map_err(err)?, self.catalog.front_of(asset).map_err(err)?) {
                    (Some(back), _) => println!("back: {}", self.shown(back)),
                    (_, Some(front)) => println!("front: {}", self.shown(front)),
                    _ => println!("not paired"),
                }
                Ok(())
            }
            [front, back] => {
                let (front, back) = (self.asset(front)?, self.asset(back)?);
                self.catalog.set_pair(front, back).map_err(err)
            }
            _ => Err("pair PATH [BACK]".into()),
        }
    }

    fn similar(&self, args: &[&str]) -> Result<(), String> {
        let (path, limit) = match args {
            [path] => (path, 10),
            [path, n] => (path, n.parse().map_err(|_| "similar PATH [N]".to_owned())?),
            _ => return Err("similar PATH [N]".into()),
        };
        let asset = self.asset(path)?;
        if self.catalog.signature(asset).map_err(err)?.is_none() {
            return Err("no signature yet (the pipeline's signature stage hasn't run on it)".into());
        }
        let everything = self.catalog.search("", 0.0).map_err(err)?;
        let candidates: Vec<AssetId> = everything.iter().filter_map(|p| self.catalog.find_asset(p).ok().flatten()).collect();
        let found = self.catalog.similar(asset, &candidates, limit).map_err(err)?;
        if found.is_empty() {
            println!("nothing highly similar");
        }
        for (other, score) in found {
            println!("{:>4.0}%  {}", score * 100.0, self.shown(other));
        }
        Ok(())
    }

    fn delete(&mut self, args: &[&str]) -> Result<(), String> {
        let [path] = args else { return Err("delete PATH".into()) };
        let asset = self.asset(path)?;
        for item in ops::delete(&mut self.catalog, asset).map_err(err)? {
            println!("trashed {}", show(&self.root, &item.original));
        }
        Ok(())
    }

    fn trash(&self) -> Result<(), String> {
        let items = self.catalog.trashed().map_err(err)?;
        if items.is_empty() {
            println!("the trash is empty");
        }
        for item in items {
            println!("{}  (trashed at {} unix)", show(&self.root, &item.original), item.trashed_at);
        }
        Ok(())
    }

    fn restore(&mut self, args: &[&str]) -> Result<(), String> {
        let [path] = args else { return Err("restore PATH".into()) };
        let path = self.resolve(path);
        let item = self.catalog.trashed().map_err(err)?.into_iter().find(|t| t.original == path || t.in_trash == path);
        let item = item.ok_or_else(|| format!("{} is not in the trash", show(&self.root, &path)))?;
        for restored in self.catalog.restore(item.asset).map_err(err)? {
            println!("restored {}", show(&self.root, &restored));
        }
        Ok(())
    }

    fn empty_trash(&mut self, args: &[&str]) -> Result<(), String> {
        let days: u64 = match args {
            [] => 0,
            [days] => days.parse().map_err(|_| "empty-trash [DAYS]".to_owned())?,
            _ => return Err("empty-trash [DAYS]".into()),
        };
        let n = self.catalog.empty_trash(std::time::Duration::from_secs(days * 86_400)).map_err(err)?;
        println!("deleted {n} files for good");
        Ok(())
    }
}

fn err(e: archive_core::catalog::Error) -> String {
    e.to_string()
}
