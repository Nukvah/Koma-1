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
nightly): `zip_index`, `sevenz`, `page_size`, `metadata`, `names`,
`variants` (decoding, resizing and encoding a page) and `epub` (the
package, the contents, and the chapter cleaner, whose output must keep to
its allowlist of tags and attributes). CI runs
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

- A series is a folder, and each CBZ, ZIP, CB7 or EPUB in it is a book. Books loose in
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
- Schema 10 adds `series.collection` (generated) and the shelf indexes;
  9 and older from 7 on are upgraded in place.
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

## Downloads

`koma::download`. `GET /d/{book}` streams the book's original: a blocking
thread reads 256 KiB chunks into a short queue that the response body
drains, so nothing is held whole in memory or written to disk. A photo
folder is not one file, so it goes out as a stored (uncompressed) ZIP of its
images, in reading order, made while it is sent: each image is read once,
its CRC-32 taken, then sent; its exact size is known before the first byte.
Folders that would need ZIP64 (over 4 GiB or 65,535 images) are not offered.
A file that changed since the scan ends the download with an error, never as
if complete. Every book card on a series page has a download link with the
size.

## Reader

`GET /r/{book}/{n}` (`html::reader`), designed in docs/design/reader*.html,
with its own `assets/reader/reader.css` and `reader.js`, served at
content-named URLs like the library's. The page alone fills the window: a
`<picture>` whose WebP `<source>` and JPEG `srcset` list the page's variants
and the page itself, with `sizes` set to the width a whole page takes
(`min(100vw, {w/h}vh)`), so the browser picks the smallest that is sharp.
Without JavaScript it still reads: the tap thirds are links (in right-to-left
books the left third goes forward), and the middle third opens the bars
through `#bars` and `:target`.

The script loads the book's pages and their variants once from
`GET /b/{book}` (JSON) and turns pages in place, keeping the URL in step:
arrows and `h`/`l` in the book's direction, space / `j` / `k` /
PageDown / PageUp, tap thirds, swipes; `m` or Enter shows the bars, Escape
hides them. Back past the first page it goes to the series. The direction is the
book's own (from its metadata), else its library's, and can be switched in
the bars for the series. That choice and the position are kept in the
browser (`localStorage`) until accounts (M5). The tap zones are shown once,
faintly, on the first open.

Three ways to read, chosen per series in the bars and kept in the browser:
**single**; **double**, two pages side by side, both on screen (each at
most half the window wide), the first on the right in right-to-left books:
the cover alone, then pairs, a wide page (wider than 1.2 times its height,
a spread already) alone with the pairs starting again after it, and `s`
shifting every pair by one page; and **strip**, every page in one column at
most 800 px wide, scrolled (space / `j` / `k` move by most of a screen, a
tap shows the bars), where only pages within two screens hold their
picture and the URL follows the page at the top. **Auto**, the default,
reads a book as a strip when most of its pages are tall strips (at least
three times as tall as wide), else one page at a time.

Turns are instant because pages are made ahead. Each coming page is an image
of its own with the page's `srcset` in one format (WebP where the browser
takes it, else JPEG; with Save-Data, the 720 variant only), fetched at low
priority and decoded (`img.decode()`) before it is needed; a turn swaps the
decoded image in, at high priority if it is still loading. How far ahead
follows the reading speed: enough pages for two seconds, from the last five
times between turns (pauses over a minute left out), 2 to 8 pages, plus the
page before; pages outside that are dropped. Measured in Chromium with the
network throttled to 1.6 Mbit/s and 150 ms round trips, reading a page every
4 s and then every 0.6 s: at every turn the page on screen was already
loaded and decoded (at most 3 ms waiting on `decode()`), and no page was
fetched at the turn. That is M4's exit check for pictures; EPUB's (its
position on another device) comes with accounts.

