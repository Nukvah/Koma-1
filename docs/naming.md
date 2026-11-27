# Naming baseline and metadata

What Koma-1 reads from a book's file name and from the metadata files inside it. Every example in the tables
below is a test: `crates/koma/tests/naming_baseline.rs` reads this file and checks each row, so the tables and
the code can't drift apart. Add a row here to pin a new case.

All names here are made up.

## Where a book's details come from

From strongest to weakest:

1. `metadata.json` inside the book (what Koma-1 will write when you edit a book).
2. `ComicInfo.xml` inside the book.
3. The file name, read by the rules below.

A field set in a stronger layer wins. Genres and tags are combined instead, the stronger layer's first, without
repeats. The file name only fills what the metadata files leave empty: the number, the chapter or volume, the
author and the publisher. A metadata file can be in any folder of the archive (or in a photo folder) and is
matched without case. One over 1 MiB, or one that does not parse, is ignored: metadata never fails a book.

| `metadata.json` key | `ComicInfo.xml` field | Stored as |
| --- | --- | --- |
| `title` | `Title` | title |
| `series` (else `title`) | `Series` | series, for future use (grouping is by folder, below) |
| `number` | `Number` | chapter number |
| | `Volume` | volume number, when there is no chapter number |
| `summary` | `Summary` | summary |
| `artists` | `Writer` | authors |
| `groups` | `Publisher` | publishers |
| `genres` (else `type`) | `Genre` | genres |
| `tags` and `parodys` | `Tags` | tags |
| `year` (else the first four digits of `date`) | `Year` | year |
| `language` | `LanguageISO` | language |
| `direction` (`ltr` / `rtl`) | `Manga` = `YesAndRightToLeft` | reading direction |
| `thumbnail` | | cover page (0-based) |

A `metadata.json` tag can be a string or `{"tag": "…", "female": true, "male": false}`, which gives `tag ♀`
and `tag ♂`. Lists in `ComicInfo.xml` are separated by commas or semicolons.

Adding a format means adding one parser to `PARSERS` in `crates/koma/src/meta.rs`, placed by strength.

## Series

A book's series is the folder it is in. A book loose in the library root goes in the series its file name
names, so loose chapters of one series group together (`Starfall Academy - Ch. 01.cbz` and
`Starfall Academy - Ch. 02.cbz` are one series); a loose file whose name is only a number goes in a series
called "Unsorted"; a loose one-shot is a series of its own.

## Manga and comics

- The number is the last one marked `Ch.`, `Chapter`, `Vol.`, `v`, `#`, `Episode`, `Part` (any case, dot
  optional). `Vol.` and `v` make it a volume; the others a chapter. Without a marker, a trailing number of up
  to four digits is taken, without saying whether it is a chapter or a volume. No number: a one-shot.
- The series is what comes before the number, without bracketed groups; a volume before a chapter
  (`Vol. 2 Ch. 10`) is not part of it.
- `[Circle (Author)]` gives the author and the publisher. `[Author] - Series` (a ` - ` right after the
  bracket) gives the author. A bare `[Name] Series Ch. 05` is more often a scanlation group, so a numbered
  book does not trust it; a one-shot does. Tags like `[English]`, `[Digital]`, `[1080p]` are never credits.
- An event in front, `(SF12)`, and a language at the end, `[English]`, are ignored for now.

`—` means "none".

| File name | Series | Kind | Number | Author | Publisher |
| --- | --- | --- | --- | --- | --- |
| `(SF12) [Moonforge (Mirelle)] Starfall Academy - Ch. 03 [English].cbz` | Starfall Academy | chapter | 3 | Mirelle | Moonforge |
| `[Moonforge (Mirelle)] Starfall Academy - Ch. 01.cbz` | Starfall Academy | chapter | 1 | Mirelle | Moonforge |
| `[Mirelle] - Starfall Academy - Ch. 01.cbz` | Starfall Academy | chapter | 1 | Mirelle | — |
| `[Inkwell Scans] Starfall Academy Ch. 05.cbz` | Starfall Academy | chapter | 5 | — | — |
| `Starfall Academy - Ch. 10.5.cbz` | Starfall Academy | chapter | 10.5 | — | — |
| `Starfall Academy Chapter 4.cbz` | Starfall Academy | chapter | 4 | — | — |
| `Starfall Academy_#7.cbz` | Starfall Academy | chapter | 7 | — | — |
| `Starfall Academy Vol. 02.cbz` | Starfall Academy | volume | 2 | — | — |
| `Starfall Academy v02.cbz` | Starfall Academy | volume | 2 | — | — |
| `Starfall Academy Vol. 2 Ch. 10.cbz` | Starfall Academy | chapter | 10 | — | — |
| `Starfall Academy 07.cbz` | Starfall Academy | — | 7 | — | — |
| `Moonforge Chronicle 2.cbz` | Moonforge Chronicle | — | 2 | — | — |
| `Ch. 12.cbz` | — | chapter | 12 | — | — |
| `Lone Comet.cbz` | Lone Comet | — | — | — | — |
| `[Mirelle] Lone Comet.cbz` | Lone Comet | — | — | Mirelle | — |
| `[English] Lone Comet.cbz` | Lone Comet | — | — | — | — |
| `[Digital (Mirelle)] Lone Comet.cbz` | Lone Comet | — | — | Mirelle | — |

## Cosplay

`[Photographer / Publisher] Cosplayer - Set - Variant`, with or without the bracket. The bracket is metadata
only (the circle); `[Circle(Cosplayer)]Set` names the cosplayer too. A second ` - ` marks a variant (`Bonus`,
`Normal`…). Until library types arrive (milestone 3) this is checked by these tests only and not yet used at
ingest; grouping variants among sibling folders comes with it.

| Name | Circle | Cosplayer | Set | Variant |
| --- | --- | --- | --- | --- |
| `[Lumen Studio] Mirelle - Starfall Bunny - Bonus` | Lumen Studio | Mirelle | Starfall Bunny | Bonus |
| `Mirelle - Starfall Bunny - Bonus` | — | Mirelle | Starfall Bunny | Bonus |
| `[Lumen Studio] Mirelle - Starfall Bunny` | Lumen Studio | Mirelle | Starfall Bunny | — |
| `Mirelle - Starfall Bunny` | — | Mirelle | Starfall Bunny | — |
| `[Dreva(Solene)]Lucky Bunny Bonus` | Dreva | Solene | Lucky Bunny Bonus | — |
| `[Digital] Mirelle - Bunny` | — | Mirelle | Bunny | — |
| `Summer Beach` | — | — | Summer Beach | — |
