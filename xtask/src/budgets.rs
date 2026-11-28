//! `budgets.toml`, the single source of truth for every budget.

use std::path::Path;

use serde::Deserialize;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Budgets {
    pub image: Image,
    pub memory: Memory,
    pub latency: Latency,
    pub worker: Worker,
    #[serde(default)]
    pub assets: Vec<AssetGroup>,
    #[serde(default)]
    pub pages: Vec<Page>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Image {
    pub max_mib: f64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Memory {
    pub serve_idle_mib: f64,
    pub worker_idle_mib: f64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Latency {
    pub ttfb_p99_ms: f64,
    pub cold_start_ms: f64,
}

/// How the worker's speed grows with the CPUs it is given.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Worker {
    /// Speed-up per CPU from 2 CPUs to all of the runner's: 1 is perfect.
    pub scaling_min: f64,
}

/// All files under `dir` whose names end in `suffix`, summed (raw bytes).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AssetGroup {
    pub name: String,
    pub dir: String,
    pub suffix: String,
    pub max_kib: f64,
}

/// A server-rendered page, fetched from the running image.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Page {
    pub path: String,
    pub max_kib: f64,
}

impl Budgets {
    pub fn load(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        Self::parse(&text).map_err(|e| format!("{}: {e}", path.display()))
    }

    pub fn parse(text: &str) -> Result<Self, String> {
        toml::from_str(text).map_err(|e| e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repository_budgets_parse() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../budgets.toml");
        let b = Budgets::load(&path).unwrap();
        assert!(b.image.max_mib > 0.0);
        assert!(b.latency.ttfb_p99_ms > 0.0);
        assert!((0.0..=1.0).contains(&b.worker.scaling_min));
        assert!(b.assets.iter().any(|a| a.suffix == ".css"));
    }

    #[test]
    fn unknown_keys_are_rejected() {
        let text = "[image]\nmax_mib = 1\nmax_gib = 2\n";
        assert!(Budgets::parse(text).is_err());
    }
}
