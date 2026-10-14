//! DV camcorders over FireWire (IEEE 1394), e.g. a Sony Digital8 DCR-TRV240 playing back
//! Hi8 / Video8 / Digital8 tapes. Analog tapes are digitized to DV by the camera itself;
//! the DV stream it sends is saved bit for bit as `<out_dir>/tape-NNNN.dv`.
//!
//! Nothing here can stall the host or another device, whatever the camera or bus does:
//! - `dvgrab` streams DV to a reader thread for as long as the panel is up. The reader
//!   writes frames to the open tape file and turns every third one into a preview, so
//!   recording starts and stops without restarting the stream.
//! - A controller thread runs the panel's state machine. It drives the tape transport
//!   over AV/C with short-lived `dvcont` processes, each killed if it hasn't answered
//!   within [`AVC_TIMEOUT`]. A hung `dvgrab` is killed and restarted, and an unplugged
//!   camera closes the open file and is picked up again when it returns.
//! - Children get `PR_SET_PDEATHSIG`, so they die with the controller (or the host process)
//!   instead of holding the camera.
//!
//! Record rewinds to the start and captures the whole tape; Record from here captures
//! from where the tape is. A capture ends at the end of the tape (the camera stops
//! playing), on Stop, after [`BLANK_STOP`] of blank picture, or at [`MAX_DURATION`].
//! `ARCHIVE_STUDIO_DV_AUTO=1` starts a whole-tape capture whenever a tape goes in.
//!
//! Only the first AV/C camcorder on the bus is used: `dvcont` addresses the first AV/C
//! node it finds.

use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, BufWriter, Read, Write};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use image::{imageops, RgbImage};

use crate::device::{CaptureDevice, DeviceEvent, DeviceKind, EventSink, RECENT};
use crate::resize;

const SYSFS: &str = "/sys/bus/firewire/devices";
/// IEEE 1394 Trade Association unit specifier for AV/C devices.
const AVC_SPECIFIER: &str = "0x00a02d";
const INSTALL_HINT: &str = "install dvgrab and dvcont: sudo apt install dvgrab libavc1394-tools";

const TICK: Duration = Duration::from_millis(100);
const POLL: Duration = Duration::from_secs(1);
/// `dvcont` answers in ~0.1 s; a camera that takes longer than this is treated as gone.
const AVC_TIMEOUT: Duration = Duration::from_secs(3);
/// No frame for this long while recording ends the capture with an error.
const SIGNAL_LOST: Duration = Duration::from_secs(4);
/// A stream that hasn't delivered a frame for this long is killed and restarted.
const STREAM_RESTART: Duration = Duration::from_secs(10);
const REWIND_TIMEOUT: Duration = Duration::from_secs(15 * 60);
const PLAY_TIMEOUT: Duration = Duration::from_secs(20);
/// Polls in a row without playback before a capture counts as at the end of the tape.
const END_POLLS: u32 = 2;
/// Continuous blank picture for this long ends a capture: the rest of the tape is unused.
const BLANK_STOP: Duration = Duration::from_secs(120);
/// Below this [`dif::Picture::luma_spread`] (0–255 scale) a frame is blank: the camera's
/// flat black or blue screen, even with its own warnings (e.g. a blinking "STAMINA")
/// drawn over it, which only touch a few percent of the picture.
const BLANK_SPREAD: f32 = 4.0;
/// A 120-minute Hi8 tape recorded in LP, plus margin.
const MAX_DURATION: Duration = Duration::from_secs(250 * 60);
/// The poster (recent strip, review thumbnail) is the first non-blank frame after this much.
const POSTER_AFTER: Duration = Duration::from_secs(10);
const PREVIEW_EVERY: u64 = 3;
const PREVIEW_SIZE: (u32, u32) = (320, 240);
/// Longest edge of the thumbnail sent with each saved tape.
const THUMB_EDGE: u32 = 192;
const FLUSH_EVERY: u64 = 30;

const RECORD_FROM_HERE: &str = "Record from here";

/// Serializes backends across Rescan: a new controller waits until the old one has
/// released the camera.
static BUS: Mutex<()> = Mutex::new(());

/// An AV/C unit found in sysfs.
#[derive(Clone, Debug)]
pub struct Unit {
    guid: String,
    name: String,
}

/// The first AV/C unit on the bus (a camcorder or DV deck). Only reads sysfs.
pub fn find() -> Option<Unit> {
    let mut nodes: Vec<PathBuf> = fs::read_dir(SYSFS).ok()?.filter_map(|e| Some(e.ok()?.path())).collect();
    nodes.sort();
    nodes.into_iter().find_map(|node| {
        let read = |name: &str| fs::read_to_string(node.join(name)).ok().map(|s| s.trim().to_owned());
        let units = read("units")?;
        if !units.split_whitespace().any(|u| u.starts_with(&format!("{AVC_SPECIFIER}:"))) {
            return None;
        }
        let name = match (read("vendor_name"), read("model_name")) {
            (Some(v), Some(m)) => format!("{v} {m}"),
            (_, Some(m)) | (Some(m), None) => m,
            _ => "DV camcorder".into(),
        };
        Some(Unit { guid: read("guid")?, name })
    })
}

fn present(sysfs: &Path, guid: &str) -> bool {
    fs::read_dir(sysfs).is_ok_and(|entries| {
        entries.flatten().any(|e| fs::read_to_string(e.path().join("guid")).is_ok_and(|g| g.trim() == guid))
    })
}

impl Unit {
    /// Vendor and model, e.g. `Sony DCR-TRV240`; also the device's name in the UI.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Directory name for this camera's tapes, e.g. `sony-dcr-trv240`.
    pub fn slug(&self) -> String {
        let s: String = self.name.chars().map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '-' }).collect();
        s.split('-').filter(|p| !p.is_empty()).collect::<Vec<_>>().join("-")
    }
}

const NO_REQUEST: u8 = 0;
const WHOLE_TAPE: u8 = 1;
const FROM_HERE: u8 = 2;

/// Flags shared between the host-side handle and the controller thread.
struct Control {
    alive: AtomicBool,
    request: AtomicU8,
    stop: AtomicBool,
}

/// The external programs and the sysfs directory the controller uses; tests swap in fakes.
#[derive(Clone)]
struct Tools {
    dvgrab: PathBuf,
    dvcont: PathBuf,
    sysfs: PathBuf,
}

impl Default for Tools {
    fn default() -> Self {
        Self { dvgrab: "dvgrab".into(), dvcont: "dvcont".into(), sysfs: SYSFS.into() }
    }
}

