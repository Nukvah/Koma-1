# Running Koma-1

Koma-1 is one image with two long-running modes that share two folders:

| Folder | Mount | Holds |
|---|---|---|
| `/library` | read-write for the worker, read-only for serve | your books: CBZ, ZIP, CB7 (7z) and EPUB files, and folders of images. Only the worker writes here, and only the changes you make in the UI (moves, renames, the trash, `metadata.json`), each journaled and undoable. |
| `/data` | read-write | the catalogue (`koma.db`), the blob store (`blobs/`), the accounts (`accounts.db`), uploads waiting to be filed (`uploads/`) and the monthly upload log (`logs/uploads-YYYY-MM.log`) |

`koma worker` scans `/library` into `/data`. `koma serve` reads both and
answers HTTP on port 8080; the only things it writes are `accounts.db` and uploads in `/data/uploads`.
Mount a folder on the library's drive at `/data/uploads` and filing an upload is a rename instead of a copy. Run them as two containers on the same volumes,
or run `koma scan` once and then only `serve`.

## Laying out /library

Each top-level folder that holds series folders is a **library**; a folder of
series folders inside a library is a **collection**. Series folders, loose
books and photo folders straight in `/library` go in the default library,
"Library", so a flat layout works as it is.

```text
/library/
  Starfall/Starfall v01.cbz               series Starfall, in "Library"
  Lone Comet.cbz                          a loose book, in "Library"
  manga/Moonforge/Moonforge v01.cbz       series Moonforge, in library "Manga"
  manga/Starfall Saga/Starfall Zero/…     collection "Starfall Saga", in "Manga"
  comics/Harbor Lights/…                  library "Comics"
```

A library's settings come from its folder's name: the name, capitalised; the
type: comics if the name says "comic", cosplay if it says "cosplay", manga
otherwise; the reading direction: right to left for manga, left to right for
the others (a book's own metadata wins); and a colour. `/settings` shows them,
and the admin can change them there: Koma-1 keeps the change in its catalogue
(the worker writes it within a few seconds when idle) and renames nothing.

A top-level folder named `~` and an account's name is that account's own:

```text
/library/
  ~mirelle/sketchbooks/Moonforge/…        library "Sketchbooks", seen by mirelle alone
  ~mirelle/Quiet Orbit/…                  a series straight in it: library "~mirelle"
```

Nobody else sees those libraries, the admin included; their owner edits their
settings. A folder whose account does not exist (yet, or any more) is seen by
nobody.

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
      - /path/to/library:/library   # read-write: moves, renames and the trash
      - koma-data:/data
    read_only: true
    cap_drop: [ALL]
    security_opt: ["no-new-privileges:true"]
    restart: unless-stopped
    # Readers first: when both are busy, serve gets four times the CPU.
    cpu_shares: 256

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

## Accounts

Every page needs an account; only page images, stylesheets and scripts, and
`/healthz` do not.

- **The first start.** With no account, every page leads to `/setup`, which
  makes the admin. It asks for a setup code that `koma serve` prints in its log
  when it starts (`docker logs koma`), so nobody else on the network can claim
  the server first. `koma account link <name>` (below) works too.
- **Adding people.** The admin adds an account on `/users` and gets a sign-in
  link to pass on; it works once, for 24 hours, and lets the person choose
  their password. A forgotten password: a new link. Roles: **reader** (read,
  download), **curator** (also organise the library, as M6 and later bring it),
  **admin** (also accounts and library settings). There is one admin, who can
  hand the role over and becomes a curator.
- **Passkeys** can be added on `/account` and used to sign in, where the site
  is on HTTPS (or `localhost`): browsers allow them nowhere else. Over plain
  HTTP on a home network, passwords work and the passkey buttons are hidden.
- **Sessions** last 90 days from the last visit with "keep me signed in";
  otherwise until the browser closes, and at most a day unused. `/account`
  lists every device and signs any of them out. After 5 wrong passwords in a
  row, a name waits a minute before each next try.
- **A lost admin password**, from the server's shell:

  ```sh
  docker exec koma /koma account link mirelle
  ```

  prints `/join/…`: open it on your server's address. With no account yet,
  it makes the admin.
- **A sign-in proxy** (oauth2-proxy, Authelia, …) in front of Koma-1:

  ```sh
  koma serve --trust-header Remote-User --trust-proxy 172.18.0.5
  ```

  The header names the account and is believed only from those addresses
  (comma-separated; networks as `10.0.0.0/8`): list only the proxy's own,
  and let nothing else reach `koma serve` from them. A name the proxy
  sends that has no account gets one, as a reader (the very first, the
  admin). Variables: `KOMA_TRUST_HEADER`, `KOMA_TRUST_PROXY`.
- **HTTPS.** Run Koma-1 behind a proxy that terminates TLS (Caddy, say) and
  sends `X-Forwarded-Proto: https`; the session cookie is then `Secure`.

Reading positions and reader settings belong to the account and follow it to
every device. What a browser kept before accounts (M4 kept them in the
browser) is added to the account at its first sign-in there, where the
account has nothing for that book or series, then cleared from the browser.

## Organising and the trash

Curators and the admin organise the shared libraries; everyone organises
their own `~name` folder. On a series page, **select** picks books to mark
read or unread (anyone, for their own reading), move to another series or
a new one, edit, or delete; **move or rename** moves the series' folder to
another library or collection, renames it, or merges it into another
series.

- Every move is shown first as a plan: each file's old and new place, and
  “(2)” where a name is taken (or skip that one). Nothing changes until it
  is applied.
- `koma serve` only queues the change; `koma worker` carries it out within
  a moment, so it needs `/library` read-write. A short change shows at once;
  a long one runs on `/jobs`, live, with pause, resume and cancel, and goes
  on after a restart. Without a running worker, nothing is changed.
- Books and series keep their ids, reading positions, edits and dates
  through every move. Every file touched is written in an append-only
  journal; **undo** puts it all back, as long as the files are as the change
  left them.
- **Delete** moves the files to `/library/.koma-trash` (same drive, so it
  is instant), and the books leave the library at once. `/trash` restores
  them (making any folder that has gone since, and adding “(2)” if the name
  is taken) or deletes them for good. The worker erases what is older than
  the keep time: 30 days unless the admin chooses 7, 14, 90 or “until
  emptied”. Moving between two drives (a `~name` folder mounted apart, say)
  is refused.
- **Details** (title, number, year, people, genres, tags, summary,
  direction, cover) are kept in the catalogue and win over the files at
  every scan. On request they are written as `metadata.json` next to the
  book (`Starfall v07.metadata.json`; inside a photo folder), which Koma-1
  reads at the next index too; an archive's own bytes are never rewritten.

## Naming

A series' page (and, from there, its library's) has **naming**: the plan of renaming its books to the naming
standard, `(Event) [Circle (Author)] Series - Ch. 003 [EN].cbz`, numbers padded to the series' largest (see
[naming.md](naming.md)). Untick what you want left; applying it is one journaled change with undo. Books
Koma-1 cannot name safely (no number in a series of several, or two that would get one name) go to the
**fix-up queue** (`/fixup`): fill what it could not read and rename, or keep the name a file has.

