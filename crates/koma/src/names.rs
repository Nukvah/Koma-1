//! What a file name says about a book: its series, its chapter or volume
//! number, and who made it. The rules are the naming baseline in
//! `docs/naming.md`; `tests/naming_baseline.rs` pins every example there.
//!
//! Plain string scanning, no regular expressions: names are short, and every
//! loop here is bounded by the name's length.

/// Names longer than this are cut before parsing (no real name is this long).
const MAX_NAME: usize = 1024;

/// Whether a number is a chapter or a volume.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Chapter,
    Volume,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Chapter => "chapter",
            Kind::Volume => "volume",
        }
    }
}

/// What a manga or comics file name says.
#[derive(Clone, Debug, PartialEq)]
pub struct BookName {
    /// The series named before the number; `None` when the name is only a
    /// number (`Ch. 12.cbz`) or has nothing left once brackets are gone.
    pub series: Option<String>,
    pub kind: Option<Kind>,
    pub number: Option<f64>,
    pub author: Option<String>,
    pub publisher: Option<String>,
}

/// Reads a manga or comics file name (without its extension).
pub fn book_name(stem: &str) -> BookName {
    let stem = cut(stem);
    let (kind, number, before) = parse_number(stem);
    let series = if number.is_some() {
        // `Starfall Vol. 2 Ch. 10`: the volume is not part of the series.
        match parse_number(&before) {
            (Some(Kind::Volume), Some(_), head) if kind == Some(Kind::Chapter) => tidy(&head),
            _ => tidy(&before),
        }
    } else {
        tidy(&strip_brackets(stem, ""))
    };
    let numbered = kind.is_some();
    let (author, publisher) = credits(stem, numbered);
    BookName {
        series: (!series.is_empty()).then_some(series),
        kind,
        number,
        author: (!author.is_empty()).then_some(author),
        publisher: (!publisher.is_empty()).then_some(publisher),
    }
}

/// What a cosplay file or folder name says:
/// `[Photographer / Publisher] Cosplayer - Set - Variant`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CosplayName {
    /// The photographer or publisher in the leading brackets: metadata only.
    pub circle: String,
    pub cosplayer: String,
    pub set: String,
    /// After a second ` - `: `Bonus`, `Normal`, …
    pub variant: String,
}

pub fn cosplay_name(stem: &str) -> CosplayName {
    let stem = cut(stem);
    let mut circle = String::new();
    let mut cosplayer = String::new();
    let mut rest = stem;
    if let Some((inner, end)) = leading_credit(stem) {
        match circle_and_artist(&inner) {
            Some((c, a)) => (circle, cosplayer) = (c, a),
            None => circle = inner,
        }
        rest = &stem[end..];
    }
    if is_not_a_credit(&circle) {
        circle.clear();
    }
    let mut work = tidy(&strip_brackets(rest, ""));
    if cosplayer.is_empty()
        && let Some((who, what)) = work.split_once(" - ")
    {
        cosplayer = who.trim().to_owned();
        work = what.trim().to_owned();
    }
    // A second " - " marks the variant: "Starfall Bunny - Bonus".
    let (set, variant) = match work.rsplit_once(" - ") {
        Some((head, tail)) if !tidy(head).is_empty() && !tidy(tail).is_empty() => {
            (tidy(head), tidy(tail))
        }
        _ => (work, String::new()),
    };
    CosplayName {
        circle,
        cosplayer,
        set,
        variant,
    }
}

fn cut(s: &str) -> &str {
    match s.char_indices().nth(MAX_NAME) {
        Some((i, _)) => &s[..i],
        None => s,
    }
}

fn is_open(c: char) -> bool {
    matches!(c, '[' | '(' | '{')
}

fn is_bracket(c: char) -> bool {
    matches!(c, '[' | ']' | '(' | ')' | '{' | '}')
}

fn closes(open: char, close: char) -> bool {
    matches!((open, close), ('[', ']') | ('(', ')') | ('{', '}'))
}

/// Removes bracketed groups (`[..]`, `(..)`, `{..}`) and the space before
/// each, innermost first until none is left, so nested groups like
/// `[Circle (Artist)]` go as a whole. Each group is replaced by `repl`.
pub fn strip_brackets(s: &str, repl: &str) -> String {
    let mut s = s.to_owned();
    loop {
        let chars: Vec<(usize, char)> = s.char_indices().collect();
        let mut found = None;
        let mut last_open: Option<usize> = None; // index into chars
        for (k, &(_, c)) in chars.iter().enumerate() {
            if is_open(c) {
                last_open = Some(k);
            } else if is_bracket(c) {
                if let Some(o) = last_open
                    && closes(chars[o].1, c)
                {
                    found = Some((o, k));
                    break;
                }
                last_open = None;
            }
        }
        let Some((o, k)) = found else {
            return s;
        };
        let mut start = o;
        while start > 0 && chars[start - 1].1.is_whitespace() {
            start -= 1;
        }
        let from = chars[start].0;
        let to = chars[k].0 + chars[k].1.len_utf8();
        s.replace_range(from..to, repl);
    }
}

