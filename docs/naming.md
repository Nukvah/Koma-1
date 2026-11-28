# Naming baseline and metadata

What Koma-1 reads from a book's file name and from the metadata files inside it. Every example in the tables
below is a test: `crates/koma/tests/naming_baseline.rs` reads this file and checks each row, so the tables and
the code can't drift apart. Add a row here to pin a new case.

All names here are made up.

## Where a book's details come from

From strongest to weakest:

1. Details edited in Koma-1 (and answers given in the fix-up queue).
2. `<name>.metadata.json` next to the book: what Koma-1 writes when asked.
3. `metadata.json` inside the book.
4. `ComicInfo.xml` inside the book.
5. The file name, read by the rules below.

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

## Naming standard

What Koma-1 renames a manga or comics book to, when asked (the naming page of a series or a library):
`(Event) [Circle (Author)] Series - Ch. 003 [EN].cbz`. Each part only when known: from the book's details,
else from the series' other books (the author, circle and language most of them have). The event is kept
from the name it had. A chapter is `Ch.`, a volume `Vol.`, a number of unknown kind stands alone. Numbers
are padded to the largest of their kind in the series (`Largest` below), decimals kept. With an author and
no circle the name is `[Author] - Series`, which reads back as the author. Every name reads back, by the
rules above, as what it was made from, so a rename loses nothing.

A book goes to the fix-up queue instead when it has no number and is not its series' only book, or when
another file would get the same name.

| Name | Series | Kind | Number | Author | Circle | Event | Language | Largest |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| `(SF12) [Moonforge (Mirelle)] Starfall Academy - Ch. 003 [EN]` | Starfall Academy | chapter | 3 | Mirelle | Moonforge | SF12 | EN | 120 |
| `[Moonforge (Mirelle)] Starfall Academy - Ch. 3 [EN]` | Starfall Academy | chapter | 3 | Mirelle | Moonforge | — | EN | 9 |
| `[Mirelle] - Starfall Academy - Vol. 02` | Starfall Academy | volume | 2 | Mirelle | — | — | — | 14 |
| `Moonforge Chronicle - Ch. 010.5` | Moonforge Chronicle | chapter | 10.5 | — | — | — | — | 300 |
| `Harbor Lights - 07` | Harbor Lights | — | 7 | — | — | — | — | 12 |
| `[Moonforge (Mirelle)] Lone Comet` | Lone Comet | — | — | Mirelle | Moonforge | — | — | — |

In a cosplay library, the worker keeps every set in `Cosplayer / Set /` folders by itself, after each scan:
`[Lumen Studio] Mirelle - Starfall Bunny - Bonus.zip` goes to `Mirelle/Starfall Bunny/`, keeping its own
name. A set with no cosplayer in its name stays where it is, in the fix-up queue. Each tidy is a journaled
change; undone, it is not done again.

## Cosplay

`[Photographer / Publisher] Cosplayer - Set - Variant`, with or without the bracket. The bracket is metadata
only (the circle); `[Circle(Cosplayer)]Set` names the cosplayer too. A second ` - ` marks a variant (`Bonus`,
`Normal`…). The cosplay tidy (above) places sets by it.

| Name | Circle | Cosplayer | Set | Variant |
| --- | --- | --- | --- | --- |
| `[Lumen Studio] Mirelle - Starfall Bunny - Bonus` | Lumen Studio | Mirelle | Starfall Bunny | Bonus |
| `Mirelle - Starfall Bunny - Bonus` | — | Mirelle | Starfall Bunny | Bonus |
| `[Lumen Studio] Mirelle - Starfall Bunny` | Lumen Studio | Mirelle | Starfall Bunny | — |
| `Mirelle - Starfall Bunny` | — | Mirelle | Starfall Bunny | — |
| `[Dreva(Solene)]Lucky Bunny Bonus` | Dreva | Solene | Lucky Bunny Bonus | — |
| `[Digital] Mirelle - Bunny` | — | Mirelle | Bunny | — |
| `Summer Beach` | — | — | Summer Beach | — |
