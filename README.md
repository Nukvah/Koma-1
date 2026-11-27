# Koma-1

A self-hosted manga and comics server: one small Rust binary, a plain web
library and reader, built to stay fast on small hardware.
Why and how: [MANIFESTO.md](MANIFESTO.md). Contributing: [AGENTS.md](AGENTS.md).

**Status: Milestone 4 (reader) in place; what needs accounts comes with M5.**
`koma worker` indexes a library of CBZ, ZIP, CB7 and EPUB files and photo
folders into SQLite and keeps it up to date, and `koma serve` shows it in a
browser: libraries and collections as an endless grid or a sortable table,
search, series pages, downloads of the originals, and a reader: pages one or
two at a time or as a strip, with a scrubber, thumbnails and zoom, and EPUB
books as text with their contents. The worker also makes smaller JPEG and WebP copies of each
large page, once.

## Run

```sh
docker build -t koma .
docker run --read-only --cap-drop ALL -p 8080:8080 koma serve
curl http://localhost:8080/healthz   # ok

# Read a library: books read-only in /library, catalogue and blobs in /data
docker run --read-only --cap-drop ALL -v /path/to/manga:/library:ro -v koma-data:/data koma scan
docker run --read-only --cap-drop ALL -v /path/to/manga:/library:ro -v koma-data:/data \
    -p 8080:8080 koma serve        # then open http://localhost:8080
```

One binary, three modes:

| Command | Does |
|---|---|
| `koma serve` | HTTP server: the library, series and reader pages, page images, `/healthz`. |
| `koma worker` | Scans the library into the catalogue, then rescans every 15 minutes (`--rescan`). |
| `koma scan` | Scans the library once and exits (3 if some books could not be read). |
| `koma scrub` | Integrity checks. Today: nothing to check. |
| `koma healthcheck` | Exits 0 if the local server is healthy; the image's `HEALTHCHECK`. |

`--listen <ip:port>` or `KOMA_LISTEN` sets the address (default `0.0.0.0:8080`);
`--library` / `KOMA_LIBRARY` (default `/library`) and `--data` / `KOMA_DATA`
(default `/data`) set the folders.
Logs are JSON lines on stderr.

## Third-party notices

This software is based in part on the work of the Independent JPEG Group
(through the `jpeg-encoder` crate, which writes JPEG page variants).
It includes libwebp, Copyright (c) 2010, Google Inc., under the BSD
3-Clause licence. Every other dependency is under the MIT, Apache-2.0,
BSD-3-Clause, Zlib or Unicode-3.0 licence; `cargo deny check` lists them.

## Docs

- [docs/running.md](docs/running.md): running Koma-1 with Docker or Compose: folders, scans, health
- [docs/development.md](docs/development.md): building, testing, budgets, test libraries
- [docs/naming.md](docs/naming.md): what Koma-1 reads from file names and metadata files
- [docs/ROADMAP.md](docs/ROADMAP.md): milestones
- [docs/design/](docs/design/README.md): UI design
