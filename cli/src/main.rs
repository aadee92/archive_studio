//! `archive [ROOT]`: Archive Studio's interactive shell. While it is open, the pipeline
//! engine runs on its own thread (see `archive_core::pipeline`): it catches up with the
//! library, watches it, and its reports are printed above the prompt.
//!
//! `-c COMMAND` runs a command and exits (repeatable); `--until-idle` exits once the
//! pipeline has nothing left to do.

use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc, Mutex};

use archive_core::catalog::runs::{Retry, RunCounts, RunStatus};
use archive_core::catalog::faces::FaceId;
use archive_core::catalog::flags::AssetFlags;
use archive_core::catalog::{AssetId, Catalog, Region, TagKind};
use archive_core::ops;
use archive_core::pipeline::describe;
use archive_core::pipeline::{self, Availability, Config, Engine, Event, Mode, Models, DESCRIBE, FACES, ORIENT, SIGNATURE};
use rustyline::error::ReadlineError;
use rustyline::{DefaultEditor, ExternalPrinter};

const HELP: &str = "\
status                     what the pipeline has done and has left
review                     rotations waiting for you
accept PATH|all            apply the proposed rotation
keep PATH|all              the file is upright as it is
rotate PATH 90|180|270     turn the file clockwise (rewrites it)
failed                     files a stage failed on, by stage, with the error
list [STATE]...            files in all of these states: review (a rotation to review),
                           failed, untagged (no tag of yours; named faces count),
                           faces, unknown-faces, has-back, is-back, tagged; no-STATE
                           for files not in it (list faces no-tagged)
retry [STAGE|PATH...]      run the failed stages again: all, one stage's, or the files'
add PATH...                take in files now, without waiting for them to settle
faces [PATH]               people and unknown faces, or the faces in one file
name FACE_ID... NAME       name faces (ids from faces PATH); tags their files People|NAME
not FACE_ID...             they aren't who they're named (or suggested) as: un-name, untag
ignore FACE_ID...          faces not worth naming (strangers, not a face)
unignore FACE_ID...        ignored faces back to unknown
ignored                    the ignored faces
reject FACE_ID... PERSON   they aren't PERSON: never suggested as them (still unknown)
unreject FACE_ID... PERSON take that back: they may be suggested as PERSON again
rejected PERSON            the faces said not to be PERSON
delete-face FACE_ID...     remove face boxes altogether (false detections)
add-face PATH X Y W H      box a face the detector missed (fractions of the picture: left,
                           top, width, height); prints its id, to name it
person rename NAME NEW     rename a person (onto an existing one: merge)
person merge NAME INTO     NAME's faces and tags become INTO's
person delete NAME         un-name their faces and drop their tag
person cover NAME FACE_ID  the face that stands for them in people lists (auto: the best)
people [ORDER] [TEXT]      people with faces: counts and cover face, by name or ORDER
                           (faces: most first, recent: named last first); TEXT narrows
sync-people                put missing People| tags on files with named faces
tags [PATH]                every tag as a tree with how many files carry it (or one
                           under it), or the file's tags
rename-tag OLD -> NEW      rename a tag and the tags under it (one word each: no ->); a
                           person stays under People| (NEW alone keeps their place)
merge-tag FROM -> INTO     FROM's files, faces and sub-tags become INTO's
delete-tag TAG             take a tag and the tags under it off every file (faces named
                           by a person tag are un-named)
tag PATH... [KIND:]TAG     add a tag to one or more files (KIND: keyword, scene, object,
                           person, place, event; People|NAME is a person)
untag PATH [KIND:]TAG      remove a tag (a bare NAME can be a person; a person tag also
                           un-names their faces there)
props PATH                 everything known about a file
open PATH                  open the file in its default application
reveal PATH                show the file in the file manager (its folder, file selected)
pair PATH                  the file's front or back
pair FRONT BACK            pair a back with its front by hand (replaces their old pairs)
unpair PATH                take the file out of its front/back pair
swap-sides PATH            the file's pair the other way round (front <-> back)
similar PATH [N]           files from the same scene or event
find WORDS...              files that fit a description (\"dog on a beach\"), best first
like PATH [N]              files that look like this one (by their descriptions), best first
delete PATH...             move the files (and their backs) to the library's trash
trash                      what is in the trash: when it was deleted, size, totals
restore PATH...            put trashed files back (their original paths)
purge PATH...              delete trashed files for good (with their backs)
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
    start_logging(&args);
    if let Err(e) = run(args) {
        log::error!("{e}");
        std::process::exit(1);
    }
    log::info!("exit");
}

