# Archive Studio GUI

Slint + Rust desktop front end. Light theme, left-sidebar navigation.

```sh
cargo run --release -- [DIR]             # DIR defaults to the current directory
ARCHIVE_STUDIO_MOCK=ff680,ff680,ff680,ff680 cargo run   # 4 simulated FF-680s → 2×2 grid
ARCHIVE_STUDIO_MOCK=ff680,vhs,reel,hi8 cargo run        # mixed scanners + video
```

## Logs

In the app, click the status bar at the bottom of the window to open the live log: the last
5000 records (filter by text or level), with what libraries print on stderr as target
`stderr`. While it is closed, the bar shows the newest record and how many warnings and
errors came in since it was last open.

Each run writes `<library>/.archive-studio/logs/archive-studio.log` (the previous four runs
are `archive-studio.log.1` … `.4`; a file is also rotated at 16 MB). It always has `debug`
lines from the app and its libraries (but tract's model optimizer and nusb's USB transfers,
which would crowd out everything else), whatever stderr shows, so after something odd happened, look there first
(`.1` if the app was restarted since). The first lines give the version and commit, the
library, the log file and the Slint version / display.

```sh
ARCHIVE_STUDIO_LOG=debug cargo run --release -- DIR       # stderr too (default: info)
ARCHIVE_STUDIO_LOG=info,archive_studio=trace cargo run -- DIR   # + scroll positions, keys typed
ARCHIVE_STUDIO_LOG=info,archive_core::pipeline=debug cargo run -- DIR  # per-file stage results
ARCHIVE_STUDIO_PAGE=2 cargo run -- DIR                     # start on People (1 Review, 4 Trash, 5 Failures), not the page shown last
tail -f DIR/.archive-studio/logs/archive-studio.log
```

What is in it (targets `archive_studio::<module>`):

- `nav`: every open / go / back with the page, the history depth and the snapshot pushed or
  restored (Review: filters, selection, scroll anchor, the selection followed from Details;
  People: person, faces selected, scroll anchor; Details: the item and its scope).
- `undo`: each undo / redo with the change and how many files it touched, or why it couldn't.
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

## Screenshots

Where no window can be opened (a terminal, an agent, CI), `--shot` runs the whole app (pipeline,
every page) on Slint's software renderer in an offscreen window, and after a while writes what
it shows to a PNG and exits (`src/shot.rs`). It doesn't read or write the user's settings, so
views show their defaults. Use a copy of some photos as the library: the pipeline writes its
catalog and thumbnails there.

```sh
mkdir -p /tmp/shotlib && cp some/photos/*.jpg /tmp/shotlib/
cargo run -- --shot out.png --page details --item 3 --size 1600x1000 /tmp/shotlib
cargo run -- --shot small.png --page details --size 900x600 --info /tmp/shotlib
cargo run -- --shot full.png --page details --fullscreen --scale 2 /tmp/shotlib
cargo run -- --shot review.png --page review --wait 60 /tmp/shotlib   # after the pipeline ran a while
```

`--page` is `review` (default), `people`, `details`, `trash`, `failures` or `acquire`;
`--item N` opens the Nth image of the library (sorted by path) on Details, stepping through
all of them; `--size WxH` is in logical pixels (default 1600x1000), `--scale` the device
pixels per logical pixel; `--wait SECONDS` (default 5) is how long the app runs before the shot
(longer for thumbnails, faces and similar items to come in); `--fullscreen` and `--info` turn
on Details' full screen and info panel. Look at the PNG with any image viewer (or an agent's
file reader). Popups and menus aren't opened, and nothing is clicked.

## Layout

