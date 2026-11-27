# Running Koma-1

Koma-1 is one image with two long-running modes that share two folders:

| Folder | Mount | Holds |
|---|---|---|
| `/library` | read-only | your CBZ files. Koma-1 never writes here. |
| `/data` | read-write | the catalogue (`koma.db`) and the blob store (`blobs/`) |

`koma worker` scans `/library` into `/data`. `koma serve` reads both and
answers HTTP on port 8080. Run them as two containers on the same volumes,
or run `koma scan` once and then only `serve`.

## Docker Compose

```yaml
services:
  serve:
    image: koma
    command: serve
    ports: ["8080:8080"]
    volumes:
      - /path/to/library:/library:ro
      - koma-data:/data
    read_only: true
    cap_drop: [ALL]
    security_opt: ["no-new-privileges:true"]
    restart: unless-stopped

  worker:
    image: koma
    command: worker
    healthcheck:
      disable: true   # the image's check probes HTTP, which only serve answers
    volumes:
      - /path/to/library:/library:ro
      - koma-data:/data
    read_only: true
    cap_drop: [ALL]
    security_opt: ["no-new-privileges:true"]
    restart: unless-stopped

volumes:
  koma-data:
```

Build the image first with `docker build -t koma .`, then run
`docker compose up -d` and open <http://localhost:8080>.

## Plain Docker

```sh
docker build -t koma .
docker run --rm --read-only --cap-drop ALL \
    -v /path/to/library:/library:ro -v koma-data:/data koma scan
docker run -d --name koma --read-only --cap-drop ALL \
    -v /path/to/library:/library:ro -v koma-data:/data -p 8080:8080 koma serve
```

`koma scan` exits 0 when every book was read and 3 when some could not be;
the log names each one.

## The data folder

The image runs as user `65532:65532`, never as root.

- **A named volume** (`koma-data` above) needs nothing more: the image
  ships an empty `/data` owned by that user, and Docker gives a new volume
  the same owner.
- **A folder on the host** must be writable by the container's user. Either
  give it to that user (`sudo chown 65532:65532 /srv/koma-data`) or run the
  containers as your own user with `--user "$(id -u):$(id -g)"` (Compose:
  `user: "1000:1000"`).

The library only needs to be readable by the container's user.

## Scanning

Today the worker scans once, when it starts. After adding or changing books,
restart it (`docker compose restart worker`) or run `koma scan` again. A
rescan only re-reads books whose size or modification time changed, and it
forgets books that were removed. `serve` picks up the new catalogue on the
next request; it does not need a restart.

Until the first scan has finished, `/` answers 503.

## Health and logs

- `GET /healthz` answers `ok`. The image's `HEALTHCHECK` runs
  `koma healthcheck`, so `docker ps` shows the server as healthy. The worker
  answers no HTTP, so turn the check off for it, as the Compose file does.
- Logs are JSON lines on stderr (`docker logs koma`). A scan ends with a
  `scan: done` line that counts the books indexed, unchanged, failed and
  removed.
- `SIGTERM` (`docker stop`) stops both modes cleanly within a second.

## Settings

| Flag | Variable | Default |
|---|---|---|
| `--listen <ip:port>` | `KOMA_LISTEN` | `0.0.0.0:8080` |
| `--library <dir>` | `KOMA_LIBRARY` | `/library` |
| `--data <dir>` | `KOMA_DATA` | `/data` |

A flag wins over its variable.