**Fit**, per series in the bars: the whole **page** in the window (the
default), its **width** or its **height**. A page larger than the window
scrolls, and a turn first scrolls by most of a screen (down for width;
for height towards the reading direction) before it turns; a new page
starts at its start (top; left, or right in right-to-left books), and going
back, at its end. `sizes` follows the fit, so the variant picked is the one
sharp at that size. **Zoom** (`z`, or the bars) doubles the pages on
screen, with `sizes` set to the new width so a sharper picture loads;
scrolling or dragging pans, and `z`, Escape, a double click or a turn
zooms out. On phones the browser's own pinch works too, and while pinched
in, taps and swipes don't turn.

The bottom bar also has a **scrubber** (a range input, in the reading
direction): while it moves, a preview of the page under it (its smallest
picture: the 720 variant, else the page); let go, it turns there. **Thumbnails**
(`t`, or the bars) are a row of every page's smallest picture, loaded
lazily as the row scrolls, the current one marked; a click turns there.
Both need the script and stay hidden without it.

**At the end** of a book, a card: the next book in the series with its
cover, its pages, and whether it was started (read from the browser), with
a button to read it from where it was left (space, or forward again) and
one back to the series; for the last book, only the way back. Back,
Escape or a tap outside closes it. The card's HTML comes with
`GET /b/{book}` (`end`), built by the server so names are escaped there.

## EPUB

