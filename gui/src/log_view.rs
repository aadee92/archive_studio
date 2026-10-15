//! The live log behind the window-wide status bar (`ui/log.slint`): everything the logger
//! keeps ([`archive_logging::recent`], then [`archive_logging::subscribe`]), which includes
//! what libraries print on stderr once it is captured.
//!
//! Records are taken on the UI thread a few times a second, in one go, so a burst of
//! thousands costs one model update. While the panel is closed only the bar is kept current
//! (the newest record, warning and error counts); the rows are built when it opens.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;
use std::time::Duration;

use archive_logging::Entry;
use log::Level;
use slint::{ComponentHandle, ModelRc, SharedString, Timer, TimerMode, VecModel};

use crate::{AppWindow, LogBridge, LogLine};

/// How often new records are taken in.
const POLL: Duration = Duration::from_millis(150);
/// How long after opening the panel goes to the end once more (see `LogBridge.to-end`).
const SETTLE: Duration = Duration::from_millis(60);
/// Records held for the panel (as many as the logger keeps).
const KEEP: usize = archive_logging::RECENT;

struct View {
    entries: VecDeque<Entry>,
    /// Older records let go of to stay within [`KEEP`].
    dropped: usize,
    filter: Filter,
    /// The rows that pass `filter`; kept only while the panel is open.
    rows: Rc<VecModel<LogLine>>,
    open: bool,
    warnings: usize,
    errors: usize,
}

#[derive(Default)]
struct Filter {
    /// Lowercase; matched against the target and the message.
    text: String,
    min: Option<Level>,
}

impl Filter {
    fn new(text: &str, min_level: i32) -> Self {
        let min = match min_level {
            1 => Some(Level::Debug),
            2 => Some(Level::Info),
            3 => Some(Level::Warn),
            _ => None,
        };
        Self { text: text.trim().to_lowercase(), min }
    }

    fn passes(&self, entry: &Entry) -> bool {
        // `Level` orders by verbosity: Error < Warn < … < Trace.
        self.min.is_none_or(|min| entry.level <= min)
            && (self.text.is_empty()
                || entry.target.to_lowercase().contains(&self.text)
                || entry.message.to_lowercase().contains(&self.text))
    }
}

fn row(entry: &Entry) -> LogLine {
    let stamp = archive_logging::timestamp(entry.time);
    LogLine {
        // `2026-10-15 14:03:12.345` -> `14:03:12.345`
        time: stamp.get(11..).unwrap_or(&stamp).into(),
        level: entry.level.as_str().into(),
        target: entry.target.as_str().into(),
        text: entry.message.as_str().into(),
    }
}

/// The bar's one-line summary of `entry`.
fn summary(entry: &Entry) -> SharedString {
    let first = entry.message.lines().next().unwrap_or_default();
    format!("{}: {first}", entry.target).into()
}

impl View {
    fn take(&mut self, new: Vec<Entry>) {
        let open = self.open;
        let mut added = Vec::new();
        for entry in new {
            match entry.level {
                Level::Error if !open => self.errors += 1,
                Level::Warn if !open => self.warnings += 1,
                _ => {}
            }
            if open && self.filter.passes(&entry) {
                added.push(row(&entry));
            }
            self.entries.push_back(entry);
        }
        let over = self.entries.len().saturating_sub(KEEP);
        if over > 0 {
            self.entries.drain(..over);
            self.dropped += over;
        }
        if !open {
            return;
        }
        // Rows of let-go records go too: rebuilt rather than removed one by one from the front.
        if over > 0 {
            self.rebuild();
        } else if !added.is_empty() {
            self.rows.extend(added);
        }
    }

    fn rebuild(&self) {
        let rows: Vec<LogLine> = if self.open { self.entries.iter().filter(|e| self.filter.passes(e)).map(row).collect() } else { Vec::new() };
        self.rows.set_vec(rows);
    }

    fn publish(&self, ui: &AppWindow) {
        let bridge = ui.global::<LogBridge>();
        bridge.set_total(self.entries.len() as i32);
        bridge.set_dropped(self.dropped as i32);
        bridge.set_unseen_warnings(self.warnings as i32);
        bridge.set_unseen_errors(self.errors as i32);
        bridge.set_latest(self.entries.back().map(summary).unwrap_or_default());
    }
}

/// Keeps the log flowing into the window; dropping it stops that.
pub struct LogView {
    _poll: Timer,
}

