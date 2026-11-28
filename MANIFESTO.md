# Koma-1 — Manifesto

> A manga server built like flight hardware: small, fast, boring, and still working in twenty years.

Koma-1 is one statically compiled binary that does almost nothing at request time. All the hard work happens once, at ingest. Serving a page should be little more than the kernel copying bytes it already has cached.

---

## Principles

1. **Requirements before code.** Performance targets are written down and enforced in CI. A change that breaks the budget does not merge.
2. **Work once, serve forever.** Never resize, transcode or extract on a reader's request.
3. **Content-addressed everything.** If the bytes don't change, the URL doesn't change, so it's cached forever.
4. **One binary, three modes.** `serve`, `worker`, `scrub`. No microservices.
5. **Stateless serving.** Scaling out means adding replicas, not rewriting code.
6. **Archives are hostile input.** Every parser is fuzzed, bounded and sandboxed.
7. **The UI is a document, not an app.** If an element can be removed without loss, it is removed.
8. **Boring technology.** SQLite before Postgres, the page cache before Redis, Compose before Kubernetes.
9. **Koma-1 keeps your books; they stay yours.** Books come in through Koma-1, uploaded or imported once from a folder, and are stored by their hash where nothing else writes. Renames, moves and conversions are worker jobs that are planned, previewed, journaled and reversible. Nothing is deleted outright: it goes to a trash with a retention period. A takeout gives everything back, at any time, as plain files.
10. **Use the whole machine, run on the smallest.** Work that can run in parallel does, on every core it is given, and gets faster with more of them. 2 CPUs and 1 GB is the floor every release must still meet, not the machine Koma-1 is tuned for. Serving comes first: the worker yields to `serve`, never the reverse.

---

## Requirements

| Requirement | Target |
|---|---|
| Idle memory (app) | < 25 MB |
| Container image | < 20 MB (distroless/scratch) |
| Page time-to-first-byte, cached | p99 < 10 ms on the server |
| Page turn felt by the reader | 0 ms (next page is already decoded) |
| Library HTML page | < 15 KB, with or without JS |
| Cold start | < 200 ms |
| Smallest hardware (the floor) | 50 concurrent readers within 2 CPUs and 1 GB of memory on ARM64 |
| More hardware | ingest and conversion throughput grows with the cores the worker is given |
| Scale-out | add nodes, change nothing else |

Every pull request runs a k6 load test and an image-size check against this table.

---

## Architecture

```
                ┌──────────── CDN / Varnish (optional) ────────────┐
                │  immutable page URLs → ~100% cache hit rate       │
                └───────────────────────┬──────────────────────────┘
                                        │
                              Caddy (TLS, HTTP/3, zstd/br)
                                        │
          ┌─────────────────────────────┼─────────────────────────────┐
      koma serve #1                koma serve #2                koma serve #N    ← stateless
          │                                                            │
          ├── SQLite (WAL, read-only replica via LiteFS) ──────────────┤
          └── Blob store: local disk  ──or──  S3-compatible (Garage/SeaweedFS)
                                        ▲
                          koma worker (1+ replicas)
                          scan → hash → index → transcode → write blobs
                                        ▲
                  uploads, one-time imports (CBZ/CB7/CBR/PDF/EPUB/folders)
                  → originals stored by hash; only the worker writes, through the journal
```

### Runtime
- Rust (axum on hyper and tokio). Images are decoded and resized in pure Rust; small vendored C encoders (libwebp) only where Rust has none. No garbage collector, predictable latency.
- Statically linked with musl, built `FROM scratch`.
- Non-root, read-only root filesystem, all capabilities dropped, seccomp profile.
- Reproducible builds via Nix, SBOM, cosign-signed images.

---

## Storage

**Content addressing.** Every page is identified by its BLAKE3 hash:

```
/p/<blake3>.<width>.<format>
Cache-Control: public, max-age=31536000, immutable
```

