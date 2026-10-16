//! The window and how its views were set up, kept across runs in the per-user settings file
//! (`archive_core::settings`, `$XDG_CONFIG_HOME/archive-studio/settings.json`).
//!
//! Kept for the app: the window's size (its normal size; maximized on top of it, never full
//! screen), the page shown last (Details comes back as Review, having no item), Review's
//! tile size, sort, media kind and search mode, Details' info panel, face box and name
//! toggles and slideshow interval, People's sort. Kept per library: the tag tree's collapsed
//! nodes. Never what is transient: selections, search text, scroll positions, filters on
//! tags or what needs attention.
//!
//! Read once at startup ([`install`], after the pages are installed); every second the state
//! is taken from the window and, once it changed and then stayed the same for a second, saved
//! on a writer thread; on close it is saved at once.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use archive_core::settings::{self, Settings};
use slint::{ComponentHandle, Timer, TimerMode};

use crate::{nav, review, AppWindow, DetailsBridge, PeopleBridge, ReviewBridge};

/// How often the window's state is looked at.
const POLL: Duration = Duration::from_secs(1);

struct State {
    path: PathBuf,
    root: PathBuf,
    /// As last saved (or read).
    saved: Settings,
    /// Taken at the last poll, differing from `saved`: saved if the next poll finds the same.
    pending: Option<Settings>,
    /// The writer thread (none if it couldn't start: saved on close only, on this thread).
    writer: Option<(mpsc::Sender<Settings>, thread::JoinHandle<()>)>,
    _timer: Timer,
}

thread_local! {
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
}

/// Reads the settings and applies them to the window (before it is shown), then keeps them.
pub fn install(ui: &AppWindow, root: &Path) {
    let Some(path) = settings::default_path() else {
        log::warn!("settings: no $XDG_CONFIG_HOME or $HOME; nothing is kept across runs");
        return;
    };
    let saved = Settings::load(&path);
    apply(ui, root, &saved);

    let (writer, jobs) = mpsc::channel::<Settings>();
    let save_to = path.clone();
    let spawned = thread::Builder::new().name("settings".into()).spawn(move || {
        while let Ok(mut settings) = jobs.recv() {
            // Only the latest of a burst.
            while let Ok(newer) = jobs.try_recv() {
                settings = newer;
            }
            let _ = settings.save(&save_to);
        }
    });
    let writer = match spawned {
        Ok(handle) => Some((writer, handle)),
        Err(e) => {
            log::warn!("settings: no writer thread ({e}); saved on close only");
            None
        }
    };

    let timer = Timer::default();
    let weak = ui.as_weak();
    timer.start(TimerMode::Repeated, POLL, move || {
        if let Some(ui) = weak.upgrade() {
            poll(&ui);
        }
    });
    let weak = ui.as_weak();
    ui.window().on_close_requested(move || {
        if let Some(ui) = weak.upgrade() {
            save_now(&ui);
        }
        slint::CloseRequestResponse::HideWindow
    });
    STATE.set(Some(State { path, root: root.to_path_buf(), saved, pending: None, writer, _timer: timer }));
}

/// The settings as the window is now, on top of `base` (which keeps keys this doesn't know).
fn capture(ui: &AppWindow, root: &Path, base: &Settings) -> Settings {
    let mut s = base.clone();
    let window = ui.window();
    let full_screen = window.is_fullscreen() || ui.global::<DetailsBridge>().get_fullscreen();
    if !full_screen {
        let maximized = window.is_maximized();
        s.app.set("window.maximized", maximized);
        if !maximized {
            let size = window.size().to_logical(window.scale_factor());
            if size.width > 0.0 && size.height > 0.0 {
                s.app.set("window.width", size.width.round());
                s.app.set("window.height", size.height.round());
            }
        }
    }
    s.app.set("page", ui.get_page());

    let review = ui.global::<ReviewBridge>();
    s.app.set("review.tile-size", review.get_tile_value());
    s.app.set("review.kind", review.get_kind_filter());
    s.app.set("review.sort", review.get_sort_index());
    s.app.set("review.search-mode", review.get_search_mode());

    let details = ui.global::<DetailsBridge>();
    s.app.set("details.info", details.get_info_open());
    s.app.set("details.boxes", details.get_show_boxes());
    s.app.set("details.names", details.get_show_names());
    s.app.set("details.slideshow-seconds", details.get_slideshow_seconds());

    s.app.set("people.sort", ui.global::<PeopleBridge>().get_sort());

    let mut library = s.library(root);
    let collapsed = review::collapsed_tags();
    if collapsed.is_empty() {
        library.remove("tags.collapsed");
    } else {
        library.set("tags.collapsed", collapsed);
    }
    s.set_library(root, library);
    s
}

