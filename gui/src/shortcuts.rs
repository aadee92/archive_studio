//! The keyboard shortcut list that ? or F1 shows (`ui/help.slint`): the shortcuts of the
//! page on screen, then those that work everywhere.
//!
//! This is the one place shortcuts are written down. The keys themselves are handled in the
//! pages' `key-pressed` handlers (`ui/*.slint`, Review's grid in `review.rs`); a shortcut
//! added or changed there must be added or changed here too.

use slint::{ComponentHandle, ModelRc, VecModel};

use crate::{nav, AppWindow, HelpBridge, HelpRow, HelpSection};

/// A titled group of shortcuts.
struct Section {
    title: &'static str,
    /// When they apply, if not always.
    note: &'static str,
    /// Keys (or a mouse gesture) and what they do.
    rows: &'static [(&'static str, &'static str)],
}

const EVERYWHERE: &[Section] = &[
    Section {
        title: "Everywhere",
        note: "",
        rows: &[
            ("Ctrl+Z", "Undo the last change"),
            ("Ctrl+Shift+Z, Ctrl+Y", "Redo"),
            ("? or F1", "This list (not while typing in a text box: F1 only)"),
            ("Right-click an item", "Its menu"),
        ],
    },
    Section {
        title: "Dialogs",
        note: "",
        rows: &[
            ("Esc", "Cancel or close"),
            ("Enter", "Confirm deleting a tag, or deleting from the trash for good"),
            ("Up / Down, Enter", "Pick a matching name or tag under a text box"),
        ],
    },
];

const REVIEW: &[Section] = &[
    Section {
        title: "Review",
        note: "On the grid: once it was clicked or the page shown, not while typing in a search box.",
        rows: &[
            ("Arrows", "Move to the next item and select it"),
            ("Page Up / Page Down", "A screen up / down"),
            ("Home / End", "First / last item"),
            ("Shift+arrows, …", "Select from the anchor to there"),
            ("Click, Shift-click, Ctrl-click", "Select one, a range, add or remove one"),
            ("Drag", "Select with a rubber band (Shift / Ctrl keep the selection)"),
            ("Ctrl+A", "Select everything shown"),
            ("Esc", "Clear the selection"),
            ("Enter, double-click", "Open on Details"),
            ("Delete", "Move the selection to the trash"),
            ("T", "Add a tag to the selection"),
            ("R / Shift+R", "Rotate the selection right / left"),
            ("Ctrl+C", "Copy the selected files (paste them in a file manager)"),
            ("Ctrl+wheel", "Tile size"),
        ],
    },
];

const PEOPLE: &[Section] = &[
    Section {
        title: "People",
        note: "On the faces.",
        rows: &[
            ("Click, Shift-click, Ctrl-click", "Select one, a range, add or remove one"),
            ("Drag", "Select with a rubber band"),
            ("Ctrl+A", "Select every face shown"),
            ("Esc", "Clear the selection and the name"),
            ("Enter", "Name the selection as the name box says (confirms suggestions)"),
            ("Backspace / Delete", "The view's action: Not Ann, Un-ignore, Suggest again"),
            ("A letter", "Start typing a name"),
            ("Double-click", "Open the picture on Details"),
        ],
    },
    Section {
        title: "Name box and Find a person",
        note: "",
        rows: &[
            ("Up / Down", "Pick a matching name"),
            ("Enter", "Name the selected faces"),
            ("Esc", "Clear it (the name box: and the selection)"),
        ],
    },
];