impl Tools {
    fn installed(&self) -> bool {
        [&self.dvgrab, &self.dvcont].into_iter().all(|program| on_path(program))
    }

    /// Runs `dvcont <command>`, killing it after [`AVC_TIMEOUT`]. Returns its last output line.
    fn avc(&self, command_name: &str) -> Result<String, String> {
        let mut child = command(&self.dvcont)
            .arg(command_name)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| e.to_string())?;
        match wait_timeout(&mut child, AVC_TIMEOUT) {
            Ok(Some(_)) => {}
            Ok(None) => {
                kill(&mut child);
                return Err("camera didn't answer".into());
            }
            Err(e) => return Err(e.to_string()),
        }
        let mut out = String::new();
        child.stdout.take().expect("piped").read_to_string(&mut out).map_err(|e| e.to_string())?;
        Ok(out.lines().map(str::trim).rfind(|l| !l.is_empty()).unwrap_or_default().to_owned())
    }

    fn transport(&self) -> Option<Transport> {
        Transport::parse(&self.avc("status").ok()?)
    }
}

pub struct Camcorder {
    unit: Unit,
    out_dir: PathBuf,
    control: Arc<Control>,
    tools: Tools,
    started: bool,
}

impl Camcorder {
    pub fn new(unit: Unit, out_dir: PathBuf) -> Self {
        let control = Control { alive: AtomicBool::new(true), request: AtomicU8::new(NO_REQUEST), stop: AtomicBool::new(false) };
        Self { unit, out_dir, control: Arc::new(control), tools: Tools::default(), started: false }
    }
}

impl Drop for Camcorder {
    fn drop(&mut self) {
        // The controller notices within a tick (or after an in-flight `dvcont`, at most
        // AVC_TIMEOUT), closes the tape file and kills its children.
        self.control.alive.store(false, Ordering::Release);
    }
}

impl CaptureDevice for Camcorder {
    fn name(&self) -> &str {
        &self.unit.name
    }

    fn kind(&self) -> DeviceKind {
        DeviceKind::Camcorder
    }

    fn start_preview(&mut self, sink: EventSink) {
        if std::mem::replace(&mut self.started, true) {
            return;
        }
        let (control, unit, out_dir, tools) = (self.control.clone(), self.unit.clone(), self.out_dir.clone(), self.tools.clone());
        thread::Builder::new()
            .name("dv".into())
            .spawn(move || {
                let _bus = BUS.lock().unwrap_or_else(|e| e.into_inner());
                Controller::new(&control, unit, out_dir, tools, sink).run();
            })
            .expect("spawn DV thread");
    }

    fn start_acquire(&mut self, _out_dir: &Path, _sink: EventSink) {
        self.control.stop.store(false, Ordering::Release);
        self.control.request.store(WHOLE_TAPE, Ordering::Release);
    }

    fn start_alt(&mut self, _out_dir: &Path, _sink: EventSink) {
        self.control.stop.store(false, Ordering::Release);
        self.control.request.store(FROM_HERE, Ordering::Release);
    }

    fn stop(&mut self) {
        self.control.request.store(NO_REQUEST, Ordering::Release);
        self.control.stop.store(true, Ordering::Release);
    }
}

/// Tape transport state, from `dvcont status`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Transport {
    NoTape,
    Stopped,
    Playing,
    Paused,
    /// Rewinding, fast-forwarding or cueing.
    Winding,
    Recording,
}

impl Transport {
    fn parse(line: &str) -> Option<Self> {
        Some(match line.trim() {
            "Loading Medium" => Self::NoTape,
            "Winding stopped" => Self::Stopped,
            "Playing" => Self::Playing,
            "Playing Paused" => Self::Paused,
            "Recording" | "Recording Paused" => Self::Recording,
            s if s.starts_with("Winding") || s.starts_with("Playing ") => Self::Winding,
            _ => return None,
        })
    }

    fn label(self) -> &'static str {
        match self {
            Self::NoTape => "No tape",
            Self::Stopped => "Tape stopped",
            Self::Playing => "Playing",
            Self::Paused => "Paused",
            Self::Winding => "Winding",
            Self::Recording => "Camera recording",
        }
    }
}

/// What the controller is doing with the tape.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Idle,
    Rewinding { since: Instant },
    Starting { since: Instant },
    Recording { not_playing: u32 },
}

/// State shared between the controller and the stream's reader thread.
#[derive(Default)]
struct Tap {
    last_frame: Option<Instant>,
    system: Option<dif::System>,
    /// Since when the picture has been blank, if it is.
    blank_since: Option<Instant>,
    /// Latest non-blank picture, kept for the poster.
    last_picture: Option<RgbImage>,
    recording: Option<Recording>,
}

struct Recording {
    path: PathBuf,
    file: Option<BufWriter<File>>,
    error: Option<io::Error>,
    frames: u64,
    bytes: u64,
    started: Instant,
    poster: Option<RgbImage>,
}

impl Recording {
    fn duration(&self, system: Option<dif::System>) -> Duration {
        Duration::from_secs_f64(self.frames as f64 / system.map_or(29.97, dif::System::fps))
    }
}

/// A running `dvgrab`, streaming DV into the reader thread.
struct Stream {
    child: Child,
    started: Instant,
    /// Last line `dvgrab` wrote to stderr, for error messages.
    log: Arc<Mutex<String>>,
}

impl Drop for Stream {
    fn drop(&mut self) {
        kill(&mut self.child);
    }
}

struct Controller<'a> {
    control: &'a Control,
    unit: Unit,
    out_dir: PathBuf,
    tools: Tools,
    sink: EventSink,
    tap: Arc<Mutex<Tap>>,
    stream: Option<Stream>,
    phase: Phase,
    transport: Option<Transport>,
    auto: bool,
    captured: u32,
    /// Outcome of the last capture, shown until the next one.
    last_result: Option<String>,
    error: Option<String>,
    sent_status: String,
    sent_alt: Option<bool>,
}

impl<'a> Controller<'a> {
    fn new(control: &'a Control, unit: Unit, out_dir: PathBuf, tools: Tools, sink: EventSink) -> Self {
        Self {
            control,
            unit,
            out_dir,
            tools,
            sink,
            tap: Arc::default(),
            stream: None,
            phase: Phase::Idle,
            transport: None,
            auto: std::env::var("ARCHIVE_STUDIO_DV_AUTO").is_ok_and(|v| v == "1"),
            captured: 0,
            last_result: None,
            error: None,
            sent_status: String::new(),
            sent_alt: None,
        }
    }

