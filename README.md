# Koma-1

A self-hosted manga and comics server: one small Rust binary, a plain web
library and reader, built to stay fast on small hardware.
Why and how: [MANIFESTO.md](MANIFESTO.md). Contributing: [AGENTS.md](AGENTS.md).

**Status: Milestone 1 in progress.** The binary, the image and the CI checks
exist, and it can index CBZ archives; it does not serve a library yet.

## Run

```sh
docker build -t koma .
docker run --read-only --cap-drop ALL -p 8080:8080 koma serve
curl http://localhost:8080/healthz   # ok
```

One binary, three modes:

| Command | Does |
|---|---|
| `koma serve` | HTTP server. Today: `GET /healthz`. |
| `koma worker` | Background jobs (ingest). Today: idles until stopped. |
| `koma scrub` | Integrity checks. Today: nothing to check. |
| `koma healthcheck` | Exits 0 if the local server is healthy; the image's `HEALTHCHECK`. |

`--listen <ip:port>` or `KOMA_LISTEN` sets the address (default `0.0.0.0:8080`).
Logs are JSON lines on stderr.

## Docs

- [docs/development.md](docs/development.md): building, testing, budgets, test libraries
- [docs/ROADMAP.md](docs/ROADMAP.md): milestones
- [docs/design/](docs/design/README.md): UI design
