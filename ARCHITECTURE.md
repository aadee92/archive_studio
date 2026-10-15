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
| `core/` | `archive-core` (lib `archive_core`) | `device`: the capture device API. `devices`: backends (Epson V850, DV over FireWire, mocks) and `discover` / `hotplug`. `catalog`: the library database. `pipeline`: the processing engine. `ops`: per-asset actions (rotate, delete, tag, properties). `exif`: basic EXIF fields |
| `cli/` | `archive-studio-cli` (bin `archive`) | Interactive shell: hosts the pipeline engine, status, review, rotate, faces |
| `logging/` | `archive-logging` (lib `archive_logging`) | The logger the binaries install behind the `log` facade: stderr + rotating file in the library, panic hook, recent records / subscribe for in-app viewers, fd 2 capture |
| `gui/` | `archive-studio-gui` (bin `archive-studio`) | Slint desktop app: acquire tiles hosting `archive_core` devices, review grid, thumbnail cache (`gui/src/thumbs`) |
| `utils/epson/v850` | `x980` | Standalone Epson GT-X980 / V850 USB driver (lib + CLI) |
| `utils/photo-duplex` | `photo-duplex` | Finds and cuts out photos on a flatbed scan, pairs fronts with backs (lib + CLI) |
| `utils/faces` | `faces` | Face detection (SCRFD), alignment, ArcFace embeddings and clustering into people (lib; `examples/faces.rs` driver) |
| `utils/orientation` | `orientation` | 90°-step orientation model (lib; used by the pipeline's orient stage) |
| `utils/*.py`, `utils/*.sh` | | Standalone scripts |

The root `Cargo.toml` is a workspace of `core`, `gui`, `cli` and `logging` sharing one `Cargo.lock` and
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

## Pipeline (`archive_core::pipeline`)

What happens to a file once it is in the library: register → orient → faces and signature, then, once
the per-file stages are quiet, identify (suggestions and clusters) and XMP sidecar sync.

- **Hosted, not a daemon**: a front end starts an `Engine` when it opens a library and drops
  it on exit. The engine catches up with what changed on disk while nothing ran, then
  watches the library (inotify) for as long as it lives. One engine per library: it holds
  an `flock` on `.archive-studio/engine.lock`; a second front end runs without one.
- **State is in the catalog**: each asset records its file's size and mtime; each stage's
  run records the fingerprint and model version it ran on (`stage_runs`). A stage is due
  when either no longer matches, so the engine can stop at any time, and a model upgrade or
  a changed file redoes exactly what it affects. Failures aren't retried until the file
  changes or the user asks.
- **Orient rewrites the file**: a confident prediction (≥ 85%) turns the image itself,
  losslessly (JPEG in the DCT domain like `jpegtran -trim`, PNG; TIFF goes to review), via
  a hidden temporary renamed over the original, and turns the asset's face and tag regions
  with it. The EXIF orientation is baked into the pixels and reset to 1, since predictions
  and regions are relative to the displayed image. The engine records the new fingerprint
  itself, so its own write doesn't look like a change.
- **The user has the last word**: rotations the user chose or settled are never redone by
  the model; reviewed faces survive rescans.
- **Threads**: per-file stages run on a worker pool; one engine thread does all catalog
  writes, scheduling and watching. Events go to the host's sink on the engine thread.
- **Models**: `$ARCHIVE_STUDIO_MODELS`, else `~/.local/share/archive-studio/models`:
  `orientation.onnx` and `buffalo_l/` (`det_10g.onnx`, `w600k_r50.onnx`). A stage whose
  model is missing is skipped.

## Catalog concepts (`archive_core::catalog`)

- **People are tags.** A person is a `person`-kind tag `People|<Name>`. Naming a face
  (`name_faces`, `confirm_faces`) puts that tag on the asset as a user tag carrying the face
  box as its region (written to sidecars as an MWG face region). Un-naming, renaming
  (`rename_person`; onto an existing name merges), `merge_people` and `delete_person` keep
  tags in step; a tag stays while any face on the asset is still that person. Taking the tag
  off an asset un-names that person's faces there. Schema 4 backfilled tags for faces named
  before this; `sync_person_tags` repairs. `face_boxes(asset)` gives boxes (0..1, displayed
  image) with names for drawing.
- **Front/back pairs** (`pairs`): `asset_pairs(front, back)`. Filled by naming when an asset
  is first seen (`X-back.*` / `X_back` / `X back` with `X-front.*` or `X.*` in the same
  directory, as the V850 / photo-duplex writes `photo-NNNN-front.png` / `-back.png`), or by
  hand (`set_pair`). `back_of` / `front_of`.
- **Trash** (`trash`): deleting moves the file and sidecar to
  `.archive-studio/trash/<when>-<id>/<path>` and the asset row with it (kept, marked
  trashed and missing, out of searches). `restore` undoes it; `empty_trash` deletes for good.
  A front takes its back along.
- **Signatures** (`similar`): a 128-bit dHash plus a 64-bin colour histogram per image,
  computed by the pipeline's `signature` stage (no model; after orient). `similar(asset,
  candidates, limit)` returns only highly similar ones (≤ 16 bits apart, ≥ 50% colour overlap).

## Logging

Every crate logs through the `log` facade; `core` depends on nothing more (no logger).
The front ends install `archive_logging` at startup:

- **stderr**, filtered by `ARCHIVE_STUDIO_LOG` in `RUST_LOG` syntax (`debug`,
  `info,archive_studio=trace,archive_core=debug`); default `info` for the GUI, `warn` for the
  shell (which prints the engine's reports itself).
- **A file in the library**: `<library>/.archive-studio/logs/archive-studio.log` (GUI) or
  `archive.log` (shell), rotated on every start and at 16 MB, the last 5 kept
  (`….log.1` is the previous run). It gets everything stderr gets plus, whatever the
  variable says, `debug` for the front end's own modules, `archive_core::ops` and
  `archive_core::catalog` (user actions and catalog changes), and panics with backtraces.
  Lines: `2026-10-15 14:03:12.345 DEBUG [thread] module::path: message`, local time.
- Levels in core: info for catch-up / identify summaries and idle, warn for stage failures
  and engine warnings, debug for per-file stage results (`archive_core::pipeline`), catalog
  changes with their arguments and outcomes, and schema migrations (info).