    fn run(mut self) {
        show_recent(&self.out_dir, &self.sink);
        self.send_alt(false);
        let mut next_poll = Instant::now();
        while self.control.alive.load(Ordering::Acquire) {
            if Instant::now() >= next_poll {
                next_poll = Instant::now() + POLL;
                self.poll();
            }
            self.handle_requests();
            thread::sleep(TICK);
        }
        // Panel dropped (Rescan, exit): close the file and stop the tape.
        if self.tap().recording.is_some() {
            self.finish("closed");
        } else if self.phase != Phase::Idle {
            let _ = self.tools.avc("stop");
        }
    }

    fn tap(&self) -> MutexGuard<'_, Tap> {
        self.tap.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Once per POLL: checks tools, the bus and the stream, reads the transport state and
    /// advances the phase.
    fn poll(&mut self) {
        if !self.tools.installed() {
            return self.fail(INSTALL_HINT.into());
        }
        if !present(&self.tools.sysfs, &self.unit.guid) {
            return self.fail("Camera disconnected: check the FireWire cable and power".into());
        }
        if let Err(e) = self.keep_streaming() {
            return self.fail(e);
        }
        let previous = self.transport;
        self.transport = self.tools.transport();
        let Some(transport) = self.transport else {
            return self.fail("Camera not answering: is it on, in PLAY/EDIT (VCR) mode?".into());
        };
        self.recovered();

        let inserted = previous.is_some_and(|t| t == Transport::NoTape) && transport != Transport::NoTape;
        if self.auto && inserted && self.phase == Phase::Idle {
            self.last_result = None;
            (self.sink)(DeviceEvent::Started);
            self.begin(true);
        }
        self.advance(transport);
        self.report(transport);
    }

    fn handle_requests(&mut self) {
        if self.control.stop.swap(false, Ordering::AcqRel) && self.phase != Phase::Idle {
            if self.tap().recording.is_some() {
                self.finish("stopped");
            } else {
                let _ = self.tools.avc("stop");
                self.last_result = Some("Cancelled".into());
                self.phase = Phase::Idle;
                (self.sink)(DeviceEvent::Finished);
            }
            self.sent_status.clear();
        }
        let request = self.control.request.swap(NO_REQUEST, Ordering::AcqRel);
        if request == NO_REQUEST || self.phase != Phase::Idle {
            return;
        }
        self.last_result = None;
        self.sent_status.clear();
        match self.transport {
            Some(Transport::NoTape) => self.refuse("Insert a tape first"),
            Some(Transport::Recording) => self.refuse("The camera is recording; switch it to PLAY/EDIT"),
            None => self.refuse("Camera not ready"),
            Some(_) => self.begin(request == WHOLE_TAPE),
        }
    }

    fn refuse(&mut self, why: &str) {
        self.last_result = Some(why.into());
        (self.sink)(DeviceEvent::Finished);
    }

    fn begin(&mut self, whole_tape: bool) {
        if whole_tape {
            let _ = self.tools.avc("stop");
            let _ = self.tools.avc("rewind");
            self.phase = Phase::Rewinding { since: Instant::now() };
        } else {
            self.play();
        }
    }

    /// Opens the tape file and starts playback; frames go to the file from now on.
    fn play(&mut self) {
        let path = match fs::create_dir_all(&self.out_dir).map(|()| next_tape(&self.out_dir)) {
            Ok(path) => path,
            Err(e) => return self.abort(format!("Can't create {}: {e}", self.out_dir.display())),
        };
        let file = match File::create(&path) {
            Ok(file) => file,
            Err(e) => return self.abort(format!("Can't create {}: {e}", path.display())),
        };
        self.tap().recording = Some(Recording {
            path,
            file: Some(BufWriter::with_capacity(1 << 20, file)),
            error: None,
            frames: 0,
            bytes: 0,
            started: Instant::now(),
            poster: None,
        });
        if let Err(e) = self.tools.avc("play") {
            return self.finish(&format!("couldn't start playback: {e}"));
        }
        self.phase = Phase::Starting { since: Instant::now() };
    }

    fn advance(&mut self, transport: Transport) {
        match self.phase {
            Phase::Idle => {}
            Phase::Rewinding { since } => match transport {
                Transport::Stopped if since.elapsed() >= POLL * 2 => self.play(),
                Transport::NoTape => self.abort("Tape removed while rewinding".into()),
                _ if since.elapsed() >= REWIND_TIMEOUT => self.abort("Rewind didn't finish".into()),
                _ => {}
            },
            Phase::Starting { since } => match transport {
                Transport::Playing => self.phase = Phase::Recording { not_playing: 0 },
                _ if since.elapsed() >= PLAY_TIMEOUT => self.finish("playback didn't start"),
                _ => self.check_recording(),
            },
            Phase::Recording { not_playing } => {
                if transport == Transport::Playing {
                    self.phase = Phase::Recording { not_playing: 0 };
                } else if not_playing + 1 >= END_POLLS {
                    let why = if transport == Transport::NoTape { "tape removed" } else { "end of tape" };
                    return self.finish(why);
                } else {
                    self.phase = Phase::Recording { not_playing: not_playing + 1 };
                }
                self.check_recording();
            }
        }
    }

    /// Ends the capture on a write error, lost signal, long blank stretch or the time cap.
    fn check_recording(&mut self) {
        let outcome = {
            let tap = self.tap();
            let Some(rec) = &tap.recording else { return };
            if let Some(e) = &rec.error {
                Some(format!("write failed: {e}"))
            } else if tap.last_frame.is_none_or(|t| t.elapsed() >= SIGNAL_LOST) && rec.started.elapsed() >= SIGNAL_LOST {
                Some("lost the DV signal".into())
            } else if tap.blank_since.is_some_and(|t| t.elapsed() >= BLANK_STOP) && rec.started.elapsed() >= BLANK_STOP {
                Some(format!("blank for {} min", BLANK_STOP.as_secs() / 60))
            } else if rec.duration(tap.system) >= MAX_DURATION {
                Some(format!("reached the {} min limit", MAX_DURATION.as_secs() / 60))
            } else {
                None
            }
        };
        if let Some(why) = outcome {
            self.finish(&why);
        }
    }

