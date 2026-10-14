//! `archive [ROOT]`: Archive Studio's interactive shell. While it is open, the pipeline
//! engine runs on its own thread (see `archive_core::pipeline`): it catches up with the
//! library, watches it, and its reports are printed above the prompt.
//!
//! `-c COMMAND` runs a command and exits (repeatable); `--until-idle` exits once the
//! pipeline has nothing left to do.

use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc, Mutex};

use archive_core::catalog::runs::{RunCounts, RunStatus};
use archive_core::catalog::Catalog;
use archive_core::pipeline::{self, Availability, Config, Engine, Event, Mode, Models, FACES, ORIENT};
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
faces                      people and unknown faces
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
            "faces" => self.faces(),
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
        self.engine()?.rotate(self.resolve(path), cw)
    }

    fn failed(&self) -> Result<(), String> {
        let mut any = false;
        for stage in [ORIENT, FACES] {
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
}