| Path | Role |
|---|---|
| `ui/app.slint` | Window: sidebar + page host (pages stay alive when hidden), status bar under every page |
| `ui/log.slint`, `src/log_view.rs` | The status bar (pipeline state with Pause / Resume, newest log record, unseen warning / error counts) and the log panel it opens above it: follows the newest record until scrolled up, text and level filter, Clear; drag its top edge to resize. Fed from `archive_logging::recent` / `subscribe`, polled on the UI thread every 150 ms |
| `ui/acquire.slint` | One `DevicePanel` per device, tiled in a near-square grid (1→1×1, 2→2×1, 4→2×2, 6→3×2…) |
| `ui/review.slint` | Search, with two chips above the box for what it searches: **Name & tags** (file names, or a tag query) or **Description** (what the picture shows, e.g. "dog on a beach", ranked by the CLIP model; a toggle rather than a prefix, so the mode is visible and the box's placeholder says what to type). In Description mode the results are the images the describe stage embedded that fit the text, best first (`TextSearch::search`: at least 0.2 similar and within 0.08 of the best match, at most 1000), the sort offers and defaults to "Relevance" (another sort orders the same matches), and the kind, tag and "Needs attention" filters narrow them as usual; typing waits 350 ms before searching, and changing the other filters, the sort or coming Back doesn't search again. A line under the box says "Searching…", "120 of 400 images described so far; only those are searched" while the pipeline hasn't described everything, or why it can't search ("Description search needs the tinyclip-8m model …"). "Find similar" in the item menu switches to Description mode with a "Looks like IMG_0571.jpg" chip (× clears it; typing replaces it): the picture first, then the images whose own embeddings are closest (`describe::similar_images`: at least 0.75 and within 0.2 of the best), other filters cleared; Back returns to the view it was asked from. Sorted by Date (when a picture was taken, from its EXIF via the pipeline's date stage, else when its file was modified, in local time), each month gets a header row ("October 2026", "42 items") and starts on a new row; clicking a header selects that month's items (Ctrl / Shift add them). Kind chips, "Needs attention" chips (Rotation to review, Failed, Unknown faces, Untagged: no tag of the user's, named faces count but detectors' tags and folder names don't; Has back: a front with a back; each one narrows the view further, with the other filters; Clear; kept in Back snapshots), sort, tag tree (`People|Ann` under `People`, ▸ / ▾ collapse a node; each with how many items carry it or a tag under it, counted over the grid's entries with `TagCounter` and kept up as files come, change and go; picking one filters by it and everything under it, `=People|Ann`, so not `People|Anna`; right-click for Rename…, Merge into…, Delete…: see `src/tags.rs`), virtualized thumbnail grid with badges in each tile's top-left corner, scaled with the tile (red "!" a stage failed, amber rotate icon a rotation to review, people icon for faces, amber while one is unnamed, two cards for either side of a front/back pair; from the entries' catalog flags, never read per tile), size slider in the bottom bar (Ctrl+wheel over the grid does the same; the pipeline's state is in the window's status bar). Multi-select as in People: click, Shift-click (range from the anchor), Ctrl-click (toggle; Ctrl+Shift adds a range), drag a rubber band (Shift / Ctrl keep what was selected; dragging past the edge scrolls), Ctrl+A, Esc clears; selected tiles have a check mark, and the bottom bar shows "N selected" with × to clear. Keys (once the grid was clicked or the page shown, never while a search box is typed in): arrows move the focus (a lighter ring) and select it, Shift extends from the anchor, Page Up / Down, Home / End; the view scrolls just enough to keep it in sight. Enter opens the focused item on Details, Delete trashes the selection, T tags it, R / Shift+R rotate it right / left, Ctrl+C copies its files (`src/clipboard.rs`) (through `actions::act_on`). Right-clicking an item of a multi-selection gives the menu for all of them ("Add tag to 12 items…", Rotate, "Delete 12 items"; pairing, Show in folder and Properties are disabled); right-clicking outside it selects that item first. The selection is a set of entry ids in Rust (`row_data` asks it per tile); it follows a file the pipeline replaces, and drops items that leave the grid, deleted or filtered out (they don't come back when the filter changes again), so an action never touches anything hidden. After the focused item is deleted, the focus moves to the next one (selected if nothing else is). Back snapshots keep it by path |
| `ui/people.slint` | People list (Unknown first, then person tags with faces, each with a small round crop of their cover face, else their initial) and the selected person's face crops: click / Shift / Ctrl / drag to select, name box (with matching names) and Ignore above the list. A person's suggested faces come first, highlighted. Above the list a "Find a person" box narrows it (Esc clears) and a sort box orders it by Name, Most faces or Recent (who had a face named last); Unknown stays first, and the sort is kept for the session (`people::arrange` in the core, on the UI thread from the list as read). Right-click a person for Rename…, Merge into… (only people offered) and Delete person… (says how many faces on how many pictures go back to Unknown), the tag dialog of `src/tags.rs` in person wording; dragging a person onto another (a chip says "Merge Ann into Bob", the row is outlined) opens Merge into… filled in, to confirm. Right-clicking a face confirmed as the person on screen adds "Set as cover for …" to the item menu (as does a named face box's menu on Details): `ops::set_person_cover` on a worker, then the list is re-read. With suggested faces selected, buttons over the grid say "Confirm N" (named as the person on screen) and "Not Ann" (`ops::reject_person`: they stay Unknown and aren't suggested as her again, not ignored); "N rejected" beside them lists the faces said not to be her ("Not Ann"), where "Suggest as Ann again" takes a rejection back (`ops::clear_rejections`) and "Back to Ann" returns. "Ignored", last in the list with its count, shows the ignored faces: "Un-ignore" (`ops::unignore_faces`) puts them back in Unknown, and the name box names them directly. The same action is in the item menu for the face right-clicked (or the selection it is in). Keys on the grid: Enter names the selection as the name box says (prefilled with the person on screen, so it confirms), Backspace / Delete does the view's action, Ctrl+A, Esc; typing a letter starts a name in the name box. All of these are masked in place like naming; once saved, a notice says what was done (the undo journal's description), with Undo |
| `ui/details.slint` | One item enlarged; opened by double-clicking a tile in Review, a face in People or a recent capture in Acquire. Back arrow / Esc / Alt+Left returns to that view as it was. Left / Right (Home / End) step through the items of that view in its order, and a filmstrip above the picture shows the ones around the shown item (one row of 64 px tiles, centered on the shown item and highlighted; small thumbnails from the cache on a worker, nearest first; hover for the name, click to show one, right-click for its menu; hidden for a single item). Stepping replaces the item in place, without history, so Back returns to the originating view, where Review's selection follows the item shown last (scrolling to it if it is out of view). The pictures of the next two items in the stepping direction and the previous one are decoded ahead on a prefetch thread, so a step shows the full picture at once (else its thumbnail until decoded). Face boxes over the picture (placed in displayed-image coordinates) with names under confirmed ones, toggled independently in the bottom bar; a face opened from People pulses 3× over 1.5 s. While the boxes are shown, the picture's faces can be fixed up (not the other side's): hovering highlights a box; a click opens a popup under it (above if there's no room, following zoom and pan) to name or rename that face, starting with its name or suggestion selected, with the people whose names match what is typed (`matching_people`, as in People's name box; Up / Down, Enter or a click assigns, Esc closes), and "Not <name>" (un-named and untagged, never suggested as them again: `ops::not_person`), Ignore, and Delete box (`ops::delete_faces`); right-clicking a box gives the same as a menu (Name… / Rename…, Not <name>, Ignore, Delete face box), anywhere else the item menu. "Add face" in the bottom bar (Esc or Cancel stops): drag a box around a face the detector missed; it is stored as a manual face with its crop and an embedding (`ops::add_face`, with the engine's face models, else ones loaded once here) and its popup opens. Changes run on a worker one at a time (each with a notice offering Undo), redraw the boxes, report the picture with `pipeline::changed` (Review's tags), make People re-read (also on Back) and refresh the suggestions. Zoom: the wheel zooms around the pointer, drag pans while the picture is larger than its box, double-click toggles between fitted and 100% (one file pixel per screen pixel) at the point clicked; keys + / - zoom around the center, 0 fits, 1 is 100% (Ctrl allowed); the bottom bar shows the zoom ("Fit · 37%"). Face boxes, names and the flash follow zoom and pan. While zoomed in, the arrow keys pan (a tenth of the box) instead of stepping; Home / End still step, and showing another item (step, flip, open) or leaving the page goes back to fitted. In "Front & back" both sides zoom and pan together; double-click on the other side still shows it instead. Zoomed in past the decoded picture, the part in view (and a quarter around it) is drawn from the file at full resolution, asked for once the view settles (150 ms). Rotate left / right, and Accept / Keep for a rotation the pipeline proposes (`ops::accept` / `ops::keep`: by the engine if it runs here, else by the core; each with a notice offering Undo). Keys (not while a face is named or drawn): R rotates right, Shift+R or L left, Enter accepts a proposed rotation, Delete moves the item to the trash (Undo in the notice; the page then shows the next item of the view, the previous one after the last, and goes Back only when none is left), T opens the Add tag dialog, F flips to the other side, B and N toggle the face boxes and names, Space starts or stops a slideshow, Ctrl+C copies the file; all through the same actions as the buttons and the item menu (`DetailsBridge.item-action` → `actions::act`). Full screen (the corners button by the title, or F11): the window goes full screen with only the page, no sidebar, status bar (and log) or bottom bar; Esc leaves it first, as does leaving the page. Slideshow (the play button, or Space; 3 / 5 / 10 s in the box next to it): steps through the view, after the last item the first, with the usual prefetching ahead; any key or click stops it; works full screen. "Show in folder" opens the file manager on the item's folder with it selected (`desktop::show_in_folder`), next to "Open externally" (`desktop::open`). Info (bottom bar, or I; stays open across items for the session) shows a panel beside the picture: name, path (selectable, Copy path), date taken, modified, the file's own dimensions, size and kind (`ops::properties`), then the item's tags grouped by kind, People first (each once; detector tags from 50%, with who proposed them), each with an × that takes it off (`ops::remove_tag`; taking a person off un-names their faces on the picture, which are redrawn, and People re-reads), and an "Add tag…" box with the catalog's matching tags under it (as the Add tag dialog; Enter or a click adds, `ops::add_tag`; Esc gives the keys back to the page). It is read on a worker, again whenever `pipeline::changed` reports the item or its other side. A two-sided photo (catalog pair, else `-front` / `-back` names for files the catalog doesn't have) has a turned-down corner that flips to the other side, and a "Front & back" toggle that shows both, front on the left, each with its faces. The header, filmstrip, similar strip and bottom bar keep their own heights; the picture takes the rest of the page. Under the picture, "Similar in …": a row of 64 px tiles of up to 12 highly similar items among those of the view it was opened from (`Catalog::similar` on a worker; hidden if none); double-click opens one (Back returns), right-click gives its menu |
| `ui/menu.slint`, `src/actions.rs` | The item context menu on every picture (Review tiles, People faces, Acquire's recent strip, Details and its similar strip), for one item or a Review multi-selection (`ItemMenu.count`; `show_menu_for`): Rotate left / right (each in turn on a worker, one notice), Add tag… (dialog with matching tags from the catalog; `kind:` prefix or `People|…` pick the kind, `ops::add_tag`, for several items `ops::add_tag_all` in one transaction), Front & back: Pair as back of previous / next item (of the view it was right-clicked in, in its order: `ops::pair`, replacing either one's old pair), Swap front & back (`ops::swap_sides`), Unpair (`ops::unpair`); every file whose other side changed is reported, so Details' dog-ear and "Front & back" follow at once, Find similar (Review in Description mode with the pictures that look like it: `review::find_similar`), Copy (the files to the clipboard, `src/clipboard.rs`; "Copy 12 items" for a multi-selection), Show in folder (`archive_core::desktop::show_in_folder`: the file manager's `org.freedesktop.FileManager1.ShowItems` over D-Bus with `gdbus` / `dbus-send`, else `xdg-open` of the folder; errors in the notice), Properties (dialog of `ops::properties`), Delete (to the library trash, `ops::delete_all`: one notice for however many items, whose Undo restores all of them). Rotating several is `ops::rotate_all`: one change to undo. A window-level `ContextMenuArea` that pages show at the pointer through `nav::context_menu`. Each action runs on a worker and reports what it changed with `pipeline::changed`, which refreshes every view |
| `src/undo.rs` | Undo / redo for the whole window: Ctrl+Z, Ctrl+Shift+Z or Ctrl+Y (`ItemMenu.step`, from the window's key handler: only keys nothing else took, so a text box with focus keeps its own undo), and the notice's Undo button after every change (rotate, accept / keep, tag, untag, pair, faces named / confirmed / ignored / rejected / deleted / added, covers, tags and people renamed, merged or deleted, delete, restore). Each is `ops::undo` / `ops::redo` on a worker over the catalog's undo journal (`archive_core::catalog::undo`), shared with the shell and kept across restarts, so the latest change is undone whoever made it. Every file it touched is reported with `pipeline::changed` (a picture turned without an engine with its pixels), the counts are redone (Trash and Failures follow), People re-reads; the notice says "Undone: Tag 12 items Holidays" with Redo (after a redo, Undo). A change that can't be undone any more (e.g. a file is back where a trashed one was) is dropped from the journal and the notice says so. Deleting from the trash for good can't be undone |
| `ui/menu.slint` (`TagEditDialog`), `src/tags.rs` | Managing a tag from Review's tag tree, or a person from People's list (`tags::edit`; the dialog then speaks of people, counts their faces and pictures on its worker, and Merge offers only people): Rename… (the path to edit, prefilled; a person's name alone keeps their place under People; onto a taken path the dialog says so and offers Merge, `ops::rename_tag`), Merge into… (with the catalog's matching tags, but itself and those under it: `ops::merge_tags`), Delete… (confirms with how many items it is on, and that faces named as a person become unknown: `ops::delete_tag`; undoable like the rest). The tags under it go along. A tag that comes from folder names or another program's sidecar says so. On a worker; every file whose tags changed is reported with `pipeline::changed` (the grid re-reads them, so the tree and Details follow), People re-reads when a person changed, a tag filter on the tag follows it (`review::tag_moved`), and a notice says what was done |
| `ui/nav.slint`, `src/nav.rs` | Navigation: `nav::open(Target)` for every double-click, `nav::go` for the sidebar (People, Trash and Failures re-read the catalog when shown); both push a snapshot of the view left (Review: filters, selection, scroll anchor; People: person, selected faces, scroll anchor; Details: its item) that `nav::back` restores. A `Target` carries the origin view's `Scope` (its filtered items, listed once on first use and shared by its clones), which Details steps through and the similar strip draws from; `nav::context_menu` is where every page's right-click goes (`nav::context_menu_for` with a multi-selection's items) |
| `ui/pipeline.slint` | `PipelineStatus`: the pipeline's status line with Pause / Resume (in the window's status bar); its "3 rotations to review" is a link that shows Review with just those items (`review::show_attention`: search, tags and type cleared, the sort kept; Back returns from another page), "2 failed" opens the Failures page (Review's "Failed" chip still filters the grid by them) |
| `ui/sidebar.slint` | Acquire, Review, People, Details at the top; at the bottom Failures (only while something failed, or while it is shown; a red count) and Trash (with how many items are in it). Both counts come from the pipeline relay's counts (`Catalog::attention_counts`, `Catalog::trash_totals`), taken with or without an engine in this window, so they follow deletes, restores, failures and retries live |
| `ui/trash.slint`, `src/trash.rs` | The library's trash, newest first: a grid of thumbnails (small tier from the files where they lie in the trash, via `thumbs::small_for_files` on a worker, for the whole list) with the name, the folder it was in, when it was deleted and its size; "N items · 3.4 GB" by the title. Click, Shift-click (range), Ctrl-click, Ctrl+A, Esc to select; Restore puts the selection back where it was (`ops::restore_all`, a front with its back; Enter too), reported with `pipeline::changed` so it is back in Review; Delete permanently (also the Delete key) and Empty trash… (everything, or with "Only items deleted more than N days ago") ask first in a dialog that says how many items and how much go for good, which can't be undone (`ops::purge`, which takes a front's trashed back along, and `ops::empty_trash`; changes in the undo journal that touched what went are dropped). Read on a worker when the page is shown and again when the trash's totals move while it is (a delete elsewhere, an Undo); every change on a worker, with a notice |
| `ui/failures.slint`, `src/failures.rs` | Every stage that failed on the current file of an item (`Catalog::failures`), grouped by stage under a heading with its count and "Retry all", each row with the file, its folder, the error and when it failed. Click / Shift / Ctrl, Ctrl+A, Esc select; "Retry N" runs those stages again (`ops::retry`: through the engine when it runs in this window, which starts on them at once; else in the catalog, for the pipeline to pick up when it next looks; the files are reported with `pipeline::changed` so Review's badges follow). Double-click, Enter or Open shows the item on Details (stepping through the failures; Back returns here), Show in folder, right-click for the item menu. Read on a worker when shown and on every recount of the relay while shown, keeping the selection; an unchanged list isn't redrawn |
| `ui/theme.slint`, `ui/types.slint` | Palette and the structs shared with Rust |
| `ui/help.slint`, `src/shortcuts.rs` | The keyboard shortcut list: ? (not while typing in a text box) or F1 anywhere opens an overlay with the shortcuts of the page on screen, then those that work everywhere (undo / redo, dialogs); Esc, ? / F1 again or a click outside closes it, and it takes every key while open (the window's capture handler in `app.slint`). The list is written down once, in `src/shortcuts.rs`: **a shortcut added or changed in a page's key handler must be added or changed there too** |
| `src/shot.rs` | `--shot`: the app rendered offscreen into a PNG (a `Platform` with a `MinimalSoftwareWindow`, an event loop running timers and other threads' events until `--wait` is over); see Screenshots |
| `src/settings.rs` | The window and its views kept across runs, in the per-user settings file (`archive_core::settings`: `$XDG_CONFIG_HOME/archive-studio/settings.json`, versioned; an unreadable file is moved aside to `settings.json.bad` and defaults used): the window's size and whether it is maximized (not full screen), the page shown last (Details comes back as Review), Review's tile size, sort, media kind and search mode, Details' info panel, face boxes, names and slideshow interval, People's sort; per library (keyed by its root) the tag tree's collapsed nodes. Not selections, search text, scroll positions or the tag / "Needs attention" filters. Read at startup; polled every second and saved on a writer thread once a change has held for a second, and on close |
| `src/clipboard.rs` | Copy (Ctrl+C on Review's grid and Details, Copy in the item menu, for a multi-selection too): the files on the clipboard as `text/uri-list`, `x-special/gnome-copied-files` and their paths as text (`desktop::CopiedFiles`), through Wayland's data-control protocol (`wl-clipboard-rs`), else `arboard` with the URI list alone (X11): paste them in a file manager, a mail or chat window, or as paths in a text field. Dragging items out of the window isn't possible: Slint 1.18 (on winit 0.30) has no drag source the desktop sees, only drags within the window, so copy and paste stands in for it |
| `src/acquire.rs` | Hosts the device backends from `archive-core` (`../core`): tiles, coalesced preview delivery, thumbnail seeding |
| `src/people.rs` | Reads people and faces from the catalog on a worker (with each person's cover crop, decoded small there and cached by face); decodes crops for the rows on screen. Named / ignored faces are masked in place (no reflow until the grid is reloaded) while a writer thread saves them (`ops::name_faces`, which tags the pictures `People|<Name>`; `ignore_faces`, `unignore_faces`, `reject_person`, `clear_rejections`), reports the pictures changed (so Review shows the tag) and refreshes the suggestions. Faces changed on Details mark the grid stale, so Back re-reads it |
| `src/details.rs` | Decodes the opened item off the UI thread (up to 4096 px) and its neighbours in the scope ahead (prefetch thread), fills the filmstrip; face hit-testing (`face-at`), the face popup and menu, and face changes on a worker (`FaceChange`); zoomed in past 4096 px, decodes the file at full resolution on a worker (`thumbs::decode_full`, at most 100 MP, one file at a time, dropped when zoomed out or on another item) and sends the part in view, scaled to no more than it is shown, as a tile drawn over the picture; the info panel (`ops::properties`, tag changes on a worker) and `data_changed` (from the relay: re-reads the panel and the other side); "Open externally", "Show in folder", Delete and T go to `actions::act` for the shown item; after the shown item is trashed, the next one of the scope (`files_gone`); full screen and the slideshow timer are the page's own; asks the engine to rotate / accept / keep on a worker, reloads when the file is rotated |
| `src/pipeline.rs` | Hosts the pipeline `Engine` while the window is open: relay thread (coalesced reports → review-grid updates, status counts), capture priority, status line |
| `src/semantic.rs` | Review's search by description on a worker: the text model is the engine's when it runs in this window, else loaded once (`ops::text_search`); requests queued behind a newer one are skipped; each result carries its generation and the catalog's `describe::coverage` |
| `src/review.rs` | Background indexer (each entry's catalog flags and date taken read per batch with `Catalog::flags_of` / `taken_of`), `GridModel` (rows sliced on demand, laid out by `Layout` with month headers when sorted by date, incremental merges while indexing and of pipeline updates; the selection as a set of entry ids, focus and anchor, the grid's keys; a search by description's ranking, path hash -> rank, is part of the query, so it filters, sorts by relevance and merges like any other, and is kept for its description so the other filters don't search again; stale search results are dropped by generation), filter thread, batched thumbnail delivery |
| `src/thumbs/` | Viewport-ranked scheduler (`sched.rs`) → CPU workers (cache lookups, decodes) + I/O threads (source reads) → writer thread (`writer.rs`) → byte-budgeted LRU of `slint::Image` |

## Pipeline

While the window is open it hosts the library's processing pipeline (`archive_core::pipeline`,
see `../ARCHITECTURE.md`): the `Engine` is started for the root on launch (on a thread, with
`Config::default()`) and dropped after the window closes. One engine per library: if another
window or the `archive` shell already runs it, this one runs without and its status line says
"Pipeline running in another window"; rotating then goes to the core directly.

- **Reports**: the engine's sink only forwards events to a relay thread, which takes each burst
  in one go (~100 ms; thousands of `Updated` during a catch-up become one batch, one entry per
  file), reads the files they name (stat, catalog tags, and the burst's flags in one
  `Catalog::flags_of`: a stage's `Review` and `Failed` reports count, so badges and the
  "Needs attention" filters follow), counts what is left (`pipeline::status`,
  `Catalog::attention_counts` and `Catalog::trash_totals`: at most twice a second while busy,
  and once on idle; the last two also without an engine here, for the sidebar) and posts
  one event-loop callback per burst. Warnings and failures go to stderr and show in the status
  line for a few seconds.
- **Captures**: each `DeviceEvent::Saved` calls `engine.add(path)` (after the thumbnail is
  seeded), so a new scan is processed at once instead of after the watcher's settle time, and
  appears in Review even without an engine here. While any device is acquiring, the engine
  runs in `Mode::Background` (one worker) so a real-time DV capture isn't starved; the status
  line's Pause holds it entirely.
- **Live refresh**: new files are merged into the Review grid in sort order; a changed file
  (e.g. rotated) gets a new entry, so it is re-sorted and its thumbnail is made afresh (the
  old entry is retired and its in-memory thumbnails dropped; the persistent cache layers are
  keyed by size + mtime, so they miss by themselves); a deleted file leaves the grid (and the
  selection). The selection follows a replaced file. Reports that come in while the indexer is still walking
  are applied when it finishes. The Details page reloads a file that was rotated; People
  re-reads the catalog after the pipeline groups faces, if it is on screen.
- **Changes made here**: the context menu's rotate / delete / tag, the tag tree's rename / merge / delete, restoring from the Trash page, retrying failures and naming faces in People
  report the files they touched with `pipeline::changed(path, pixels)`, which goes through the
  same relay: the grid re-reads their tags (the tag tree's counts follow; tags the catalog wrote
  to a sidecar itself aren't read back from it, since they may be stale until the next sync),
  a trashed file leaves the grid and Acquire's strip (and Details shows the next item of its view if it showed it, or goes back),
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
  position holds (`row_changed` is a no-op for rows the page hasn't instantiated).
  Batches of ≥ 20k entries (fast disks) go to the filter thread instead.
- Re-filtering (search, kind, sort; debounced 120 ms) of libraries ≥ 20k entries runs on a
  filter thread over a snapshot of the chunk list; the UI thread just swaps in the result.
  Each request has a generation, so results for superseded queries are dropped (and queued
  requests skipped), and entries indexed meanwhile are merged in when it lands.
- Thumbnail results go to a mailbox; only the push that finds it idle schedules an
  event-loop callback, which applies everything queued (≤ 4 ms per turn, the rest on the
  next turn) and notifies each affected row once.
- Rows come from a `Layout` of the shown order: `columns` tiles a row, and sorted by Date a
  header row ("October 2026 · 42 items") before each month's items, which start a row of
  their own. Groups are found by binary search on the date sort keys (months × log n), so the
  layout is rebuilt after every merge or removal; row ↔ slot lookups and a row's position
  are binary searches over the groups. Selection ranges, the keys (Up / Down keep the column
  across headers), the rubber band, thumbnail scheduling, Back's scroll anchor and the rows
  re-read after a change all go through it.
- The page places the rows itself in a `ScrollView` (a `ListView` places rows by their
  average height, and headers are lower than tile rows): `review.slint` asks Rust where a row
  starts and which row is at a height (`row-y`, `row-at-y`, with the layout's serial so
  bindings follow it), and is handed only the rows in view plus two either side
  (`WindowRows`, moved with the viewport by rows added and removed at its edges);
  `GridModel::row_data` slices the ids of a row, so only those rows are ever materialized.
  Column count is derived from the view width in Slint.
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

Stubs: video/audio thumbnails, the in-app viewer, real device backends, persistent index.
