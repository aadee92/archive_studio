//! Screenshots without a display (development only): `archive-studio --shot OUT.png [options] DIR`
//! runs the whole app on Slint's software renderer in an offscreen window of a given size,
//! with the pipeline and every page as usual, and after a while writes what the window shows
//! to a PNG and exits. For looking at a layout from a terminal or an agent, where no window
//! can be opened (see gui/README.md, "Screenshots").
//!
//! Options: `--size 1600x1000` (logical pixels; default), `--scale 2` (device pixels per
//! logical pixel), `--page review|people|details|trash|failures|acquire` (default review),
//! `--item N` (Details: the Nth image of the library by path, default 0), `--wait SECONDS`
//! (how long the app runs before the shot, default 5), `--fullscreen`, `--info` (Details:
//! full screen; the info panel open), `--steps N` and `--step-ms MS` (Details: steps to the
//! next item N times, one every MS milliseconds (default 500) from a second in, as Right
//! does: for timing steps in the log). The user's settings are neither read nor written.

use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use slint::platform::software_renderer::{MinimalSoftwareWindow, RepaintBufferType};
use slint::platform::{Platform, PlatformError, WindowAdapter};
use slint::{ComponentHandle, PhysicalSize};

use crate::{nav, AppWindow, DetailsBridge};
use archive_core::media::{classify, MediaKind};

/// What `--shot` asked for.
#[derive(Debug, Clone)]
pub struct Options {
    pub out: PathBuf,
    pub width: u32,
    pub height: u32,
    pub scale: f32,
    pub page: String,
    pub item: usize,
    pub wait: Duration,
    pub fullscreen: bool,
    pub info: bool,
    pub steps: u32,
    pub step_every: Duration,
}

/// `--shot OUT.png [options] [DIR]` from the arguments after the program's name: the options
/// and DIR, if `--shot` comes first; else `None` (a normal run).
pub fn parse(args: &[String]) -> Result<Option<(Options, Option<PathBuf>)>, String> {
    if args.first().map(String::as_str) != Some("--shot") {
        return Ok(None);
    }
    let mut it = args[1..].iter();
    let out = it.next().ok_or("--shot needs an output file")?.into();
    let mut options = Options {
        out,
        width: 1600,
        height: 1000,
        scale: 1.0,
        page: "review".into(),
        item: 0,
        wait: Duration::from_secs(5),
        fullscreen: false,
        info: false,
        steps: 0,
        step_every: Duration::from_millis(500),
    };
    let mut dir = None;
    while let Some(arg) = it.next() {
        let mut value = || it.next().cloned().ok_or(format!("{arg} needs a value"));
        match arg.as_str() {
            "--size" => {
                let v = value()?;
                let (w, h) = v.split_once('x').ok_or(format!("--size {v}: not WxH"))?;
                options.width = w.parse().map_err(|_| format!("--size {v}"))?;
                options.height = h.parse().map_err(|_| format!("--size {v}"))?;
            }
            "--scale" => options.scale = value()?.parse().map_err(|_| "--scale: a number")?,
            "--page" => options.page = value()?,
            "--item" => options.item = value()?.parse().map_err(|_| "--item: a number")?,
            "--wait" => options.wait = Duration::from_secs_f32(value()?.parse().map_err(|_| "--wait: seconds")?),
            "--fullscreen" => options.fullscreen = true,
            "--info" => options.info = true,
            "--steps" => options.steps = value()?.parse().map_err(|_| "--steps: a number")?,
            "--step-ms" => options.step_every = Duration::from_millis(value()?.parse().map_err(|_| "--step-ms: milliseconds")?),
            _ if !arg.starts_with("--") && dir.is_none() => dir = Some(arg.into()),
            _ => return Err(format!("--shot: unknown argument {arg}")),
        }
    }
    Ok(Some((options, dir)))
}

/// Installs the offscreen platform; before the window is made.
pub fn install_platform(options: &Options) -> Result<(), String> {
    let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    let physical = PhysicalSize::new((options.width as f32 * options.scale) as u32, (options.height as f32 * options.scale) as u32);
    let platform = ShotPlatform {
        window,
        physical,
        scale: options.scale,
        start: Instant::now(),
        events: Arc::default(),
        options: options.clone(),
    };
    slint::platform::set_platform(Box::new(platform)).map_err(|e| format!("set_platform: {e}"))
}

/// Shows the page asked for, once everything is installed.
pub fn show_page(ui: &AppWindow, root: &Path, options: &Options) {
    // Sized already, as a real window is by the time anything is opened (Details decodes for it).
    ui.window().set_size(PhysicalSize::new((options.width as f32 * options.scale) as u32, (options.height as f32 * options.scale) as u32));
    let details = ui.global::<DetailsBridge>();
    details.set_info_open(options.info);
    let page = match options.page.as_str() {
        "acquire" => 0,
        "review" => nav::REVIEW,
        "people" => nav::PEOPLE,
        "details" => nav::DETAILS,
        "trash" => nav::TRASH,
        "failures" => nav::FAILURES,
        other => {
            log::warn!("shot: unknown page {other:?}; showing Review");
            nav::REVIEW
        }
    };
    if page == nav::DETAILS {
        let mut paths = Vec::new();
        images_under(root, &mut paths);
        paths.sort();
        log::info!("shot: {} images under {}", paths.len(), root.display());
        let item = options.item.min(paths.len().saturating_sub(1));
        if let Some(path) = paths.get(item).cloned() {
            let scope = nav::Scope::of("Review · Images".into(), paths, Some(item));
            nav::open(ui, nav::Target::new(path, scope));
        } else {
            nav::start_on(ui, page);
        }
    } else {
        nav::start_on(ui, page);
    }
    if options.fullscreen {
        details.set_fullscreen(true);
    }
    if page == nav::DETAILS && options.steps > 0 {
        let (weak, every, mut left) = (ui.as_weak(), options.step_every, options.steps);
        slint::Timer::single_shot(Duration::from_secs(1), move || {
            let timer = Box::leak(Box::new(slint::Timer::default()));
            timer.start(slint::TimerMode::Repeated, every, move || {
                if let Some(ui) = weak.upgrade().filter(|_| left > 0) {
                    left -= 1;
                    log::info!("shot: step");
                    ui.global::<DetailsBridge>().invoke_step(1);
                }
            });
        });
    }
}

