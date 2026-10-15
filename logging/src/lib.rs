//! The logger behind the `log` facade for Archive Studio's front ends (`archive-core` only
//! uses the facade; this crate is what the binaries install).
//!
//! Every record goes to stderr and to a log file in the library
//! (`<library>/.archive-studio/logs/<name>.log`), each with its own filter:
//!
//! - **stderr**: `ARCHIVE_STUDIO_LOG`, in `RUST_LOG` syntax (`debug`,
//!   `info,archive_studio=trace,archive_core=debug`, …); a front end's default otherwise.
//! - **file**: whatever stderr shows, plus at least what the front end asks for (the GUI
//!   records `debug` for its own modules), so there is a useful log after the fact even when
//!   nobody set the variable.
//!
//! In process, the last [`RECENT`] records are kept ([`recent`]) and new ones can be
//! followed ([`subscribe`]), e.g. for a log window; both get what the file gets. With
//! [`capture_stderr`], output that bypasses `log` (C libraries writing to fd 2) is logged too.
//!
//! Lines are `2026-10-15 14:03:12.345 DEBUG [main] archive_studio::nav: …` (local time).
//! The file is rotated when the program starts and when it passes [`MAX_BYTES`]; the last
//! [`KEEP`] files are kept (`name.log`, `name.log.1`, … oldest last).

use std::collections::VecDeque;
use std::fmt::Write as _;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write as _};
use std::os::fd::FromRawFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{mpsc, Mutex};
use std::time::SystemTime;

use log::{Level, LevelFilter, Log, Metadata, Record};

/// The environment variable with the stderr filter.
pub const ENV: &str = "ARCHIVE_STUDIO_LOG";
/// The commit the binary was built from (`git rev-parse --short`), or `unknown`.
pub const GIT_HASH: &str = env!("GIT_HASH");
/// Log files kept: the current one and this many minus one older ones.
pub const KEEP: usize = 5;
/// A log file is rotated once it is this big.
pub const MAX_BYTES: u64 = 16 << 20;

/// Where a library's log files go.
pub fn dir_for(library: &Path) -> PathBuf {
    library.join(".archive-studio").join("logs")
}

/// What a front end sets up.
pub struct Config<'a> {
    /// The directory for the log file (see [`dir_for`]); `None` logs to stderr only.
    pub dir: Option<PathBuf>,
    /// The file's name in it, e.g. `archive-studio.log`.
    pub file_name: &'a str,
    /// The stderr filter when [`ENV`] is unset, e.g. `info`.
    pub stderr_default: &'a str,
    /// What the file records at least, whatever the stderr filter, e.g.
    /// `info,archive_studio=debug`.
    pub file_minimum: &'a str,
}

/// How [`init`] went, for the startup lines.
pub struct Started {
    /// The log file, if one could be opened.
    pub file: Option<PathBuf>,
    /// Why there is no log file.
    pub file_error: Option<String>,
    /// The stderr filter in effect.
    pub stderr_filter: String,
    /// Why [`ENV`] was ignored (it didn't parse).
    pub env_error: Option<String>,
}

/// Installs the logger (once; later calls just report what they would have done) and a
/// panic hook that logs panics before the default hook prints them.
pub fn init(config: Config) -> Started {
    let (stderr_spec, env_error) = match std::env::var(ENV) {
        Ok(spec) if !spec.trim().is_empty() => match Filter::parse(&spec) {
            Ok(_) => (spec, None),
            Err(e) => (config.stderr_default.to_owned(), Some(format!("{ENV}={spec:?}: {e}; using {:?}", config.stderr_default))),
        },
        _ => (config.stderr_default.to_owned(), None),
    };
    let stderr = Filter::parse(&stderr_spec).unwrap_or_else(|_| Filter::level(LevelFilter::Info));
    let file_filter = Filter::parse(config.file_minimum).unwrap_or_else(|_| Filter::level(LevelFilter::Debug));

    let (file, file_error) = match config.dir.as_deref().map(|dir| LogFile::open(dir, config.file_name)) {
        Some(Ok(file)) => (Some(file), None),
        Some(Err(e)) => (None, Some(e)),
        None => (None, None),
    };
    let path = file.as_ref().map(|f| f.path.clone());
    let max = stderr.max().max(file_filter.max());
    let logger = Logger { stderr, file_filter, file: file.map(Mutex::new) };
    if log::set_boxed_logger(Box::new(logger)).is_ok() {
        log::set_max_level(max);
        install_panic_hook();
    }
    Started { file: path, file_error, stderr_filter: stderr_spec, env_error }
}