    /// Closes the tape file, stops the tape and reports the capture. The file is deleted
    /// if the tape never played.
    fn finish(&mut self, why: &str) {
        let played = matches!(self.phase, Phase::Recording { .. });
        let (rec, system, fallback) = {
            let mut tap = self.tap();
            let rec = tap.recording.take();
            (rec, tap.system, tap.last_picture.clone())
        };
        let _ = self.tools.avc("stop");
        self.phase = Phase::Idle;
        self.sent_status.clear();
        let Some(mut rec) = rec else {
            (self.sink)(DeviceEvent::Finished);
            return;
        };
        let closed = rec.file.take().map_or(Ok(()), |mut f| f.flush().and_then(|()| f.get_ref().sync_all()));
        let name = rec.path.file_name().unwrap_or_default().to_string_lossy().into_owned();
        if rec.frames == 0 || !played {
            let _ = fs::remove_file(&rec.path);
            self.last_result = Some(format!("Nothing captured: {why}"));
        } else {
            self.captured += 1;
            (self.sink)(DeviceEvent::Captured(self.captured));
            if let Some(poster) = rec.poster.take().or(fallback) {
                let poster = imageops::resize(&poster, PREVIEW_SIZE.0, PREVIEW_SIZE.1, imageops::FilterType::Triangle);
                let thumb = resize::fit(&poster, THUMB_EDGE);
                let image = closed.is_ok().then(|| Arc::new(poster));
                (self.sink)(DeviceEvent::Saved { path: rec.path.clone(), thumb, image });
            }
            let error = rec.error.as_ref().map(ToString::to_string).or(closed.err().map(|e| e.to_string()));
            self.last_result = Some(format!(
                "Saved {name} · {} · {} · {why}{}",
                clock(rec.duration(system)),
                size(rec.bytes),
                error.map(|e| format!(" · write error: {e}")).unwrap_or_default(),
            ));
        }
        (self.sink)(DeviceEvent::Finished);
    }

    /// Gives up before anything was recorded.
    fn abort(&mut self, why: String) {
        let _ = self.tools.avc("stop");
        self.phase = Phase::Idle;
        self.last_result = Some(why);
        self.sent_status.clear();
        (self.sink)(DeviceEvent::Finished);
    }

    /// Shows an error, ending any capture first. Sent once until the device recovers.
    fn fail(&mut self, error: String) {
        let mut shown = error.clone();
        if self.tap().recording.is_some() {
            self.finish(&error.to_lowercase());
            // The error replaces the status line, so it carries the saved file's name.
            shown = format!("{error} · {}", self.last_result.as_deref().unwrap_or_default());
        } else if self.phase != Phase::Idle {
            self.phase = Phase::Idle;
        }
        self.transport = None;
        self.send_alt(false);
        if self.error.as_ref() != Some(&error) {
            (self.sink)(DeviceEvent::Error(shown));
            self.error = Some(error);
            self.sent_status.clear();
        }
    }

    fn recovered(&mut self) {
        if self.error.take().is_some() {
            (self.sink)(DeviceEvent::Finished);
        }
    }

    /// Starts `dvgrab` if it isn't running, and restarts it if it exited or stalled.
    fn keep_streaming(&mut self) -> Result<(), String> {
        let starved = self.tap().last_frame.is_none_or(|t| t.elapsed() >= STREAM_RESTART);
        if let Some(stream) = &mut self.stream {
            let exited = !matches!(stream.child.try_wait(), Ok(None));
            let stalled = starved && stream.started.elapsed() >= STREAM_RESTART;
            if !exited && !stalled {
                return Ok(());
            }
            let log = stream.log.lock().unwrap_or_else(|e| e.into_inner()).clone();
            self.stream = None;
            if exited && self.tap().recording.is_some() {
                return Err(if log.is_empty() { "dvgrab exited".into() } else { format!("dvgrab: {log}") });
            }
        }
        self.stream = Some(self.spawn_stream().map_err(|e| format!("Can't run dvgrab: {e}"))?);
        Ok(())
    }

    fn spawn_stream(&self) -> io::Result<Stream> {
        // -noavc: the transport is ours (via dvcont); dvgrab would otherwise stop the tape
        // when it exits.
        let mut child = command(&self.tools.dvgrab)
            .args(["-guid", &self.unit.guid, "-noavc", "-format", "raw", "-"])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let (stdout, stderr) = (child.stdout.take().expect("piped"), child.stderr.take().expect("piped"));
        let log: Arc<Mutex<String>> = Arc::default();
        let last = log.clone();
        thread::Builder::new().name("dvgrab-log".into()).spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                let line = line.trim();
                if !line.is_empty() && !line.contains("Failed to get channels available") {
                    *last.lock().unwrap_or_else(|e| e.into_inner()) = line.to_owned();
                }
            }
        })?;
        let (tap, sink) = (self.tap.clone(), self.sink.clone());
        thread::Builder::new().name("dv-reader".into()).spawn(move || read_frames(stdout, &tap, &sink))?;
        Ok(Stream { child, started: Instant::now(), log })
    }

    /// Status line and secondary action, sent only when they change.
    fn report(&mut self, transport: Transport) {
        let (system, recording, blank) = {
            let tap = self.tap();
            let rec = tap.recording.as_ref().map(|r| (r.duration(tap.system), r.bytes, r.path.clone()));
            let signal = tap.last_frame.is_some_and(|t| t.elapsed() < SIGNAL_LOST);
            (signal.then_some(tap.system).flatten(), rec, tap.blank_since.is_some())
        };
        let format = system.map_or("no DV signal", dif::System::label);
        let status = match (self.phase, recording) {
            (Phase::Rewinding { .. }, _) => "Rewinding to the start…".to_owned(),
            (Phase::Starting { .. }, _) => "Starting playback…".to_owned(),
            (_, Some((duration, bytes, path))) => format!(
                "{} · {} · {}{}",
                clock(duration),
                path.file_name().unwrap_or_default().to_string_lossy(),
                size(bytes),
                if blank { " · blank" } else { "" },
            ),
            _ => {
                let next = match transport {
                    Transport::NoTape if self.auto => "insert a tape to capture it",
                    Transport::NoTape => "insert a tape",
                    _ => "Record captures the whole tape",
                };
                match &self.last_result {
                    Some(result) => format!("{result} · {}", transport.label()),
                    None => format!("{} · {format} · {next}", transport.label()),
                }
            }
        };
        if status != self.sent_status {
            (self.sink)(DeviceEvent::Status(status.clone()));
            self.sent_status = status;
        }
        self.send_alt(self.phase == Phase::Idle && !matches!(transport, Transport::NoTape | Transport::Recording));
    }

    fn send_alt(&mut self, enabled: bool) {
        if self.sent_alt != Some(enabled) {
            (self.sink)(DeviceEvent::AltAction(RECORD_FROM_HERE.into(), enabled));
            self.sent_alt = Some(enabled);
        }
    }
}