/// The logger: stderr filtered by `ARCHIVE_STUDIO_LOG` (default `warn`: the shell prints the
/// engine's reports itself), and `<root>/.archive-studio/logs/archive.log`, which gets
/// `debug` for the shell, the catalog and ops, `info` for the rest.
fn start_logging(args: &Args) {
    let started = archive_logging::init(archive_logging::Config {
        dir: Some(archive_logging::dir_for(&args.root)),
        file_name: "archive.log",
        stderr_default: "warn",
        file_minimum: "info,archive=debug,archive_core::ops=debug,archive_core::catalog=debug,panic=debug",
    });
    log::info!(
        "archive shell {} ({}), pid {}, library {}, commands {:?}, until idle {}",
        env!("CARGO_PKG_VERSION"),
        archive_logging::GIT_HASH,
        std::process::id(),
        args.root.display(),
        args.commands,
        args.until_idle
    );
    match (&started.file, &started.file_error) {
        (Some(file), _) => log::info!("log file {} (local time, UTC{})", file.display(), archive_logging::utc_offset()),
        (None, Some(e)) => log::warn!("no log file: {e}"),
        (None, None) => {}
    }
    log::info!("stderr filter {:?} (set {})", started.stderr_filter, archive_logging::ENV);
    if let Some(e) = started.env_error {
        log::warn!("{e}");
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

    let mut shell = Shell { root: root.clone(), catalog, engine, faces: None };
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
    /// Loaded on the first `add-face` without an engine here.
    faces: Option<Arc<archive_core::faces::Faces>>,
}

impl Shell {
    /// Runs one command line; false to quit.
    fn execute(&mut self, line: &str) -> bool {
        let words: Vec<&str> = line.split_whitespace().collect();
        let Some((&command, rest)) = words.split_first() else { return true };
        log::debug!("command: {line}");
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
            "list" => self.list(rest),
            "retry" => self.retry(rest),
            "add" => self.engine().map(|engine| {
                for arg in rest {
                    engine.add(self.resolve(arg));
                }
            }),
            "faces" if rest.is_empty() => self.faces(),
            "faces" => self.faces_in(rest),
            "name" => self.name(rest),
            "not" => self.each_face(rest, "not", Catalog::not_person),
            "ignore" => self.each_face(rest, "ignore", Catalog::ignore_faces),
            "unignore" => self.each_face(rest, "unignore", Catalog::unignore_faces),
            "reject" => self.reject(rest, false),
            "unreject" => self.reject(rest, true),
            "ignored" => self.list_faces(archive_core::catalog::faces::FaceFilter::Ignored),
            "rejected" => existing_person(&self.catalog, &rest.join(" "))
                .and_then(|person| self.list_faces(archive_core::catalog::faces::FaceFilter::Rejected(person))),
            "delete-face" => self.each_face(rest, "delete-face", Catalog::delete_faces),
            "add-face" => self.add_face(rest),
            "person" => self.person(rest),
            "people" => self.people(rest),
            "sync-people" => self.catalog.sync_person_tags().map(|n| println!("added {n} person tags")).map_err(|e| e.to_string()),
            "tags" if rest.is_empty() => self.tag_tree(),
            "tags" => self.tags(rest),
            "rename-tag" => self.rename_tag(rest),
            "merge-tag" => self.merge_tag(rest),
            "delete-tag" => self.delete_tag(rest),
            "tag" => self.tag(rest),
            "untag" => self.untag(rest),
            "props" => self.props(rest),
            "open" => self.open(rest),
            "reveal" => self.reveal(rest),
            "pair" => self.pair(rest),
            "unpair" => self.unpair(rest),
            "swap-sides" => self.swap_sides(rest),
            "similar" => self.similar(rest),
            "find" => self.find(rest),
            "like" => self.like(rest),
            "delete" => self.delete(rest),
            "trash" => self.trash(),
            "restore" => self.restore(rest),
            "purge" => self.purge(rest),
            "empty-trash" => self.empty_trash(rest),
            "pause" => self.engine().map(|e| e.set_mode(Mode::Paused)),
            "resume" => self.engine().map(|e| e.set_mode(Mode::Normal)),
            "background" => self.engine().map(|e| e.set_mode(Mode::Background)),
            _ => Err(format!("unknown command {command}; try help")),
        };
        if let Err(e) = result {
            log::debug!("command {command} failed: {e}");
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
        let described = pipeline::describe_status(&self.catalog, available).map_err(|e| e.to_string())?;
        line(DESCRIBE, models.describe.is_some(), described);
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
        let failures = self.catalog.failures().map_err(err)?;
        if failures.is_empty() {
            println!("no failures");
        }
        for f in failures {
            println!("{}: {}: {}  ({})", f.stage, show(&self.root, &f.path), f.error, ago(f.at));
        }
        Ok(())
    }

    fn retry(&self, args: &[&str]) -> Result<(), String> {
        let stages = [ORIENT, FACES, SIGNATURE, DESCRIBE];
        let which = match args {
            [] => Retry::All,
            [stage] if stages.contains(stage) => Retry::Stage((*stage).to_owned()),
            paths => {
                let assets = paths.iter().map(|p| self.asset(p)).collect::<Result<Vec<_>, _>>()?;
                let failures = self.catalog.failures().map_err(err)?;
                Retry::Runs(failures.into_iter().filter(|f| assets.contains(&f.asset)).map(|f| (f.asset, f.stage)).collect())
            }
        };
        let n = ops::retry(&self.catalog, self.engine.as_ref(), which)?;
        println!("{n} failed runs to try again{}", if self.engine.is_none() && n > 0 { " (when the pipeline next runs)" } else { "" });
        Ok(())
    }

    fn list(&self, args: &[&str]) -> Result<(), String> {
        let (mut with, mut without) = (AssetFlags::NONE, AssetFlags::NONE);
        for arg in args {
            let arg = arg.trim_start_matches("--");
            let (negated, name) = match arg.strip_prefix("no-") {
                Some(name) => (true, name),
                None => (false, arg),
            };
            let (flag, negated) = match name {
                "untagged" => (AssetFlags::TAGGED, !negated),
                "rotation" => (AssetFlags::REVIEW, negated),
                _ => match AssetFlags::NAMED.iter().find(|(_, n)| *n == name) {
                    Some(&(flag, _)) => (flag, negated),
                    None => return Err(format!("unknown state {arg:?}; see help")),
                },
            };
            if negated {
                without |= flag;
            } else {
                with |= flag;
            }
        }
        let paths = self.catalog.assets_flagged(with, without).map_err(|e| e.to_string())?;
        for path in &paths {
            println!("{}", show(&self.root, path));
        }
        println!("{} files", paths.len());
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

    /// `people [name|faces|recent] [TEXT]`.
    fn people(&self, args: &[&str]) -> Result<(), String> {
        use archive_core::catalog::people::{arrange, PeopleOrder};
        let (order, filter) = match args.split_first() {
            Some((first, rest)) => match PeopleOrder::from_name(first) {
                Some(order) => (order, rest.join(" ")),
                None => (PeopleOrder::Name, args.join(" ")),
            },
            None => (PeopleOrder::Name, String::new()),
        };
        let people = arrange(self.catalog.people_with_faces().map_err(err)?, order, &filter);
        for p in &people {
            let cover = p.cover.map_or_else(|| "-".to_owned(), |f| f.0.to_string());
            println!("{:>5} confirmed  {:>5} suggested  cover {cover:<6}  {}", p.confirmed, p.suggested, p.path);
        }
        println!("{} people", people.len());
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

    /// `reject FACE_ID... PERSON` (`undo`: `unreject`).
    fn reject(&mut self, args: &[&str], undo: bool) -> Result<(), String> {
        let command = if undo { "unreject" } else { "reject" };
        let ids: Vec<FaceId> = args.iter().map_while(|a| a.parse().ok().map(FaceId)).collect();
        let name = args[ids.len()..].join(" ");
        if ids.is_empty() || name.is_empty() {
            return Err(format!("{command} FACE_ID... PERSON"));
        }
        let person = existing_person(&self.catalog, &name)?;
        if undo {
            self.catalog.clear_rejections(&ids, person).map_err(err)?;
        } else {
            self.catalog.reject_person(&ids, person).map_err(err)?;
        }
        println!("{command}: {} faces", ids.len());
        Ok(())
    }

    /// The faces `filter` selects, with their files.
    fn list_faces(&self, filter: archive_core::catalog::faces::FaceFilter) -> Result<(), String> {
        use archive_core::catalog::faces::{FaceOrder, FaceStatus};
        let faces = self.catalog.faces(filter, FaceOrder::Asset).map_err(err)?;
        for f in &faces {
            let status = match f.status {
                FaceStatus::Confirmed(_) => "  (named)",
                FaceStatus::Ignored => "  (ignored)",
                FaceStatus::Unreviewed => "",
            };
            println!("face {:<6} {}{status}", f.id.0, self.shown(f.asset));
        }
        println!("{} faces", faces.len());
        Ok(())
    }

    /// `COMMAND FACE_ID...`: `change` on those faces.
    fn each_face(
        &mut self,
        args: &[&str],
        command: &str,
        change: impl FnOnce(&mut Catalog, &[FaceId]) -> archive_core::catalog::Result<()>,
    ) -> Result<(), String> {
        let ids: Vec<FaceId> = args.iter().map(|a| a.parse().map(FaceId)).collect::<Result<_, _>>().map_err(|_| format!("{command} FACE_ID..."))?;
        if ids.is_empty() {
            return Err(format!("{command} FACE_ID..."));
        }
        change(&mut self.catalog, &ids).map_err(err)?;
        println!("{command}: {} faces", ids.len());
        Ok(())
    }

    /// `add-face PATH X Y W H`.
    fn add_face(&mut self, args: &[&str]) -> Result<(), String> {
        let usage = || "add-face PATH X Y W H (fractions of the picture)".to_owned();
        let [path, numbers @ ..] = args else { return Err(usage()) };
        let numbers: Vec<f32> = numbers.iter().map(|n| n.parse()).collect::<Result<_, _>>().map_err(|_| usage())?;
        let [x, y, w, h] = numbers[..] else { return Err(usage()) };
        let asset = self.asset(path)?;
        let models = match self.engine.as_ref().and_then(Engine::face_models) {
            Some(models) => Some(models),
            None => {
                if self.faces.is_none() {
                    self.faces = ops::face_models().map(Arc::new).map_err(|e| println!("{e}; the face is added without an embedding")).ok();
                }
                self.faces.clone()
            }
        };
        let face = ops::add_face(&mut self.catalog, asset, Region { x, y, w, h }, models.as_deref())?;
        println!("added face {} on {}", face.0, self.shown(asset));
        Ok(())
    }

    /// `person rename|merge|delete ...`; two names are separated by `->` (or are one word each).
    fn person(&mut self, args: &[&str]) -> Result<(), String> {
        let usage = "person rename NAME -> NEW | person merge NAME -> INTO | person delete NAME | person cover NAME FACE_ID|auto";
        let Some((&action, rest)) = args.split_first() else { return Err(usage.into()) };
        let two = || two_names(rest, usage);
        let existing = existing_person;
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
            "cover" => {
                let [name @ .., face] = rest else { return Err(usage.into()) };
                let face = match *face {
                    "auto" => None,
                    id => Some(FaceId(id.parse().map_err(|_| usage.to_owned())?)),
                };
                let person = existing(&self.catalog, &name.join(" "))?;
                self.catalog.set_person_cover(person, face).map_err(err)?;
                let cover = self.catalog.person_cover(person).map_err(err)?;
                println!("cover: {}", cover.map_or_else(|| "none (no face named as them)".to_owned(), |f| format!("face {}", f.0)));
                return Ok(());
            }
            _ => return Err(usage.into()),
        }
        println!("done");
        Ok(())
    }

    /// Every tag as an indented tree, with how many files carry it or a tag under it.
    fn tag_tree(&self) -> Result<(), String> {
        let nodes = ops::tag_tree(&self.catalog).map_err(err)?;
        if nodes.is_empty() {
            println!("no tags");
        }
        for node in nodes {
            println!("{:>6}  {}{}", node.count, "  ".repeat(node.depth), node.name);
        }
        Ok(())
    }

    fn rename_tag(&mut self, args: &[&str]) -> Result<(), String> {
        let (old, new) = two_names(args, "rename-tag OLD -> NEW")?;
        match ops::rename_tag(&mut self.catalog, &old, &new, false).map_err(err)? {
            ops::Renamed::Done(done) => println!("now {}; {} files changed", done.to, done.assets.len()),
            ops::Renamed::Taken(path) => return Err(format!("{path} already exists; merge-tag {old} -> {path} merges them")),
        }
        Ok(())
    }

    fn merge_tag(&mut self, args: &[&str]) -> Result<(), String> {
        let (from, into) = two_names(args, "merge-tag FROM -> INTO")?;
        let done = ops::merge_tags(&mut self.catalog, &from, &into).map_err(err)?;
        println!("merged into {}; {} files changed", done.to, done.assets.len());
        Ok(())
    }

    fn delete_tag(&mut self, args: &[&str]) -> Result<(), String> {
        if args.is_empty() {
            return Err("delete-tag TAG".into());
        }
        let assets = ops::delete_tag(&mut self.catalog, &args.join(" ")).map_err(err)?;
        println!("deleted; it was on {} files", assets.len());
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

    /// `tag PATH... [KIND:]TAG WORDS`: the leading words naming files are the files (at
    /// least one), the rest is the tag; several files are tagged in one transaction.
    fn tag(&mut self, args: &[&str]) -> Result<(), String> {
        let files = args.iter().take(args.len().saturating_sub(1)).take_while(|a| self.resolve(a).exists()).count().max(1);
        let (paths, words) = args.split_at(files.min(args.len()));
        if paths.is_empty() || words.is_empty() {
            return Err("tag PATH... [KIND:]TAG".into());
        }
        let assets = paths.iter().map(|p| self.asset(p)).collect::<Result<Vec<_>, _>>()?;
        ops::add_tag_all(&mut self.catalog, &assets, &words.join(" ")).map_err(err)?;
        if assets.len() > 1 {
            println!("tagged {} files", assets.len());
        }
        Ok(())
    }

    fn untag(&mut self, args: &[&str]) -> Result<(), String> {
        let Some((path, words)) = args.split_first().filter(|(_, w)| !w.is_empty()) else { return Err("untag PATH TAG".into()) };
        let asset = self.asset(path)?;
        if !ops::remove_tag(&mut self.catalog, asset, &words.join(" ")).map_err(err)? {
            return Err("the file has no such tag".into());
        }
        Ok(())
    }

    fn open(&self, args: &[&str]) -> Result<(), String> {
        let [path] = args else { return Err("open PATH".into()) };
        archive_core::desktop::open(&self.resolve(path))
    }

    fn reveal(&self, args: &[&str]) -> Result<(), String> {
        let [path] = args else { return Err("reveal PATH".into()) };
        match archive_core::desktop::show_in_folder(&self.resolve(path))? {
            archive_core::desktop::Shown::Selected => {}
            archive_core::desktop::Shown::Folder => println!("opened its folder (no file manager on D-Bus to select it)"),
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
                ops::pair(&mut self.catalog, front, back).map(|_| ()).map_err(err)
            }
            _ => Err("pair PATH [BACK]".into()),
        }
    }

    fn unpair(&mut self, args: &[&str]) -> Result<(), String> {
        let [path] = args else { return Err("unpair PATH".into()) };
        let asset = self.asset(path)?;
        match ops::unpair(&mut self.catalog, asset).map_err(err)? {
            Some(other) => println!("unpaired from {}", self.shown(other)),
            None => println!("not paired"),
        }
        Ok(())
    }

    fn swap_sides(&mut self, args: &[&str]) -> Result<(), String> {
        let [path] = args else { return Err("swap-sides PATH".into()) };
        let asset = self.asset(path)?;
        let (front, back) = ops::swap_sides(&mut self.catalog, asset).map_err(err)?;
        println!("front: {}\nback:  {}", self.shown(front), self.shown(back));
        Ok(())
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
            println!("nothing from the same scene");
        }
        for (other, score) in found {
            println!("{:>4.0}%  {}", score * 100.0, self.shown(other));
        }
        Ok(())
    }

    /// `find WORDS...`: the files whose CLIP embedding fits the words best.
    fn find(&mut self, args: &[&str]) -> Result<(), String> {
        if args.is_empty() {
            return Err("find WORDS...".into());
        }
        let search = ops::text_search(self.engine.as_ref())?;
        let found = search.search(&self.catalog, &args.join(" "), 20)?;
        if found.is_empty() {
            println!("nothing fits");
        }
        for (path, similarity) in found {
            println!("{similarity:.3}  {}", show(&self.root, &path));
        }
        self.print_coverage()
    }

    /// `like PATH [N]`: the files whose CLIP embedding is most like the file's.
    fn like(&self, args: &[&str]) -> Result<(), String> {
        let (path, limit) = match args {
            [path] => (path, 20),
            [path, n] => (path, n.parse().map_err(|_| "like PATH [N]".to_owned())?),
            _ => return Err("like PATH [N]".into()),
        };
        let asset = self.asset(path)?;
        let found = describe::similar_images(&self.catalog, asset, limit)
            .map_err(err)?
            .ok_or("not described yet (the pipeline's describe stage hasn't run on it)")?;
        if found.is_empty() {
            println!("nothing looks like it");
        }
        for (path, similarity) in found {
            println!("{similarity:.3}  {}", show(&self.root, &path));
        }
        self.print_coverage()
    }

    /// How many images a search by description can find, when that isn't all of them.
    fn print_coverage(&self) -> Result<(), String> {
        let coverage = describe::coverage(&self.catalog).map_err(err)?;
        if coverage.described < coverage.images {
            println!("({} of {} images described so far; only those are searched)", coverage.described, coverage.images);
        }
        Ok(())
    }

    fn delete(&mut self, args: &[&str]) -> Result<(), String> {
        if args.is_empty() {
            return Err("delete PATH...".into());
        }
        let assets = args.iter().map(|p| self.asset(p)).collect::<Result<Vec<_>, _>>()?;
        let mut failed = 0;
        for (asset, result) in ops::delete_all(&mut self.catalog, &assets) {
            match result {
                Ok(items) => items.iter().for_each(|item| println!("trashed {}", show(&self.root, &item.original))),
                Err(e) => {
                    failed += 1;
                    println!("can't delete {}: {e}", self.shown(asset));
                }
            }
        }
        if failed > 0 {
            return Err(format!("{failed} of {} not deleted", assets.len()));
        }
        Ok(())
    }

    fn trash(&self) -> Result<(), String> {
        let items = self.catalog.trashed().map_err(err)?;
        if items.is_empty() {
            println!("the trash is empty");
            return Ok(());
        }
        for item in &items {
            let size = item.size.map_or_else(|| "-".to_owned(), megabytes);
            println!("{}  ({}, {size})", show(&self.root, &item.original), ago(item.trashed_at));
        }
        let totals = self.catalog.trash_totals().map_err(err)?;
        println!("{} files, {}", totals.items, megabytes(totals.bytes));
        Ok(())
    }

    fn purge(&mut self, args: &[&str]) -> Result<(), String> {
        if args.is_empty() {
            return Err("purge PATH...".into());
        }
        let trashed = self.catalog.trashed().map_err(err)?;
        let mut assets = Vec::new();
        for arg in args {
            let path = self.resolve(arg);
            let item = trashed.iter().find(|t| t.original == path || t.in_trash == path);
            assets.push(item.ok_or_else(|| format!("{} is not in the trash", show(&self.root, &path)))?.asset);
        }
        let n = self.catalog.purge(&assets).map_err(err)?;
        println!("deleted {n} files for good");
        Ok(())
    }

    fn restore(&mut self, args: &[&str]) -> Result<(), String> {
        if args.is_empty() {
            return Err("restore PATH...".into());
        }
        let trashed = self.catalog.trashed().map_err(err)?;
        let mut assets = Vec::new();
        for arg in args {
            let path = self.resolve(arg);
            let item = trashed.iter().find(|t| t.original == path || t.in_trash == path);
            assets.push(item.ok_or_else(|| format!("{} is not in the trash", show(&self.root, &path)))?.asset);
        }
        let mut failed = 0;
        for (_, result) in ops::restore_all(&mut self.catalog, &assets) {
            match result {
                Ok(paths) => paths.iter().for_each(|p| println!("restored {}", show(&self.root, p))),
                Err(e) => {
                    failed += 1;
                    println!("{e}");
                }
            }
        }
        if failed > 0 {
            return Err(format!("{failed} of {} not restored", assets.len()));
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

/// Two names (tags, people) from the arguments: separated by `->`, or one word each.
/// The person tag `name` names: its path, or a display name only one person has.
fn existing_person(catalog: &Catalog, name: &str) -> Result<archive_core::catalog::TagId, String> {
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
}

fn two_names(args: &[&str], usage: &str) -> Result<(String, String), String> {
    match args.iter().position(|w| *w == "->") {
        Some(i) if i > 0 && i + 1 < args.len() => Ok((args[..i].join(" "), args[i + 1..].join(" "))),
        None if args.len() == 2 => Ok((args[0].to_owned(), args[1].to_owned())),
        _ => Err(usage.to_owned()),
    }
}

/// "today", "1 day ago", "12 days ago" for a time in seconds since the epoch.
fn ago(at: i64) -> String {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64);
    match (now - at).max(0) / 86_400 {
        0 => "today".to_owned(),
        1 => "1 day ago".to_owned(),
        days => format!("{days} days ago"),
    }
}

/// "3.4 MB".
fn megabytes(bytes: u64) -> String {
    format!("{:.1} MB", bytes as f64 / 1_000_000.0)
}

fn err(e: archive_core::catalog::Error) -> String {
    e.to_string()
}
