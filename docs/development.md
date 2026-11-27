# Development

## Layout

| Path | What |
|---|---|
| `crates/koma` | The `koma` binary and its library: CLI, server, archives, catalogue, scan |
| `crates/koma-fixtures` | Made-up test material: PNG pages, a zip writer, a test library |
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
docker build -t koma:dev . && cargo xtask budgets --image koma:dev
```

## Budgets

[budgets.toml](../budgets.toml) holds every budget: image size, idle memory, time
to first byte, cold start, CSS and JavaScript sizes, page size. CI builds the
image on ARM64 and x86_64 and runs `cargo xtask budgets` in a container capped
at 2 CPUs and 1 GB; any budget exceeded fails the pull request. A budget may be
raised only with a `Budget change:` line in the pull request saying why.

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

Pages are image members (`jpg`, `jpeg`, `png`, `webp`, `gif`, `avif`, `bmp`,
`jxl`), skipping directories, encrypted members, hidden files and `__MACOSX`.
They are put in reading order with `koma::natsort`: numbers by value
(`2 < 10`, full-width digits too), letters case-insensitively, folder by
folder, with the extension only breaking ties.

## Catalogue and scan

`koma worker` (at start) and `koma scan` index the library into
`<data>/koma.db`, a SQLite database in WAL mode (`koma::catalog`); `koma serve`
only reads it.

- A series is a folder, and each CBZ in it is a book. A CBZ in the library
  root is a series of its own. Hidden files and symbolic links are skipped.
- Every page gets a BLAKE3 hash and has its CRC-32 and size checked against
  the archive.
- Stored pages are recorded in place as (book file, offset, length), so they
  can be served straight from the archive.
- Deflated pages are inflated once, at scan time, into the blob store
  `<data>/blobs/ab/<hash>.<ext>`; identical pages share one blob. Inflating
  stops at the declared size, at 256 MiB, or if the declared ratio is over
  1000:1.
- A book whose size and modification time haven't changed is skipped; books
  whose files are gone are removed, and empty series with them.
- A file that can't be read is logged, stored in `scan_errors` with the
  reason, and skipped; it never stops the scan.

Order is natural everywhere: SQL uses the `natsort` collation, which is
`koma::natsort::compare_paths`.

## Serving

`koma serve` (`koma::serve`) opens the catalogue read-only and renders plain
HTML (`koma::html`): no JavaScript, a few lines of CSS. The real design
(docs/design) comes with the library UI milestone.

| Route | |
|---|---|
| `GET /` | the library: every series, natural order |
| `GET /s/{id}` | a series: its books |
| `GET /r/{book}/{n}` | the reader, page `n` (1-based); the image links to the next page, and the last page back to the series |
| `GET /p/{hash}.{ext}` | a page's bytes, `Cache-Control: public, max-age=31536000, immutable`, `ETag` = hash |
| `GET /healthz` | health check |

Page bytes come from the book file at the recorded offset (stored pages) or
from the blob store (pages that were compressed): nothing is decompressed or
resized at request time. Each page is read with one positioned read; a true
`sendfile()` path can replace it later if the budgets ask for it. Until the
first scan, `/` answers 503.

The test `crates/koma/tests/read.rs` is M1's done-test: it scans the made-up
library, then walks it over HTTP from the library to the last page of every
book and checks every image against the original bytes.
