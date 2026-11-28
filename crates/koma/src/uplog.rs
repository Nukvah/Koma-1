//! The upload log: `<data>/logs/uploads-YYYY-MM.log`, one short entry per
//! drop, written by the worker when it has filed the drop. Files that went
//! in fine are only counted; every one that did not gets a line of its own
//! with the reason, so a failure can be traced after the message is gone.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

/// How a drop went.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Outcome {
    pub by: String,
    pub library: String,
    /// Files added.
    pub added: usize,
    /// Skipped: the same bytes were already there.
    pub already: usize,
    /// Files that could not go in, with the reason (from the worker or the
    /// browser).
    pub failed: Vec<(String, String)>,
    pub cancelled: usize,
    pub ignored: usize,
}

impl Outcome {
    /// Files the drop meant to add.
    pub fn total(&self) -> usize {
        self.added + self.already + self.failed.len() + self.cancelled
    }

    /// `5 out of 7 added without problems (1 already there) · 1 failed`.
    pub fn summary(&self) -> String {
        let mut s = format!(
            "{} out of {} added without problems",
            self.added + self.already,
            self.total()
        );
        if self.already > 0 {
            s.push_str(&format!(" ({} already there)", self.already));
        }
        if !self.failed.is_empty() {
            s.push_str(&format!(" · {} failed", self.failed.len()));
        }
        if self.cancelled > 0 {
            s.push_str(&format!(" · {} cancelled", self.cancelled));
        }
        if self.ignored > 0 {
            s.push_str(&format!(" · {} ignored", self.ignored));
        }
        s
    }
}

/// The month `unix` falls in, as `YYYY-MM`.
pub fn month(unix: i64) -> String {
    crate::html::date(unix)[..7].to_owned()
}

/// The log file of the month `unix` falls in.
pub fn path(data: &Path, unix: i64) -> PathBuf {
    month_path(data, &month(unix))
}

/// The log file of a month, `YYYY-MM`.
pub fn month_path(data: &Path, month: &str) -> PathBuf {
    data.join("logs").join(format!("uploads-{month}.log"))
}

/// Whether `m` is a month as `YYYY-MM`.
pub fn is_month(m: &str) -> bool {
    let b = m.as_bytes();
    b.len() == 7
        && b[4] == b'-'
        && b.iter()
            .enumerate()
            .all(|(i, c)| i == 4 || c.is_ascii_digit())
        && matches!(
            &m[5..],
            "01" | "02" | "03" | "04" | "05" | "06" | "07" | "08" | "09" | "10" | "11" | "12"
        )
}

/// The months with a log, newest first.
pub fn months(data: &Path) -> Vec<String> {
    let mut out: Vec<String> = fs::read_dir(data.join("logs"))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let n = e.file_name().into_string().ok()?;
            let m = n.strip_prefix("uploads-")?.strip_suffix(".log")?.to_owned();
            is_month(&m).then_some(m)
        })
        .collect();
    out.sort_unstable_by(|a, b| b.cmp(a));
    out
}

/// Who made a drop and the library it went to, from an entry's head.
pub fn parties(head: &str) -> Option<(&str, &str)> {
    let (_, rest) = head.split_once(" UTC · ")?;
    let (by, lib) = rest.rsplit_once(" → library “")?;
    Some((by, lib.strip_suffix('”')?))
}

/// Appends a drop's entry.
pub fn write(data: &Path, unix: i64, o: &Outcome) -> io::Result<()> {
    let path = path(data, unix);
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let secs = unix.rem_euclid(86_400);
    let mut entry = format!(
        "{} {:02}:{:02} UTC · {} → library “{}”\n  {}\n",
        crate::html::date(unix),
        secs / 3600,
        secs % 3600 / 60,
        o.by,
        o.library,
        o.summary()
    );
    for (name, why) in &o.failed {
        entry.push_str(&format!("  ✗ {name}: {why}\n"));
    }
    let mut f = OpenOptions::new().create(true).append(true).open(path)?;
    f.write_all(entry.as_bytes())
}

/// The log's entries for a month (`YYYY-MM`), newest first, as `(head,
/// summary, failures)`.
pub fn read(data: &Path, month: &str) -> Vec<(String, String, Vec<String>)> {
    if !is_month(month) {
        return Vec::new();
    }
    let Ok(text) = fs::read_to_string(month_path(data, month)) else {
        return Vec::new();
    };
    let mut out: Vec<(String, String, Vec<String>)> = Vec::new();
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("  ✗ ") {
            if let Some(last) = out.last_mut() {
                last.2.push(rest.to_owned());
            }
        } else if let Some(rest) = line.strip_prefix("  ") {
            if let Some(last) = out.last_mut() {
                last.1 = rest.to_owned();
            }
        } else if !line.is_empty() {
            out.push((line.to_owned(), String::new(), Vec::new()));
        }
    }
    out.reverse();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_entry_per_drop_failures_written_out() {
        let data = std::env::temp_dir().join(format!("koma-uplog-{}", std::process::id()));
        let _ = fs::remove_dir_all(&data);
        let t = 1_795_000_000; // 2026-11-18
        let ok = Outcome {
            by: "tamsin".into(),
            library: "manga".into(),
            added: 3,
            ..Outcome::default()
        };
        write(&data, t, &ok).unwrap();
        let bad = Outcome {
            by: "mirelle".into(),
            library: "cosplay".into(),
            added: 2,
            already: 1,
            failed: vec![(
                "Starfall 03.cbz".into(),
                "This archive is damaged: 001.jpg".into(),
            )],
            cancelled: 1,
            ignored: 2,
        };
        write(&data, t + 60, &bad).unwrap();
        let text = fs::read_to_string(path(&data, t)).unwrap();
        assert!(path(&data, t).ends_with("logs/uploads-2026-11.log"));
        assert!(
            text.contains("  3 out of 3 added without problems\n"),
            "{text}"
        );
        assert!(text.contains(
            "3 out of 5 added without problems (1 already there) · 1 failed · 1 cancelled · 2 ignored"
        ));
        assert!(text.contains("  ✗ Starfall 03.cbz: This archive is damaged: 001.jpg\n"));
        let entries = read(&data, &month(t));
        assert_eq!(entries.len(), 2);
        assert!(entries[0].0.contains("mirelle"), "newest first");
        assert_eq!(parties(&entries[0].0), Some(("mirelle", "cosplay")));
        assert_eq!(months(&data), vec!["2026-11".to_owned()]);
        assert!(read(&data, "../x").is_empty() && !is_month("2026-13"));
        assert_eq!(entries[0].2.len(), 1);
    }
}
