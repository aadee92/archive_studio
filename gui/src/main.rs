//! Archive Studio desktop GUI.
//!
//! Two modes, selected from the left sidebar:
//! - **Acquire** (`acquire.rs`): one live panel per connected capture device.
//! - **Review** (`review.rs`): a virtualized grid over every image / video / audio
//!   file under the working directory.

mod acquire;
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

    let ui = AppWindow::new()?;
    let _acquire = acquire::install(&ui, &root);
    review::install(&ui, root);
    ui.run()?;
    Ok(())
}