A **cosplay library** (by its name or its settings) is kept in `Cosplayer / Set /` folders by the worker
itself, after each scan and after each change: a journaled change of its own on `/jobs`, with undo. A set
with no cosplayer in its name is left where it is, in the fix-up queue. A tidy undone is not done again.

## Scanning

The worker scans when it starts and then every 15 minutes (`--rescan`), so
books you add, change or remove show up on their own. `serve` picks up the
new catalogue on the next request; it does not need a restart.

Rescans are cheap. A folder whose modification time hasn't changed is not
read again, and a book whose size and modification time haven't changed is
never re-indexed, so a restart re-reads nothing. A book overwritten in place
doesn't change its folder's time; the worker looks at every known book once
a day and on each start, and `koma scan` always does, so those are found too.

Books and series keep their ids, and the date they were added, when you move
or rename them, also with a file manager. A book that disappears is kept out
of sight for 30 days, so a drive mounted late or a folder put back brings its
books back as they were, without reading them again.

The worker makes page copies on every CPU it may use (`--concurrency`), as
many pages at once as fit in half its memory, and at the lowest priority
(nice 19), so that it takes what is left over. Nice only orders the threads
of one container, though: between containers the kernel shares CPU by
weight, which is why the Compose file gives the worker `cpu_shares: 256`
(serve keeps the default 1024). A CPU limit (`cpus:`) caps the worker too;
`--concurrency` follows it.

Only one worker or `koma scan` can use a data folder at a time; a second one
exits with an error. A worker that is stopped or killed in the middle of a
book picks that book up again when it restarts. A book that stops the worker
three times in a row is skipped and logged.

Until the first scan has finished, `/` answers 503.

## Health and logs

- `GET /healthz` answers `ok`. The image's `HEALTHCHECK` runs
  `koma healthcheck`, so `docker ps` shows the server as healthy. The worker
  answers no HTTP, so turn the check off for it, as the Compose file does.
- Logs are JSON lines on stderr (`docker logs koma`). A scan ends with a
  `scan: done` line that counts the books indexed, unchanged, failed and
  removed, the folders read and skipped and the pages given copies, with the
  time it took (`ms`) and the CPU time it used (`cpu_ms`).
- `SIGTERM` (`docker stop`) stops `serve` cleanly within a second, and the
  worker after the book it is indexing.

## Settings

| Flag | Variable | Default |
|---|---|---|
| `--listen <ip:port>` | `KOMA_LISTEN` | `0.0.0.0:8080` |
| `--library <dir>` | `KOMA_LIBRARY` | `/library` |
| `--data <dir>` | `KOMA_DATA` | `/data` |
| `--rescan <minutes>` (worker) | `KOMA_RESCAN` | `15`; `0` scans only at start |
| `--concurrency <pages>` (worker, scan) | `KOMA_CONCURRENCY` | `auto`: one page per CPU koma may use |
| `--trust-header <name>` (serve) | `KOMA_TRUST_HEADER` | off |
| `--trust-proxy <addresses>` (serve) | `KOMA_TRUST_PROXY` | none |

A flag wins over its variable.
