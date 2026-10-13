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
| `src/review.rs` | Background indexer, `GridModel` (rows sliced on demand, incremental merges while indexing), filter thread, batched thumbnail delivery |
| `src/thumbs/` | Viewport-ranked scheduler (`sched.rs`) → CPU workers (cache lookups, decodes) + I/O threads (source reads) → writer thread (`writer.rs`) → byte-budgeted LRU of `slint::Image` |

## Acquire

Backends implement `CaptureDevice` (`name`, `kind`, `start_preview`, `start_acquire`, `stop`)
and report through an `EventSink` from their own threads. Preview frames are coalesced per
device (latest wins, one UI update in flight), so N devices at 30 fps can't flood the event loop.
Real backends to add in `acquire::discover()`: SANE/Epson SDK for FF-680W, V4L2 + ALSA for
VHS/Hi8 analog capture and the Kodak Reels (UVC), `dvgrab` for FireWire camcorders.

**Epson Perfection V850** (`src/acquire/v850.rs`, driver in `utils/epson/v850`): detected by USB
id on launch/Rescan. A scan starts from the panel's Scan button or the scanner's own button;
it streams to `<dir>/epson-v850/scan-NNNN.png` (full bed, 600 dpi; `ARCHIVE_STUDIO_V850_DPI`
overrides) with a live preview that also seeds the review thumbnail. Stop cancels mid-scan; USB
errors and unplugging reconnect automatically. While the GUI runs it holds the scanner
exclusively (the standalone `x980` CLI and SANE can't use it). Hardware check:
`V850_OUT=/tmp/x cargo test --release -- --ignored --nocapture v850_hardware_scan`.

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
- Capture backends call `thumbs::store_for_new_file(path, &image)` after saving a scan, so
  new scans are cached from the in-memory image without ever being decoded.
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

Stubs: video/audio thumbnails, the in-app viewer
(double-click currently hands off to `xdg-open`), real device backends, persistent index.