/// Logs a panic (thread, location, message; the backtrace at debug, so in the file) and
/// then runs the hook that was there before (by default, the message on stderr).
fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let thread = std::thread::current();
        let name = thread.name().map_or_else(|| format!("{:?}", thread.id()), str::to_owned);
        let message = info
            .payload()
            .downcast_ref::<&str>()
            .map(|s| (*s).to_owned())
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "(non-string payload)".to_owned());
        let location = info.location().map_or_else(|| "?".to_owned(), |l| format!("{}:{}", l.file(), l.line()));
        log::error!(target: "panic", "thread '{name}' panicked at {location}: {message}");
        if log::log_enabled!(target: "panic", Level::Debug) {
            log::debug!(target: "panic", "backtrace of '{name}':\n{}", std::backtrace::Backtrace::force_capture());
        }
        previous(info);
    }));
}

/// A `RUST_LOG`-style filter: comma-separated `level`, `target` or `target=level`; the
/// most specific target prefix (by `::` path segments) wins. Unlisted targets are off
/// unless a bare level is given. `-` in a target reads as `_` (crate names).
#[derive(Clone, Debug, PartialEq)]
pub struct Filter {
    default: LevelFilter,
    /// Longest target first.
    directives: Vec<(String, LevelFilter)>,
}

impl Filter {
    pub fn level(level: LevelFilter) -> Self {
        Self { default: level, directives: Vec::new() }
    }

    pub fn parse(spec: &str) -> Result<Self, String> {
        let mut filter = Self::level(LevelFilter::Off);
        for part in spec.split(',').map(str::trim).filter(|p| !p.is_empty()) {
            match part.split_once('=') {
                Some((target, level)) => {
                    let level = parse_level(level).ok_or_else(|| format!("unknown level {:?}", level.trim()))?;
                    filter.set(target.trim(), level);
                }
                None => match parse_level(part) {
                    Some(level) => filter.default = level,
                    None => filter.set(part, LevelFilter::Trace),
                },
            }
        }
        Ok(filter)
    }

    fn set(&mut self, target: &str, level: LevelFilter) {
        let target = target.replace('-', "_");
        self.directives.retain(|(t, _)| *t != target);
        self.directives.push((target, level));
        self.directives.sort_by(|a, b| b.0.len().cmp(&a.0.len()));
    }

    /// The level `target` logs at.
    pub fn level_for(&self, target: &str) -> LevelFilter {
        self.directives
            .iter()
            .find(|(t, _)| target == t || (target.starts_with(t.as_str()) && target[t.len()..].starts_with("::")))
            .map_or(self.default, |(_, level)| *level)
    }

    pub fn enabled(&self, target: &str, level: Level) -> bool {
        level <= self.level_for(target)
    }

    /// The most verbose level anything logs at.
    pub fn max(&self) -> LevelFilter {
        self.directives.iter().map(|(_, l)| *l).fold(self.default, Ord::max)
    }
}

fn parse_level(s: &str) -> Option<LevelFilter> {
    Some(match s.trim().to_ascii_lowercase().as_str() {
        "off" | "none" => LevelFilter::Off,
        "error" => LevelFilter::Error,
        "warn" | "warning" => LevelFilter::Warn,
        "info" => LevelFilter::Info,
        "debug" => LevelFilter::Debug,
        "trace" => LevelFilter::Trace,
        _ => return None,
    })
}

