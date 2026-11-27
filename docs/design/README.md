# UI design proposal

Static mockups of the chosen direction for every screen except the reader.
Open `library.html` or `series.html` in a browser; both use `koma.css` and no JavaScript.
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
│              │ cover cover cover cover      4 × 3 = 12 per page       │
│              │ cover cover cover cover                                │
│              │ keys · 1–12 of 214 · next ›                            │
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
- **Grid**: 4 columns × 3 rows, 12 series per page, with a grid/table toggle.
  The table view (not drawn yet) is a real `<table>`, as the manifesto requires.

## Series page

A dotted panel with the cover (offset amber shadow) next to a panel with the
title, chips (author, volume count, status, size, reading direction), the summary
and actions. Volumes below use the same cover card.

## Paging vs infinite scroll

Server-rendered pages with real URLs (`?page=2`) are the baseline. They work
without JavaScript, keep links and the back button correct, and keep each
library page under the 15 KB budget. Infinite scroll can be layered on as
progressive enhancement without losing speed:

1. When the last row approaches the viewport, fetch the next page's HTML and append it.
2. Update the URL with `history.replaceState` so reload and back return to the same spot.
3. Window the grid: rows far from the viewport keep their height but drop their
   images. Every cover has a fixed 2:3 ratio, so heights are known and nothing shifts.

Covers are immutable and cached for a year, so scrolling back costs no network.
Fetch 24–36 series per step.

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
