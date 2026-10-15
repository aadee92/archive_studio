# Archive Studio GUI

Slint + Rust desktop front end. Light theme, left-sidebar navigation.

```sh
cargo run --release -- [DIR]             # DIR defaults to the current directory
ARCHIVE_STUDIO_MOCK=ff680,ff680,ff680,ff680 cargo run   # 4 simulated FF-680s → 2×2 grid
ARCHIVE_STUDIO_MOCK=ff680,vhs,reel,hi8 cargo run        # mixed scanners + video
```

## Logs

Each run writes `<library>/.archive-studio/logs/archive-studio.log` (the previous four runs
are `archive-studio.log.1` … `.4`; a file is also rotated at 16 MB). It always has this app's
`debug` lines, whatever stderr shows, so after something odd happened, look there first
(`.1` if the app was restarted since). The first lines give the version and commit, the
library, the log file and the Slint version / display.

```sh
ARCHIVE_STUDIO_LOG=debug cargo run --release -- DIR       # stderr too (default: info)
ARCHIVE_STUDIO_LOG=info,archive_studio=trace cargo run -- DIR   # + scroll positions, keys typed
ARCHIVE_STUDIO_LOG=info,archive_core::pipeline=debug cargo run -- DIR  # per-file stage results
ARCHIVE_STUDIO_PAGE=2 cargo run -- DIR                     # start on People (1 Review)
tail -f DIR/.archive-studio/logs/archive-studio.log
```

What is in it (targets `archive_studio::<module>`):

- `nav`: every open / go / back with the page, the history depth and the snapshot pushed or
  restored (Review: filters, selection, scroll anchor; People: person, faces selected, scroll
  anchor; Details: the item and its scope).
- `people`: page shown, each load (which person, why, generation), its result (people, faces,
  time) or why it was dropped, model resets, Back restores (requested vs. applied), clicks on
  the list and the grid (row → person / face, or why it was ignored), naming / ignoring
  (change number, face ids) and the writer thread's results, suggestion refreshes.
- `ui`: what Slint does without telling Rust otherwise (`UiLog` in `ui/nav.slint`): the page
  shown, the People lists' row counts and scroll state (content-y vs. content and visible
  height, and when a list is scrolled past its end), the loading flag, presses and clicks on
  people, keys (Esc, Enter, Tab, shortcuts; which handler took Esc, and keys nobody handled),
  the Details page's keyboard focus, the context menu shown, the tag / properties dialogs and
  the notice appearing and going.
- `review`, `details`, `actions`, `acquire`, `pipeline`, `thumbs`: filter changes and
  refilters, double-click opens, context-menu actions and their results, device events,
  worker threads starting and stopping, failures.
- `archive_core::…`: catalog changes (naming, renaming, merging, tagging, trash / restore)
  with their arguments and outcomes, pipeline summaries and failures.
- `stderr`: whatever libraries print to stderr themselves (fd 2 is captured, and still shown).
- `panic`: panics, with the thread and a backtrace.

Thumbnail decodes, preview frames and per-file pipeline updates are trace or not logged, so
the file stays readable.

Launcher icon (GNOME and other freedesktop desktops; the window matches it via the
`archive-studio` app id):

```sh
install -Dm644 data/archive-studio.desktop ~/.local/share/applications/archive-studio.desktop
install -Dm644 ui/icons/app.svg ~/.local/share/icons/hicolor/scalable/apps/archive-studio.svg
```

`Exec=archive-studio` expects the binary on `PATH`; edit it to an absolute path otherwise.

## Layout

