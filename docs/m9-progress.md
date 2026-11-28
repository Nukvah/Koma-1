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
| 2a | Presets and the book converter, with kept originals and Undo | to do |
| 2b | Auto-convert new uploads; a night window with Run now | to do |
| 3a | The optimize sheet: per series, collection, selection or library; estimates per preset from sample pages; a before-and-after compare | to do |
| 3b | The storage page; README, roadmap and docs | to do |

## Decisions already taken

- **AVIF encoder:** `ravif` on rav1e, pure Rust. The image builds rav1e's
  assembly (`--features asm`, `nasm` in the Dockerfile for x86_64); tests
  run without it. BSD-2-Clause is allowed in `deny.toml` for it.
- **AVIF variants:** quality 70, speed 10, one thread per encode (pages run
  in parallel from wave 1d). On a made-up
  1440-wide line-art page speed 10 took 0.8 s against 4.1 s for speed 6,
  for a file 13% bigger that is still smaller than the WebP one. The
  converter (wave 2) uses the presets' speed 6.
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
4. **Within one encode (converter, wave 2, and M10 photo sets).** Not for
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

## Notes for wave 2

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
  temporary file, check every page, swap it in through the M6 journal so it
  keeps its id and progress, and keep the original under the keep time.