struct Logger {
    stderr: Filter,
    file_filter: Filter,
    file: Option<Mutex<LogFile>>,
}

impl Logger {
    fn to_stderr(&self, metadata: &Metadata) -> bool {
        // Captured stderr (see `capture_stderr`) is already on the real stderr.
        metadata.target() != STDERR_TARGET && self.stderr.enabled(metadata.target(), metadata.level())
    }

    /// Kept in the file (if any), the recent records and for subscribers.
    fn to_file(&self, metadata: &Metadata) -> bool {
        self.stderr.enabled(metadata.target(), metadata.level()) || self.file_filter.enabled(metadata.target(), metadata.level())
    }
}

impl Log for Logger {
    fn enabled(&self, metadata: &Metadata) -> bool {
        self.to_file(metadata)
    }

    fn log(&self, record: &Record) {
        if !self.to_file(record.metadata()) {
            return;
        }
        let thread = std::thread::current();
        let entry = Entry {
            time: SystemTime::now(),
            level: record.level(),
            target: record.target().to_owned(),
            thread: thread.name().map_or_else(|| format!("{:?}", thread.id()), str::to_owned),
            message: record.args().to_string(),
        };
        let line = entry.line();
        if self.to_stderr(record.metadata()) {
            write_stderr(line.as_bytes());
        }
        if let Some(log_file) = &self.file {
            if let Ok(mut log_file) = log_file.lock() {
                log_file.write(line.as_bytes());
            }
        }
        publish(entry);
    }

    fn flush(&self) {}
}

/// One log record, as kept for [`recent`] and sent to [`subscribe`]rs.
#[derive(Clone, Debug)]
pub struct Entry {
    pub time: SystemTime,
    pub level: Level,
    pub target: String,
    /// The thread's name, or its id if it has none.
    pub thread: String,
    pub message: String,
}

impl Entry {
    /// The record as a line of the log file, with its newline.
    pub fn line(&self) -> String {
        let mut line = timestamp(self.time);
        let _ = writeln!(line, " {:<5} [{}] {}: {}", self.level, self.thread, self.target, self.message);
        line
    }
}

/// Records kept in memory for [`recent`].
pub const RECENT: usize = 5000;
/// Records a subscriber may fall behind by before new ones are dropped for it.
pub const SUBSCRIBER_QUEUE: usize = 4096;

struct Hub {
    recent: VecDeque<Entry>,
    subscribers: Vec<mpsc::SyncSender<Entry>>,
}

static HUB: Mutex<Hub> = Mutex::new(Hub { recent: VecDeque::new(), subscribers: Vec::new() });

fn publish(entry: Entry) {
    let Ok(mut hub) = HUB.lock() else { return };
    // Never blocks: a subscriber that is behind misses records; a gone one is dropped.
    hub.subscribers.retain(|s| !matches!(s.try_send(entry.clone()), Err(mpsc::TrySendError::Disconnected(_))));
    if hub.recent.len() == RECENT {
        hub.recent.pop_front();
    }
    hub.recent.push_back(entry);
}

/// The last [`RECENT`] records logged (everything the file gets, whether or not there is a
/// file), oldest first.
pub fn recent() -> Vec<Entry> {
    HUB.lock().map(|hub| hub.recent.iter().cloned().collect()).unwrap_or_default()
}

/// Every record logged from now on (as [`recent`] keeps them). The logging thread never
/// waits for the receiver: if it falls [`SUBSCRIBER_QUEUE`] records behind, newer ones are
/// dropped for it. Dropping the receiver unsubscribes. To miss nothing between the two, call
/// this before [`recent`] and skip the overlap by time.
pub fn subscribe() -> mpsc::Receiver<Entry> {
    let (send, receive) = mpsc::sync_channel(SUBSCRIBER_QUEUE);
    if let Ok(mut hub) = HUB.lock() {
        hub.subscribers.push(send);
    }
    receive
}