pub fn install(ui: &AppWindow) -> LogView {
    // Subscribed first so nothing falls between the two; the overlap is skipped by time.
    let receiver = archive_logging::subscribe();
    let entries: VecDeque<Entry> = archive_logging::recent().into();
    let since = entries.back().map(|e| e.time);
    let rows = Rc::new(VecModel::default());
    let view = Rc::new(RefCell::new(View {
        entries,
        dropped: 0,
        filter: Filter::default(),
        rows: rows.clone(),
        open: false,
        warnings: 0,
        errors: 0,
    }));

    let bridge = ui.global::<LogBridge>();
    bridge.set_lines(ModelRc::from(rows));
    view.borrow().publish(ui);

    let (v, weak) = (view.clone(), ui.as_weak());
    bridge.on_opened(move |open| {
        let Some(ui) = weak.upgrade() else { return };
        let mut view = v.borrow_mut();
        view.open = open;
        if open {
            (view.warnings, view.errors) = (0, 0);
        }
        view.rebuild();
        view.publish(&ui);
        if open {
            let weak = weak.clone();
            Timer::single_shot(SETTLE, move || {
                if let Some(ui) = weak.upgrade() {
                    let bridge = ui.global::<LogBridge>();
                    bridge.set_to_end(bridge.get_to_end().wrapping_add(1));
                }
            });
        }
    });
    let (v, weak) = (view.clone(), ui.as_weak());
    bridge.on_filter_changed(move |text, min_level| {
        let Some(ui) = weak.upgrade() else { return };
        let mut view = v.borrow_mut();
        view.filter = Filter::new(&text, min_level);
        view.rebuild();
        view.publish(&ui);
    });
    let (v, weak) = (view.clone(), ui.as_weak());
    bridge.on_clear(move || {
        let Some(ui) = weak.upgrade() else { return };
        let mut view = v.borrow_mut();
        view.entries.clear();
        view.dropped = 0;
        (view.warnings, view.errors) = (0, 0);
        view.rebuild();
        view.publish(&ui);
    });

    let weak = ui.as_weak();
    let mut since = since;
    let poll = Timer::default();
    poll.start(TimerMode::Repeated, POLL, move || {
        let new: Vec<Entry> = receiver.try_iter().filter(|e| since.is_none_or(|t| e.time > t)).collect();
        if new.is_empty() {
            return;
        }
        // Only the overlap with `recent` needed skipping.
        since = None;
        let Some(ui) = weak.upgrade() else { return };
        let mut view = view.borrow_mut();
        view.take(new);
        view.publish(&ui);
    });
    LogView { _poll: poll }
}

#[cfg(test)]
mod tests {
    use super::*;
    use slint::Model;
    use std::time::SystemTime;

    fn entry(level: Level, target: &str, message: &str) -> Entry {
        Entry { time: SystemTime::now(), level, target: target.into(), thread: "main".into(), message: message.into() }
    }

    #[test]
    fn filter_by_level_and_text() {
        let debug = entry(Level::Debug, "archive_core::pipeline", "orient IMG_1.jpg");
        let warn = entry(Level::Warn, "stderr", "libpng warning: iCCP");
        let trace = entry(Level::Trace, "archive_studio::review", "viewport 0..12");

        let all = Filter::new("", 0);
        assert!(all.passes(&debug) && all.passes(&warn) && all.passes(&trace));
        let info_up = Filter::new("", 2);
        assert!(!info_up.passes(&debug) && info_up.passes(&warn) && !info_up.passes(&trace));
        let debug_up = Filter::new("", 1);
        assert!(debug_up.passes(&debug) && !debug_up.passes(&trace));

        assert!(Filter::new(" PIPELINE ", 0).passes(&debug), "target, case-insensitive");
        assert!(Filter::new("iccp", 0).passes(&warn), "message");
        assert!(!Filter::new("iccp", 0).passes(&debug));
    }

    #[test]
    fn closed_view_counts_and_keeps_bounded() {
        let mut view = View {
            entries: VecDeque::new(),
            dropped: 0,
            filter: Filter::default(),
            rows: Rc::new(VecModel::default()),
            open: false,
            warnings: 0,
            errors: 0,
        };
        let mut burst: Vec<Entry> = (0..KEEP + 10).map(|i| entry(Level::Debug, "t", &format!("{i}"))).collect();
        burst.push(entry(Level::Warn, "t", "w"));
        burst.push(entry(Level::Error, "t", "e"));
        view.take(burst);
        assert_eq!((view.entries.len(), view.dropped), (KEEP, 12));
        assert_eq!((view.warnings, view.errors), (1, 1));
        assert_eq!(view.rows.row_count(), 0, "no rows while closed");

        view.open = true;
        view.filter = Filter::new("", 3);
        view.rebuild();
        assert_eq!(view.rows.row_count(), 2);
        view.take(vec![entry(Level::Error, "t", "again"), entry(Level::Info, "t", "quiet")]);
        assert_eq!(view.rows.row_count(), 3, "appended, filtered");
        assert_eq!(view.errors, 1, "not counted while open");
        assert_eq!(view.rows.row_data(2).unwrap().text, "again");
    }
}
