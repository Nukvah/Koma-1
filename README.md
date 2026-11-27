# Koma-1

A self-hosted manga and comics server: one small Rust binary, a plain web
library and reader, built to stay fast on small hardware.
Why and how: [MANIFESTO.md](MANIFESTO.md). Contributing: [AGENTS.md](AGENTS.md).

**Status: Milestone 0 (foundations).** The binary, the image and the CI checks
exist; it does not read a library yet.

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

## Develop

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo deny check                        # licences and advisories
cargo xtask budgets --assets-only       # asset sizes, no Docker needed
docker build -t koma:dev . && cargo xtask budgets --image koma:dev
```

## Budgets

[budgets.toml](budgets.toml) holds every budget: image size, idle memory, time
to first byte, cold start, CSS and JavaScript sizes, page size. CI builds the
image on ARM64 and x86_64 and runs `cargo xtask budgets` in a container capped
at 2 CPUs and 1 GB; any budget exceeded fails the pull request. A budget may be
raised only with a `Budget change:` line in the pull request saying why.