/// The target of lines captured from file descriptor 2 by [`capture_stderr`].
pub const STDERR_TARGET: &str = "stderr";

/// Where the logger's own stderr output goes: fd 2, or the real stderr once captured.
static STDERR_FD: AtomicI32 = AtomicI32::new(2);

fn write_stderr(mut bytes: &[u8]) {
    let fd = STDERR_FD.load(Ordering::Relaxed);
    while !bytes.is_empty() {
        // SAFETY: writes from a valid buffer to a descriptor this process keeps open.
        let n = unsafe { libc::write(fd, bytes.as_ptr().cast(), bytes.len()) };
        if n <= 0 {
            if n < 0 && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return;
        }
        bytes = &bytes[n as usize..];
    }
}

/// Puts a pipe over file descriptor 2, so what C libraries (and `eprintln!`, and the
/// default panic hook) write to stderr also reaches the log: a reader thread passes each
/// line on to the real stderr and logs it at info with target [`STDERR_TARGET`]. The
/// logger's own stderr output goes straight to the real stderr. Once per process; call it
/// after [`init`].
pub fn capture_stderr() -> Result<(), String> {
    static DONE: AtomicBool = AtomicBool::new(false);
    if DONE.swap(true, Ordering::SeqCst) {
        return Ok(());
    }
    let os = |what: &str| format!("{what}: {}", std::io::Error::last_os_error());
    let mut fds = [0; 2];
    // SAFETY: plain descriptor calls; every result is checked.
    unsafe {
        let real = libc::fcntl(2, libc::F_DUPFD_CLOEXEC, 3);
        if real < 0 {
            return Err(os("dup stderr"));
        }
        if libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) != 0 {
            libc::close(real);
            return Err(os("pipe"));
        }
        if libc::dup2(fds[1], 2) < 0 {
            libc::close(real);
            libc::close(fds[0]);
            libc::close(fds[1]);
            return Err(os("dup2"));
        }
        libc::close(fds[1]);
        STDERR_FD.store(real, Ordering::SeqCst);
    }
    // SAFETY: the read end is ours alone from here on.
    let pipe = unsafe { File::from_raw_fd(fds[0]) };
    std::thread::Builder::new()
        .name("stderr-capture".into())
        .spawn(move || {
            let mut reader = BufReader::new(pipe);
            let mut line = Vec::new();
            while reader.read_until(b'\n', &mut line).is_ok_and(|n| n > 0) {
                write_stderr(&line);
                let text = String::from_utf8_lossy(&line);
                log::info!(target: STDERR_TARGET, "{}", text.trim_end_matches(['\n', '\r']));
                line.clear();
            }
        })
        .map_err(|e| format!("stderr reader: {e}"))?;
    Ok(())
}

/// `2026-10-15 14:03:12.345`, local time.
pub fn timestamp(t: SystemTime) -> String {
    let since = t.duration_since(SystemTime::UNIX_EPOCH).unwrap_or_default();
    let secs = since.as_secs() as libc::time_t;
    // SAFETY: `localtime_r` only writes the `tm` we pass it.
    let tm = unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&secs, &mut tm).is_null() {
            return format!("{}.{:03}", since.as_secs(), since.subsec_millis());
        }
        tm
    };
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:03}",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min,
        tm.tm_sec,
        since.subsec_millis()
    )
}

/// The local time zone's offset from UTC, e.g. `+02:00`.
pub fn utc_offset() -> String {
    let secs = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map_or(0, |d| d.as_secs()) as libc::time_t;
    // SAFETY: as in `timestamp`.
    let offset = unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&secs, &mut tm).is_null() {
            return String::new();
        }
        tm.tm_gmtoff
    };
    let sign = if offset < 0 { '-' } else { '+' };
    format!("{sign}{:02}:{:02}", offset.abs() / 3600, offset.abs() % 3600 / 60)
}

struct LogFile {
    path: PathBuf,
    file: Option<File>,
    written: u64,
}