/// Reads DV frames from `dvgrab` until it exits: writes them to the open tape file and
/// sends every [`PREVIEW_EVERY`]th one as a preview.
fn read_frames(mut out: ChildStdout, tap: &Mutex<Tap>, sink: &EventSink) {
    let mut buf = vec![0u8; dif::System::Pal.frame_bytes()];
    let mut count = 0u64;
    loop {
        if out.read_exact(&mut buf[..dif::BLOCK]).is_err() {
            return;
        }
        // dvgrab writes whole frames, so this only skips blocks after a corrupt one.
        let Some(system) = dif::frame_system(&buf[..dif::BLOCK]) else { continue };
        let data = &mut buf[..system.frame_bytes()];
        if out.read_exact(&mut data[dif::BLOCK..]).is_err() {
            return;
        }
        count += 1;
        let picture = count.is_multiple_of(PREVIEW_EVERY).then(|| dif::Picture::decode(data, system));

        let mut tap = tap.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        tap.last_frame = Some(now);
        tap.system = Some(system);
        if let Some(picture) = &picture {
            if picture.luma_spread < BLANK_SPREAD {
                tap.blank_since.get_or_insert(now);
            } else {
                tap.blank_since = None;
                tap.last_picture = Some(picture.image.clone());
            }
        }
        let blank = tap.blank_since.is_some();
        if let Some(rec) = &mut tap.recording {
            if let Some(file) = &mut rec.file {
                let written = file.write_all(data).and_then(|()| if rec.frames.is_multiple_of(FLUSH_EVERY) { file.flush() } else { Ok(()) });
                match written {
                    Ok(()) => {
                        rec.frames += 1;
                        rec.bytes += data.len() as u64;
                    }
                    Err(e) => {
                        rec.error = Some(e);
                        rec.file = None;
                    }
                }
            }
            if let Some(picture) = &picture {
                if rec.poster.is_none() && !blank && rec.duration(Some(system)) >= POSTER_AFTER {
                    rec.poster = Some(picture.image.clone());
                }
            }
        }
        drop(tap);
        if let Some(picture) = picture {
            let preview = imageops::resize(&picture.image, PREVIEW_SIZE.0, PREVIEW_SIZE.1, imageops::FilterType::Triangle);
            sink(DeviceEvent::Preview(preview));
        }
    }
}

/// `tape-NNNN.dv`, one past the highest number already in `dir`.
fn next_tape(dir: &Path) -> PathBuf {
    let next = fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| e.file_name().to_str()?.strip_prefix("tape-")?.strip_suffix(".dv")?.parse::<u32>().ok())
        .max()
        .map_or(1, |n| n + 1);
    dir.join(format!("tape-{next:04}.dv"))
}

/// Reports the last few tapes already in `dir` as recent captures, oldest first.
fn show_recent(dir: &Path, sink: &EventSink) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    let mut tapes: Vec<(SystemTime, PathBuf)> = entries
        .flatten()
        .filter(|e| e.file_name().to_str().is_some_and(|n| n.starts_with("tape-") && n.ends_with(".dv")))
        .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path())))
        .collect();
    tapes.sort();
    for (_, path) in tapes.into_iter().rev().take(RECENT).rev() {
        sink(DeviceEvent::Recent(path));
    }
}


fn clock(d: Duration) -> String {
    let s = d.as_secs();
    format!("{:02}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
}

fn size(bytes: u64) -> String {
    if bytes >= 1 << 30 {
        format!("{:.1} GB", bytes as f64 / (1u64 << 30) as f64)
    } else {
        format!("{} MB", bytes >> 20)
    }
}

/// A command whose process is killed when the spawning thread exits.
fn command(program: impl AsRef<std::ffi::OsStr>) -> Command {
    let mut cmd = Command::new(program);
    cmd.stdin(Stdio::null());
    // SAFETY: prctl is async-signal-safe; nothing else runs between fork and exec.
    unsafe {
        cmd.pre_exec(|| {
            libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
            Ok(())
        });
    }
    cmd
}

/// Kills `child` and reaps it if it goes within half a second. A process stuck in the
/// kernel (a wedged FireWire driver) is left behind rather than waited on.
fn kill(child: &mut Child) {
    let _ = child.kill();
    let _ = wait_timeout(child, Duration::from_millis(500));
}

fn wait_timeout(child: &mut Child, timeout: Duration) -> io::Result<Option<std::process::ExitStatus>> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(Some(status));
        }
        if Instant::now() >= deadline {
            return Ok(None);
        }
        thread::sleep(Duration::from_millis(10));
    }
}

fn on_path(program: &Path) -> bool {
    if program.components().count() > 1 {
        return program.is_file();
    }
    std::env::var_os("PATH").is_some_and(|path| std::env::split_paths(&path).any(|dir| dir.join(program).is_file()))
}

/// Just enough of the DV (IEC 61834 / SMPTE 314M, 25 Mbit/s) frame format to see the
/// picture: the DC coefficient of every 8×8 block, i.e. a 1/8-scale image (90×60 NTSC,
/// 90×72 PAL), with no inverse DCT. Macroblock placement follows FFmpeg's `dv.c`.
pub mod dif {
    use image::RgbImage;

    pub const BLOCK: usize = 80;
    const BLOCKS_PER_SEQUENCE: usize = 150;
    const GRID_WIDTH: usize = 90;

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum System {
        /// 525/60, 720×480, 4:1:1.
        Ntsc,
        /// 625/50, 720×576, 4:2:0.
        Pal,
    }

    impl System {
        fn sequences(self) -> usize {
            match self {
                Self::Ntsc => 10,
                Self::Pal => 12,
            }
        }

        pub fn frame_bytes(self) -> usize {
            self.sequences() * BLOCKS_PER_SEQUENCE * BLOCK
        }

        pub fn fps(self) -> f64 {
            match self {
                Self::Ntsc => 30000.0 / 1001.0,
                Self::Pal => 25.0,
            }
        }

        fn grid_height(self) -> usize {
            match self {
                Self::Ntsc => 60,
                Self::Pal => 72,
            }
        }

        pub fn label(self) -> &'static str {
            match self {
                Self::Ntsc => "NTSC DV",
                Self::Pal => "PAL DV",
            }
        }