/// Runs of spaces and underscores become one space; spaces, dashes,
/// underscores and dots at either end go.
pub fn tidy(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut gap = false;
    for c in s.chars() {
        if c.is_whitespace() || c == '_' {
            gap = true;
        } else {
            if gap && !out.is_empty() {
                out.push(' ');
            }
            gap = false;
            out.push(c);
        }
    }
    out.trim_matches(|c: char| matches!(c, ' ' | '-' | '_' | '.'))
        .to_owned()
}

/// Words that mark a number: `Ch. 3`, `Vol 2`, `#5`, `Episode 10`…
const NUMBER_WORDS: [&str; 12] = [
    "chapter", "episode", "volume", "chap", "part", "vol", "ch", "pt", "ep", "c", "v", "#",
];

fn is_separator(c: char) -> bool {
    c.is_whitespace() || matches!(c, '_' | '-' | '.')
}

/// The chapter or volume number in a name, and the text before it.
///
/// The last marked number counts (`Ch. 3`, `Vol.2`, `#5`, after a space,
/// `_`, `-`, `.` or at the start); `v`/`vol`/`volume` make it a volume.
/// Otherwise a bare number of up to four digits at the very end counts, with
/// no kind. Brackets are ignored.
pub fn parse_number(stem: &str) -> (Option<Kind>, Option<f64>, String) {
    let s = strip_brackets(stem, " ");
    let chars: Vec<(usize, char)> = s.char_indices().collect();
    let mut best: Option<(usize, Kind, f64)> = None; // (byte start, kind, number)
    for p in 0..chars.len() {
        // A match starts at the start of the name or at a separator.
        let (word_at, start) = if p == 0 && !is_separator(chars[0].1) {
            (0, 0)
        } else if is_separator(chars[p].1) {
            (p + 1, chars[p].0)
        } else {
            continue;
        };
        let byte = chars.get(word_at).map_or(s.len(), |&(b, _)| b);
        if let Some((kind, number)) = marked_number(&s[byte..]) {
            best = Some((start, kind, number));
        }
    }
    if let Some((start, kind, number)) = best {
        return (Some(kind), Some(number), s[..start].to_owned());
    }
    // A bare number at the end: "\s(\d{1,4}(\.\d+)?)\s*$".
    let trimmed = s.trim_end();
    let tail_start = trimmed
        .char_indices()
        .rev()
        .take_while(|&(_, c)| c.is_ascii_digit() || c == '.')
        .last()
        .map(|(i, _)| i);
    if let Some(i) = tail_start
        && let Some(before) = trimmed[..i].chars().last()
        && before.is_whitespace()
        && let Some(n) = number_text(&trimmed[i..], 4)
    {
        let space = trimmed[..i].len() - before.len_utf8();
        return (None, Some(n), s[..space].to_owned());
    }
    (None, None, s)
}

/// At the start of `rest`: a number word, an optional dot, spaces, and a
/// number that ends the name or is followed by a separator.
fn marked_number(rest: &str) -> Option<(Kind, f64)> {
    for word in NUMBER_WORDS {
        if !rest
            .get(..word.len())
            .is_some_and(|p| p.eq_ignore_ascii_case(word))
        {
            continue;
        }
        let mut i = word.len();
        if rest[i..].starts_with('.') {
            i += 1;
        }
        while rest[i..].starts_with(char::is_whitespace) {
            i += rest[i..].chars().next()?.len_utf8();
        }
        let digits_end = rest[i..]
            .char_indices()
            .find(|&(_, c)| !(c.is_ascii_digit() || c == '.'))
            .map_or(rest.len(), |(j, _)| i + j);
        // The number may not swallow a trailing dot: "Ch. 3." is 3.
        let mut text = &rest[i..digits_end];
        while text.ends_with('.') {
            text = &text[..text.len() - 1];
        }
        let end = i + text.len();
        let after_ok = rest[end..].chars().next().is_none_or(is_separator);
        if let Some(n) = number_text(text, usize::MAX)
            && after_ok
        {
            let kind = if word.starts_with('v') {
                Kind::Volume
            } else {
                Kind::Chapter
            };
            return Some((kind, n));
        }
    }
    None
}