No cache invalidation, ever. Duplicate pages and archives are stored once.

**Zero-copy serving from CBZ files.** Most CBZ archives store JPEGs uncompressed. At ingest, the zip central directory is indexed to `(file, offset, length)` per page. Original-quality pages are served with `sendfile()` on a byte range of the archive: no extraction, no temp files. CBR and PDF are extracted once at ingest.

**Derived variants, made once at ingest:**
- Widths 720, 1080, 1440 and original, and a 240 thumbnail for the reader's strip and scrubber. Never upscale.
- AVIF and WebP, JPEG fallback. Format is chosen in the URL via `<picture>`, never via `Accept` negotiation, which fragments caches.
- Grayscale detection: black-and-white pages are stored as 8-bit single-channel images.
- Optional lossless JPEG XL recompression of originals for archival (bit-exact reversible).
- Webtoon strips are sliced into ~2000 px tiles.
- Width and height are stored for every page so the layout never shifts.

**Managed storage.** Originals are stored like pages, by their BLAKE3 hash, in the blob store, and only `koma worker` writes there; nothing outside Koma-1 changes them, so Koma-1 never has to guess what happened while it was not looking. Books reach it in two ways: uploads, which land in a separate inbox and pass the same hostile-input checks before they are stored, and a one-time import of an existing folder, hard-linked when the folder is on the same mount as the blob store and copied otherwise. Folders, series and file names are the catalogue's: a rename or move changes no bytes. Libraries are the catalogue's too: each has a name, a kind (manga, comics or cosplay) and either an owner, who alone sees and organises it, or is shared, seen by everyone and organised by curators and the admin. There is no default library: setup makes the admin's account, then its first library, private unless made shared. Every rename, move, conversion or delete is a job: the worker plans it, the UI shows a dry run, the worker applies it and records old and new names and hashes in an append-only journal, and any batch can be undone from that journal. Books and series keep their ids, so progress and edits follow them.

**Takeout.** A job writes a library, a series or everything out as plain files: the folder tree with the standard names ([docs/naming.md](docs/naming.md)) and each book's `metadata.json`, in ZIP files of under 4 GiB each (stored, classic ZIP, so any tool opens them; a book bigger than that gets one of its own, in ZIP64), with a manifest of checksums. A takeout imports back as it was.

**Metadata** is read at ingest from `metadata.json`, then `ComicInfo.xml` (where both exist, `metadata.json` wins and tags are combined), then the file name. Edits made in Koma-1 are saved in SQLite at once and written into files only on request.

**The database** lives in SQLite (WAL, mmap, prepared statements). Search uses FTS5 with the trigram tokenizer, so Japanese and Korean titles work without a word segmenter. Litestream streams continuous backups to S3.

**Integrity.** The `scrub` mode re-hashes blobs on a schedule. A corrupt variant is regenerated from its original; a corrupt original raises an alert.

---

## Ingest pipeline

```
upload or import job
  → full BLAKE3 hash                                           a book already stored is not stored again
  → parse archive (fuzzed parser)
  → page order (natural sort), spread detection (aspect > 1.2 = double page)
  → job queue (SQLite table)
  → variants: decode, resize, encode; pages in parallel, one per core the worker
    is given (--concurrency, default: all of them), nice 19, ionice idle
```

Ingest may be slow. Serving never is.

---

## Reader

- **Adaptive prefetch.** Reading speed is measured per session: `prefetch = clamp(ceil(2s / avg_page_time), 2, 8)` pages.
- **Decode ahead.** `img.decode()` on upcoming pages, so a turn shows an already-decoded image.
- `fetchpriority="high"` for the current page, `low` for prefetches. `saveData` drops to 720 px.
- `srcset` and `sizes` choose the variant by viewport and pixel density.
- A service worker keeps chapters offline with an LRU limit.
- Modes: single page, spread-aware double page (with a one-page shift toggle), vertical scroll for webtoons.
- Input: arrows, `h`/`l`, `j`/`k`, space, tap thirds, swipe. Right-to-left by default, stored per series.
- Progress is sent with `sendBeacon`, debounced, never blocking a turn.
- Under 10 KB of plain JavaScript. No framework.