        /// Top-left of macroblock `m` (0–4) of video segment `slot` in DIF sequence `seq`,
        /// in 8×8 blocks, and whether its four luma blocks are a 2×2 square rather than a
        /// 4×1 row.
        fn macroblock(self, seq: usize, slot: usize, m: usize) -> (usize, usize, bool) {
            const OFF: [usize; 5] = [2, 6, 8, 0, 4];
            match self {
                Self::Ntsc => {
                    const START: [usize; 5] = [9, 4, 13, 0, 18];
                    const SERPENT: [usize; 30] =
                        [0, 1, 2, 3, 4, 5, 5, 4, 3, 2, 1, 0, 0, 1, 2, 3, 4, 5, 5, 4, 3, 2, 1, 0, 0, 1, 2, 3, 4, 5];
                    let i = (seq + OFF[m]) % self.sequences();
                    let k = slot + if m == 1 || m == 2 { 3 } else { 0 };
                    let x = START[m] + k / 6;
                    // The rightmost column (x = 22, pixels 704–719) holds 16×16 macroblocks.
                    if x > 21 { (x * 4, SERPENT[k] * 2 + i * 6, true) } else { (x * 4, SERPENT[k] + i * 6, false) }
                }
                Self::Pal => {
                    const START: [usize; 5] = [18, 9, 27, 0, 36];
                    const SERPENT: [usize; 27] = [0, 1, 2, 2, 1, 0, 0, 1, 2, 2, 1, 0, 0, 1, 2, 2, 1, 0, 0, 1, 2, 2, 1, 0, 0, 1, 2];
                    let x = START[m] + slot / 3;
                    let y = SERPENT[slot] + (seq + OFF[m]) % self.sequences() * 3;
                    (x * 2, y * 2, true)
                }
            }
        }
    }

    /// The system of the frame whose first DIF block is `block`, or `None` if `block`
    /// isn't a frame's header block (section type 0, sequence 0, block 0).
    pub fn frame_system(block: &[u8]) -> Option<System> {
        if block.len() < 4 || block[0] >> 5 != 0 || block[1] >> 4 != 0 || block[2] != 0 {
            return None;
        }
        Some(if block[3] & 0x80 != 0 { System::Pal } else { System::Ntsc })
    }

    pub struct Picture {
        /// 90×60 (NTSC) or 90×72 (PAL); stretch to 4:3 for display.
        pub image: RgbImage,
        /// How far luma strays from the picture's median, at the 90th percentile of
        /// 8×8 blocks: about zero for a flat screen, however much text is drawn on a
        /// small part of it.
        pub luma_spread: f32,
    }

    /// 9-bit signed DC coefficient at the start of a DCT block, as a 0–255 sample.
    fn dc(block: &[u8]) -> f32 {
        let raw = (i32::from(block[0]) << 1) | i32::from(block[1] >> 7);
        let dc = if raw & 0x100 != 0 { raw - 0x200 } else { raw };
        128.0 + dc as f32 / 2.0
    }

    impl Picture {
        /// `frame` must be `system.frame_bytes()` long. Blocks that aren't video (a
        /// damaged frame) are left grey.
        pub fn decode(frame: &[u8], system: System) -> Self {
            let (w, h) = (GRID_WIDTH, system.grid_height());
            let (mut y, mut cb, mut cr) = (vec![128f32; w * h], vec![128f32; w * h], vec![128f32; w * h]);
            for seq in 0..system.sequences() {
                // Each sequence: header, 2 subcode and 3 VAUX blocks, then 9 × (1 audio + 15 video).
                let mut p = seq * BLOCKS_PER_SEQUENCE + 6;
                for slot in 0..27usize {
                    if slot.is_multiple_of(3) {
                        p += 1;
                    }
                    for m in 0..5 {
                        let block = &frame[(p + m) * BLOCK..(p + m + 1) * BLOCK];
                        if block[0] >> 5 != 4 {
                            continue;
                        }
                        let (bx, by, square) = system.macroblock(seq, slot, m);
                        let cells: [(usize, usize); 4] =
                            if square { [(0, 0), (1, 0), (0, 1), (1, 1)] } else { [(0, 0), (1, 0), (2, 0), (3, 0)] };
                        // Byte 3 is STA/QNO; then four 14-byte luma blocks, Cr and Cb (10 bytes each).
                        let (vr, vb) = (dc(&block[60..]), dc(&block[70..]));
                        for (j, (dx, dy)) in cells.into_iter().enumerate() {
                            let (cx, cy) = (bx + dx, by + dy);
                            if cx < w && cy < h {
                                y[cy * w + cx] = dc(&block[4 + j * 14..]);
                                cr[cy * w + cx] = vr;
                                cb[cy * w + cx] = vb;
                            }
                        }
                    }
                    p += 5;
                }
            }

            let mut sorted = y.clone();
            sorted.sort_by(f32::total_cmp);
            let median = sorted[sorted.len() / 2];
            let mut deviation: Vec<f32> = y.iter().map(|v| (v - median).abs()).collect();
            deviation.sort_by(f32::total_cmp);
            let luma_spread = deviation[deviation.len() * 9 / 10];
            // BT.601, studio range.
            let image = RgbImage::from_fn(w as u32, h as u32, |x, row| {
                let i = row as usize * w + x as usize;
                let (l, u, v) = (1.164 * (y[i] - 16.0), cb[i] - 128.0, cr[i] - 128.0);
                let px = |c: f32| c.round().clamp(0.0, 255.0) as u8;
                image::Rgb([px(l + 1.596 * v), px(l - 0.813 * v - 0.391 * u), px(l + 2.018 * u)])
            });
            Self { image, luma_spread }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// Mean absolute difference against FFmpeg's full decode of the same frame,
        /// area-scaled to the 8×8 block grid: (luma, RGB). Luma blocks are 8×8 in both, so
        /// luma must match closely; a misplaced macroblock costs tens of levels. RGB also
        /// carries chroma, which DV holds per macroblock (32×8 px at 4:1:1), so colour
        /// edges that don't fall on that grid bleed.
        fn compare(dv: &[u8], reference: &[u8]) -> (f64, f64) {
            let system = frame_system(dv).expect("header block");
            assert_eq!(dv.len(), system.frame_bytes());
            let picture = Picture::decode(dv, system);
            let reference = image::load_from_memory(reference).unwrap().to_rgb8();
            assert_eq!(picture.image.dimensions(), reference.dimensions());
            let luma = |p: &image::Rgb<u8>| 0.299 * f64::from(p[0]) + 0.587 * f64::from(p[1]) + 0.114 * f64::from(p[2]);
            let pairs = || picture.image.pixels().zip(reference.pixels());
            let n = f64::from(reference.width() * reference.height());
            let y = pairs().map(|(a, b)| (luma(a) - luma(b)).abs()).sum::<f64>() / n;
            let rgb = pairs().flat_map(|(a, b)| (0..3).map(move |c| f64::from(a[c].abs_diff(b[c])))).sum::<f64>() / (n * 3.0);
            (y, rgb)
        }

        #[test]
        fn ntsc_dc_matches_full_decode() {
            let (y, rgb) = compare(include_bytes!("testdata/ntsc.dv"), include_bytes!("testdata/ntsc-dc.png"));
            assert!(y < 2.5 && rgb < 12.0, "mean abs diff: luma {y}, rgb {rgb}");
        }

        #[test]
        fn pal_dc_matches_full_decode() {
            let (y, rgb) = compare(include_bytes!("testdata/pal.dv"), include_bytes!("testdata/pal-dc.png"));
            assert!(y < 2.0 && rgb < 6.0, "mean abs diff: luma {y}, rgb {rgb}");
        }

        #[test]
        fn test_pattern_is_not_blank() {
            let dv = include_bytes!("testdata/ntsc.dv");
            assert!(Picture::decode(dv, System::Ntsc).luma_spread > 20.0 * super::super::BLANK_SPREAD);
        }

        /// A real frame from a DCR-TRV240 with no tape: black, with its blinking battery
        /// warning drawn on it.
        #[test]
        fn camera_blank_screen_with_warning_is_blank() {
            let dv = include_bytes!("testdata/trv240-no-tape.dv");
            let picture = Picture::decode(dv, System::Ntsc);
            assert!(picture.luma_spread < super::super::BLANK_SPREAD, "spread {}", picture.luma_spread);
            let text = picture.image.pixels().filter(|p| p[0] > 100 && p[1] < 40).count();
            assert!(text > 20, "the warning text should be visible: {text} red blocks");
        }

        #[test]
        fn rejects_non_header_blocks() {
            let dv = include_bytes!("testdata/ntsc.dv");
            assert_eq!(frame_system(&dv[..BLOCK]), Some(System::Ntsc));
            assert_eq!(frame_system(&dv[BLOCK..2 * BLOCK]), None);
            assert_eq!(frame_system(include_bytes!("testdata/pal.dv")), Some(System::Pal));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_dvcont_states() {
        assert_eq!(Transport::parse("Loading Medium"), Some(Transport::NoTape));
        assert_eq!(Transport::parse("Winding stopped\n"), Some(Transport::Stopped));
        assert_eq!(Transport::parse("Winding backward at incredible speed"), Some(Transport::Winding));
        assert_eq!(Transport::parse("Playing Fast Forward"), Some(Transport::Winding));
        assert_eq!(Transport::parse("Playing Paused"), Some(Transport::Paused));
        assert_eq!(Transport::parse("Playing"), Some(Transport::Playing));
        assert_eq!(Transport::parse("Couldn't get a raw1394 handle"), None);
    }

    #[test]
    fn numbers_tapes_after_the_highest() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(next_tape(dir.path()), dir.path().join("tape-0001.dv"));
        for name in ["tape-0002.dv", "tape-0007.dv", "tape-0009.dv.bak", "notes.txt"] {
            File::create(dir.path().join(name)).unwrap();
        }
        assert_eq!(next_tape(dir.path()), dir.path().join("tape-0008.dv"));
    }

    #[test]
    fn slug_from_model_name() {
        let unit = Unit { guid: String::new(), name: "Sony DCR-TRV240".into() };
        assert_eq!(unit.slug(), "sony-dcr-trv240");
    }

    #[test]
    fn hung_command_is_killed() {
        let mut child = command("sleep").arg("30").spawn().unwrap();
        let started = Instant::now();
        assert!(wait_timeout(&mut child, Duration::from_millis(100)).unwrap().is_none());
        kill(&mut child);
        assert!(child.try_wait().unwrap().is_some());
        assert!(started.elapsed() < Duration::from_secs(2));
    }
}

/// The real controller against fake `dvgrab` / `dvcont` scripts and a fake sysfs: a
/// whole-tape capture, a camera that stops answering, and an unplug mid-recording. The host
/// side (`start_*`, `stop`, drop) must return at once throughout.
#[cfg(test)]
mod simulated {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::mpsc;

    const DVCONT: &str = r#"#!/bin/sh
d=$(dirname "$0")
[ -e "$d/hang" ] && exec sleep 30
case "$1" in
  status) cat "$d/state" ;;
  rewind) echo "Winding reverse" > "$d/state"
          (sleep 1; echo "Winding stopped" > "$d/state") >/dev/null 2>&1 & ;;
  play) echo "Playing" > "$d/state"
        if [ -e "$d/tape_secs" ]; then
          (sleep "$(cat "$d/tape_secs")"; echo "Winding stopped" > "$d/state") >/dev/null 2>&1 &
        fi ;;
  stop) echo "Winding stopped" > "$d/state" ;;