| Path | Role |
|---|---|
| `ui/app.slint` | Window: sidebar + page host (pages stay alive when hidden) |
| `ui/acquire.slint` | One `DevicePanel` per device, tiled in a near-square grid (1→1×1, 2→2×1, 4→2×2, 6→3×2…) |
| `ui/review.slint` | Search, kind chips, sort, tag list (`People|Ann` shown as "People / Ann"; picking one filters by it), virtualized thumbnail grid, size slider in the bottom bar |
| `ui/people.slint` | People list (Unknown first, then person tags with faces) and the selected person's face crops: click / Shift / Ctrl / drag to select, name box (with matching names) and Ignore above the list. A person's suggested faces come first, highlighted |
| `ui/details.slint` | One item enlarged; opened by double-clicking a tile in Review, a face in People or a recent capture in Acquire. Back arrow / Esc returns to that view as it was. Face boxes over the picture (placed in displayed-image coordinates) with names under confirmed ones, toggled independently in the bottom bar; a face opened from People pulses 3× over 1.5 s. Rotate left / right, and Accept / Keep for a rotation the pipeline proposes. A two-sided photo (catalog pair, else `-front` / `-back` names) has a turned-down corner that flips to the other side, and a "Front & back" toggle that shows both, front on the left, each with its faces. Under the picture, "Similar in …": up to 12 highly similar items among those of the view it was opened from (`Catalog::similar` on a worker; hidden if none); double-click opens one (Back returns), right-click gives its menu |
| `ui/menu.slint`, `src/actions.rs` | The item context menu on every picture (Review tiles, People faces, Acquire's recent strip, Details and its similar strip): Rotate left / right, Add tag… (dialog with matching tags from the catalog; `kind:` prefix or `People|…` pick the kind, `ops::add_tag`), Properties (dialog of `ops::properties`), Delete (to the library trash, with Undo in the notice at the bottom). A window-level `ContextMenuArea` that pages show at the pointer through `nav::context_menu`. Each action runs on a worker and reports what it changed with `pipeline::changed`, which refreshes every view |
| `ui/nav.slint`, `src/nav.rs` | Navigation: `nav::open(Target)` for every double-click, `nav::go` for the sidebar; both push a snapshot of the view left (Review: filters, selection, scroll anchor; People: person, selected faces, scroll anchor; Details: its item) that `nav::back` restores. A `Target` carries the origin view's `Scope` (its filtered items), which the similar strip draws from; `nav::context_menu` is where every page's right-click goes |
| `ui/pipeline.slint` | `PipelineStatus`: the pipeline's status line with Pause / Resume (in Review's bottom bar) |
| `ui/theme.slint`, `ui/types.slint` | Palette and the structs shared with Rust |
| `src/acquire.rs` | Hosts the device backends from `archive-core` (`../core`): tiles, coalesced preview delivery, thumbnail seeding |
| `src/people.rs` | Reads people and faces from the catalog on a worker; decodes crops for the rows on screen. Named / ignored faces are masked in place (no reflow until the grid is reloaded) while a writer thread saves them (`Catalog::name_faces`, which tags the pictures `People|<Name>`), reports the pictures changed (so Review shows the tag) and refreshes the suggestions |
| `src/details.rs` | Decodes the opened item off the UI thread (up to 4096 px), "Open externally"; asks the engine to rotate / accept / keep on a worker, reloads when the file is rotated |
| `src/pipeline.rs` | Hosts the pipeline `Engine` while the window is open: relay thread (coalesced reports → review-grid updates, status counts), capture priority, status line |
| `src/review.rs` | Background indexer, `GridModel` (rows sliced on demand, incremental merges while indexing and of pipeline updates), filter thread, batched thumbnail delivery |
| `src/thumbs/` | Viewport-ranked scheduler (`sched.rs`) → CPU workers (cache lookups, decodes) + I/O threads (source reads) → writer thread (`writer.rs`) → byte-budgeted LRU of `slint::Image` |

## Pipeline

While the window is open it hosts the library's processing pipeline (`archive_core::pipeline`,
see `../ARCHITECTURE.md`): the `Engine` is started for the root on launch (on a thread, with
`Config::default()`) and dropped after the window closes. One engine per library: if another
window or the `archive` shell already runs it, this one runs without and its status line says
"Pipeline running in another window"; rotating then goes to the core directly.

- **Reports**: the engine's sink only forwards events to a relay thread, which takes each burst
  in one go (~100 ms; thousands of `Updated` during a catch-up become one batch, one entry per
  file), reads the files they name (stat, catalog tags), counts what is left
  (`pipeline::status`: at most twice a second while busy, and once on idle) and posts one
  event-loop callback per burst. Warnings and failures go to stderr and show in the status
  line for a few seconds.
- **Captures**: each `DeviceEvent::Saved` calls `engine.add(path)` (after the thumbnail is
  seeded), so a new scan is processed at once instead of after the watcher's settle time, and
  appears in Review even without an engine here. While any device is acquiring, the engine
  runs in `Mode::Background` (one worker) so a real-time DV capture isn't starved; the status
  line's Pause holds it entirely.
- **Live refresh**: new files are merged into the Review grid in sort order; a changed file
  (e.g. rotated) gets a new entry, so it is re-sorted and its thumbnail is made afresh (the
  old entry is retired and its in-memory thumbnails dropped; the persistent cache layers are
  keyed by size + mtime, so they miss by themselves); a deleted file leaves the grid. The
  selection follows a replaced file. Reports that come in while the indexer is still walking
  are applied when it finishes. The Details page reloads a file that was rotated; People
  re-reads the catalog after the pipeline groups faces, if it is on screen.
