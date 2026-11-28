//! Collects results and prints them as a table (terminal) and Markdown (CI summary).

use std::fmt::Write as _;
use std::fs::OpenOptions;
use std::io::Write as _;
use std::path::Path;

struct Row {
    name: String,
    measured: String,
    limit: String,
    ok: bool,
}

#[derive(Default)]
pub struct Report {
    rows: Vec<Row>,
}

impl Report {
    /// A budget: passes when `measured <= limit`.
    pub fn budget(&mut self, name: &str, measured: f64, limit: f64, unit: &str, note: &str) {
        let mut shown = format!("{measured:.1} {unit}");
        if !note.is_empty() {
            let _ = write!(shown, " ({note})");
        }
        self.rows.push(Row {
            name: name.to_owned(),
            measured: shown,
            limit: format!("{limit} {unit}"),
            ok: measured <= limit,
        });
    }

    /// A floor: passes when `measured >= min`.
    pub fn at_least(&mut self, name: &str, measured: f64, min: f64, unit: &str, note: &str) {
        let mut shown = format!("{measured:.2}{unit}");
        if !note.is_empty() {
            let _ = write!(shown, " ({note})");
        }
        self.rows.push(Row {
            name: name.to_owned(),
            measured: shown,
            limit: format!("at least {min}{unit}"),
            ok: measured >= min,
        });
    }

    /// A number that is shown, not judged.
    pub fn info(&mut self, name: &str, detail: impl Into<String>) {
        self.rows.push(Row {
            name: name.to_owned(),
            measured: detail.into(),
            limit: "shown".to_owned(),
            ok: true,
        });
    }

    /// A rule that has no number, such as "runs as non-root".
    pub fn check(&mut self, name: &str, ok: bool, detail: impl Into<String>) {
        self.rows.push(Row {
            name: name.to_owned(),
            measured: detail.into(),
            limit: "required".to_owned(),
            ok,
        });
    }

    pub fn passed(&self) -> bool {
        self.rows.iter().all(|r| r.ok)
    }

    pub fn text(&self) -> String {
        let w0 = self
            .rows
            .iter()
            .map(|r| r.name.len())
            .max()
            .unwrap_or(0)
            .max(6);
        let w1 = self
            .rows
            .iter()
            .map(|r| r.measured.len())
            .max()
            .unwrap_or(0)
            .max(8);
        let w2 = self
            .rows
            .iter()
            .map(|r| r.limit.len())
            .max()
            .unwrap_or(0)
            .max(5);
        let mut out = format!(
            "{:w0$}  {:w1$}  {:w2$}  result\n",
            "budget", "measured", "limit"
        );
        for r in &self.rows {
            let verdict = if r.ok { "ok" } else { "FAIL" };
            let _ = writeln!(
                out,
                "{:w0$}  {:w1$}  {:w2$}  {verdict}",
                r.name, r.measured, r.limit
            );
        }
        out
    }

    pub fn markdown(&self, image: Option<&str>) -> String {
        let mut out = String::from("### Budgets");
        if let Some(image) = image {
            let _ = write!(out, " ({}, {image})", std::env::consts::ARCH);
        }
        out.push_str("\n\n| Budget | Measured | Limit | Result |\n|---|---|---|---|\n");
        for r in &self.rows {
            let verdict = if r.ok { "✅" } else { "❌" };
            let _ = writeln!(
                out,
                "| {} | {} | {} | {verdict} |",
                r.name, r.measured, r.limit
            );
        }
        out.push('\n');
        out
    }

    pub fn append_markdown(&self, path: &Path, image: Option<&str>) -> Result<(), String> {
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .and_then(|mut f| f.write_all(self.markdown(image).as_bytes()))
            .map_err(|e| format!("{}: {e}", path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn over_budget_fails_the_report() {
        let mut r = Report::default();
        r.budget("Image", 39.9, 40.0, "MiB", "");
        r.budget("CSS", 10.0, 10.0, "KiB", "2 files");
        assert!(r.passed());
        r.budget("RAM", 40.1, 40.0, "MiB", "");
        assert!(!r.passed());
        assert!(r.text().contains("FAIL"));
        assert!(
            r.markdown(None)
                .contains("| RAM | 40.1 MiB | 40 MiB | ❌ |")
        );
    }

    #[test]
    fn failed_check_fails_the_report() {
        let mut r = Report::default();
        r.check("Non-root", false, "user root");
        assert!(!r.passed());
        assert_eq!(r.rows[0].limit, "required");
    }
}