/// `12` or `12.5`: digits, optionally a dot and more digits. At most
/// `max_int` digits before the dot.
fn number_text(t: &str, max_int: usize) -> Option<f64> {
    let (int, frac) = match t.split_once('.') {
        Some((i, f)) => (i, Some(f)),
        None => (t, None),
    };
    let digits = |x: &str| !x.is_empty() && x.len() <= 18 && x.bytes().all(|b| b.is_ascii_digit());
    if !digits(int) || int.len() > max_int || frac.is_some_and(|f| !digits(f)) {
        return None;
    }
    t.parse().ok()
}

/// Leading bracket words that are not people.
const NOT_A_CREDIT: [&str; 58] = [
    "english",
    "eng",
    "en",
    "japanese",
    "jp",
    "jpn",
    "raw",
    "raws",
    "chinese",
    "cn",
    "korean",
    "kr",
    "spanish",
    "french",
    "german",
    "russian",
    "portuguese",
    "italian",
    "digital",
    "decensored",
    "uncensored",
    "censored",
    "colorized",
    "colourized",
    "color",
    "colour",
    "full color",
    "translated",
    "textless",
    "hd",
    "hq",
    "lq",
    "complete",
    "completed",
    "ongoing",
    "oneshot",
    "one shot",
    "sample",
    "preview",
    "anthology",
    "various",
    "unknown",
    "n/a",
    "dl版",
    "中国翻訳",
    "無修正",
    "中国語",
    "english translated",
    "fan edition",
    "webtoon",
    "eng translated",
    "official",
    "scan",
    "scans",
    "fixed",
    "remastered",
    "uncut",
    "sfw",
];

fn is_not_a_credit(v: &str) -> bool {
    let low = v.to_lowercase();
    NOT_A_CREDIT.contains(&low.as_str())
}

/// The first `[..]` of a name, after any number of leading `(..)` groups
/// (an event: `(SF12) [Circle (Author)] …`). Returns its inside with spaces
/// collapsed, and the byte index just after the `]`.
fn leading_credit(s: &str) -> Option<(String, usize)> {
    let mut rest = s.trim_start();
    while let Some(r) = rest.strip_prefix('(') {
        let close = r.find(')')?;
        if r[..close].contains('(') {
            return None;
        }
        rest = r[close + 1..].trim_start();
    }
    let r = rest.strip_prefix('[')?;
    let close = r.find(']')?;
    let inner = &r[..close];
    if inner.is_empty() || inner.contains('[') {
        return None;
    }
    let end = s.len() - r.len() + close + 1;
    Some((inner.split_whitespace().collect::<Vec<_>>().join(" "), end))
}

/// `Circle (Artist)` → `(Circle, Artist)`.
fn circle_and_artist(inner: &str) -> Option<(String, String)> {
    let t = inner.trim_end();
    let body = t.strip_suffix(')')?;
    let open = body.rfind('(')?;
    let artist = body[open + 1..].trim();
    let circle = body[..open].trim();
    if artist.is_empty() || artist.contains(')') || circle.is_empty() {
        return None;
    }
    Some((circle.to_owned(), artist.to_owned()))
}

/// Things in brackets that are never names: numbers, `c90`, `1080p`, `v2`,
/// `1920x1080`.
fn is_junk(v: &str) -> bool {
    if v.is_empty() || is_not_a_credit(v) {
        return true;
    }
    let low = v.to_lowercase();
    let all = |f: fn(char) -> bool, s: &str| !s.is_empty() && s.chars().all(f);
    let digit = |c: char| c.is_ascii_digit();
    all(
        |c| c.is_ascii_digit() || c.is_whitespace() || matches!(c, '.' | '_' | '-' | '/'),
        &low,
    ) || low
        .strip_prefix('c')
        .is_some_and(|d| (2..=3).contains(&d.len()) && all(digit, d))
        || low
            .strip_suffix(['p', 'k'])
            .is_some_and(|d| (3..=4).contains(&d.len()) && all(digit, d))
        || low.strip_prefix('v').is_some_and(|d| all(digit, d))
        || low
            .split_once('x')
            .is_some_and(|(a, b)| all(digit, a) && all(digit, b))
}