/// The images under `dir`, but the library's own folder.
fn images_under(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if entry.file_name().to_string_lossy().starts_with('.') {
            continue;
        }
        if path.is_dir() {
            images_under(&path, out);
        } else if classify(&path) == Some(MediaKind::Image) {
            out.push(path);
        }
    }
}

type Event = Box<dyn FnOnce() + Send>;

#[derive(Default)]
struct Events {
    queue: Mutex<Vec<Event>>,
    wake: Condvar,
    quit: AtomicBool,
}

struct ShotPlatform {
    window: Rc<MinimalSoftwareWindow>,
    physical: PhysicalSize,
    scale: f32,
    start: Instant,
    events: Arc<Events>,
    options: Options,
}

impl Platform for ShotPlatform {
    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, PlatformError> {
        Ok(self.window.clone())
    }

    fn duration_since_start(&self) -> Duration {
        self.start.elapsed()
    }

    fn new_event_loop_proxy(&self) -> Option<Box<dyn slint::platform::EventLoopProxy>> {
        Some(Box::new(Proxy(self.events.clone())))
    }

    /// Runs timers and what other threads send until `wait` is over, drawing what changed
    /// every 100 ms as a window on screen would (lists only make the rows they draw), then
    /// renders the window once into `out`.
    fn run_event_loop(&self) -> Result<(), PlatformError> {
        let window = self.window.window();
        window.dispatch_event(slint::platform::WindowEvent::ScaleFactorChanged { scale_factor: self.scale });
        self.window.set_size(self.physical);
        let started = Instant::now();
        let (w, h) = (self.physical.width as usize, self.physical.height as usize);
        let mut frame = vec![slint::Rgb8Pixel::new(0, 0, 0); w * h];
        let mut drawn = Instant::now();
        loop {
            if drawn.elapsed() >= Duration::from_millis(100) {
                self.window.draw_if_needed(|renderer| {
                    renderer.render(&mut frame, w);
                });
                drawn = Instant::now();
            }
            slint::platform::update_timers_and_animations();
            let events = std::mem::take(&mut *self.events.queue.lock().unwrap());
            for event in events {
                event();
            }
            if self.events.quit.load(Ordering::Relaxed) {
                return Ok(());
            }
            if started.elapsed() >= self.options.wait {
                break;
            }
            let next = slint::platform::duration_until_next_timer_update().unwrap_or(Duration::from_millis(50));
            let queue = self.events.queue.lock().unwrap();
            if queue.is_empty() {
                let _ = self.events.wake.wait_timeout(queue, next.min(Duration::from_millis(50)));
            }
        }
        slint::platform::update_timers_and_animations();
        self.window.request_redraw();
        let mut pixels = vec![slint::Rgb8Pixel::new(0, 0, 0); w * h];
        self.window.draw_if_needed(|renderer| {
            renderer.render(&mut pixels, w);
        });
        let raw: Vec<u8> = pixels.iter().flat_map(|p| [p.r, p.g, p.b]).collect();
        let saved = image::RgbImage::from_raw(w as u32, h as u32, raw)
            .ok_or_else(|| PlatformError::Other("pixel buffer size".into()))?
            .save(&self.options.out);
        match saved {
            Ok(()) => log::info!("shot: wrote {} ({w}x{h})", self.options.out.display()),
            Err(e) => return Err(PlatformError::Other(format!("writing {}: {e}", self.options.out.display()))),
        }
        Ok(())
    }
}

struct Proxy(Arc<Events>);

impl slint::platform::EventLoopProxy for Proxy {
    fn quit_event_loop(&self) -> Result<(), slint::EventLoopError> {
        self.0.quit.store(true, Ordering::Relaxed);
        self.0.wake.notify_all();
        Ok(())
    }

    fn invoke_from_event_loop(&self, event: Box<dyn FnOnce() + Send>) -> Result<(), slint::EventLoopError> {
        self.0.queue.lock().unwrap().push(event);
        self.0.wake.notify_all();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_shot_arguments() {
        let args = |s: &str| s.split_whitespace().map(String::from).collect::<Vec<_>>();
        assert!(parse(&args("/lib")).unwrap().is_none());
        let (o, dir) = parse(&args("--shot out.png --size 800x600 --page details --item 3 --info /lib")).unwrap().unwrap();
        assert_eq!((o.out, o.width, o.height, o.page.as_str(), o.item, o.info), ("out.png".into(), 800, 600, "details", 3, true));
        assert_eq!(dir, Some("/lib".into()));
        assert!(parse(&args("--shot out.png --size big")).is_err());
        assert!(parse(&args("--shot")).is_err());
    }
}
