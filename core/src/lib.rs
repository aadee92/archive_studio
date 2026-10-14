//! Archive Studio's headless core: everything a front end (the Slint GUI, a CLI, a
//! script) drives, with no UI dependency.
//!
//! - [`device`]: the capture device API. A host sends commands through
//!   [`CaptureDevice`](device::CaptureDevice) and receives [`DeviceEvent`](device::DeviceEvent)s
//!   from the backend's own threads.
//! - [`devices`]: the backends and [`discover`](devices::discover).
//! - [`catalog`]: the library database of assets and their tags, and tag search.
//! - [`media`]: media kinds and file classification.
//! - [`pipeline`]: what happens to files in the library (orient, faces, identify, sidecars),
//!   run by an [`Engine`](pipeline::Engine) thread while a front end is open.
//!
//! See `ARCHITECTURE.md` at the repository root for the layering rule.

pub mod catalog;
pub mod device;
pub mod devices;
pub mod media;
pub mod pipeline;
mod resize;

/// The `image` version [`device::Frame`] comes from.
pub use image;
/// The face models [`catalog::faces::scan_image`] runs.
pub use faces;
