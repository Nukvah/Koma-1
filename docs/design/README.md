# UI design proposal

Static mockups of the chosen direction for every screen except the reader.
Open any `.html` file here in a browser; they share `koma.css` and use no JavaScript.

| File | Screen |
|---|---|
| `library.html` | the library: continue hero and cover grid |
| `series.html` | a series and its volumes |
| `table.html` | the library as a real `<table>`, sorted by a heading link |
| `search.html` | search results across libraries, grouped by kind |
| `collections.html` | collections as stacked covers, then what one holds |
| `settings.html` | one library's settings, with the "applied by the worker" notice |
| `states.html` | empty and error states, one block per screen |
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
- Below 760 px the panel stacks above the content and stops being sticky.

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
- Not designed yet: the reader, the table view, search results, ingest, settings, sign-in.
- Phone layout is only the stacked fallback; a dedicated phone design is still to do.

## Libraries and collections (M3)

- Every top-level folder under `/library` is a library (`/library/manga`,
  `/library/cosplay`…), with default settings for its type until changed.
- Inside a library, a folder that holds series folders is a collection, shown as
  a stack of covers that opens to its series.
- Settings (name, type, reading direction, colour) are changed in the UI, but
  `koma serve` never writes the catalogue: it queues the change, and the worker
  applies it within seconds. The page says so until it has.

## Search (M3)

SQLite FTS5 with the trigram tokenizer over series names, book titles, authors,
genres and tags, so any part of a word matches and Japanese and Korean titles
need no word splitter. `author:`, `tag:` and `genre:` narrow it. Results are
server-rendered and grouped: series, books, then tags and people.
