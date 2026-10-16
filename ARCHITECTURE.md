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
| `core/` | `archive-core` (lib `archive_core`) | `device`: the capture device API. `devices`: backends (Epson V850, DV over FireWire, mocks) and `discover` / `hotplug`. `catalog`: the library database. `pipeline`: the processing engine. `ops`: every user-level change, each recorded in the undo journal (`undo` / `redo` / `history`): per-asset actions (rotate, accept / keep a proposed rotation, delete, tag / untag, pair / unpair / swap sides, properties), faces and people (name, confirm, not, ignore / unignore, delete, add, reject / clear rejections, covers, rename / merge / delete people), bulk ones for a multi-selection (`delete_all` / `restore_all` with per-asset results, `add_tag_all` in one transaction, `rotate_all`), tags as a whole (`tag_tree`, `rename_tag`, `merge_tags`, `delete_tag`), the trash (`purge`, `empty_trash`: not undoable), and `retry` of failed stages (through the engine if one runs here). `desktop`: open a file in its default application, show it in the file manager (freedesktop `FileManager1.ShowItems` over D-Bus, else the folder). `exif`: basic EXIF fields |
| `cli/` | `archive-studio-cli` (bin `archive`) | Interactive shell: hosts the pipeline engine, status, review, list by state (`list failed`, `review`, `untagged`, `unknown-faces`, `has-back`, …, `no-…`), rotate, faces (name, not, ignore, delete-face, add-face; `reject` / `unreject FACE_ID... PERSON`, `rejected PERSON`, `unignore`, `ignored`), people (`people [name|faces|recent] [TEXT]` with counts and cover face, `person rename / merge / delete / cover`), tags (tag, untag; `tag` and `delete` / `restore` take several files; `tags` as a tree with counts, `rename-tag`, `merge-tag`, `delete-tag`), front/back (pair, unpair, swap-sides), find by description (`find WORDS...`), files that look like one (`like PATH`), failures (`failed` with errors, `retry [STAGE|PATH...]`), trash (`trash` with dates, sizes and totals, `restore`, `purge PATH...`, `empty-trash [DAYS]`), `undo`, `redo`, `history [N]` |
| `logging/` | `archive-logging` (lib `archive_logging`) | The logger the binaries install behind the `log` facade: stderr + rotating file in the library, panic hook, recent records / subscribe for in-app viewers, fd 2 capture |
| `gui/` | `archive-studio-gui` (bin `archive-studio`) | Slint desktop app: acquire tiles hosting `archive_core` devices, review grid, thumbnail cache (`gui/src/thumbs`) |
| `utils/epson/v850` | `x980` | Standalone Epson GT-X980 / V850 USB driver (lib + CLI) |
| `utils/photo-duplex` | `photo-duplex` | Finds and cuts out photos on a flatbed scan, pairs fronts with backs (lib + CLI) |
| `utils/faces` | `faces` | Face detection (SCRFD), alignment, ArcFace embeddings (also of a hand-drawn box) and clustering into people (lib; `examples/faces.rs` driver) |
| `utils/orientation` | `orientation` | 90°-step orientation model (lib; used by the pipeline's orient stage) |
| `utils/clip` | `clip` | TinyCLIP image and text embeddings, CLIP tokenizer, grouped zero-shot labels (lib; `examples/clip.rs` driver; `export_onnx.py` makes the model) |
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

What happens to a file once it is in the library: register → orient → faces, signature and
describe, then, once the per-file stages are quiet, identify (suggestions and clusters) and
XMP sidecar sync.

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
  `orientation.onnx`, `buffalo_l/` (`det_10g.onnx`, `w600k_r50.onnx`) and `tinyclip-8m/`
  (`visual.onnx`, `textual.onnx`, from `utils/clip/export_onnx.py`). A stage whose model is
  missing is skipped.

## Catalog concepts (`archive_core::catalog`)

- **People are tags.** A person is a `person`-kind tag `People|<Name>`. Naming a face
  (`name_faces`, `confirm_faces`) puts that tag on the asset as a user tag carrying the face
  box as its region (written to sidecars as an MWG face region). Un-naming, renaming
  (`rename_person`; onto an existing name merges), `merge_people` and `delete_person` keep
  tags in step; a tag stays while any face on the asset is still that person. Taking the tag
  off an asset un-names that person's faces there. Schema 4 backfilled tags for faces named
  before this; `sync_person_tags` repairs. `face_boxes(asset)` gives boxes (0..1, displayed
  image) with names for drawing. `matching_people` is what a name box offers for a typed name.
- **Covers and people lists** (`people`): `person_cover(person)` is the face that stands for
  someone: the one picked with `set_person_cover` while it is still confirmed as them, else
  their best confirmed face (with a crop, found by a scan, highest detector score scaled
  down for boxes under 15% of the picture's height); `people_with_faces` carries it, and
  when a face was last named as them (`faces.named_at`). A merge keeps the cover of whoever
  had one. `people::arrange` sorts such a list by name, most faces or recently named and
  narrows it by name (`PeopleOrder`). Schema 7.
- **Rejections and ignored faces** (`faces`): `reject_person(faces, person)` says faces
  aren't that person whoever they are suggested as by now (they stay unreviewed, a
  suggestion of them is dropped, refresh_suggestions skips them), `clear_rejections(faces,
  person)` takes it back; `unignore_faces` returns only ignored faces to unreviewed (a face
  named meanwhile keeps its name, unlike `unreview_faces`). `FaceFilter::Rejected(person)`
  and `FaceFilter::Ignored` list them; `count_faces(filter)` counts any filter without
  reading the faces.
- **Fixing faces on one picture** (`faces`): `not_person` (un-named and untagged as above,
  and that person never suggested for it again), `ignore_faces`, `delete_faces` (the row,
  its crop and rejections; the tag follows as for un-naming; a rescan may find a deleted
  detection again). `add_face` stores a face the user boxed as *manual* (schema 6): rescans
  keep it like a reviewed one. `faces::scan_region` (via `ops::add_face`) gives its crop and
  embedding: `faces::Faces::embed_region` runs the detector again on that part of the image
  and aligns on the landmarks of the best detection centred in the box, else on ArcFace's
  template laid over the box. Without the models the embedding is empty and suggestions and
  clusters skip the face. The models come from `Engine::face_models`, else
  `ops::face_models` loads them.
- **Front/back pairs** (`pairs`): `asset_pairs(front, back)`. Filled by naming when an asset
  is first seen (`X-back.*` / `X_back` / `X back` with `X-front.*` or `X.*` in the same
  directory, as the V850 / photo-duplex writes `photo-NNNN-front.png` / `-back.png`), or by
  hand (`set_pair`, `unpair`). `back_of` / `front_of`. Front ends go through `ops::pair`
  (both images, not trashed, not the same; old pairs of either are replaced; returns every
  asset whose other side changed, to refresh), `ops::unpair` and `ops::swap_sides`.
- **Untagging** (`ops::remove_tag`): what the user typed, matched against the asset's own
  tags (ignoring case; `kind:path` restricts the kind; a bare name can be a person). A
  person tag taken off un-names that person's faces on the asset rather than being refused:
  the tag is what says they are in the picture (`Catalog::remove_tag`).
- **Tags as a whole** (`tags`): `tag_counts(min_confidence)` gives every tag with how many
  live assets (not trashed) carry it or a tag under it, in one query (detector tags from the
  confidence asked, as searches count them); `tags::tree` lays counts out as the hierarchy,
  and `TagCounter` counts the same way in memory for a front end that holds every asset's
  tags. `rename_tag` moves a tag with its subtree (a change of case too), makes missing
  ancestors of the new path and fails, changing nothing, onto a taken path; `merge_tags`
  moves each tag of `from`'s subtree onto its counterpart under `into` (or renames it there),
  with its asset rows, faces named or suggested as it and rejections; `delete_tag` takes the
  subtree off every asset and un-names faces named as one of them. All mark the assets for
  sidecar sync, and drop ancestors left with nothing on or under them. `tag_assets` lists
  the live assets a change touches. Front ends go through `ops::tag_tree`, `ops::rename_tag`
  (`Renamed::Taken` onto another tag's path unless asked to merge), `ops::merge_tags`,
  `ops::delete_tag`: a person stays under `People|` and goes through `rename_person` /
  `merge_people` / `delete_person`; only people merge with people; `People` itself is kept.
  A front end that reads sidecars as well skips what the catalog wrote there itself
  (`sidecar_tags`): it may be stale until the next sync.
- **Search syntax** (`query`): a plain term matches consecutive segments of a tag path
  anywhere; `=People|Ann` is that path from the top and its subtree (not `People|Anna`),
  what a tag tree filters by.
- **Many at once** (`ops::delete_all`, `ops::restore_all`, `ops::add_tag_all`): deleting
  and restoring go asset by asset (files move; one failing doesn't stop the others) and
  return each asset's result, so a front end can undo exactly what went; tagging is one
  transaction (`Catalog::add_tag_to_all`), all or nothing.
- **Flags** (`flags`): what a front end shows on a tile and filters by, read in bulk:
  `asset_flags()` (every asset in the library, by path, a few scans) or `flags_of(assets)`
  (a batch, indexed lookups in chunks), never per tile. `AssetFlags`: `FACES` (not ignored
  ones), `UNKNOWN_FACES` (one isn't named), `HAS_BACK` / `IS_BACK`, `REVIEW` (a stage wants the
  user to look at the current file: orient's rotation), `FAILED` (a stage failed on the
  current file; runs on an older version don't count), `TAGGED` (a tag of the user's, people
  named on it included; *untagged* is its absence, detector tags don't count).
  `assets_flagged(with, without)` lists by them; `attention_counts()` counts `REVIEW` and
  `FAILED` cheaply for a status line.
- **Trash** (`trash`): deleting moves the file and sidecar to
  `.archive-studio/trash/<when>-<id>/<path>` and the asset row with it (kept, marked
  trashed and missing, out of searches). `restore` undoes it; `empty_trash(older_than)`
  deletes for good what was trashed that long ago, `purge(assets)` chosen items. A front
  takes its back along (each `Trashed` names its `back`). `trashed()` lists the trash
  newest first with each item's original path, file in the trash (to draw a thumbnail
  from), when and size; `trash_totals()` counts items and bytes in one query.
- **Failures** (`runs`): `failures()` lists every stage that failed on the current file of
  a live asset (stage, path, error, when), by stage then path; a failure isn't retried
  until the file changes or `retry(Retry::All | Stage(s) | Runs(asset, stage…))` forgets
  it. A running engine needs telling: `Engine::retry` does it on the engine thread and
  looks for work at once; front ends go through `ops::retry(catalog, engine)`.
- **Undo journal** (`undo`, schema 8): every user change goes through `ops`, which records
  it as one change with a description (`Tag 12 items Holidays`): `Catalog::journaled(desc,
  f)` turns on temporary triggers on this connection (installed once per connection) that
  record each row `f` inserts, deletes or updates in the user's tables (`undo::JOURNALED`:
  tags, asset_tags, faces, face_crops, face_rejections, person_covers, asset_pairs), the rows
  the catalog's own triggers and foreign keys change along with them included; an update
  keeps its key and the columns it changed (values stored exactly: no type affinity). What
  isn't rows is a `FileAction` (`record_action`): `Trash` / `Restore` of assets, `Orient`
  (pictures turned `cw`, with each one's orient run before and after). `ops::undo(catalog,
  engine)` performs the inverse action (restore what was trashed, turn back losslessly (a JPEG's
  partial edge blocks went with the first turn), through the engine if one runs here, and put the orient run back on the file as it is now) and then
  `apply_change` replays the rows backwards, inverted, in one transaction with foreign keys
  deferred (asset tag changes mark sidecars for sync as any change does; a tag the change
  made is dropped only while nothing else uses it). `ops::redo` replays forwards; both return
  the files touched (before and after: a file moved to or from the trash both). A stack
  persisted in the catalog, so the shell and the GUI share it and it survives restarts:
  `next_undo` is the latest change not undone, `next_redo` the earliest undone one, `history(n)`
  lists them; a new change drops the undone ones, and only the last `KEEP` (200) are kept. A
  change that can't be applied any more (what it touched changed since) is dropped with an
  error, so the next undo goes on. Purging from the trash isn't undoable and drops the changes
  that touched what was purged. Changes other connections make (the pipeline's) aren't
  recorded: an undo restores the user's rows as they were, whatever the pipeline wrote since.
- **Signatures** (`similar`): a 128-bit dHash plus a 64-bin colour histogram per image,
  computed by the pipeline's `signature` stage (no model; after orient). `similar(asset,
  candidates, limit)` returns only highly similar ones (≤ 16 bits apart, ≥ 50% colour overlap).
- **Describe** (`semantic`, `pipeline::describe`): the `describe` stage (after orient) stores
  a CLIP embedding per image (schema 5, `image_embeddings`, with the model's name) and
  proposes tags from a fixed vocabulary as detector `clip/tinyclip-8m`: `Scenes|…`,
  `Objects|…`, `Events|…`, `Photo|…`. Labels compete within their group (softmax against
  "none of these" prompts), not across groups. Tags from 35% are stored; as for any
  detector, those from 50% are searched and written to sidecars. `TextSearch` (text model
  only) ranks images against a description: `search_embedding(query, model, limit)` scans
  every embedding, fine for tens of thousands of images. Changing the vocabulary means
  bumping `describe::VERSION`, which redoes the stage.
- **Search by description**: `ops::text_search(engine)` gives the text model, the engine's
  (`Engine::text_search`, shared with its describe stage) else one loaded on first use and
  kept for the process; front ends (the GUI's Review, the shell's `find`) go through it.
  `TextSearch::search` and `describe::similar_images` (images like one image, by its own
  embedding, itself left out) rank with `semantic::rank` and a `Cutoff`: at least `min`
  similar and within `window` of the best match (`TEXT_CUTOFF` 0.2 / 0.08, `IMAGE_CUTOFF`
  0.75 / 0.2, measured on family photos), then a limit. `describe::coverage` (`Catalog::
  embedding_coverage`) says how many live images have an embedding of the current model, of
  all of them, so a front end can say a search only sees part of the library yet.

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