`koma::epub`, at ingest (`scan::index_epub`). `META-INF/container.xml` names
the package (OPF): its metadata (title, creators, subjects as tags,
language, date, description, series and position from EPUB 3
`belongs-to-collection` or Calibre's tags, `page-progression-direction`),
its files and their order. That metadata is the weakest layer, under
`ComicInfo.xml` and `metadata.json`. The contents come from the EPUB 3
navigation document (its `epub:type="toc"` list), else the EPUB 2 NCX.
Fixed-layout books (`rendition:layout` `pre-paginated`) are refused with a
scan error until M11.

The book's pictures (JPEG, PNG, GIF, WebP) are its **pages**, the cover
first, so covers, page variants, `/p/` and downloads work as for any book.
Its **chapters** (the spine) are rewritten by `epub::clean` and stored in
the blob store as `<hash>.html` (`chapters` table); `toc` holds the
contents. A book's `page_count` is its number of chapters.

`clean` tokenizes the XHTML (quick-xml, forgiving: end tags need not
match, HTML's named entities are known) and writes the body back from an
allowlist: headings, paragraphs, emphasis, lists, tables, figures, ruby,
links and pictures; `section`, `article` and the like become `div`; unknown
elements are unwrapped (their text kept); `script`, `style`, forms, frames,
objects, media, MathML and `hidden` content are dropped with what is in
them; of SVG only `<image>` is kept, as a picture. Attributes kept: `id`
(as `e-<id>`, so it cannot clash with the reader's own), `lang`, `dir`,
`colspan`/`rowspan`, `ol` `start`/`type`, and `href`/`src` rewritten: a
link to another chapter becomes its number (relative to `/e/<book>/`), to a
picture its `/p/` URL; links out keep `http(s):` and `mailto:` only and
open in a new tab without a referrer. Everything is written by `clean`
itself, every text and value escaped, and output stays well-formed
whatever the input. The book's CSS and fonts are not kept: every book reads
in the reader's typography.

**Reading** (`GET /e/{book}/{n}`, `html::epub_chapter`, design
docs/design/reader-epub.html): a column of text on paper, one chapter a
page, scrolled. The page is complete without JavaScript: where it is (the
series, the contents at `#toc`, chapter n of N), the chapter, and the
chapters before and after; the last one leads to the next book in the
series, or back to the series. The next chapter is prefetched. It is sent
with a Content-Security-Policy that allows only Koma-1's own stylesheets,
script and pictures: the cleaner is the first wall, this the second.
`/r/` sends an EPUB to `/e/`.

`assets/epub/epub.js` (budget "EPUB reader JS", loaded only here, in
`<head>` so the text is drawn in its size from the start) adds the text's
size (14 to 28 px), width (narrow, medium, wide) and spacing (tight,
normal, loose), kept in the browser for every book; the bars on a tap or
`m`; the contents on `c`; the next and previous chapters on the arrow keys,
and the next on space at the end of one. The position is the chapter and
the first block of text on screen (`koma:epos:<book>`), kept in the
browser until accounts (M5); a book opened from outside it (the series, a
search) goes back there. Vertical writing (Japanese books) comes later;
every book reads horizontally for now.

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

**Infinite scroll** (`assets/library/scroll.js`, the only script outside
the reader, served like the stylesheet at a content-named URL; budget
"Library JS", 4 KiB). Near the end of the grid it fetches the next
`?page=` and appends its cards; there is no other endpoint, and without
JavaScript the `?page=` links do the same. A card far from view keeps only
its box, at the height every card has (measured once, so no layout per
card), and its contents wait in a map until it comes near: the DOM stays at
about one node per far card and only covers near the view hold images.
When scrolling stops, the URL is set to the chunk at the top of the view
(`history.replaceState`), so reload and back return there.

The shelf is paged in SQL: `series.collection` is a generated column
(schema 10) and `series_shelf (library, collection, name)` and
`series_names (library, name)` are indexes in natural order, so a chunk
reads its 24 rows without sorting the library on each request; the table
counts books in one pass and looks up people and genres only for the rows
shown.

M3's exit check: `tests/exit_m3.rs` serves 10,000 made-up series and walks
every grid chunk (417) and every table page (200) by their plain links:
each under 15 KiB (the largest grid chunk 9.6 KB), with no JavaScript. In
a release build a grid chunk takes about 10 ms and a table page about 20 ms
end to end. Scrolling through all 10,000 in Chromium (headless, 1400 x 900,
a distinct cover each) with the script: no long tasks, 56 covers holding an
image at any time, about one DOM node per far card, and renderer memory
after garbage collection from 142 MiB at the start to 217 MiB at the end,
about the 79 MB of cover files the browser keeps cached.

`?view=table` shows the same series as a real `<table>`: title, collection
(for a whole library that has collections), first authors, books, pages,
first genres and the date its first book was added, 50 rows a page. Each
heading links to `&sort=<column>`; on the current one it turns the order
round (`-<column>`, `aria-sort` says which).

| Route | |
|---|---|
| `GET /` | the default library, or the first one: its collections, then its series, natural order |
| `GET /l/{folder}` | a library (folder name percent-encoded); both take `?c={collection}` (one collection's series), `?page={n}`, and `?view=table` with `&sort={column}` |
| `GET /settings` | the libraries and their settings, read-only |
| `GET /search?q={query}&l={folder}` | search, in every library or one (see Search) |
| `GET /s/{id}` | a series: its cover, facts and books, each with its cover |
| `GET /a/koma.{hash}.css`, `/a/scroll.{hash}.js` | the stylesheet and the library script, cached for a year |
| `GET /d/{book}` | the book's original: the file as it is, or a photo folder as a stored ZIP of its images in reading order |
| `GET /r/{book}/{n}` | the reader, page `n` (1-based): see Reader |
| `GET /e/{book}/{n}` | an EPUB's chapter `n` (1-based): see EPUB |
| `GET /b/{book}` | the reader's JSON: the book's pages with their sizes and variants, and the card for its end |
| `GET /p/{hash}.{ext}` | a page's bytes, `Cache-Control: public, max-age=31536000, immutable`, `ETag` = hash |
| `GET /p/{hash}.{width}.v{version}.{format}` | a page variant (`jpg` or `webp`; `variants::url`), cached the same way; 404 if that variant was not made, or not with this version |
| `GET /healthz` | health check |

Page bytes come from the book file at the recorded offset (stored pages) or
from the blob store (pages that were compressed, and variants): nothing is
decompressed or resized at request time. The reader shows them through
`<picture>` and `srcset` (see Reader). Each
page is read with one positioned read; a true
`sendfile()` path can replace it later if the budgets ask for it. Until the
first scan, `/` answers 503.

The test `crates/koma/tests/read.rs` is M1's done-test: it scans the made-up
library, then walks it over HTTP from the library to the last page of every
book and checks every image against the original bytes.