const DETAILS: &[Section] = &[
    Section {
        title: "Details",
        note: "",
        rows: &[
            ("Left / Right", "Previous / next item of the view (zoomed in: pan)"),
            ("Home / End", "First / last item of the view"),
            ("Esc, Alt+Left", "Back (first leaves full screen, closes a face's popup or stops adding a face)"),
            ("F11", "Full screen"),
            ("Space", "Start or stop the slideshow (any key stops it)"),
            ("I", "Info panel"),
            ("R / Shift+R or L", "Rotate right / left"),
            ("Enter", "Accept the proposed rotation"),
            ("Delete", "Move to the trash"),
            ("T", "Add a tag"),
            ("F", "Flip to the other side"),
            ("B / N", "Show or hide face boxes / names"),
            ("Ctrl+C", "Copy the file (paste it in a file manager)"),
        ],
    },
    Section {
        title: "Zoom",
        note: "",
        rows: &[
            ("+ / -", "Zoom in / out"),
            ("0", "Fit"),
            ("1", "100% (one file pixel per screen pixel)"),
            ("Wheel", "Zoom around the pointer"),
            ("Double-click", "Fit / 100% at that point"),
            ("Drag, arrows", "Pan while zoomed in"),
        ],
    },
    Section {
        title: "Face boxes",
        note: "",
        rows: &[
            ("Click a box", "Name or rename it"),
            ("Up / Down, Enter", "Pick a matching name and assign it"),
            ("Esc", "Close the popup, or stop adding a face"),
        ],
    },
];

const TRASH: &[Section] = &[
    Section {
        title: "Trash",
        note: "",
        rows: &[
            ("Click, Shift-click, Ctrl-click", "Select one, a range, add or remove one"),
            ("Ctrl+A", "Select everything"),
            ("Esc", "Clear the selection"),
            ("Enter", "Restore the selection"),
            ("Delete", "Delete the selection permanently (asks first)"),
        ],
    },
];

const FAILURES: &[Section] = &[
    Section {
        title: "Failures",
        note: "",
        rows: &[
            ("Click, Shift-click, Ctrl-click", "Select one, a range, add or remove one"),
            ("Ctrl+A", "Select everything"),
            ("Esc", "Clear the selection"),
            ("Enter, double-click", "Open the item on Details"),
        ],
    },
];

/// The sections for `page` (`AppWindow.page`), the page's own first.
fn sections(page: i32) -> impl Iterator<Item = &'static Section> {
    let own: &[Section] = match page {
        nav::REVIEW => REVIEW,
        nav::PEOPLE => PEOPLE,
        nav::DETAILS => DETAILS,
        nav::TRASH => TRASH,
        nav::FAILURES => FAILURES,
        _ => &[],
    };
    own.iter().chain(EVERYWHERE)
}

pub fn install(ui: &AppWindow) {
    let weak = ui.as_weak();
    ui.global::<HelpBridge>().on_show(move |page| {
        let Some(ui) = weak.upgrade() else { return };
        log::debug!("shortcut list for page {page}");
        let sections: Vec<HelpSection> = sections(page)
            .map(|s| HelpSection {
                title: s.title.into(),
                note: s.note.into(),
                rows: ModelRc::new(VecModel::from(
                    s.rows.iter().map(|&(keys, does)| HelpRow { keys: keys.into(), does: does.into() }).collect::<Vec<_>>(),
                )),
            })
            .collect();
        let bridge = ui.global::<HelpBridge>();
        bridge.set_sections(ModelRc::new(VecModel::from(sections)));
        bridge.set_open(true);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_page_lists_the_global_shortcuts_last() {
        for page in 0..=nav::FAILURES {
            let titles: Vec<&str> = sections(page).map(|s| s.title).collect();
            assert_eq!(titles[titles.len() - 2..], ["Everywhere", "Dialogs"], "page {page}");
        }
        assert_eq!(sections(nav::DETAILS).next().unwrap().title, "Details");
    }

    #[test]
    fn no_key_is_listed_twice_in_a_section() {
        for page in 0..=nav::FAILURES {
            for s in sections(page) {
                let mut keys: Vec<&str> = s.rows.iter().map(|r| r.0).collect();
                keys.sort_unstable();
                let count = keys.len();
                keys.dedup();
                assert_eq!(keys.len(), count, "{}", s.title);
                assert!(s.rows.iter().all(|r| !r.0.is_empty() && !r.1.is_empty()), "{}", s.title);
            }
        }
    }
}