- **Changes made here**: the context menu's rotate / delete / tag and naming faces in People
  report the files they touched with `pipeline::changed(path, pixels)`, which goes through the
  same relay: the grid re-reads their tags (the tag list is recomputed when a tag disappears),
  a trashed file leaves the grid and Acquire's strip (and Details goes back if it showed it),
  a file rotated without an engine gets fresh thumbnails and is reloaded on Details.
- **Rotations**: Details shows "Rotate 90°?" with Accept / Keep when orient left one for
  review, and Rotate left / right for any image. The engine rewrites the file on its own
  thread (about a second for a large scan), so these are asked from a worker; an error (e.g.
  a format that can't be rotated losslessly yet) is shown under the picture. Without an engine
  here, rotating goes straight to the core (`ops::rotate`); Accept / Keep need the engine.

## Acquire

The backends live in the `archive-core` crate (`../core/src/devices/`, see
`../ARCHITECTURE.md`). They implement `CaptureDevice` (`name`, `kind`, `start_preview`,
`start_acquire`, `stop`, optionally `start_alt` for a secondary action such as "Scan backs") and
report through an `EventSink` from their own threads. Preview frames are coalesced per device
(latest wins, one UI update in flight), so N devices at 30 fps can't flood the event loop. Each
saved capture is reported with a small thumbnail and its in-memory image
(`DeviceEvent::Saved`); the sink seeds the thumbnail cache from it on the backend's thread.
Captures from earlier sessions arrive as `DeviceEvent::Recent(path)`. Every panel shows its last
five under the preview.
Real backends to add in `archive_core::devices::discover()`: SANE/Epson SDK for FF-680W, V4L2 +
ALSA for VHS/Hi8 analog capture and the Kodak Reels (UVC).

**Epson Perfection V850** (`core/src/devices/v850.rs`, driver in `utils/epson/v850`, photo detection
in `utils/photo-duplex`): detected by USB id on launch/Rescan. It scans several photos at once
against the white document mat (which must be in the lid). Scan, or one press of the scanner
button, scans the whole bed (600 dpi; `ARCHIVE_STUDIO_V850_DPI` overrides) and saves each photo,
straightened, as `<dir>/epson-v850/photo-NNNN-front.png`, each reported (`Saved`) as soon as
it is written: one-sided photos need nothing more, and the next batch numbers on. Scan backs, or
two presses about a second apart, is optional: it scans again after the double-sided photos of
the latest batch were flipped over in place and saves `photo-NNNN-back.png` beside each front.
(When there are fronts to pair with, a single press waits those two seconds before scanning.)
Closing the app (or Rescan) right after a scan waits for its photos to be saved. Run a release
build: in a debug build the cut-out is ~20× slower (tens of seconds per batch instead of ~1 s).
The whole bed appears only in the live preview and is never saved; the recent strip and review
grid show the cut-out photos, whose thumbnails are seeded from memory. Stop cancels mid-scan; USB errors and unplugging reconnect automatically
(the last batch of fronts is kept). While the GUI runs it holds the scanner exclusively (the
standalone `x980` / `photo-duplex` CLIs and SANE can't use it). Hardware check:
`V850_OUT=/tmp/x cargo test --release -p archive-core -- --ignored --nocapture v850_hardware_scan`.

**DV / Digital8 camcorders over FireWire** (`core/src/devices/dv.rs`; needs
`sudo apt install dvgrab libavc1394-tools`): the first AV/C unit in `/sys/bus/firewire/devices`
is detected on launch/Rescan, and within ~2 s when switched on or plugged in later, without
disturbing other devices. E.g. a Sony DCR-TRV240, which plays Hi8/Video8 tapes and digitizes
them to DV itself. Put the camera on AC power in PLAY/EDIT (VCR) mode. The panel shows the live
picture and tape state. Record rewinds and captures the whole tape; Record from here starts where
the tape is. Each capture is the camera's DV stream, bit for bit, as
`<dir>/<model>/tape-NNNN.dv` (~13 GB/hour), and ends at the end of the tape, on Stop, after 2 min
of blank picture or at 250 min. `ARCHIVE_STUDIO_DV_AUTO=1` starts a whole-tape capture whenever a
tape is inserted, so a tape swap is the only step. `dvgrab` streams continuously into a reader
thread (file writes, previews); a controller thread drives the transport with short-lived `dvcont`
calls, each killed after 3 s, so a hung camera, a stalled stream or a pulled cable shows up as an
error on the panel (any open file is closed and kept) and recovers by itself, never blocking the
UI. The preview is decoded in Rust from the DC coefficients alone (90×60, no ffmpeg) and doubles as
blank detection; the poster frame seeds the review thumbnail. Check that the camera's on-screen
display goes to the LCD only: anything it draws on the video output is in the DV stream. Tests:
`simulated::capture_hang_and_unplug` runs the controller against fake tools; with the camera
attached, `cargo test --release -p archive-core -- --ignored --nocapture dv_hardware_stream`.
`utils/dv-tape-capture.sh` is the same capture loop as a standalone script.

## Review performance

- Index = append-only list of shared chunks (`Arc<[Entry]>`, one per indexer batch) with sort
  keys precomputed on the indexer thread (16-byte lowercase-name prefix, mtime, size);
  filter/sort yield a `Vec<u32>` of ids. No per-file UI objects.
- Indexing streams batches (every ~150 ms) that are filtered, sorted on their own and merged
  into the shown order in place (no re-sort; the merge compares a contiguous key array and
  only touches entries on ties). The view gets `row_changed` from the first shifted row on
  plus `row_added` at the end, never `reset`: on-screen rows update in place and the scroll
  position holds (`row_changed` is a no-op for rows the `ListView` hasn't instantiated).
  Batches of ≥ 20k entries (fast disks) go to the filter thread instead.
- Re-filtering (search, kind, sort; debounced 120 ms) of libraries ≥ 20k entries runs on a
  filter thread over a snapshot of the chunk list; the UI thread just swaps in the result.
  Each request has a generation, so results for superseded queries are dropped (and queued
  requests skipped), and entries indexed meanwhile are merged in when it lands.
- Thumbnail results go to a mailbox; only the push that finds it idle schedules an
  event-loop callback, which applies everything queued (≤ 4 ms per turn, the rest on the
  next turn) and notifies each affected row once.
- `ListView` of rows; `GridModel::row_data` slices `columns` ids per row, so only visible
  rows are ever materialized. Column count is derived from the view width in Slint.
- Thumbnail priority comes from the view: `review.slint` reports the visible row range
  (`viewport-changed`, only when the first/last row index changes) and Rust schedules
  visible tiles top to bottom, then 2 screens ahead in the scroll direction, then half a
  screen behind (≤ 512 tiles). Each report replaces the queue under one lock: tiles that
  left the window are cancelled before any thread starts on them, and work parked between
  stages is re-ranked or dropped at its next hand-off. `row_data` never requests.
- Stages: cache lookups run on the CPU pool (cores − 1, ≤ 16), so hits never wait behind
  disk reads. Misses go to a few I/O threads (2 if the root is on a rotational disk per
  `/sys/dev/block/*/queue/rotational`, 8 on SSD, 4 if unknown; override with
  `ARCHIVE_STUDIO_IO_THREADS` / `ARCHIVE_STUDIO_DECODE_THREADS`) that read the whole file
  and hand the bytes back to the CPU pool (read-ahead bounded); TIFFs are decoded while
  streaming on the I/O thread, since a reduced page may be all that is needed.
- Placeholders: a JPEG's EXIF thumbnail is delivered from the first 128 KB read, before
  the rest of the file; every thumbnail's average color is kept (and stored in the cache)
  and painted as the tile background until the image arrives.
- JPEG encoding and SQLite inserts happen on one low-priority writer thread, batched 16
  files per transaction, after the thumbnail is delivered. Its queue is bounded (64 files);
  when full, writes are dropped rather than stalling decoders (the file is decoded again
  next time). App exit waits up to 2 s for queued writes. Databases from before the
  `color` column are migrated on open (old rows get their color computed on load).
- Measured (32 cores, WD 5400 rpm HDD; viewport 8×5 tiles at 192 px; medians of 3; `bench_schedule`):

  | Scenario | before | after |
  |---|---|---|
  | first screen, empty thumb cache, 2,000 small files in page cache | 56 ms | 30 ms |
  | same, 200 × 3600×2400 JPEGs | 168 ms (first tile 24 ms) | 122 ms (first tile 2 ms) |
  | same, files evicted from page cache (HDD) | 245–310 ms | 250–320 ms (disk-bound) |
  | first screen, warm thumb cache | 4.2 ms | 3.1 ms |
  | visible tiles with a thumbnail during a 40 rows/s scroll, small files / big JPEGs | 99.6% / 56% | 100% / 82% (91% incl. EXIF previews) |
  | same, big JPEGs from cold HDD (disk saturated) | 27%, settles 154 ms after stop | 34%, settles 70 ms |

  On the HDD, 2–4 concurrent source reads beat 1 or 8 (8: ~1.8× slower first screen).
- Two size tiers (192 / 384 px longest edge), picked from tile size × window scale factor.
  While zooming, the other tier's thumbnail stands in until the right one arrives.
- Cache layers, fastest first: decoded RGB `Image`s on the UI thread (256 MB LRU) →
  compressed JPEGs shared by workers (64 MB LRU, ~1 ms to re-decode) → SQLite at
  `$XDG_CACHE_HOME/archive-studio/thumbs.sqlite` (WAL, one connection per worker) →
  full decode of the source, which stores both tiers (+ average color). Keys hash (canonical path, size,
  mtime ns, tier), so edited files miss instead of showing stale thumbnails. An unusable
  database just disables the disk layer. The DB is never pruned yet.
- On each `DeviceEvent::Saved`, the acquire sink calls `thumbs::store_for_new_file(path, &image)`
  with the backend's in-memory image, so new scans are cached without ever being decoded.
- Search matches pre-lowercased names.
- UI-thread cost, before → after (`bench4_*` in `review.rs`; 200k = synthetic tree of sparse
  files on tmpfs, name sort unless noted; 32 cores):

  | Scenario | before | after |
  |---|---|---|
  | indexing 200k, ~150 ms batches (5–10 batches): UI total / worst callback | 427 ms / 153 ms | 1 ms / 0.2 ms (+ filter thread) |
  | indexing 200k in 2,000-entry batches (slow disk): UI total / median / worst | 4,515 ms / 43 ms / 123 ms | 128 ms / 1.3 ms / 4.5 ms |
  | same, date sort | 1,959 ms / 20 ms / 50 ms | 118 ms / 1.2 ms / 3.3 ms |
  | refilter 2k entries (sync): latency = UI blocked | 0.1–0.6 ms | 0.2–0.4 ms |
  | refilter 200k, name / type / date sort: latency | 97 / 98 / 40 ms | 21 / 21 / 20 ms |
  | refilter 200k: UI blocked | 11–98 ms | 0.1–0.3 ms |
  | deliver 2,000 thumbnails: event-loop callbacks, UI free-running / busy 2 ms per turn | 2,000 / 2,000 | ~1,150 / ~11 |
  | same, UI time / row notifications (busy UI) | 5–6 ms / 2,000 | 1.4–2.4 ms / ~1,200 |

Source decoding (`src/thumbs/decode.rs`, cache misses only) never builds more pixels than the
384 px tier needs; images are EXIF/TIFF-orientation corrected:
- JPEG: `jpeg-decoder` IDCT-domain downscaling (1/8, 1/4, 1/2), then a short resize
  (3600×2400 scan: 110 → 37 ms; 24 MP camera JPEG: 295 → 85 ms). Grayscale, CMYK and
  progressive supported; anything it rejects falls back to `image`.
- TIFF: streamed from the file (no whole-file read); if it has a reduced-resolution page
  (NewSubfileType = 1 or a SubIFD) the smallest one ≥ 384 px is decoded instead
  (86 MB scan: 225 → 80 ms full, ~2 ms with a reduced page). Backends should save scans with
  `thumbs::write_tiff_with_reduced_page`.
- PNG/WebP/BMP: full `image` decode; all resizing uses `fast_image_resize` (SIMD bilinear
  convolution, ~5x faster than `image`'s `thumbnail()` on large downscales).
- `thumbs::decode_preview` returns a JPEG's embedded EXIF thumbnail (~160 px) from the
  metadata segments alone, for use as an instant placeholder.
- Decoder panics on malformed files are caught; the file just gets no thumbnail.

- Pipeline updates (`update_files`): a new file is merged like an indexer batch; a changed one
  is appended as a new entry and the old id retired (`Index::gone`, left out of every filter
  and removed from the shown order with `row_removed`, never `reset`). A path → id map
  (64-bit path hashes computed on the indexer thread) finds the entry.

Stubs: video/audio thumbnails, the in-app viewer
(Details has no zoom yet), real device backends, persistent index.