esac
"#;
    const DVGRAB: &str = r#"#!/bin/sh
d=$(dirname "$0")
while cat "$d/frame.dv"; do sleep 0.03; done
"#;

    struct Rig {
        dir: tempfile::TempDir,
        events: Arc<Mutex<Vec<String>>>,
    }

    impl Rig {
        fn new() -> (Self, Camcorder) {
            let dir = tempfile::tempdir().unwrap();
            let bin = dir.path();
            for (name, script) in [("dvcont", DVCONT), ("dvgrab", DVGRAB)] {
                fs::write(bin.join(name), script).unwrap();
                fs::set_permissions(bin.join(name), fs::Permissions::from_mode(0o755)).unwrap();
            }
            fs::write(bin.join("frame.dv"), include_bytes!("testdata/ntsc.dv")).unwrap();
            fs::write(bin.join("state"), "Winding stopped\n").unwrap();
            let rig = Self { dir, events: Arc::default() };
            rig.plug();
            let unit = Unit { guid: "0xtest".into(), name: "Fake DV".into() };
            let mut camera = Camcorder::new(unit, rig.path("tapes"));
            camera.tools = Tools { dvgrab: rig.path("dvgrab"), dvcont: rig.path("dvcont"), sysfs: rig.path("sysfs") };
            (rig, camera)
        }

        fn path(&self, name: &str) -> PathBuf {
            self.dir.path().join(name)
        }

        fn plug(&self) {
            fs::create_dir_all(self.path("sysfs/fw1")).unwrap();
            fs::write(self.path("sysfs/fw1/guid"), "0xtest\n").unwrap();
        }

        fn sink(&self) -> EventSink {
            let events = self.events.clone();
            Arc::new(move |event| {
                let line = match event {
                    DeviceEvent::Status(s) => format!("status {s}"),
                    DeviceEvent::Error(e) => format!("error {e}"),
                    DeviceEvent::Started => "started".into(),
                    DeviceEvent::Saved { .. } => "saved".into(),
                    DeviceEvent::Finished => "finished".into(),
                    _ => return,
                };
                events.lock().unwrap().push(line);
            })
        }

        /// Waits for an event starting with `prefix` and containing `text`, after event `from`.
        fn wait(&self, from: usize, prefix: &str, text: &str, timeout: Duration) -> (usize, String) {
            let deadline = Instant::now() + timeout;
            loop {
                {
                    let events = self.events.lock().unwrap();
                    let found = events.iter().enumerate().skip(from).find(|(_, e)| e.starts_with(prefix) && e.contains(text));
                    if let Some((i, e)) = found {
                        return (i + 1, e.clone());
                    }
                    assert!(Instant::now() < deadline, "no `{prefix}` with `{text}` after #{from}: {events:#?}");
                }
                thread::sleep(Duration::from_millis(20));
            }
        }

        fn len(&self) -> usize {
            self.events.lock().unwrap().len()
        }
    }

    /// Runs a host-side call and checks it didn't block.
    fn quick(what: &str, f: impl FnOnce()) {
        let started = Instant::now();
        f();
        assert!(started.elapsed() < Duration::from_millis(5), "{what} took {:?}", started.elapsed());
    }

    #[test]
    fn capture_hang_and_unplug() {
        let (rig, mut camera) = Rig::new();
        let sink = rig.sink();
        quick("start_preview", || camera.start_preview(sink.clone()));
        let (at, _) = rig.wait(0, "status", "Tape stopped · NTSC DV", Duration::from_secs(10));

        // Whole tape: rewind, play for 3 s, end of tape.
        fs::write(rig.path("tape_secs"), "3").unwrap();
        quick("start_acquire", || camera.start_acquire(Path::new("."), sink.clone()));
        let (at, _) = rig.wait(at, "status", "Rewinding", Duration::from_secs(5));
        let (at, _) = rig.wait(at, "status", "tape-0001.dv", Duration::from_secs(10));
        let (at, done) = rig.wait(at, "status", "Saved tape-0001.dv", Duration::from_secs(15));
        assert!(done.contains("end of tape"), "{done}");
        let len = fs::metadata(rig.path("tapes/tape-0001.dv")).unwrap().len();
        assert!(len > 0 && len % 120_000 == 0, "{len} bytes");
        assert!(rig.events.lock().unwrap().iter().any(|e| e == "saved"));
        fs::remove_file(rig.path("tape_secs")).unwrap();

        // The camera stops answering: dvcont hangs, gets killed, the panel shows an error.
        fs::write(rig.path("hang"), "").unwrap();
        quick("start_alt while hung", || camera.start_alt(Path::new("."), sink.clone()));
        quick("stop while hung", || camera.stop());
        let (at, _) = rig.wait(at, "error", "not answering", Duration::from_secs(10));
        fs::remove_file(rig.path("hang")).unwrap();
        let (at, _) = rig.wait(at, "finished", "", Duration::from_secs(10));

        // Unplugged while recording from here: the file is closed and kept.
        let (at, _) = rig.wait(at, "status", "Tape stopped", Duration::from_secs(10));
        quick("start_alt", || camera.start_alt(Path::new("."), sink.clone()));
        let (at, _) = rig.wait(at, "status", "tape-0002.dv", Duration::from_secs(10));
        thread::sleep(Duration::from_secs(1));
        fs::remove_dir_all(rig.path("sysfs/fw1")).unwrap();
        let (at, error) = rig.wait(at, "error", "disconnected", Duration::from_secs(10));
        assert!(error.contains("Saved tape-0002.dv"), "{error}");
        let len = fs::metadata(rig.path("tapes/tape-0002.dv")).unwrap().len();
        assert!(len > 0 && len % 120_000 == 0, "{len} bytes");

        // Plugged back in: recovers on its own.
        rig.plug();
        rig.wait(at, "finished", "", Duration::from_secs(10));

        // Dropping returns at once; the controller then exits and frees the bus.
        quick("drop", || drop(camera));
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let _bus = BUS.lock().unwrap_or_else(|e| e.into_inner());
            let _ = tx.send(());
        });
        rx.recv_timeout(Duration::from_secs(10)).expect("controller didn't exit");
        assert!(rig.len() > 0);
    }
}

