# M9 progress: optimize and storage

Where milestone 9 ([ROADMAP.md](ROADMAP.md#m9--optimize-and-storage)) stands, for
whoever picks it up next. Update this file as waves land; remove it when M9 is done.

## The plan, in waves

Each wave is a few small pull requests, each green before the next starts.

| Wave | What | State |
|---|---|---|
| 1a | AVIF page variants next to WebP and JPEG | done (PR 35) |
| 1b | Variant widths as a library setting | done (PR 35) |
| 1c | jpegli (`koma-jpegli`) against `jpeg-encoder`, measured on made-up pages; switch only if it wins | next |
| 2a | Presets and the book converter, with kept originals and Undo | to do |
| 2b | Auto-convert new uploads; a night window with Run now | to do |
| 3a | The optimize sheet: per series, collection, selection or library; estimates per preset from sample pages; a before-and-after compare | to do |
| 3b | The storage page; README, roadmap and docs | to do |

## Decisions already taken

- **AVIF encoder:** `ravif` on rav1e, pure Rust. The image builds rav1e's
  assembly (`--features asm`, `nasm` in the Dockerfile for x86_64); tests
  run without it. BSD-2-Clause is allowed in `deny.toml` for it.
- **AVIF variants:** quality 70, speed 10, one thread. On a made-up
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
  4 1440) which widths they were made for. Catalogue schema 16, accounts
  schema 4.
- `variants::run_job` does, per book: new pages (`make_some` at the widths
  wanted), old pages lacking AVIF, old pages lacking a thumbnail, pages made
  for other widths than wanted now (`other_widths`). `scan::run_jobs` queues
  books for each of these once the queue is empty.
- The library setting goes through the usual path: `POST /settings` queues a
  `SettingChange` in accounts.db (with `widths`), the worker writes it into
  `library_settings`.

## Notes for wave 2

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
