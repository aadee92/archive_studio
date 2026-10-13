# Archive Studio GUI

Slint + Rust desktop front end. Light theme, left-sidebar navigation.

```sh
cargo run --release -- [DIR]             # DIR defaults to the current directory
ARCHIVE_STUDIO_MOCK=ff680,ff680,ff680,ff680 cargo run   # 4 simulated FF-680s → 2×2 grid
ARCHIVE_STUDIO_MOCK=ff680,vhs,reel,hi8 cargo run        # mixed scanners + video
```

## Layout

| Path | Role |
|---|---|
| `ui/app.slint` | Window: sidebar + page host (pages stay alive when hidden) |
| `ui/acquire.slint` | One `DevicePanel` per device, tiled in a near-square grid (1→1×1, 2→2×1, 4→2×2, 6→3×2…) |
| `ui/review.slint` | Search, kind chips, sort, zoom slider, virtualized thumbnail grid |
| `ui/theme.slint`, `ui/types.slint` | Palette and the structs shared with Rust |
| `src/acquire.rs` | `CaptureDevice` trait, discovery, coalesced preview delivery, mock devices |
| `src/review.rs` | Background indexer, `GridModel` (rows sliced on demand), filter/sort |
| `src/thumbs.rs` | LIFO bounded decode queue → worker pool → LRU of `slint::Image` |

## Acquire

Backends implement `CaptureDevice` (`name`, `kind`, `start_preview`, `start_acquire`, `stop`)
and report through an `EventSink` from their own threads. Preview frames are coalesced per
device (latest wins, one UI update in flight), so N devices at 30 fps can't flood the event loop.
Real backends to add in `acquire::discover()`: SANE/Epson SDK for FF-680W, V4L2 + ALSA for
VHS/Hi8 analog capture and the Kodak Reels (UVC), `dvgrab` for FireWire camcorders.

## Review performance

- Index = flat `Vec<Entry>`; filter/sort yield a `Vec<u32>` of ids. No per-file UI objects.
- `ListView` of rows; `GridModel::row_data` slices `columns` ids per row, so only visible
  rows are ever materialized. Column count is derived from the view width in Slint.
- Thumbnails are requested only from `row_data`, decoded on worker threads, newest request
  first; overflowed (scrolled-past) requests are dropped. Decoded `Image`s live in a bounded LRU.
- Search is debounced (120 ms) and matches pre-lowercased names.

Stubs: `thumbs::decode` (returns `None`, so placeholders show), the in-app viewer
(double-click currently hands off to `xdg-open`), real device backends, persistent index.