/// Watches the real camera for a few seconds through the same controller the panel uses.
/// Needs a DV camcorder on FireWire in PLAY/EDIT mode (a tape is optional):
/// `cargo test --release -- --ignored --nocapture dv_hardware_stream`
#[cfg(test)]
mod hardware {
    use super::*;

    #[test]
    #[ignore]
    fn dv_hardware_stream() {
        let unit = find().expect("no AV/C camcorder on FireWire");
        eprintln!("found {} ({})", unit.name, unit.guid);
        let events: Arc<Mutex<Vec<String>>> = Arc::default();
        let log = events.clone();
        let sink: EventSink = Arc::new(move |event| {
            let line = match event {
                DeviceEvent::Preview(f) => format!("preview {}x{}", f.width(), f.height()),
                DeviceEvent::Status(s) => format!("status {s}"),
                DeviceEvent::Error(e) => format!("error {e}"),
                DeviceEvent::AltAction(label, enabled) => format!("alt {label} {enabled}"),
                DeviceEvent::Finished => "finished".into(),
                _ => return,
            };
            if !line.starts_with("preview") {
                eprintln!("{line}");
            }
            log.lock().unwrap().push(line);
        });
        let dir = tempfile::tempdir().unwrap();
        let mut camera = Camcorder::new(unit, dir.path().into());
        camera.start_preview(sink);
        thread::sleep(Duration::from_secs(6));
        let started = Instant::now();
        drop(camera);
        assert!(started.elapsed() < Duration::from_millis(10), "drop must not block");
        // The next controller waits for the bus, then the old children must be gone.
        let _bus = BUS.lock().unwrap_or_else(|e| e.into_inner());
        let events = events.lock().unwrap();
        let previews = events.iter().filter(|e| e.starts_with("preview")).count();
        eprintln!("{previews} previews");
        assert!(previews > 10, "no DV stream: {events:?}");
        assert!(!events.iter().any(|e| e.starts_with("error")), "{events:?}");
    }
}
