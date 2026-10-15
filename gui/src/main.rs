//! Archive Studio desktop GUI.
//!
//! Two modes, selected from the left sidebar:
//! - **Acquire** (`acquire.rs`): one live panel per connected capture device.
//! - **Review** (`review.rs`): a virtualized grid over every image / video / audio
//!   file under the working directory.
//!
//! While the window is open it hosts the library's processing pipeline (`pipeline.rs`).
//! Every item can be opened on the Details page (`details.rs`, via `nav.rs`) and has a
//! context menu (`actions.rs`).

mod acquire;
mod actions;
mod details;
mod nav;
mod people;
mod pipeline;
mod review;
mod thumbs;

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

    let ui = AppWindow::new()?;
    // Matches archive-studio.desktop, which is how GNOME (Wayland) finds the launcher
    // icon; the window's own `icon` covers X11 and other platforms. Needs the platform
    // that `AppWindow::new` sets up, and must come before the window is shown.
    slint::set_xdg_app_id("archive-studio")?;
    nav::install(&ui);
    let pipeline = pipeline::install(&ui, root.clone());
    let _acquire = acquire::install(&ui, &root, pipeline.clone());
    actions::install(&ui, root.clone(), pipeline.clone());
    details::install(&ui, root.clone(), pipeline);
    people::install(&ui, root.clone());
    review::install(&ui, root);
    ui.run()?;
    pipeline::shutdown();
    review::shutdown();
    Ok(())
}
