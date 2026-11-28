# M9 progress: optimize and storage

Where milestone 9 ([ROADMAP.md](ROADMAP.md#m9--optimize-and-storage)) stands, for
whoever picks it up next. Update this file as waves land; remove it when M9 is done.

## The plan, in waves

Each wave is a few small pull requests, each green before the next starts.

| Wave | What | State |
|---|---|---|
| 1a | AVIF page variants next to WebP and JPEG | done (PR 35) |
| 1b | Variant widths as a library setting | done (PR 35) |
| 1c | jpegli (`koma-jpegli`) against `jpeg-encoder`, measured on made-up pages; switch only if it wins | done: switched (variants version 2) |
| 1d | The worker in parallel: pages on every core it is given, at low priority (see [Threading](#threading)) | next |
| 2a | Managed storage: originals in the blob store by hash; uploads stored there; folders and names in the catalogue (see [Managed storage](#managed-storage)) | to do |
| 2b | Import of a folder: a job the admin starts, dry run first; hard links or copies | to do |
| 2c | Takeout: stored ZIPs under 4 GiB with a checksum manifest, resumable; imports back | to do |
| 3a | Presets and the book converter, with kept originals and Undo | to do |
| 3b | Auto-convert new uploads; a night window with Run now | to do |
| 4a | The optimize sheet: per series, collection, selection or library; estimates per preset from sample pages; a before-and-after compare | to do |
| 4b | The storage page; README, roadmap and docs | to do |

## Decisions already taken

- **AVIF encoder:** `ravif` on rav1e, pure Rust. The image builds rav1e's
  assembly (`--features asm`, `nasm` in the Dockerfile for x86_64); tests
  run without it. BSD-2-Clause is allowed in `deny.toml` for it.
- **AVIF variants:** quality 70, speed 10, one thread per encode (pages run
  in parallel from wave 1d). On a made-up
  1440-wide line-art page speed 10 took 0.8 s against 4.1 s for speed 6,
  for a file 13% bigger that is still smaller than the WebP one. The
  converter (wave 3) uses the presets' speed 6.
- **Covers** stay WebP and JPEG: another `<source>` on every card costs the
  library page's 15 KiB budget.
- **Kept originals** (owner's choice): kept like the trash. The admin picks
  7, 14, 30 or 90 days or until removed, 30 by default; Undo works while
  they are kept, and the storage page shows them and can free them early.
  Reuse the trash's keep-time setting pattern (`curate.rs`, `/trash`).
- **ffmpeg** is M10's question. Recommendation for then: video thumbnails
  only when ffmpeg is present, through an optional separate media image.

## How 1a and 1b work

See [development.md](development.md#page-variants). In short:

- `variant_pages.formats` (bits 1 JPEG, 2 WebP, 4 AVIF) says which formats a
  page's variants were tried in; `variant_pages.widths` (bits 1 720, 2 1080,
  4 1440) which widths they were made for. Catalogue schema 16 (17: see
  below), accounts
  schema 4.
- `variants::run_job` does, per book: new pages (`make_some` at the widths
  wanted), old pages lacking AVIF, old pages lacking a thumbnail, pages made
  for other widths than wanted now (`other_widths`). `scan::run_jobs` queues
  books for each of these once the queue is empty.
- The library setting goes through the usual path: `POST /settings` queues a
  `SettingChange` in accounts.db (with `widths`), the worker writes it into
  `library_settings`.

## Wave 1c: jpegli measured

Made-up pages drawn at 2880 x 4320 and brought down with Lanczos3 as the
worker does: gray line art with screentone and text-like strokes, a flat
colour page with gradients, a photo-like page with grain. `jpeg-encoder` at
today's settings (quality 82, optimised Huffman tables) sets a SSIMULACRA2
score; jpegli's quality was searched for the same score or better. Every
file was decoded with libjpeg-turbo, as browsers do.

| Page | Width | jpeg-encoder KB | jpegli KB (progressive) | Size | Encode time |
|---|---|---|---|---|---|
| gray line art | 1440 / 1080 / 720 | 438 / 286 / 147 | 411 / 266 / 139 | −6% | −35% |
| colour | 1440 / 1080 / 720 | 237 / 159 / 88 | 180 / 105 / 62 | −24 to −34% | −30% |
| photo-like | 1440 / 1080 / 720 | 219 / 96 / 31 | 164 / 61 / 21 | −25 to −36% | −33% |

- Gray pages gain only through progressive mode; baseline jpegli is the
  same size there.
- Binary: about +0.2 MB (encoder, static, stripped). The 40 MiB budget is
  far off.
- Build: jpegli is C++ (libjxl 0.10.2 through `jpegli-sys`, with highway),
  built with CMake.
- The IJG notice stays: jpegli is built against libjpeg-turbo's
  `jpeglib.h` and `jmorecfg.h`, under the IJG licence. libjxl (BSD-3-Clause)
  and highway would join the notices.
- `jpegli-sys`'s errors unwind through the C++ code (`resume_unwind` in
  `extern "C-unwind"`).
- Switching raises `variants::VERSION`, which makes every copy again in
  every format, AVIF included: hours to days of worker time on a large
  library. JPEG is the reader's last choice after AVIF and WebP, so the gain
  is disk, not what readers download.

**Switched (owner's choice).** One quality for every page: at 85,
progressive, each made-up page scored at least as well as `jpeg-encoder`'s
at 82 (worst −0.1):

| Page | Size against `jpeg-encoder` 82 | SSIMULACRA2 |
|---|---|---|
| gray line art | −6 to −7% | +0.0 to +0.2 |
| colour | +9 to +12% | +8 to +13 |
| photo-like | −25 to −36% | −0.1 to +1.2 |

`jpeg-encoder` scores low on flat colour (its 4:2:0 chroma on saturated
edges): to look as good, a colour page costs a little more, but looks far
better than before. 84 would save 2 to 3 points more on gray and photos
while losing up to 0.7 on gray.

- `koma-jpegli` wraps the `jpegli` crate: no `unsafe` of its own, libjpeg
  errors caught at its one function.
- `variants::VERSION` is 2: the worker makes every page again (all formats,
  thumbnails included), and removes each page's version 1 files as it goes.
- The Alpine stage installs `cmake`, `make` and `g++`; libstdc++ is linked
  statically into the musl binary.
- README: jpegli (libjxl) and Highway notices; the IJG one stays.

### Found on the way: thumbnails of colour pages

With optimised Huffman tables `jpeg-encoder` writes a baseline JPEG with one
scan per component, and its quantisation tables after the frame header. Both
are allowed; browsers and libjpeg-turbo read the files right. zune-jpeg
0.5.15 (`image`'s JPEG decoder) misreads them in colour (nearly black and
green), and jpegli's decoder refuses them. Koma decoded only one of its own
JPEGs: the 720 copy, to add thumbnails to pages made before thumbnails were
(PR 34). Those thumbnails are now made from the 720 WebP; schema 17 drops
every thumbnail row so the worker makes them again, and stale thumbnail
files are replaced. Thumbnails a browser already cached keep their old URL
for a year (the URL changes only with `VERSION`).

## Threading

Owner's decision (manifesto, principle 10): work that can run in parallel
does, on every core the worker is given; 2 CPUs and 1 GB is the floor every
release must still meet, not the target. Until now the worker did one book,
one page and one encode at a time, and the `nice 19, ionice idle` the
manifesto promised was not done.

What changes, in order:

1. **Pages in parallel (wave 1d).** The variants job runs up to N pages at
   once, N from `--concurrency` / `KOMA_CONCURRENCY`, by default
   `std::thread::available_parallelism()` (which follows the container's CPU
   quota). Each page is still encoded on one thread, so every copy comes out
   byte for byte as now: no `VERSION` change, nothing made again. The
   catalogue keeps one writer: encoder threads hand finished pages to the
   thread holding the connection, which writes blobs' rows as now (blobs can
   be written by the encoder threads). A page's decoded pixels are its
   biggest cost, so N is also capped by memory: a page starts only while the
   decoded sizes of the pages in flight (from each page's stored width and
   height) fit a memory budget for the worker. A manga page (about 8 MP,
   24 MB as RGB) runs many at once; a 64 MP photo (`Limits::max_pixels`,
   about 190 MB) may run alone on the 1 GB floor. Capping by
   `Limits::max_alloc` (512 MiB a page) instead would allow only one page on
   the floor.
2. **The worker yields to `serve`.** Lower the worker's CPU and I/O priority
   at start (nice 19, idle I/O class), so more worker threads never slow a
   reader down. Needs a safe wrapper for `setpriority` and `ioprio_set`
   (the workspace forbids `unsafe` outside `koma-webp`); `rustix` has both.
3. **Budgets.** The 2 CPU / 1 GB checks stay as they are, as the floor. Add
   a measure of worker throughput (pages a minute on a made-up library) at 2
   CPUs and at all the runner's cores, and a budget that it grows with them.
4. **Within one encode (converter, wave 3, and M10 photo sets).** Not for
   variants: ravif sets as many AVIF tiles as threads, so the bytes would
   depend on the machine and files grow a little. Where one huge page is
   the whole job (a 50 MP photo converted at speed 6), use threads inside
   the encode with a fixed tile count, and the owner's `rav1e-fae` patch,
   whose parallel loop filters give the same bytes as one thread. Measure
   then; it needs rav1e's threading on and a tile count ravif does not take
   from its thread count.

Consequences: step 1 changes no output, so the library is not made again for
it. `VERSION` 2 (wave 1c) makes every page again anyway: landing 1d before
that is deployed lets that remake run on every core.

Code that says "one thread" today, to update with step 1: `variants::run_job`
(the page loop), the `AVIF_QUALITY` / `AVIF_SPEED` comment in
`crates/koma/src/variants.rs`, `scan::run_jobs` (one job at a time), and
docs/development.md (page variants).

## Managed storage

Owner's decision (manifesto, principle 9): Koma-1 keeps the books, stored by
hash where nothing else writes; plain files come back through takeout. No
folder is mounted for Koma-1 to follow: what came from outside comes in once,
by import.

**What it removes.** Koma-1 no longer guesses what changed while it was not
looking: no rescans to find changes, no stable ids through outside renames
and moves (`scan.rs`'s matching, `tests/stable_ids.rs`), no grace period for
missing folders. Every change is one Koma-1 made, in the journal. The
periodic scan becomes `scrub`: rehash and repair.

**What changes.**
- Originals are blobs (`blobs/ab/<hash>.<ext>`), next to the variants. Zero
  copy serving stays: a stored CBZ is still a file to `sendfile()` a byte
  range of.
- A book is a catalogue row pointing at its original's hash, with its
  library, folder and file name. Organising and the naming standard (M6, M7)
  edit rows; the journal records old and new rows; the trash is rows with a
  date, and their blob goes when no row points at it after the keep time.
- Uploads (M8): `filing.rs` stores into the blob store instead of moving into
  `/library`.
- The converter (wave 3) gets simpler: a converted book is a new blob, the
  book row points at it, the old blob is the kept original; Undo points back.
- Identical books, uploaded twice or in two libraries, are stored once.

**Import (2b).** A job the admin starts (CLI or the jobs page) reads a
mounted folder once, with today's reading of folders and names. Its dry run
says how many books, whether hard links will work or why not ("the folder and
/data are separate mounts"), and the space a copy would take. Then it stores
each book: `link()` when the folder is on the
same mount as `/data`, a copy otherwise (`link()` fails across mount points,
two Docker bind mounts included, with `EXDEV`). A hard-linked file is still
the user's: one edited in place changes the stored book, which `scrub` finds
by its hash.

**No upgrade path (owner's decision).** There are no installs: the only
library holds test books. Managed storage starts from an empty catalogue, and
test books are imported again. Its catalogue schema can start over too:
older catalogues are dropped and rebuilt, as those before schema 7 were, so
no in-place migration is written for it.

**Libraries (owner's decision).** A library is a catalogue row, not a
folder: name, kind (manga, comics, cosplay: how names are read, the reading
direction) and either an owner (an account id) or shared. The folder rules
of `libraries.rs` go: the default `""` library for books loose at the top,
`~name` folders for private libraries, the kind read from the folder name.
- Setup (`docs/design/setup.html`): after the admin's account, a second step
  makes its first library, private by default with "shared" as a choice;
  until then every page leads there, as for the account today. Then the
  library page offers upload, or import into it.
- Roles (owner's decision), replacing `reader` and `curator`. One rule
  under them: a shared library is the admin's to keep, a private one its
  owner's.

  | | Admin (one) | User | Reader |
  |---|---|---|---|
  | Read and download shared libraries | yes | yes | yes |
  | Make, organise and set up shared libraries (upload, import, rename, move, edit, convert, settings) | yes | no | no |
  | Own private libraries and do anything in them (upload, rename, move, edit, convert, takeout) | yes | yes | no |
  | Take out a shared library | yes | no | no |
  | Add and remove accounts, make readers users and users readers | yes | no | no |

  - A private library is seen by its owner alone; the admin does not see
    others'.
  - Every account after the admin starts as a reader.
  - The admin hands the role to any account, a reader included, and becomes
    a user, in one step. There is always exactly one admin, who cannot be
    removed or demoted any other way.
  - Import reads a folder on the server, so it is the admin's, into a shared
    library or the admin's own; users add books by upload.
  - `curate::may_organise` becomes: the library is the viewer's own private
    one, or it is shared and the viewer is the admin.
  - accounts.db can start over like the catalogue (no installs), so the role
    names change without a migration.
- Taking libraries away (owner's decision). Nothing is deleted outright:
  - Making a user a reader: if they have private libraries, the admin is
    warned first, with how many and their size (never what is in them):
    they go to the trash for the keep time. The reader is told at next
    sign-in that they were made a reader, until when their libraries are
    kept, and offered a takeout of them meanwhile. Made a user again before
    the keep time ends, they get them back from the trash.
  - Removing an account: its private libraries go to the trash for the keep
    time the same way, and its owner is offered a takeout first.
- Every upload and import goes into a library chosen first.
- A book stored once for two libraries is never mentioned to anyone: saying
  "already there" across accounts would tell what is in a private library.
  Duplicates are only reported within the library being added to.

**Other apps (owner's decision).** Two ways out, nothing else: takeout, for
everything at once, and OPDS (M12), for reader apps, one book at a time and
always current. No mirror folder, no WebDAV; nothing outside Koma-1 sees the
stored files. Takeout's resumable downloads and M12's download API share the
same byte ranges.

**Takeout (2c).** The plan of a takeout puts books into chunks of under
4 GiB (classic ZIP, stored: books are compressed already), in library and
series order, with fewer than 65,535 entries each; a book bigger than a chunk
gets a chunk of its own, in ZIP64. In each chunk: the folder tree with the
standard names and each book's `metadata.json` (edits included). A manifest
lists the chunks, their files and BLAKE3 hashes. Chunks are streamed, not
written to disk first: their layout is fixed when planned (CRC-32s computed
then by the worker), so each can be served with byte ranges and resumed.
`download.rs` streams whole downloads today, without ranges: ranges are new
here, and M12's sync API wants them too. `archive::zipout` (M8) writes ZIP
and ZIP64 already.

**Exit check.** A made-up library imported by hard links takes no new space
for originals; its takeout, imported into an empty Koma-1, gives the same
books (hashes), names and metadata.

## Notes for the converter (wave 3)

- jpegli's JPEGs decode right with `image`; version 1 ones (from
  `jpeg-encoder`) did not in colour (see above). Test pages are still made
  with `jpeg-encoder` (a dev-dependency).
- The converter needs to decode AVIF pages (to check each written page, and
  to make variants of converted books later). Nothing in the tree decodes
  AVIF yet (`variants::decode` returns `Unsupported("AVIF")`). Candidates:
  `rav1d` (pure-Rust port of dav1d, BSD-2-Clause) or `zenavif`. Check the
  image budget (40 MiB) and `cargo deny check`.
- Lossless preset: `koma-webp` has lossy encoding only; add a lossless
  function there (libwebp's `WebPEncodeLosslessRGB`).
- A converted book is a new file with new page hashes: write it to a
  temporary file, check every page, store it as a blob and point the book at
  it through the journal, so it keeps its id and progress; the old blob is
  the kept original (see [Managed storage](#managed-storage)).
