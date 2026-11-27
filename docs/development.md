# Development

## Layout

| Path | What |
|---|---|
| `crates/koma` | The `koma` binary and its library: CLI, server, archive reading |
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
