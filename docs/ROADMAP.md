# Roadmap

Fourteen milestones take Koma-1 from an empty repository to a signed 1.0, all in Rust.
The library is readable in a browser from M1. From M6 on, Koma-1 also organises the
library (moving, trash, editing details, renaming to the naming standard, uploads, AVIF),
always with a preview and an undo.

Much of that behaviour already works and is tested in an earlier reader written in Python.
Its documented behaviour and tests are the spec: Koma-1 rebuilds them in Rust, with
made-up names, and ports no code. Those items are marked *(proven)*.

## How the plan is shaped

- **Walking skeleton first.** M1 runs the whole path (scan, index, serve, read) end to end,
  so every later milestone improves something that already works.
- **Budgets from day one.** The checks land in M0 and every milestone after must stay green.
- **Design before build, one screen at a time.** Each UI milestone opens with a design step
  in the chosen direction (see [design/README.md](design/README.md)).
- **Reading before writing.** Every organising feature needs the library UI to show what it
  is about to change.

```
Read it      M0 Foundations → M1 Skeleton → M2 Ingest, metadata → M3 Library UI → M4 Reader
Organise it  M5 Accounts → M6 Jobs, trash → M7 Naming → M8 Uploads    (all through the undo journal)
Grow it      M9 AVIF, storage → M10 Photo sets, video
Reach        M11 Formats → M12 Sync, offline → M13 Ship 1.0
```

M11 and M12 only need ingest, so either can move earlier.

---

## Read it

### M0 · Foundations

Goal: an empty but real Rust project that already enforces the rules.