/// `(author, publisher)` from a leading `[..]`: `[Circle (Author)]` gives
/// both; `[Author] - Series` gives the author. For numbered books a bare
/// `[Name] Series Ch. 05` is more often a scanlation group, so it isn't
/// trusted. Empty strings when nothing is credited.
pub fn credits(stem: &str, numbered: bool) -> (String, String) {
    let none = (String::new(), String::new());
    let Some((inner, end)) = leading_credit(stem) else {
        return none;
    };
    let pair = circle_and_artist(&inner);
    let (circle, artist) = pair.clone().unwrap_or_else(|| (String::new(), inner));
    let after = stem[end..].trim_start();
    let dashed = after
        .strip_prefix('-')
        .is_some_and(|r| r.starts_with(char::is_whitespace));
    if numbered && pair.is_none() && !dashed {
        return none;
    }
    if is_junk(&artist) {
        return none;
    }
    let circle = if is_junk(&circle) {
        String::new()
    } else {
        circle
    };
    (artist, circle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn brackets_go_innermost_first() {
        assert_eq!(
            strip_brackets("[Moonforge (Mirelle)] Starfall", ""),
            " Starfall"
        );
        assert_eq!(strip_brackets("Starfall (2012) {x}[y]", ""), "Starfall");
        assert_eq!(
            strip_brackets("unbalanced ] ( here", ""),
            "unbalanced ] ( here"
        );
    }

    #[test]
    fn tidy_names() {
        assert_eq!(tidy("  Starfall__Academy -  "), "Starfall Academy");
        assert_eq!(tidy("...Moon.forge..."), "Moon.forge");
    }

    #[test]
    fn numbers() {
        let n = |s: &str| {
            let (k, n, before) = parse_number(s);
            (k, n, tidy(&before))
        };
        let ch = Some(Kind::Chapter);
        let vol = Some(Kind::Volume);
        assert_eq!(n("Starfall - Ch. 03"), (ch, Some(3.0), "Starfall".into()));
        assert_eq!(
            n("Starfall Vol.2 Ch.10.5"),
            (ch, Some(10.5), "Starfall Vol.2".into())
        );
        assert_eq!(n("Starfall v02"), (vol, Some(2.0), "Starfall".into()));
        assert_eq!(n("Starfall_#7"), (ch, Some(7.0), "Starfall".into()));
        assert_eq!(n("Starfall 12"), (None, Some(12.0), "Starfall".into()));
        assert_eq!(n("Starfall 12345"), (None, None, "Starfall 12345".into()));
        assert_eq!(n("Chapter 4 [Group]"), (ch, Some(4.0), "".into()));
        assert_eq!(
            n("Starfall Academy"),
            (None, None, "Starfall Academy".into())
        );
        // A word that only starts like a marker is not one.
        assert_eq!(
            n("Starfall Chronicle 2"),
            (None, Some(2.0), "Starfall Chronicle".into())
        );
        assert_eq!(n("Vanilla 3"), (None, Some(3.0), "Vanilla".into()));
    }

    #[test]
    fn credits_from_names() {
        let c = |s: &str, numbered| credits(s, numbered);
        let pair = |a: &str, b: &str| (a.to_owned(), b.to_owned());
        assert_eq!(
            c("[Moonforge (Mirelle)] Starfall - Ch. 01", true),
            pair("Mirelle", "Moonforge")
        );
        assert_eq!(
            c("(SF12) [Moonforge (Mirelle)] Starfall", true),
            pair("Mirelle", "Moonforge")
        );
        assert_eq!(
            c("[Mirelle] - Starfall - Ch. 01", true),
            pair("Mirelle", "")
        );
        assert_eq!(c("[Inkwell Scans] Starfall Ch. 05", true), pair("", ""));
        assert_eq!(c("[Mirelle] Starfall", false), pair("Mirelle", ""));
        assert_eq!(c("[English] Starfall", false), pair("", ""));
        assert_eq!(c("[1080p] Starfall", false), pair("", ""));
        assert_eq!(
            c("[Digital (Mirelle)] Starfall", false),
            pair("Mirelle", "")
        );
    }

    #[test]
    fn cosplay() {
        let c = cosplay_name("[Lumen Studio] Mirelle - Starfall Bunny - Bonus");
        assert_eq!(
            (
                c.circle.as_str(),
                c.cosplayer.as_str(),
                c.set.as_str(),
                c.variant.as_str()
            ),
            ("Lumen Studio", "Mirelle", "Starfall Bunny", "Bonus")
        );
        let c = cosplay_name("[Dreva(Solene)]Lucky Bunny Bonus");
        assert_eq!(
            (c.circle.as_str(), c.cosplayer.as_str(), c.set.as_str()),
            ("Dreva", "Solene", "Lucky Bunny Bonus")
        );
        let c = cosplay_name("Summer Beach");
        assert_eq!((c.cosplayer.as_str(), c.set.as_str()), ("", "Summer Beach"));
        let c = cosplay_name("[Digital] Mirelle - Bunny");
        assert_eq!(
            (c.circle.as_str(), c.cosplayer.as_str(), c.set.as_str()),
            ("", "Mirelle", "Bunny")
        );
    }

    #[test]
    fn long_and_odd_names_dont_panic() {
        let long = "[(".repeat(5000);
        book_name(&long);
        cosplay_name(&long);
        book_name("Ch.");
        book_name("ÄÖ Ch. 1 ǅ");
        book_name("İstanbul ch 3");
        parse_number("  ");
    }
}
