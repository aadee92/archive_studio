//! Archive Studio desktop GUI.
//!
//! Two modes, selected from the left sidebar:
//! - **Acquire** (`acquire.rs`): one live panel per connected capture device.
//! - **Review** (`review.rs`): a virtualized grid over every image / video / audio
//!   file under the working directory.
//!
//! While the window is open it hosts the library's processing pipeline (`pipeline.rs`).
//! Every item can be opened on the Details page (`details.rs`, via `nav.rs`) and has a
//! context menu (`actions.rs`). The status bar under every page opens the live log
//! (`log_view.rs`). At the bottom of the sidebar: the pipeline's failures (`failures.rs`,
//! while there are any) and the library's trash (`trash.rs`). ? or F1 lists the keyboard
//! shortcuts (`shortcuts.rs`); the window and its views are kept across runs (`settings.rs`).

mod acquire;
mod actions;
mod clipboard;
mod details;
mod failures;
mod log_view;
mod nav;
mod people;
mod pipeline;
mod review;
mod semantic;
mod settings;
mod shortcuts;
mod tags;
mod thumbs;
mod trash;
mod undo;

use std::path::PathBuf;

slint::include_modules!();

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // `archive-studio [DIR]` — defaults to the current directory.
    let root = match std::env::args_os().nth(1) {
        Some(dir) => PathBuf::from(dir),
        None => std::env::current_dir()?,
    };
    // Absolute like the catalog's root, so the paths the pipeline reports match the grid's.
    let root = std::path::absolute(root)?;
    start_logging(&root);

    let ui = AppWindow::new()?;
    // Matches archive-studio.desktop, which is how GNOME (Wayland) finds the launcher
    // icon; the window's own `icon` covers X11 and other platforms. Needs the platform
    // that `AppWindow::new` sets up, and must come before the window is shown.
    slint::set_xdg_app_id("archive-studio")?;
    let _log = log_view::install(&ui);
    nav::install(&ui);
    let pipeline = pipeline::install(&ui, root.clone());
    let _acquire = acquire::install(&ui, &root, pipeline.clone());
    actions::install(&ui, root.clone(), pipeline.clone());
    undo::install(&ui, root.clone(), pipeline.clone());
    details::install(&ui, root.clone(), pipeline.clone());
    people::install(&ui, root.clone());
    tags::install(&ui, root.clone());
    trash::install(&ui, root.clone(), pipeline.clone());
    failures::install(&ui, root.clone(), pipeline.clone());
    review::install(&ui, root.clone(), pipeline);
    shortcuts::install(&ui);
    // The window and its views as they were left (the last page too).
    settings::install(&ui, &root);
    // Diagnostics: start on another page (1 Review, 2 People), as if picked in the sidebar.
    if let Some(page) = std::env::var("ARCHIVE_STUDIO_PAGE").ok().and_then(|p| p.parse().ok()) {
        log::info!("ARCHIVE_STUDIO_PAGE: starting on page {page}");
        nav::start_on(&ui, page);
    }
    log::info!("window shown; entering the event loop");
    let result = ui.run();
    log::info!("event loop ended ({}); shutting down", if result.is_ok() { "window closed" } else { "error" });
    settings::shutdown();
    pipeline::shutdown();
    review::shutdown();
    log::info!("exit");
    result?;
    Ok(())
}

/// The logger (stderr, filtered by `ARCHIVE_STUDIO_LOG`, default `info`; and
/// `<root>/.archive-studio/logs/archive-studio.log`, which always gets `debug` from this app
/// and its libraries, as does the log panel),
/// and what the log should say about this run.
fn start_logging(root: &std::path::Path) {
    let started = archive_logging::init(archive_logging::Config {
        dir: Some(archive_logging::dir_for(root)),
        file_name: "archive-studio.log",
        stderr_default: "info",
        // Everything at debug for the log panel, but for two libraries that would crowd it
        // out: tract's graph optimizer (tens of thousands of lines while a model loads) and
        // nusb's per-transfer lines (a dozen a second while a scanner is plugged in).
        file_minimum: "debug,tract_core=info,tract_hir=info,tract_onnx=info,tract_linalg=info,nusb=info",
    });
    // C libraries (graphics drivers, ONNX Runtime) print to fd 2: keep that in the log too.
    let captured = archive_logging::capture_stderr();
    log::info!(
        "Archive Studio {} ({}), pid {}, library {}",
        env!("CARGO_PKG_VERSION"),
        archive_logging::GIT_HASH,
        std::process::id(),
        root.display()
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
    if let Err(e) = captured {
        log::warn!("stderr isn't captured: {e}");
    }
    let env = |name: &str| std::env::var(name).unwrap_or_else(|_| "-".into());
    log::info!(
        "slint {} backend {} (SLINT_BACKEND), style fluent-light, WAYLAND_DISPLAY {} DISPLAY {}",
        env!("SLINT_VERSION"),
        env("SLINT_BACKEND"),
        env("WAYLAND_DISPLAY"),
        env("DISPLAY")
    );
}
