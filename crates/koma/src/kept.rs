//! What an account keeps between devices: the reading position in each
//! book, the reader settings of each series, and a few of the readers'
//! own. The readers' scripts read these from their page and send changes to
//! `POST /kept`; the keys are theirs (they were `localStorage` keys before
//! accounts, under `koma:`), checked here.
//!
//! | Key | Value |
//! |---|---|
//! | `pos:<book>` | the page reached (picture books) |
//! | `epos:<book>` | `<chapter>:<block>` (EPUB) |
//! | `dir:<series>`, `mode:<series>`, `shift:<series>`, `fit:<series>` | the reader's settings |
//! | `seen` | the tap zones were shown once |
//! | `esize`, `ewidth`, `espace` | the EPUB reader's text size, width, spacing |

/// Keys for one book and its series, as the reader asks for them.
pub fn reader_keys(book: i64, series: i64) -> Vec<String> {
    let mut keys: Vec<String> = ["dir", "mode", "shift", "fit"]
        .iter()
        .map(|k| format!("{k}:{series}"))
        .collect();
    keys.push(format!("pos:{book}"));
    keys.push("seen".into());
    keys
}

/// Keys for an EPUB.
pub fn epub_keys(book: i64) -> Vec<String> {
    vec![
        format!("epos:{book}"),
        "esize".into(),
        "ewidth".into(),
        "espace".into(),
    ]
}

/// Whether `key` may be kept, with `value`.
pub fn allowed(key: &str, value: &str) -> bool {
    let key_ok = match key.split_once(':') {
        None => matches!(key, "seen" | "esize" | "ewidth" | "espace"),
        Some((kind, id)) => {
            matches!(kind, "pos" | "epos" | "dir" | "mode" | "shift" | "fit")
                && !id.is_empty()
                && id.len() <= 18
                && id.bytes().all(|b| b.is_ascii_digit())
        }
    };
    key_ok
        && !value.is_empty()
        && value.len() <= 32
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b':' | b'.' | b'-' | b'_'))
}

/// The pairs of a form or JSON object that may be kept; others are left
/// out.
pub fn only_allowed<I: IntoIterator<Item = (String, String)>>(pairs: I) -> Vec<(String, String)> {
    pairs
        .into_iter()
        .filter(|(k, v)| allowed(k, v))
        .take(10_000)
        .collect()
}

/// What a browser kept before accounts, as the sign-in form sends it: a JSON
/// object of strings (the `localStorage` keys without `koma:`).
pub fn from_json(json: &str) -> Vec<(String, String)> {
    if json.is_empty() || json.len() > 1 << 20 {
        return Vec::new();
    }
    let Ok(serde_json::Value::Object(map)) = serde_json::from_str(json) else {
        return Vec::new();
    };
    only_allowed(map.into_iter().filter_map(|(k, v)| match v {
        serde_json::Value::String(s) => Some((k, s)),
        serde_json::Value::Number(n) => Some((k, n.to_string())),
        _ => None,
    }))
}

/// Pairs as a JSON object, for a page's `data-kept`.
pub fn to_json(pairs: &[(String, String)]) -> String {
    let map: serde_json::Map<String, serde_json::Value> = pairs
        .iter()
        .map(|(k, v)| (k.clone(), serde_json::Value::String(v.clone())))
        .collect();
    serde_json::Value::Object(map).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_and_values() {
        assert!(allowed("pos:12", "40"));
        assert!(allowed("epos:12", "3:17"));
        assert!(allowed("dir:5", "rtl"));
        assert!(allowed("seen", "1"));
        assert!(!allowed("pos:", "4"));
        assert!(!allowed("pos:x", "4"));
        assert!(!allowed("theme", "dark"));
        assert!(!allowed("pos:1", ""));
        assert!(!allowed("pos:1", "<script>"));
        assert!(!allowed("pos:1", &"9".repeat(33)));
    }

    #[test]
    fn json() {
        let got = from_json(r#"{"pos:4":"12","seen":1,"evil":"x","fit:2":"width","pos:5":{}}"#);
        assert_eq!(
            got,
            vec![
                ("fit:2".to_owned(), "width".to_owned()),
                ("pos:4".to_owned(), "12".to_owned()),
                ("seen".to_owned(), "1".to_owned()),
            ]
        );
        assert!(from_json("[1]").is_empty());
        assert!(from_json("not json").is_empty());
        assert_eq!(to_json(&got[..1]), r#"{"fit:2":"width"}"#);
    }
}
