# UI design proposal

Static mockups of the chosen direction for every screen, the reader included.
Open any `.html` file here in a browser; they share `koma.css` and use no JavaScript.

| File | Screen |
|---|---|
| `library.html` | the library: continue hero and cover grid |
| `series.html` | a series and its volumes |
| `table.html` | the library as a real `<table>`, sorted by a heading link |
| `search.html` | search results across libraries, grouped by kind |
| `collections.html` | collections as stacked covers, then what one holds |
| `settings.html` | the libraries and their settings, editable by the admin (M5), with a change queued for the worker |
| `signin.html` | sign in: name and password, or a passkey |
| `setup.html` | first start: the admin account, claimed with a code from the server's log |
| `join.html` | a one-time sign-in link: the new account chooses its password |
| `account.html` | your account: password, passkeys, devices signed in, where your progress is |
| `users.html` | user management (admin): roles, sign-in links, removing, handing over the admin role |
| `states.html` | empty and error states, one block per screen |
| `reader.html` | reading: the page only, with the tap zones shown once on the first open |
| `reader-bars.html` | the bars a tap shows: title and back; scrubber with page preview, thumbnails, mode, fit, direction |
| `reader-double.html` | two pages side by side, right to left, with the one-page shift |
| `reader-strip.html` | a webtoon chapter: one vertical scroll, chosen by itself for tall strips |
| `reader-end.html` | the end of a book: the next one, or back to the series |
| `reader-epub.html` | an EPUB: a column of text, its size, width and spacing, and the contents |

The reader pages use `reader.css` (not `koma.css`); part of it only draws
the made-up pages.
They follow `prefers-color-scheme`, so switch your OS theme to see dark and light.

All titles, authors and users in the mockups are made up.

## Direction

A dense, monospace, keyboard-first interface ("flight deck") with panel framing
and a dotted halftone fill borrowed from print manga. One accent colour (amber),
system fonts only, no icon fonts, no animation. It should look as plausible in
a terminal-era screenshot as in a modern browser.

## Layout

```
┌──────────────┬─────────────────────────────────────────────┬───────────┐
│ KOMA-1       │ / search all libraries                      │ M user ▾  │  top bar
├──────────────┼─────────────────────────────────────────────┴───────────┤
│ // go        │ ┌ continue (hero) ───────────────┐ ┌ also reading ┐    │
│ // libraries │ │ big cover · title · read on    │ ├ also reading ┤    │
│ // colls     │ └────────────────────────────────┘ └ also reading ┘    │
│ status       │ ~/manga 214 series · sort · filter        grid|table   │
│ (pinned)     │ cover cover cover cover                                │
│              │ cover cover cover cover      4 columns, scrolls on     │
│              │ cover cover cover cover                                │
│              │ … more rows load as you scroll …                       │
└──────────────┴────────────────────────────────────────────────────────┘
```

- **Top bar** holds only the brand, search and the user menu. The brand cell is
  exactly as wide as the side panel, so their edges line up.
- **Side panel** is flush left, 260 px, and pinned (`position: sticky`) so it never
  scrolls with the library. Top to bottom:
  1. `// go`: library, recent, collections, ingest, settings, each with a `g x` shortcut.
  2. `// libraries`: a file tree. The open library expands to reading, unread and
     finished; the others stay collapsed. Several libraries are first-class.
  3. `// collections`, then one line of ingest status.
  On a series page, `// this series` and `// more by …` follow the libraries.
- Below 760 px the panel folds away behind a `menu` button in the top bar and opens
  above the content when tapped. The button is a label for a checkbox, so it works
  without JavaScript, and the checkbox can be reached with the keyboard.

## Cover card (progress on the cover)

Progress lives on the cover, so the text under it stays clean:

- a 7 px **edge strip** along the bottom, filled to the read percentage;
- a **corner tag**: the percentage while reading, `NEW` for unread or new volumes,
  `READ` when finished;
- finished covers are desaturated and dimmed.

The same card is used for series in the library grid, volumes on the series page
and the continue hero. Under the cover: title, then one line of facts (volumes, pages).

## Library page

- **Hero**: the series you're reading now (large cover, title, position, pages left,
  `read on` with the `r` shortcut) next to three "also reading" cards.
- **Grid**: 4 columns that scroll on without end, with a grid/table toggle.
  The table view (not drawn yet) is a real `<table>`, as the manifesto requires.

## Series page

A dotted panel with the cover (offset amber shadow) next to a panel with the
title, chips (author, volume count, status, size, reading direction), the summary
and actions. Volumes below use the same cover card.

## Infinite scroll

The library scrolls without end; there are no next and previous buttons.

1. The server renders the grid in chunks of 24–36 series. When the last row
   approaches the viewport, the next chunk's HTML is fetched and appended.
2. The camera trick: only rows near the viewport hold images. Rows far from it
   keep their height but drop their covers. Every cover has a fixed 2:3 ratio,
   so heights are known, nothing shifts, and memory stays flat however long
   the library is.
3. The URL follows the position with `history.replaceState`, so reload and back
   return to the same spot.

Covers are immutable and cached for a year, so scrolling back costs no network.
Without JavaScript the same chunks are plain pages (`?page=2`) with a next link,
so the fallback is almost free and each chunk stays under the 15 KB budget.

## Tokens

| Token | Dark | Light | Use |
|---|---|---|---|
| `--bg` | `#0F1215` | `#F4F5F6` | page |
| `--panel` | `#14181C` | `#EBEDEF` | side panel, dotted fill |
| `--line` / `--line2` | `#2A313A` / `#3A424C` | `#C9CED4` / `#A9B1BA` | rules, borders |
| `--text` / `--dim` | `#D6DCE1` / `#8A949E` | `#1B2026` / `#4F5862` | text |
| `--accent` | `#F2A33A` | `#B86E00` | the one accent |

