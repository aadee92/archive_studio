# Architecture

Archive Studio is a headless core with thin front ends.

## Layering rule

- `core/` (`archive-core`) holds all domain logic: device control, capture state machines,
  file naming, and (as they move over) the thumbnail store and media index. It has no UI
  dependency, directly or transitively: `cargo tree -p archive-core | grep -i slint` is empty.
- `gui/` (Slint) and a future `cli/` are front ends. They send commands to the core and
  render the events it reports; they own presentation only.
- Anything a script would also need belongs in core. If a front end grows logic another
  front end would have to duplicate, move it down.

## Crate map

| Path | Crate | Role |
|---|---|---|
| `core/` | `archive-core` (lib `archive_core`) | `device`: the capture device API. `devices`: backends (Epson V850, DV over FireWire, mocks) and `discover` / `hotplug` |
| `gui/` | `archive-studio-gui` (bin `archive-studio`) | Slint desktop app: acquire tiles hosting `archive_core` devices, review grid, thumbnail cache (`gui/src/thumbs`) |
| `utils/epson/v850` | `x980` | Standalone Epson GT-X980 / V850 USB driver (lib + CLI) |
| `utils/photo-duplex` | `photo-duplex` | Finds and cuts out photos on a flatbed scan, pairs fronts with backs (lib + CLI) |
| `utils/faces` | `faces` | Face detection (SCRFD), alignment, ArcFace embeddings and clustering into people (lib; `examples/faces.rs` driver) |
| `utils/orientation`, `utils/*.py`, `utils/*.sh` | | Standalone tools and scripts |

The root `Cargo.toml` is a workspace of `core` and `gui` sharing one `Cargo.lock` and
`target/`. `utils/` is excluded: each crate there builds on its own with its own lockfile and
is used by core as a path dependency.

## Device API contract (`archive_core::device`)

- **Discovery**: `devices::discover(out_dir)` probes every backend and returns
  `Vec<Box<dyn CaptureDevice>>`; `devices::hotplug(out_dir, &known)` returns devices that
  appeared since, cheap enough to poll every few seconds. Each device saves under its own
  subdirectory of `out_dir`.
- **Commands in**: `start_preview(sink)`, `start_acquire(out_dir, sink)`,
  `start_alt(out_dir, sink)`, `stop()`. They never block, on any host thread: they signal the
  backend and return. Dropping a device stops its threads and releases the hardware.
- **Events out**: everything comes back as `DeviceEvent` through the host's `EventSink`
  (`Arc<dyn Fn(DeviceEvent) + Send + Sync>`): `Preview`, `Status`, `Started`, `Captured`,
  `Saved { path, thumb, image }`, `Recent(path)`, `AltAction`, `Finished`, `Error`.
- **Threading**: the sink is invoked on the backend's own threads. It must be cheap or hand
  off; a slow sink stalls the device. The GUI's sink does its blocking work there (seeding the
  thumbnail cache from `Saved.image`, loading thumbnails for `Recent`), then posts to the UI
  event loop, coalescing previews to the latest frame.
- **Files**: backends write captures themselves and report each with `Saved` once the file is
  closed. They know nothing about thumbnails or caches; `Saved` carries the in-memory image so
  a host can make its own without decoding the file.
