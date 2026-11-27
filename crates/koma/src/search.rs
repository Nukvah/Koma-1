//! Search over the catalogue: series (by name, and by their books' people,
//! genres and tags) and books (by title).
//!
//! The index is an FTS5 table with the trigram tokenizer, so any part of a
//! word matches and Japanese or Korean titles need no word splitter. Trigrams
//! need three characters: a shorter term (`港`, `x2`) is matched with LIKE
//! instead, on the same rows.
//!
//! A query is words, `"quoted phrases"`, and `author:`, `tag:` or `genre:`
//! before either to look only there. Every term must match.

/// Where a term looks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Field {
    /// Names, titles, people, genres and tags.
    Any,
    People,
    Genres,
    Tags,
}

impl Field {
    fn column(self) -> Option<&'static str> {
        match self {
            Field::Any => None,
            Field::People => Some("people"),
            Field::Genres => Some("genres"),
            Field::Tags => Some("tags"),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Term {
    pub field: Field,
    pub text: String,
}

/// Most terms read from one query; the rest are ignored.
const MAX_TERMS: usize = 8;

/// Most characters of a query that are read.
const MAX_QUERY: usize = 200;

/// Splits a query into terms: words and `"phrases"`, each maybe prefixed by
/// `author:`, `tag:` or `genre:` (`people:` too). Empty terms are dropped.
pub fn parse(query: &str) -> Vec<Term> {
    let query: String = query.chars().take(MAX_QUERY).collect();
    let mut terms = Vec::new();
    let mut rest = query.trim_start();
    while !rest.is_empty() && terms.len() < MAX_TERMS {
        let (field, after) = match rest.split_once(':') {
            Some((prefix, after)) if !prefix.contains(char::is_whitespace) => {
                match prefix.to_lowercase().as_str() {
                    "author" | "people" => (Field::People, after),
                    "tag" => (Field::Tags, after),
                    "genre" => (Field::Genres, after),
                    _ => (Field::Any, rest),
                }
            }
            _ => (Field::Any, rest),
        };
        let (text, next) = match after.strip_prefix('"') {
            Some(quoted) => quoted.split_once('"').unwrap_or((quoted, "")),
            None => after.split_once(char::is_whitespace).unwrap_or((after, "")),
        };
        let text = text.trim();
        if !text.is_empty() {
            terms.push(Term {
                field,
                text: text.to_owned(),
            });
        }
        rest = next.trim_start();
    }
    terms
}

/// A parsed query ready for [`crate::catalog::Catalog::search`]: an FTS5
/// query for terms of three characters or more, and LIKE patterns for the
/// shorter ones.
#[derive(Debug, PartialEq, Eq)]
pub struct Plan {
    pub fts: Option<String>,
    pub likes: Vec<(&'static str, String)>,
}

pub fn plan(terms: &[Term]) -> Plan {
    let mut fts = Vec::new();
    let mut likes = Vec::new();
    for t in terms {
        if t.text.chars().count() >= 3 {
            // A quoted FTS5 string: the term as typed, nothing it holds is
            // read as query syntax.
            let quoted = format!("\"{}\"", t.text.replace('"', "\"\""));
            fts.push(match t.field.column() {
                Some(c) => format!("{c} : {quoted}"),
                None => quoted,
            });
        } else {
            likes.push((t.field.column().unwrap_or("any"), like_pattern(&t.text)));
        }
    }
    Plan {
        fts: (!fts.is_empty()).then(|| fts.join(" AND ")),
        likes,
    }
}

/// `%text%` with LIKE's own characters escaped by `\`.
pub fn like_pattern(text: &str) -> String {
    let mut p = String::from("%");
    for c in text.chars() {
        if matches!(c, '%' | '_' | '\\') {
            p.push('\\');
        }
        p.push(c);
    }
    p.push('%');
    p
}

/// The byte ranges of `text` where any of `needles` occurs, ignoring case,
/// merged and in order: what to mark in a result.
pub fn matches(text: &str, needles: &[&str]) -> Vec<(usize, usize)> {
    // Compare character by character, each lowered on its own, so ranges
    // stay on the original's character boundaries.
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    let lower = |c: char| c.to_lowercase().next().unwrap_or(c);
    let mut found: Vec<(usize, usize)> = Vec::new();
    for needle in needles {
        let n: Vec<char> = needle.chars().map(lower).collect();
        if n.is_empty() || n.len() > chars.len() {
            continue;
        }
        for start in 0..=chars.len() - n.len() {
            if (0..n.len()).all(|i| lower(chars[start + i].1) == n[i]) {
                let from = chars[start].0;
                let to = chars.get(start + n.len()).map_or(text.len(), |(i, _)| *i);
                found.push((from, to));
            }
        }
    }
    found.sort_unstable();
    let mut merged: Vec<(usize, usize)> = Vec::new();
    for (from, to) in found {
        match merged.last_mut() {
            Some(last) if from <= last.1 => last.1 = last.1.max(to),
            _ => merged.push((from, to)),
        }
    }
    merged
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(field: Field, text: &str) -> Term {
        Term {
            field,
            text: text.into(),
        }
    }

    #[test]
    fn parses_words_phrases_and_fields() {
        assert_eq!(
            parse(r#"  harbor  tag:"harbor town" author:Vance genre:drama x"#),
            [
                t(Field::Any, "harbor"),
                t(Field::Tags, "harbor town"),
                t(Field::People, "Vance"),
                t(Field::Genres, "drama"),
                t(Field::Any, "x"),
            ]
        );
        // Not a known field: the whole word is the term.
        assert_eq!(parse("re:zero"), [t(Field::Any, "re:zero")]);
        // An unclosed quote runs to the end; empty terms are dropped.
        assert_eq!(parse(r#""star fall"#), [t(Field::Any, "star fall")]);
        assert_eq!(parse(r#"tag:"" """#), []);
        assert_eq!(parse("a b c d e f g h i j").len(), MAX_TERMS);
    }

    #[test]
    fn short_terms_go_to_like() {
        let p = plan(&parse(r#"harbor 港 tag:sea x"y"#));
        // `x"y` is three characters: a trigram term, its quote doubled.
        assert_eq!(
            p.fts.as_deref(),
            Some(r#""harbor" AND tags : "sea" AND "x""y""#)
        );
        assert_eq!(p.likes, [("any", "%港%".into())]);
        // Quotes inside a term stay text.
        let p = plan(&parse(r#"a"b"c"d"#));
        assert_eq!(p.fts.as_deref(), Some(r#""a""b""c""d""#));
        assert!(p.likes.is_empty());
        let p = plan(&[t(Field::Any, r#"say "hi""#)]);
        assert_eq!(p.fts.as_deref(), Some(r#""say ""hi""""#));
        assert_eq!(like_pattern("50%_\\"), "%50\\%\\_\\\\%");
    }

    #[test]
    fn marks_matches_ignoring_case() {
        assert_eq!(matches("Glass Harbor", &["harbor"]), [(6, 12)]);
        assert_eq!(
            matches("Harbor of harbors", &["HARBOR"]),
            [(0, 6), (10, 16)]
        );
        // Overlapping and touching matches merge; multibyte text keeps its
        // character boundaries.
        assert_eq!(matches("星の港の港", &["港", "の港"]), [(3, 15)]);
        assert_eq!(matches("港と港", &["港"]), [(0, 3), (6, 9)]);
        assert_eq!(matches("Ébauche", &["éb"]), [(0, 3)]);
        assert_eq!(matches("abc", &["", "abcd"]), []);
    }
}