/// Puts `s` on the window; values out of range are left at their defaults.
fn apply(ui: &AppWindow, root: &Path, s: &Settings) {
    let app = &s.app;
    let window = ui.window();
    if let (Some(w), Some(h)) = (app.get::<f32>("window.width"), app.get::<f32>("window.height")) {
        if w >= 400.0 && h >= 300.0 {
            window.set_size(slint::LogicalSize::new(w, h));
        }
    }
    if app.get::<bool>("window.maximized") == Some(true) {
        window.set_maximized(true);
    }

    let review = ui.global::<ReviewBridge>();
    if let Some(size) = app.get::<f32>("review.tile-size").filter(|v| (96.0..=320.0).contains(v)) {
        review.set_tile_value(size);
    }
    if let Some(kind) = app.get::<i32>("review.kind").filter(|v| (0..=3).contains(v)) {
        review.set_kind_filter(kind);
    }
    let mode = app.get::<i32>("review.search-mode").filter(|v| (0..=1).contains(v)).unwrap_or(0);
    review.set_search_mode(mode);
    // Relevance (4) only ranks a search by description.
    if let Some(sort) = app.get::<i32>("review.sort").filter(|&v| (0..=3).contains(&v) || (v == 4 && mode == 1)) {
        review.set_sort_index(sort);
    }
    review::restore_settings(ui, s.library(root).get::<Vec<String>>("tags.collapsed").unwrap_or_default());

    let details = ui.global::<DetailsBridge>();
    if let Some(open) = app.get::<bool>("details.info") {
        details.set_info_open(open);
    }
    if let Some(on) = app.get::<bool>("details.boxes") {
        details.set_show_boxes(on);
    }
    if let Some(on) = app.get::<bool>("details.names") {
        details.set_show_names(on);
    }
    if let Some(seconds) = app.get::<i32>("details.slideshow-seconds").filter(|v| [3, 5, 10].contains(v)) {
        details.set_slideshow_seconds(seconds);
    }

    if let Some(sort) = app.get::<i32>("people.sort").filter(|v| (0..=2).contains(v)) {
        ui.global::<PeopleBridge>().set_sort(sort);
    }

    // Last: showing a page reads what it shows with the settings above.
    match app.get::<i32>("page").filter(|v| (0..=nav::FAILURES).contains(v)) {
        Some(nav::DETAILS) => nav::start_on(ui, nav::REVIEW),
        Some(page) if page != 0 => nav::start_on(ui, page),
        _ => {}
    }
}

/// Saves the window's state once it changed and then stayed so for a poll.
fn poll(ui: &AppWindow) {
    STATE.with_borrow_mut(|state| {
        let Some(state) = state.as_mut() else { return };
        let now = capture(ui, &state.root, &state.saved);
        if now == state.saved {
            state.pending = None;
        } else if state.pending.as_ref() == Some(&now) {
            log::trace!("settings: changed; saving");
            state.pending = None;
            state.saved = now.clone();
            if let Some((writer, _)) = &state.writer {
                let _ = writer.send(now);
            }
        } else {
            state.pending = Some(now);
        }
    });
}

/// Saves the window's state if it changed (the window is closing): after whatever the writer
/// thread has yet to save, which [`shutdown`] waits for.
fn save_now(ui: &AppWindow) {
    STATE.with_borrow_mut(|state| {
        let Some(state) = state.as_mut() else { return };
        let now = capture(ui, &state.root, &state.saved);
        if now == state.saved {
            return;
        }
        log::debug!("settings: saving on close");
        match &state.writer {
            Some((writer, _)) => {
                let _ = writer.send(now.clone());
            }
            None => {
                let _ = now.save(&state.path);
            }
        }
        state.saved = now;
    });
}

/// Waits for the writer thread to save what it was given (after the event loop ended).
pub fn shutdown() {
    let Some(state) = STATE.take() else { return };
    if let Some((writer, handle)) = state.writer {
        drop(writer);
        let _ = handle.join();
    }
}
