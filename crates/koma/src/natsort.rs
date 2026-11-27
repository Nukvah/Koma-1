//! Natural sort: "page 2" before "page 10", the way people number pages,
//! chapters and volumes.
//!
//! - Runs of digits compare by value, so `2 < 10` and `007 == 7` (ties are
//!   broken at the end, so the order is still total and stable).
//! - Full-width digits (`０`–`９`, common in Japanese names) count as digits.
//! - Letters compare case-insensitively.
//! - Numbers sort before letters and punctuation, so `10 < 10.5 < 11 < 11a`.
//! - Paths compare folder by folder, and a file's extension only breaks ties
//!   between equal names, so `1.jpg < 1-2.jpg < 2.jpg`.

use std::cmp::Ordering;
use std::iter::Peekable;
use std::str::Chars;

/// Compares two names naturally. Equal only when the strings are identical.
pub fn compare(a: &str, b: &str) -> Ordering {
    compare_loose(a, b).then_with(|| a.cmp(b))
}

/// Compares two `/`-separated paths component by component, naturally.
pub fn compare_paths(a: &str, b: &str) -> Ordering {
    let mut x = a.split('/');
    let mut y = b.split('/');
    loop {
        let ord = match (x.next(), y.next()) {
            (None, None) => break,
            (None, Some(_)) => Ordering::Less,
            (Some(_), None) => Ordering::Greater,
            (Some(p), Some(q)) => compare_file_names(p, q),
        };
        if ord != Ordering::Equal {
            return ord;
        }
    }
    a.cmp(b)
}

/// Compares stems first, then extensions.
fn compare_file_names(a: &str, b: &str) -> Ordering {
    let (stem_a, ext_a) = split_extension(a);
    let (stem_b, ext_b) = split_extension(b);
    compare_loose(stem_a, stem_b).then_with(|| compare_loose(ext_a, ext_b))
}

fn split_extension(name: &str) -> (&str, &str) {
    match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() && looks_like_extension(ext) => (stem, ext),
        _ => (name, ""),
    }
}

/// `jpg`, `png`, `cbz`… but not the `5` of "Ch. 10.5" or the ` 10` of "Ch. 10".
fn looks_like_extension(ext: &str) -> bool {
    (1..=5).contains(&ext.len())
        && ext.chars().all(|c| c.is_ascii_alphanumeric())
        && ext.chars().any(|c| c.is_ascii_alphabetic())
}

/// Natural comparison without the final tie-break.
fn compare_loose(a: &str, b: &str) -> Ordering {
    let mut x = a.chars().peekable();
    let mut y = b.chars().peekable();
    loop {
        let (Some(&p), Some(&q)) = (x.peek(), y.peek()) else {
            return x.peek().is_some().cmp(&y.peek().is_some());
        };
        let ord = match (digit(p), digit(q)) {
            (Some(_), Some(_)) => compare_numbers(&mut x, &mut y),
            (Some(_), None) => Ordering::Less,
            (None, Some(_)) => Ordering::Greater,
            (None, None) => {
                x.next();
                y.next();
                p.to_lowercase().cmp(q.to_lowercase())
            }
        };
        if ord != Ordering::Equal {
            return ord;
        }
    }
}

/// Consumes a run of digits from each side and compares their values,
/// without parsing (runs can be longer than any integer type).
fn compare_numbers(x: &mut Peekable<Chars<'_>>, y: &mut Peekable<Chars<'_>>) -> Ordering {
    let a = digit_run(x);
    let b = digit_run(y);
    let a = trim_zeros(&a);
    let b = trim_zeros(&b);
    a.len().cmp(&b.len()).then_with(|| a.cmp(b))
}

fn digit_run(chars: &mut Peekable<Chars<'_>>) -> Vec<u8> {
    let mut run = Vec::new();
    while let Some(d) = chars.peek().and_then(|&c| digit(c)) {
        run.push(d);
        chars.next();
    }
    run
}

fn trim_zeros(run: &[u8]) -> &[u8] {
    let first = run.iter().position(|&d| d != 0).unwrap_or(run.len());
    &run[first..]
}

/// ASCII and full-width decimal digits.
fn digit(c: char) -> Option<u8> {
    match c {
        '0'..='9' => Some(c as u8 - b'0'),
        '０'..='９' => Some((c as u32 - '０' as u32) as u8),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sorted(names: &[&str]) -> Vec<String> {
        let mut v: Vec<String> = names.iter().map(|s| s.to_string()).collect();
        v.sort_by(|a, b| compare_paths(a, b));
        v
    }

    #[test]
    fn numbers_by_value() {
        assert_eq!(
            sorted(&["10.png", "2.png", "1.png", "01.png", "100.png"]),
            ["01.png", "1.png", "2.png", "10.png", "100.png"]
        );
    }

    #[test]
    fn chapters_with_decimals_and_suffixes() {
        assert_eq!(
            sorted(&["Ch. 11", "Ch. 10.5", "Ch. 10", "Ch. 11a", "ch. 9"]),
            ["ch. 9", "Ch. 10", "Ch. 10.5", "Ch. 11", "Ch. 11a"]
        );
    }

    #[test]
    fn case_insensitive_with_stable_ties() {
        assert_eq!(sorted(&["b", "A", "a", "B"]), ["A", "a", "B", "b"]);
        assert_eq!(compare("Page", "page"), Ordering::Less);
        assert_eq!(compare("x", "x"), Ordering::Equal);
    }

    #[test]
    fn full_width_digits() {
        assert_eq!(
            sorted(&["page１０.png", "page２.png", "page1.png"]),
            ["page1.png", "page２.png", "page１０.png"]
        );
    }

    #[test]
    fn extension_breaks_ties_only() {
        assert_eq!(
            sorted(&["2.jpg", "1-2.jpg", "1.jpg", "1.png"]),
            ["1.jpg", "1.png", "1-2.jpg", "2.jpg"]
        );
    }

    #[test]
    fn folders_before_their_contents_order() {
        assert_eq!(
            sorted(&["b/1.png", "a/10.png", "a/9.png", "cover.png"]),
            ["a/9.png", "a/10.png", "b/1.png", "cover.png"]
        );
    }

    #[test]
    fn huge_numbers_do_not_overflow() {
        let big = "9".repeat(60);
        let bigger = format!("1{}", "0".repeat(60));
        assert_eq!(compare(&big, &bigger), Ordering::Less);
    }

    #[test]
    fn total_order_on_samples() {
        let samples = [
            "", "0", "00", "1", "01", "1a", "a1", "A1", "a01", "a.1", "a-1", "a 1", "１", "z",
            "10", "9", "1.5", "1.10", "x/1", "x/01", "x", ".hidden", "1.jpg", "1-2.jpg", "1.png",
            "Ch. 10", "Ch. 10.5", "a.b.c", "x/1.jpg",
        ];
        for a in samples {
            for b in samples {
                assert_eq!(compare(a, b), compare(b, a).reverse(), "{a:?} {b:?}");
                assert_eq!(compare(a, b) == Ordering::Equal, a == b);
                assert_eq!(compare_paths(a, b), compare_paths(b, a).reverse());
                assert_eq!(compare_paths(a, b) == Ordering::Equal, a == b);
                for c in samples {
                    if compare(a, b).is_le() && compare(b, c).is_le() {
                        assert!(compare(a, c).is_le(), "{a:?} {b:?} {c:?}");
                    }
                    if compare_paths(a, b).is_le() && compare_paths(b, c).is_le() {
                        assert!(compare_paths(a, c).is_le(), "{a:?} {b:?} {c:?}");
                    }
                }
            }
        }
    }
}