- Cargo workspace, one `koma` binary with `serve`, `worker`, `scrub` subcommands (stubs).
- CI: fmt, clippy, tests, `cargo-deny`; static musl build, `FROM scratch` image, non-root.
- Budget checks wired in (numbers: see [Open questions](#open-questions)).

Exit: `koma serve` answers a health check from a container inside the image budget, and a
PR that breaks a budget fails CI.

### M1 · Walking skeleton

Goal: read a CBZ from your own library in a browser.

- Scan a folder of CBZ files, index each zip central directory to `(file, offset, length)` per page.
- SQLite schema (WAL), natural sort of pages, BLAKE3 page hashes, `sendfile()` byte ranges.
- Plain server-rendered pages: library list, series, and a bare reader.

Exit: a test library of made-up CBZ files reads front to back.

### M2 · Ingest and metadata

Goal: ingest does all the work once, and knows what each file is.

- `koma worker` with a job queue; page variants in WebP and JPEG (AVIF variants come in M9);
  page sizes stored without reading whole pages *(proven)*.
- Incremental scan: only folders whose modification time changed are re-read, and a restart
  re-reads nothing *(proven)*.
- Stable ids: a book and a series keep their id, progress, edits and date added through
  moves and renames, even ones made outside Koma-1 *(proven)*.
- Metadata: `metadata.json` wins over `ComicInfo.xml` where both exist, tags are combined,
  then the file name fills the gaps *(proven)*. A parser interface so further parsers drop in.
- Naming baseline as a written spec with a golden test for every example: the three manga
  patterns and the cosplay pattern *(proven)*.
- Formats: CBZ, ZIP, CB7, and photo folders read as one chapter, `cover.*` first *(proven)*.
- Archive limits and the first fuzz targets.

Exit: killing the worker mid-job and restarting it finishes cleanly, and every
naming-baseline example passes.

### M3 · Library UI

Goal: the library from `docs/design/` on real data.

- Design step: table view, search, collections, library settings, empty and error states.
- Libraries: every top-level folder is one, found by itself at the next scan *(proven)*, with
  a name, type (comics, manga, cosplay), reading direction and colour taken from the folder.
  The UI shows them read-only; editing them comes with accounts (M5).
- Collections: folders that group series, shown as stacked-cover cards *(proven)*.
- Side panel, edge-strip progress, continue hero, Continue / Unread / Finished views, real
  `<table>` view.
- Infinite scroll with windowing as the main view; plain `?page=` pages without JavaScript.
- Genres and tags as chips; search matches tags.
- Download the original file of any volume.

Exit: 10,000 made-up series scroll smoothly with flat memory; every chunk is under 15 KB
and works without JavaScript.

### M4 · Reader

Goal: page turns that feel instant, and the tools that make long reading comfortable.

- Design step: the reader.
- `<picture>` with `srcset`, adaptive prefetch, `img.decode()` ahead.
- Single, spread-aware double, and webtoon vertical scroll chosen automatically for tall
  strips *(proven)*. No panel-by-panel view and no decorative effects.
- Zoom with pan, a thumbnail strip, a scrubber with page preview, and a "read next" card at
  the end *(proven)*.
- Reader settings per series (direction, mode, fit) stored per account, so they follow you
  between devices *(proven)*. Until accounts (M5) they and the reading position stay in the
  browser.
- Keys, tap, swipe; progress via `sendBeacon`.
- EPUB book reader: reflowable text, adjustable size, width and spacing, table of contents,
  saved position *(proven)*.

Exit: on a throttled connection the next page is already decoded when you turn; an EPUB
opens to the last position on another device.

---

## Organise it

### M5 · Accounts

Goal: know who is asking before anything can change the library.

- Design step: sign-in, account settings, user management.
- Signed session cookies, passkeys, per-user progress; trusted forwarded-user header for SSO.
- The first account is the admin; the admin adds and removes accounts and can hand the role
  over *(proven)*.
- Each account owns a folder and its libraries are private to it *(proven)*.
- Roles: reader (read, download) and curator (organise, upload, convert).
- Library settings editable by the admin (name, type, reading direction, colour): `koma serve`
  queues the change and the worker applies it, so the worker stays the catalogue's only writer.

Exit: a reader gets a refusal for every write action; two accounts never see each other's
libraries.

### M6 · Jobs, organising and trash

Goal: every change to the library is safe, visible and undoable.

- One job system: live progress over one server-sent events stream, pause, resume and
  cancel, jobs that continue after a restart; quick changes stay instant *(proven)*.
- The write path every later milestone reuses: plan, dry run, apply, undo, with an
  append-only journal.
- Move chapters and series between series, collections and libraries; merge series;
  multi-select with mark read and delete; an Unsorted series for chapters with a number but
  no series name *(proven)*.
- Trash for deletes: gone from the library at once with Undo; a trash page with size, days
  left and where it was; restore that recreates missing folders and renames on a clash; keep
  time of 7, 14, 30 or 90 days or until emptied *(proven)*.
- Edit details: author, publisher, genres, tags, summary, year, direction and cover, saved in
  SQLite at once and written into the files as `metadata.json` only on request *(proven)*.

Exit: moving and renaming a series keeps its id, progress and edits; a deleted series comes
back with its progress after its folder was removed.

### M7 · Naming standard

Goal: straighten up file names without ever losing a file.

- Naming templates for the manga and cosplay patterns, for example
  `(Event) [Moonforge (Mirelle)] Starfall - Ch. 03 [EN].cbz`.
- A rename plan through the M6 journal, with a preview, conflicts and undo.
- A background tidy that keeps a cosplay library in Cosplayer / Set / variant form *(proven)*.
- A fix-up queue for files too ambiguous to rename automatically.

Exit: renaming a made-up library of 500 messy files and undoing it gives back
byte-identical paths and contents.

### M8 · Uploads

Goal: drop anything in, and it lands already tidy.

- Drag and drop of files and whole folders; the server plans what each drop becomes and
  shows it as a review sheet first *(proven)*.
- Photo folders packed on the server, written straight into the archive as files arrive;
  CBR and CB7 converted to CBZ with stored pages *(proven)*.
- Duplicates skipped; resumable uploads for large archives.
- An upload log per month: successes counted, every failure written out with its reason *(proven)*.
- The same hostile-input checks as ingest, then the naming standard.

Exit: a golden set of drops with made-up names gives exactly the expected plans; a 2 GB
upload survives a dropped connection.

---

## Grow it

### M9 · Optimize and storage

Goal: save disk space when you decide to, and see where it went.

- Opt-in per series, collection, selection or library; an estimate per preset from sample
  pages and a before-and-after page compare *(proven)*.
- Presets: Default (AVIF, quality 70, speed 6, full colour, grayscale for black-and-white
  pages), Lossless (lossless WebP for PNG, BMP and TIFF books), and custom ones *(proven)*.
- AVIF page variants next to the WebP and JPEG ones, with the same encoder.
- Variant widths as a library setting (today 720, 1080 and 1440): fewer widths for less disk.
- JPEG variants: measure jpegli (through its own small crate, `koma-jpegli`, built like
  `koma-webp`) against `jpeg-encoder` on made-up pages: size at equal visual quality, encode
  time, image size, and whether it still needs the IJG notice. Switch only if it wins, by
  raising `variants::VERSION`.
- A page that would not get smaller is left alone; each book is written to a temporary file,
  every page checked, then swapped in keeping its id and progress *(proven)*.
- Originals kept for a set number of days with Undo; new uploads can be converted
  automatically; jobs can wait for a night window with Run now *(proven)*.
- A storage page: size per library, space saved, the biggest books not converted yet, kept
  originals and trash *(proven)*.

Exit: converting and undoing a volume restores the original bit for bit, with progress intact.

### M10 · Photo sets and video

Goal: cosplay libraries as first-class citizens.

- The cosplay library type: `[Photographer] Cosplayer - Set - Variant`, sets kept as plain
  `.zip` *(proven)*.
- Videos in a set or inside an archive (stored uncompressed, streamed with byte ranges),
  played between photos with the reader's bar as player controls *(proven)*.
- Video thumbnails taken from a frame *(proven; needs ffmpeg, see open questions)*.

Exit: a made-up set with photos and a video plays through and continues into the next set.

---

## Reach

### M11 · Hostile formats and integrity

Goal: every remaining format, without letting a bad file hurt the server.

- CBR, PDF and fixed-layout EPUB decoded in a sandboxed subprocess, extracted once at ingest.
- HEIC and JPEG XL books converted once, with an offer to optimise them to AVIF *(proven)*.
- Continuous fuzzing of every parser; `koma scrub` re-hashes blobs, removes variants of pages no
  longer in the library, and checks the journal against the disk.

Exit: a corpus of malformed archives crashes no more than its own job.

### M12 · Sync and offline

Goal: read without the server, in the browser now and in an Android app later.

- A change feed that only sends what changed since the client last asked *(proven)*.
- Download API: whole volumes, resumable with byte ranges, with a checksum per file;
  progress sync both ways.
- OPDS 1.2 and 2.0; the JSON API documented as the contract for the Android app.
- Offline chapters in the browser with a service worker and a size limit.

Exit: a client downloads a series, reads it in airplane mode, and its progress lands on the
server when it reconnects.

### M13 · Ship 1.0

Goal: the requirements table holds, and the release is trustworthy.

- Metrics, traces, JSON logs, SLO burn-rate alerts; k6 load test on every PR.
- Small-hardware test without owning a Raspberry Pi:
  - every PR: an ARM64 build in a container capped to 2 CPUs and 1 GB of memory, with 50
    simulated readers (GitHub's ARM runners are free only for public repositories);
  - before each release: the same test for an hour on a rented ARM cloud server;
  - emulated ARM (QEMU) only checks that the build runs.
- Browser tests with real clicks against a real server.
- Chaos tests: kill workers mid-rename, fill the disk during an upload, corrupt a blob.
- Compose file, Nix reproducible build, SBOM, signed images.

Exit: every row of the requirements table passes, and a signed 1.0 image is published.

---

## After 1.0

The Android app is its own project, built on the M12 sync API. The scaling path
(S3-compatible blobs, LiteFS replicas, a CDN) waits until one node is proven. Also parked:
lossless JPEG XL archival and a dedicated phone layout.

## Open questions

- **Budgets.** Proposed new numbers, not yet in the manifesto. Proposed rule: a budget
  protects speed, it does not block a feature; a PR may raise one if it says why, and the
  reader-facing speed targets never move.

  | Budget | Now | Proposed | Why |
  |---|---|---|---|
  | CSS | 5 KB | 16 KB (10 until M3, 12 until M4, 14 until the EPUB reader) | the library UI's design CSS is 9.6 KB before its real pages; both readers add theirs; curation and upload screens add more |
  | Reader JS | 10 KB | 16 KB | offline and sync logic |
  | EPUB reader JS | none | 16 KB, loaded only for EPUBs | reflowable book reader |
  | Curator pages JS | none | 24 KB, loaded only there | upload progress, rename preview |
  | Container image | 20 MB | 40 MB | AVIF encoder and vendored codecs (libwebp) |
  | Idle RAM (serve) | 25 MB | 40 MB | metadata and sync state |
  | Idle RAM (worker) | none | 64 MB | rename plans and upload buffers |

- **CSS per area.** One CSS budget covers the library and both readers, though
  each page loads only its own stylesheets. Split it so each area has its own
  number (library, page reader, EPUB reader, curation), as the scripts are.
- **Vertical writing.** EPUB books read horizontally for now; Japanese books
  written vertically (`writing-mode: vertical-rl`) want their own reading.
- **ffmpeg.** Video thumbnails need ffmpeg, which alone is bigger than the image budget.
  Options: a separate optional media image, thumbnails only when ffmpeg is present, or no
  video thumbnails.
- **AVIF originals.** Conversion is lossy. Is keeping originals for a set number of days enough?