---

## UI

- Server-rendered HTML. Library, search and series pages work without JavaScript.
- System font stack, one accent color, light/dark via `prefers-color-scheme`.
- No icon fonts, no animations; `prefers-reduced-motion` respected regardless.
- Under 5 KB of CSS.
- Library: an infinitely scrolling, fixed-ratio cover grid. Only rows near the viewport hold images; rows far away keep their height and drop their covers, so memory stays flat. Without JavaScript the same chunks are plain pages (`?page=2`). Plus a list view that is a real `<table>`.
- Reader: the page, a black background, nothing else. Controls appear on tap or mouse move and fade after 2 seconds.
- Semantic elements, visible focus, fully keyboard-operable.

**The test:** it should look equally reasonable in 2006 and in 2046.

---

## Interoperability

- OPDS 1.2 / 2.0 with page streaming: Mihon/Tachiyomi, Panels, KOReader and Chunky work out of the box.
- A small JSON API — the same one the web UI uses.
- A download and sync API for offline clients (including a future Android app): whole volumes, resumable with byte ranges, checksummed, with a change feed and two-way progress sync.
- Auth: signed session cookies and passkeys (WebAuthn). For organizational SSO, OIDC/SAML via oauth2-proxy with a trusted forwarded-user header.

---

## Security and reliability

- Archive parsers fuzzed continuously (cargo-fuzz).
- Limits against zip bombs (expanded size and ratio caps), path traversal, symlinks and decompression bombs (pixel caps before decode).
- RAR and PDF decoding in a sandboxed subprocess with seccomp and rlimits: a crash kills one job, not the server.
- Prometheus metrics, OpenTelemetry traces, structured JSON logs.
- SLOs with error-budget burn-rate alerts, not CPU alerts.
- Chaos testing before each release: kill workers mid-job, fill the disk, corrupt a blob, verify recovery.

---

## Deployment (single node)

```yaml
services:
  caddy:
    image: caddy:2-alpine
    ports: ["443:443", "443:443/udp"]
    volumes: [./Caddyfile:/etc/caddy/Caddyfile:ro, caddy:/data]

  serve:
    image: ghcr.io/koma-1/koma:1.0   # FROM scratch, ~15 MB
    command: ["serve"]
    read_only: true
    user: "65532:65532"
    cap_drop: [ALL]
    volumes:
      - data:/data                  # serve never writes
    deploy: { resources: { limits: { memory: 128m } } }

  worker:
    image: ghcr.io/koma-1/koma:1.0
    command: ["worker", "--concurrency=auto"]
    read_only: true
    cap_drop: [ALL]
    # The only writer of the blob store. To import an existing library with hard
    # links, the data must be a host folder holding it: /srv/koma:/data, with
    # the library moved to /srv/koma/import (one mount, so links work).
    volumes: [data:/data]
    # Any number of CPUs: the worker uses what it is given; 2 is the floor.
    deploy: { resources: { limits: { memory: 512m } } }

  litestream:
    image: litestream/litestream
    command: replicate
    volumes: [data:/data, ./litestream.yml:/etc/litestream.yml:ro]

volumes: { data: {}, caddy: {} }
```

### Scaling path
More `serve` replicas → blobs on Garage or SeaweedFS → per-node read replicas with LiteFS → CDN in front. The code never changes; only configuration does.

---

## Non-goals

- No Kubernetes until there is more than one site.
- No Redis — SQLite and the page cache are faster than a network hop.
- No microservices.
- No on-the-fly image resizing.
- No recommendation engine, social features or animated transitions.
- No scrapers or downloaders that fetch from other sites. Downloading your own books from Koma-1 is a feature.
