# AGENTS.md

Quick orientation for any agent contributing to Koma-1. Full design: [MANIFESTO.md](MANIFESTO.md). Repository rules: [CLAUDE.md](CLAUDE.md) — they apply to every agent, not only Claude.

## What Koma-1 is

A self-hosted manga server with a web library and reader, shipped as Docker on Linux. Goals, in priority order: **speed, low resource use, horizontal scalability, a plain timeless UI.**

## Build this

- **One Rust binary** (axum + tokio, libvips) with three modes: `serve`, `worker`, `scrub`. Static musl build, `FROM scratch` image.
- **Ingest does the work, serving does not.** Workers scan the library, hash pages (BLAKE3), index archives and pre-generate image variants (720/1080/1440/original; AVIF, WebP, JPEG). Grayscale pages stored as 8-bit gray.
- **Content-addressed, immutable page URLs** (`/p/<hash>.<width>.<format>`, cached for a year).
- **Zero-copy CBZ serving:** index the zip central directory, `sendfile()` byte ranges.
- **SQLite** (WAL, FTS5 trigram search) for metadata; local disk or S3-compatible storage for blobs.
- **Server-rendered HTML.** Library pages work without JavaScript. Reader JS < 10 KB, CSS < 5 KB, no framework.
- **Reader:** adaptive prefetch, decode-ahead, `srcset`, RTL default, single/double/vertical modes, keyboard and touch.
- **OPDS** feed and a small JSON API.
- **Hardened archive parsing:** fuzzed, size/ratio/pixel limits, no path traversal, risky decoders sandboxed.

## Do not build

- On-the-fly image resizing or transcoding at request time.
- SPA frameworks (React, Vue, …), CSS frameworks, icon fonts, animations, decorative UI.
- Redis, message brokers, microservices, Kubernetes manifests.
- Postgres (unless multi-writer is proven necessary).
- Recommendations, social features, comments, ratings, trackers of external sites, scrapers or downloaders.
- Anything that breaks the budgets below.

## Budgets (CI-enforced)

The numbers live in [budgets.toml](budgets.toml); `cargo xtask budgets` checks them on every pull request,
on ARM64 in a container capped at 2 CPUs and 1 GB. At the time of writing:

| | Limit |
|---|---|
| Image size | 40 MiB |
| Idle RAM | serve 40 MiB, worker 64 MiB |
| Cached page TTFB | p99 < 10 ms |
| Cold start | < 200 ms |
| Library HTML page | < 15 KiB |
| CSS / reader JS / curator JS | 10 / 16 / 24 KiB |

A budget protects speed; it does not block a feature. Raise one only with a `Budget change:` line in the pull
request saying why. The reader-facing targets (TTFB, cold start, page size) never move.

## Commands

See [README.md](README.md#develop) for build, test and budget commands.

## Before you start

1. Read MANIFESTO.md's section for the area you touch.
2. If a change adds a dependency, a service or a UI element, justify it against the goals above — the default answer is no.
3. Follow CLAUDE.md: no personal data, no real titles or people (use made-up names like "Starfall"), check your diff before pushing.