Font: `ui-monospace, "SF Mono", Menlo, Consolas, "Liberation Mono", monospace`.

## Open items

- `koma.css` is about 7.7 KB raw (2.4 KB gzipped), over the 5 KB CSS budget.
  It carries both pages plus the dotted fill and light theme; it needs trimming
  when it becomes the real stylesheet.
- Not designed yet: ingest.
- Phone layout is only the stacked fallback; a dedicated phone design is still to do.

## Libraries and collections (M3)

- Every top-level folder under `/library` is a library (`/library/manga`,
  `/library/cosplay`…), with default settings for its type until changed.
- Inside a library, a folder that holds series folders is a collection, shown as
  a stack of covers that opens to its series.
- A library's settings come from its folder: the name, capitalised; the type from
  the name (manga, comics, webtoon, cosplay…), manga if it says nothing; right to
  left for manga, left to right otherwise; a colour from a fixed list in folder
  order. The settings page shows them read-only. Editing them comes with accounts
  (M5): `koma serve` will queue a change and the worker apply it, so the worker
  stays the catalogue's only writer.

## Search (M3)

SQLite FTS5 with the trigram tokenizer over series names, book titles, authors,
genres and tags, so any part of a word matches and Japanese and Korean titles
need no word splitter. `author:`, `tag:` and `genre:` narrow it. Results are
server-rendered and grouped: series, books, then tags and people.

## Reader (M4)

Chosen with the owner:

- **The page only.** No chrome while reading. A tap in the middle third (or
  a key) shows the bars: on top the series, book and chapter, the page, and
  the keys; at the bottom a scrubber with a preview of the page under it, the
  thumbnail strip (`t`), and single / double / strip, fit page / width /
  height and right to left / left to right. Another tap hides them.
- **Tap thirds that follow the direction.** In right-to-left books the left
  third goes forward and the right third back; left-to-right, the other way.
  Swipes and keys (arrows, `h`/`l`, `j`/`k`, space) too. The zones are
  shown once, faintly, the first time a reader opens.
- **Double pages** sit side by side, both always on screen (scaled to fit a
  narrow window), with a one-page shift (`s`) for books whose spreads start
  on the left. A page wider than tall is shown alone.
- **Tall strips** switch to one vertical scroll by themselves.
- **At the end** of a book: the next one, with its cover, or back to the
  series.
- **EPUB:** a column of serif text on paper, with size, width and spacing,
  and a table of contents. One chapter at a time, scrolled (not cut into
  screens). The book's own CSS and fonts are dropped: every book reads in the
  reader's typography. Horizontal for every book for now; vertical writing
  (Japanese books) comes later.
- **Progress and reader settings** stay in the browser (localStorage) until
  accounts (M5), which move them to the account so they follow you between
  devices.

## Accounts (M5)

Chosen with the owner:

- **Password and passkeys.** Every account has a password (argon2id); passkeys
  can be added and used wherever the site is on HTTPS or localhost. Over plain
  HTTP on a home network only the password works and the passkey button is
  not shown.
- **Your own folder is `/library/~name`.** A top-level folder named after an
  account with a `~` in front holds that account's private libraries
  (`/library/~mirelle/sketchbooks`). Nobody else sees them, the admin
  included, and a private book, series or library answers 404 to anyone
  else, the same as one that does not exist. Every other top-level folder is
  shared. In the side panel the private libraries are under `// only yours`.
- **`koma serve` writes only its own database**, `<data>/accounts.db`:
  accounts, sessions, passkeys, progress, reader settings, and a queue of
  library-setting changes. The catalogue stays the worker's alone; the
  worker applies queued changes on its next pass (within a minute).

Also chosen with the owner (built in M5 as proposed here):

- **First start.** With no account, every page leads to `/setup`. Creating
  the admin there needs a code that `koma serve` prints to its log at start,
  so whoever reaches the page first on the network cannot claim the server.
- **No passwords set by the admin.** Adding an account (or "new sign-in link")
  makes a link that works once, for 24 hours, where the person chooses their
  own password. A lost admin password: `koma account link <name>` prints a
  link from the server's shell.
- **Roles.** Reader: read, download, own progress and settings. Curator: also
  organises (M6 on). Admin: also accounts, roles, shared library settings.
  One admin; handing the role over (confirmed with the password) makes the
  old admin a curator.
- **Sessions.** A random token in an `HttpOnly`, `SameSite=Lax` cookie
  (`Secure` on HTTPS), signed with a key in the data folder and stored only
  as a hash; the account page lists each device and signs one out. "Keep me
  signed in" lasts 90 days from the last visit; without it, until the
  browser closes, and at most a day unused. Changing the password signs out the other devices. Every
  form that changes something also checks the request's `Origin`.
- **Sign-in through a proxy.** `--trust-header Remote-User` with
  `--trust-proxy <address>`: the header is believed only from that address.
  A name with no account gets one, as a reader (the very first, the admin).
- **Progress and reader settings** move from the browser to the account. On
  the first sign-in in a browser, what it kept is sent once and added where
  the account has nothing for that book or series.
- **Removing an account** keeps its folder on disk, unseen, until an account
  of that name exists again; its progress and sessions are deleted.
- **Failed sign-ins**: after 5 wrong passwords in a row a name waits a
  minute before the next try.
- Not built: the mockup's "undo" on a queued settings change (change it
  back instead).
- **Page images stay public.** `/p/<hash>` URLs name a page's content and are
  cached for a year by any cache; only someone who already has the page can
  know its hash. Every HTML page, `/b/`, `/e/` and `/d/` need a signed-in
  account that may see the book.
