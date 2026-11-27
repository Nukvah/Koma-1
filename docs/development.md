# Development

## Layout

| Path | What |
|---|---|
| `crates/koma` | The `koma` binary and its library: CLI, server, archives, catalogue, scan |
| `crates/koma-fixtures` | Made-up test material: PNG pages, zip and 7z writers, a test library |
| `crates/koma-webp` | Lossy WebP encoding: one safe function over libwebp, the only crate allowed `unsafe` |
| `fuzz` | Fuzz targets for every parser of hostile input (nightly, own lock file) |
| `xtask` | Project tasks: `cargo xtask budgets`, `cargo xtask fixtures` |
| `budgets.toml` | Every performance and size budget |

## Checks

These are what CI runs:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo deny check                        # licences, advisories, sources
cargo xtask budgets --assets-only       # asset sizes, no Docker needed
(cd fuzz && cargo +nightly fuzz run zip_index -- -max_total_time=60)   # and each target
docker build -t koma:dev . && cargo xtask budgets --image koma:dev
```

## Budgets

[budgets.toml](../budgets.toml) holds every budget: image size, idle memory, time
to first byte, cold start, CSS and JavaScript sizes, page size. CI builds the
image on ARM64 and x86_64 and runs `cargo xtask budgets` in a container capped
at 2 CPUs and 1 GB; any budget exceeded fails the pull request. A budget may be
raised only with a `Budget change:` line in the pull request saying why.

The image checks run on real data. `koma worker` scans the made-up test library
(below) into a data folder, then `koma serve` reads the same library and data
folder. Time to first byte is timed on a reader page and on a page image found
by following links from `/`, and the `[[pages]]` sizes are those of the pages
it serves. The containers run as the host's user so the runner can clean up
what they write.

## Test libraries

```sh
cargo xtask fixtures /tmp/koma-library
```

writes a small library of made-up series as CBZ files. It includes the awkward
cases real libraries have: unpadded and full-width page numbers, pages written
out of order, deflated pages, Zip64, pages in a sub-folder, a double-page
spread, code page 437 names, `ComicInfo.xml` and macOS junk files. Tests use
the same library through the `koma-fixtures` crate, together with the reading
order each book must have.

Never use real titles, authors or files in tests, even when a bug was found
on one: rebuild the case with made-up names.

## Archives

`koma::archive::zip` reads a CBZ's index: for every page, where its bytes are
in the file. It reads the central directory, checks each local header, and
decompresses nothing. It rejects archives whose offsets point outside the file
or into the central directory, whose members overlap, that have data before
the archive or after its end record, that span several disks, or that exceed
the entry, name and directory size limits. Zip64 archives, code page 437 names
and the Info-ZIP Unicode path field are supported.

`koma::archive::sevenz` reads CB7 (and `.7z`) books with `sevenz-rust2`, pure
Rust, LZMA and LZMA2 only. 7z is usually solid, so a book is read once, front
to back, at ingest, and every page goes to the blob store. Before anything is
decoded it checks the member count (50,000), the decoder memory an archive asks
for (128 MiB; LZMA dictionaries can ask for gigabytes), the total declared size
(8 GiB) and its ratio to the file's size (1000:1). Each member is cut at its
own limit while decoding (256 MiB a page, 1 MiB a metadata file), and the
decoder checks every member's size and CRC. Encrypted archives are rejected.

Pages are image members (`jpg`, `jpeg`, `png`, `webp`, `gif`, `avif`, `bmp`,
`jxl`), skipping directories, encrypted members, hidden files and `__MACOSX`.
They are put in reading order with `koma::natsort`: numbers by value
(`2 < 10`, full-width digits too), letters case-insensitively, folder by
folder, with the extension only breaking ties.

## Fuzzing

Every parser of hostile input has a fuzz target in `fuzz/` (cargo-fuzz,
nightly): `zip_index`, `sevenz`, `page_size`, `metadata`, `names` and
`variants` (decoding, resizing and encoding a page). CI runs
each for 60 seconds on every pull request, seeded with the made-up library,
and keeps any crashing input as a build artifact. To run one locally:

```sh
cargo install cargo-fuzz
cd fuzz && cargo +nightly fuzz run sevenz -- -max_total_time=60
```

A crash is saved under `fuzz/artifacts/<target>/`; `cargo +nightly fuzz run
<target> <file>` replays it. Fix the bug and add the case as a regular test.

## Page sizes

`koma::archive::size` reads every page's width and height from its header at
ingest, without decoding it: JPEG (frame header, with the EXIF orientation:
5 to 8 swap width and height), PNG, GIF, WebP (lossy, lossless, extended),
BMP, AVIF (the largest `ispe`, turned by `irot`) and JPEG XL (codestream or
container). Sizes are stored as displayed (`pages.width`, `pages.height`), so
the reader can lay pages out before they load. A header that can't be read
gives no size (NULL); the reader falls back to a default shape. A page wider
than 1.2 times its height is a spread (`Size::is_spread`); one at least three
times as tall as wide is a tall strip, as in webtoons (`Size::is_tall_strip`).

## Page variants

The worker makes smaller copies of every page once, after indexing, so that
serving never resizes or transcodes (`koma::variants`):

- Widths 720, 1080 and 1440, only those narrower than the page: the original
  is always served as it is, and a page no wider than 720 has none. Each width
  in JPEG (quality 82, through `jpeg-encoder`, pure Rust) and WebP (quality 80,
  through `koma-webp`: libwebp 1.6 built from its vendored C source). AVIF
  variants come in M9. WebP stops at 16,383 pixels, so a very long strip's
  720 variant may be JPEG only. A variant that comes out no smaller than the
  original is dropped: clean line art saved as PNG is often smaller than any
  lossy copy of it, and the original serves instead.
- On made-up scan-like pages (1600 x 2400, JPEG at quality 90, 1.7 MB each)
  the WebP variants were 7% (720), 23% (1080) and 51% (1440) of the page's
  size, and all six variants together about 1.7 times the originals' space.
  On the 2-CPU, 1 GB budget container: about 0.9 s a page, at most 35 MiB of
  memory, back to 3.4 MiB idle.
- Pages are decoded in pure Rust (`image`: JPEG, PNG, WebP, GIF's first frame,
  BMP) and resized with Lanczos3 (`fast_image_resize`). AVIF and JPEG XL pages
  get no variants yet; they are served as they are.
- EXIF orientation is applied, so variants have none; transparency is
  flattened onto white. A page whose every pixel has its channels within 8 of
  each other is neutral gray and is encoded as 8-bit gray; a tinted page
  (sepia, yellowed paper) keeps its colour.
- Limits before decoding: 64 million pixels and 512 MiB for the decoder.
  A page over them, or one that can't be decoded, is logged and kept
  without variants (`variant_pages.error`).
- Variants belong to page content, not to books: `variants` and
  `variant_pages` are keyed by the page's hash, so a page in two books, or a
  book moved, is not redone. Their bytes are blobs next to the page,
  `<data>/blobs/ab/<hash>.<width>.v<version>.<format>`. Variants of a page
  that leaves the library stay on disk until `koma scrub` removes them.
- Once the job queue is empty, every present book with a page lacking
  variants gets a `variants` job. Index jobs always go first, so a new book is
  readable before its variants exist. A job makes one page at a time and
  records each before the next, so a stop (checked between pages; the job
  goes back in the queue without counting the attempt) or a kill redoes at
  most one page. If the worker dies on the same page three times, that page
  alone is given up. The test `tests/variants.rs` kills `koma worker` with
  SIGKILL mid-book and checks that a restart finishes, redoing nothing else.
- `variants::VERSION` names the settings (widths, formats, qualities) and is
  part of every variant's file name and URL. Raising it makes the worker redo
  every page under the new settings, at new URLs: copies already cached for
  a year under the old URLs are never wrong, only old.
- One page at a time on one thread: on the 2-CPU target the other CPU stays
  free for `koma serve`.

## Catalogue and scan

`koma worker` (at start, then every `--rescan` minutes) and `koma scan` index the library into
`<data>/koma.db`, a SQLite database in WAL mode (`koma::catalog`); `koma serve`
only reads it.

- A series is a folder, and each CBZ, ZIP or CB7 in it is a book. Books loose in
  the library root group by the series their file name names (see
  [naming.md](naming.md)). Hidden files and symbolic links are skipped.
- Each book's details (title, number, chapter or volume, authors, genres,
  tags…) come from `metadata.json` over `ComicInfo.xml`, with gaps filled
  from the file name (`koma::meta`, `koma::names`; rules and every example in
  [naming.md](naming.md), checked by `tests/naming_baseline.rs`). Authors,
  publishers, genres and tags are in `book_terms`.
- A photo folder is a folder that holds page images and nothing else that is
  read (no archives, no subfolders): it is one book, named as the folder, in
  the series of the folder above it. `cover.*` is its first page, then the
  images in natural order. Each image is served in place from its own file
  (`pages.file`). Its stamp is the images' total size and the latest time of
  the folder and its images. A series folder with a `cover.jpg` next to its
  volumes is not a photo folder.
- Every page gets a BLAKE3 hash and has its CRC-32 and size checked against
  the archive.
- Stored pages are recorded in place as (book file, offset, length), so they
  can be served straight from the archive.
- Deflated pages are inflated once, at scan time, into the blob store
  `<data>/blobs/ab/<hash>.<ext>`; identical pages share one blob. Inflating
  stops at the declared size, at 256 MiB, or if the declared ratio is over
  1000:1.
- The scan is incremental. A folder whose modification time is unchanged
  since it was last read in full is not listed again, only its known
  subfolders are visited (`folders` table). A folder modified less than two
  seconds before it is read is read again next time, for file systems with
  coarse times. A `Check::Files` scan (worker start, once a day, `koma scan`)
  also stats every known book, to catch files overwritten in place.
- A book whose size and modification time haven't changed is skipped; books
  whose files are gone are removed, and empty series with them.
- Stable ids. A book keeps its id and date added (`books.added`) when it is
  rewritten in place, moved or renamed, even outside Koma-1. A book whose
  file vanishes is marked gone (`books.gone`), hidden everywhere, and kept
  for 30 days. A book found at a new path with the size and modification
  time of a gone one is that book: it is relinked without being read again
  (same file name first; otherwise only a single candidate counts), and so
  is a book back at its own path. A series whose last present book moves
  goes with it and keeps its id, so a series folder renamed outside Koma-1
  keeps its id; a book split off into a new folder gets a new series. Ids
  are never reused (`AUTOINCREMENT`).
- The scan only queues an index job per changed book (`jobs` table);
  `scan::run_jobs` works through the queue. Each job is one transaction, so
  a killed worker leaves the catalogue as it was. On start, jobs left running
  are queued again; a job claimed three times without finishing is given up
  and recorded as an error. `tests/kill.rs` kills `koma worker` with SIGKILL
  partway through indexing and checks that a restart reads only what was not
  stored yet and gets every page back byte for byte. A lock file (`<data>/worker.lock`) keeps a second
  worker or scan out.
- A file or folder that can't be read is logged, stored in `scan_errors`
  with the reason (and the file's size and time), and skipped; it never stops
  the scan, and is retried only once it changes.
- The catalogue's schema version is in `PRAGMA user_version`. From 7 on, an
  older version is upgraded in place, one step at a time, since remaking page
  variants is costly; before 7 it is dropped and rebuilt by the next scan.

Order is natural everywhere: SQL uses the `natsort` collation, which is
`koma::natsort::compare_paths`.

## Libraries

`koma::libraries`. A library is not stored: it is the first folder of a
series' path, kept as a generated column (`series.library`, indexed). A
series whose path has one part (a folder in the root, a root photo folder, or
books loose in the root grouped by name) is in the default library, `''`,
"Library". So a top-level folder holding books directly is a series, one
holding series folders is a library, and no series changes group or id when a
folder gains a subfolder. A collection is the folder between the library and
the series (`manga/Starfall Saga/Starfall`), found from the path
(`libraries::collection_of`).

Settings come from the folder name (`Library::from_folder`): name
capitalised; type comics if it contains "comic", cosplay if "cosplay", else
manga; direction right to left for manga, else left to right; colour from a
fixed list in library order (the default first, then natural order). They are
read-only until accounts (M5).

Books loose directly in a library folder (`manga/Ch. 12.cbz`) are a series
named after that folder, in the default library: put them in a series folder.

Schema 7 is upgraded in place to 8 (one generated column and its index), so
the catalogue and its page variants are kept; older versions are rebuilt by
the next scan, as before.

## Search

`koma::search` and the `search` table: SQLite FTS5 with the trigram
tokenizer, so any part of a word matches, in any case, and Japanese and
Korean titles need no word splitter. One row per present series (its name,
and its books' people, genres and tags, so a tag finds the series) and per
present book (its title, else its file name). The worker rebuilds it in one
transaction once indexing is done and before variants start, when the scan
indexed, removed or moved a book, or when the index is empty (just upgraded).

A query is words and `"quoted phrases"`, each maybe after `author:`, `tag:`
or `genre:`; every term must match. A term is passed to FTS5 as a quoted
string, so nothing typed is read as query syntax. Trigrams need three
characters: a shorter term (`港`) is matched with LIKE on the same rows
instead. `GET /search?q=…&l=…` shows series (cards with covers), books,
then people, genres and tags named like the first plain term, with the
matches in `<mark>`, in every library or one.

Schema 8 is upgraded in place to 9 (the empty index, filled by the next scan).

## Serving

`koma serve` (`koma::serve`) opens the catalogue read-only and renders HTML
(`koma::html`) with no JavaScript, following docs/design: a top bar, a side
panel (the libraries, the open one with its collections) that folds behind a
`menu` button on phones (a checkbox and its label), and the page itself. One
stylesheet, `crates/koma/assets/koma.css`, built into the binary and served
at a URL named by its content (`html::css_url`), so it is cached for good;
its size counts against the CSS budget. The reader keeps a few inline rules
until its own design (M4).

A library page shows its collections as stacked cards first, then its own
series, `html::PER_PAGE` (24) cards a page. Each card's cover is the first
page of the series' first book (or the page its metadata names), shown as its
720-wide variant: WebP in a `<picture>` with JPEG to fall back on, or the page
itself if it has no variants. Covers are looked up only for the cards on the
page.

| Route | |
|---|---|
| `GET /` | the default library, or the first one: its collections, then its series, natural order |
| `GET /l/{folder}` | a library (folder name percent-encoded); both take `?c={collection}` (one collection's series) and `?page={n}` |
| `GET /settings` | the libraries and their settings, read-only |
| `GET /search?q={query}&l={folder}` | search, in every library or one (see Search) |
| `GET /s/{id}` | a series: its cover, facts and books, each with its cover |
| `GET /a/koma.{hash}.css` | the stylesheet, cached for a year |
| `GET /r/{book}/{n}` | the reader, page `n` (1-based); the image links to the next page, and the last page back to the series |
| `GET /p/{hash}.{ext}` | a page's bytes, `Cache-Control: public, max-age=31536000, immutable`, `ETag` = hash |
| `GET /p/{hash}.{width}.v{version}.{format}` | a page variant (`jpg` or `webp`; `variants::url`), cached the same way; 404 if that variant was not made, or not with this version |
| `GET /healthz` | health check |

Page bytes come from the book file at the recorded offset (stored pages) or
from the blob store (pages that were compressed, and variants): nothing is
decompressed or resized at request time. The reader still shows originals;
`<picture>` and `srcset` over the variants come with the reader (M4). Each
page is read with one positioned read; a true
`sendfile()` path can replace it later if the budgets ask for it. Until the
first scan, `/` answers 503.

The test `crates/koma/tests/read.rs` is M1's done-test: it scans the made-up
library, then walks it over HTTP from the library to the last page of every
book and checks every image against the original bytes.
