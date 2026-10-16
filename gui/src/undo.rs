//! Undo and redo for the whole window: Ctrl+Z, Ctrl+Shift+Z / Ctrl+Y (`ItemMenu.step` in
//! `ui/app.slint`, for keys no element took: a text box keeps its own undo while it has
//! focus) and the notice's Undo / Redo button ([`actions::notify`]).
//!
//! Each step is [`ops::undo`] / [`ops::redo`] on a worker: the catalog's journal, shared
//! with the `archive` shell and kept across restarts, so the latest change is undone
//! whichever front end made it. Every file it touched is reported with
//! [`pipeline::changed`] (the grid, its tag tree and Details follow), the counts are redone
//! (Trash and Failures re-read with them) and People re-reads its faces. The notice says
//! what was undone, with Redo (and after a redo, Undo).

use std::cell::Cell;
use std::path::PathBuf;
use std::thread;

use slint::ComponentHandle;

use crate::actions;
use crate::pipeline::{self, Handle};
use crate::{AppWindow, ItemMenu};
use archive_core::ops;

thread_local! {
    static CONTEXT: std::cell::RefCell<Option<(PathBuf, Handle)>> = const { std::cell::RefCell::new(None) };
    /// A step is running: another key press waits for it rather than racing it.
    static BUSY: Cell<bool> = const { Cell::new(false) };
}

pub fn install(ui: &AppWindow, root: PathBuf, pipeline: Handle) {
    CONTEXT.set(Some((root, pipeline)));
    let weak = ui.as_weak();
    ui.global::<ItemMenu>().on_step(move |undo| {
        if let Some(ui) = weak.upgrade() {
            step(&ui, undo);
        }
    });
}

/// Undoes (`undo`) or redoes the last change, on a worker.
pub fn step(ui: &AppWindow, undo: bool) {
    let verb = if undo { "undo" } else { "redo" };
    if BUSY.get() {
        log::debug!("{verb}: the last step is still running - ignored");
        return;
    }
    let Some((root, pipeline)) = CONTEXT.with_borrow(Clone::clone) else { return };
    BUSY.set(true);
    actions::hide_notice(ui);
    let weak = ui.as_weak();
    thread::spawn(move || {
        let engine = pipeline.engine();
        let result = actions::open_catalog(&root).and_then(|mut catalog| {
            if undo {
                ops::undo(&mut catalog, engine.as_deref())
            } else {
                ops::redo(&mut catalog, engine.as_deref())
            }
        });
        match &result {
            Ok(Some(step)) => {
                log::info!("{verb}: {:?} ({} files)", step.change.description, step.paths.len());
                for path in &step.paths {
                    pipeline::changed(path, step.pixels);
                }
            }
            Ok(None) => log::info!("{verb}: nothing to {verb}"),
            Err(e) => log::warn!("{verb}: {e}"),
        }
        pipeline.recount();
        let _ = weak.upgrade_in_event_loop(move |ui| {
            BUSY.set(false);
            match result {
                Ok(Some(step)) => {
                    crate::people::faces_changed(&ui);
                    let done = if undo { "Undone" } else { "Redone" };
                    actions::notice(&ui, format!("{done}: {}", step.change.description), false, Some(!undo));
                }
                Ok(None) => actions::notify(&ui, format!("Nothing to {verb}"), false, false),
                Err(e) => actions::notify(&ui, capitalized(&e), true, false),
            }
        });
    });
}

fn capitalized(text: &str) -> String {
    let mut chars = text.chars();
    chars.next().map_or_else(String::new, |first| first.to_uppercase().chain(chars).collect())
}
