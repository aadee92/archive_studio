//! Navigation: which page is shown, opening items on the Details page, and the history that
//! Back walks.
//!
//! Every way of opening an item (double-click in Review, People or Acquire) builds a
//! [`Target`] and calls [`open`]; the sidebar calls [`go`]. Either first snapshots the view
//! being left ([`View`]: the page plus whatever it needs to look the same again, e.g. the
//! Review filters, selection and scroll position) onto a history stack, and [`back`] pops
//! and restores it. Pages stay alive while hidden, so restoring is mostly re-asserting what
//! they still show, but a snapshot also survives what happened meanwhile (a window resize
//! changing the column count, the file being rotated and re-indexed under a new id, another
//! person picked in People and back again).

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

use slint::ComponentHandle;

use crate::{actions, details, people, review, AppWindow, NavBridge, UiLog};
use archive_core::catalog::faces::FaceId;

/// Sidebar indices of the pages (`AppWindow.page`); 0 is Acquire.
pub const REVIEW: i32 = 1;
pub const PEOPLE: i32 = 2;
pub const DETAILS: i32 = 3;

/// Views kept for Back.
const HISTORY: usize = 64;

/// An item to show on the Details page, or to act on from the context menu.
#[derive(Clone)]
pub struct Target {
    pub path: PathBuf,
    /// A face in it to point out on arrival (opened from that face in People).
    pub face: Option<FaceId>,
    /// The items of the view it was opened from.
    pub scope: Scope,
}

impl Target {
    pub fn new(path: PathBuf, scope: Scope) -> Self {
        Self { path, face: None, scope }
    }
}

impl std::fmt::Debug for Target {
    /// For the log: `"/lib/a.jpg" face 12 from "Review · Images" (40 items, #3)`.
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "{:?}", self.path)?;
        if let Some(face) = self.face {
            write!(f, " face {}", face.0)?;
        }
        write!(f, " from {:?} ({} items", self.scope.label, self.scope.len)?;
        match self.scope.current {
            Some(i) => write!(f, ", #{i})"),
            None => write!(f, ")"),
        }
    }
}

/// The items a view showed when something was opened from it, as filtered and ordered there:
/// what the "similar in your filter" strip on the Details page draws from. Listing them is
/// deferred ([`paths`](Self::paths)) since a Review filter can match hundreds of thousands
/// of files; the view's state is captured when the scope is made, so it doesn't change
/// with the view afterwards. Catalog asset ids are `Catalog::find_asset` of the paths.
#[derive(Clone)]
pub struct Scope {
    /// Where it comes from, e.g. `Review · “beach” · Images`.
    pub label: String,
    pub len: usize,
    /// The opened item's position among them, if it is one of them.
    pub current: Option<usize>,
    items: Rc<dyn Fn() -> Vec<PathBuf>>,
}

impl Scope {
    pub fn new(label: String, len: usize, current: Option<usize>, items: impl Fn() -> Vec<PathBuf> + 'static) -> Self {
        Self { label, len, current, items: Rc::new(items) }
    }

    /// A fixed list.
    pub fn of(label: String, paths: Vec<PathBuf>, current: Option<usize>) -> Self {
        let len = paths.len();
        Self::new(label, len, current, move || paths.clone())
    }

    /// The items, in the view's order.
    pub fn paths(&self) -> Vec<PathBuf> {
        (self.items)()
    }
}

/// A view as it was left: enough to show it again just so.
#[derive(Debug)]
enum View {
    /// Nothing more to it than the page (Acquire; Details with nothing open).
    Page(i32),
    Review(review::Snapshot),
    People(people::Snapshot),
    Details(Target),
}

thread_local! {
    static HISTORY_STACK: RefCell<Vec<View>> = const { RefCell::new(Vec::new()) };
}

pub fn install(ui: &AppWindow) {
    let ui_log = ui.global::<UiLog>();
    ui_log.on_debug(|source, message| log::debug!(target: "archive_studio::ui", "{source}: {message}"));
    ui_log.on_trace(|source, message| log::trace!(target: "archive_studio::ui", "{source}: {message}"));
    let nav = ui.global::<NavBridge>();
    let weak = ui.as_weak();
    nav.on_navigate(move |page| {
        if let Some(ui) = weak.upgrade() {
            go(&ui, page);
        }
    });
    let weak = ui.as_weak();
    nav.on_back(move || {
        if let Some(ui) = weak.upgrade() {
            back(&ui);
        }
    });
}

/// Shows `target` on the Details page; Back returns to the view it was opened from.
pub fn open(ui: &AppWindow, target: Target) {
    log::debug!("open {target:?} (from page {})", ui.get_page());
    leave(ui);
    details::show(ui, target);
    ui.set_page(DETAILS);
}

/// The sidebar: shows `page` as it is now (People re-reads the catalog).
pub fn go(ui: &AppWindow, page: i32) {
    if page == ui.get_page() {
        log::debug!("go {page}: already shown");
        return;
    }
    log::debug!("go {page} (from page {})", ui.get_page());
    leave(ui);
    ui.set_page(page);
    if page == PEOPLE {
        people::entered();
    }
}

/// Back to the view left last, as it was.
pub fn back(ui: &AppWindow) {
    let Some(view) = HISTORY_STACK.with_borrow_mut(Vec::pop) else {
        log::debug!("back: history empty, nothing to do (page {})", ui.get_page());
        return;
    };
    log::debug!("back from page {} to {view:?} (history now {})", ui.get_page(), depth());
    publish(ui);
    match view {
        View::Page(page) => ui.set_page(page),
        View::Review(snapshot) => {
            ui.set_page(REVIEW);
            review::restore(snapshot);
        }
        View::People(snapshot) => {
            ui.set_page(PEOPLE);
            people::restore(snapshot);
        }
        View::Details(target) => {
            details::show(ui, target);
            ui.set_page(DETAILS);
        }
    }
}

/// The item context menu (rotate, delete, add tag, properties) for `target`, at the
/// pointer: every page forwards right-clicks on items here with the item resolved (see
/// `actions`).
pub fn context_menu(ui: &AppWindow, target: Target) {
    log::debug!("context menu for {target:?} (page {})", ui.get_page());
    actions::show_menu(ui, target);
}

/// Whether there is a view to go back to.
pub fn can_go_back(_ui: &AppWindow) -> bool {
    HISTORY_STACK.with_borrow(|history| !history.is_empty())
}

/// Pushes the view on screen onto the history.
fn leave(ui: &AppWindow) {
    let page = ui.get_page();
    let view = match page {
        REVIEW => review::snapshot().map(View::Review),
        PEOPLE => people::snapshot().map(View::People),
        DETAILS => details::snapshot().map(View::Details),
        _ => None,
    }
    .unwrap_or(View::Page(page));
    log::debug!("leave: push {view:?} (history now {})", depth() + 1);
    HISTORY_STACK.with_borrow_mut(|history| {
        if history.len() == HISTORY {
            log::debug!("history full: oldest view dropped");
            history.remove(0);
        }
        history.push(view);
    });
    publish(ui);
}

fn depth() -> usize {
    HISTORY_STACK.with_borrow(Vec::len)
}

fn publish(ui: &AppWindow) {
    let can = HISTORY_STACK.with_borrow(|history| !history.is_empty());
    ui.global::<NavBridge>().set_can_go_back(can);
}