impl LogFile {
    /// Rotates what an earlier run left and starts a new file.
    fn open(dir: &Path, name: &str) -> Result<Self, String> {
        fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        let path = dir.join(name);
        if fs::metadata(&path).is_ok_and(|m| m.len() > 0) {
            rotate(&path);
        }
        let file = OpenOptions::new().create(true).append(true).open(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        Ok(Self { path, file: Some(file), written: 0 })
    }

    fn write(&mut self, line: &[u8]) {
        if self.written >= MAX_BYTES {
            self.file = None;
            rotate(&self.path);
            self.file = OpenOptions::new().create(true).append(true).open(&self.path).ok();
            self.written = 0;
        }
        if let Some(file) = self.file.as_mut() {
            // Unbuffered: what was logged before a crash is in the file.
            if file.write_all(line).is_ok() {
                self.written += line.len() as u64;
            }
        }
    }
}

/// `name.log` → `name.log.1` → … → `name.log.{KEEP - 1}`, dropping the oldest.
fn rotate(path: &Path) {
    let numbered = |i: usize| PathBuf::from(format!("{}.{i}", path.display()));
    let _ = fs::remove_file(numbered(KEEP - 1));
    for i in (1..KEEP - 1).rev() {
        let _ = fs::rename(numbered(i), numbered(i + 1));
    }
    let _ = fs::rename(path, numbered(1));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filters() {
        let f = Filter::parse("info,archive_studio=trace,archive-core::pipeline=warn").unwrap();
        assert_eq!(f.level_for("archive_studio::people"), LevelFilter::Trace);
        assert_eq!(f.level_for("archive_studio"), LevelFilter::Trace);
        assert_eq!(f.level_for("archive_studios"), LevelFilter::Info, "a prefix must end at a path segment");
        assert_eq!(f.level_for("archive_core::pipeline::rotate"), LevelFilter::Warn);
        assert_eq!(f.level_for("archive_core::catalog"), LevelFilter::Info);
        assert_eq!(f.max(), LevelFilter::Trace);
        assert_eq!(Filter::parse("debug").unwrap().level_for("x"), LevelFilter::Debug);
        assert_eq!(Filter::parse("archive_core").unwrap().level_for("slint"), LevelFilter::Off);
        assert!(Filter::parse("archive_core=loud").is_err());
    }

    #[test]
    fn recent_and_subscribers() {
        let entry = |message: &str| Entry {
            time: SystemTime::now(),
            level: Level::Debug,
            target: "t".into(),
            thread: "main".into(),
            message: message.into(),
        };
        let receive = subscribe();
        for i in 0..RECENT + 10 {
            publish(entry(&i.to_string()));
        }
        let recent = recent();
        assert_eq!(recent.len(), RECENT);
        assert_eq!(recent.last().unwrap().message, (RECENT + 9).to_string());
        let got: Vec<Entry> = receive.try_iter().collect();
        assert_eq!(got.len(), SUBSCRIBER_QUEUE, "a full subscriber drops instead of blocking");
        assert_eq!(got[0].message, "0");
        drop(receive);
        publish(entry("after"));
        assert!(HUB.lock().unwrap().subscribers.is_empty(), "a dropped receiver unsubscribes");
        assert!(entry("x").line().ends_with(" DEBUG [main] t: x\n"));
    }

    #[test]
    fn rotation_keeps_the_last_files() {
        let dir = std::env::temp_dir().join(format!("archive-logging-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        for run in 0..KEEP + 2 {
            let mut file = LogFile::open(&dir, "x.log").unwrap();
            file.write(format!("run {run}\n").as_bytes());
        }
        let mut names: Vec<String> = fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        names.sort();
        assert_eq!(names.len(), KEEP);
        assert_eq!(fs::read_to_string(dir.join("x.log")).unwrap(), format!("run {}\n", KEEP + 1));
        assert_eq!(fs::read_to_string(dir.join("x.log.1")).unwrap(), format!("run {}\n", KEEP));
        fs::remove_dir_all(&dir).unwrap();
    }
}
