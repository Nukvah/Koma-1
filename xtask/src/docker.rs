//! Thin wrappers around the `docker` command line.

use std::process::Command;

pub fn run(args: &[&str]) -> Result<String, String> {
    let output = Command::new("docker")
        .args(args)
        .output()
        .map_err(|e| format!("cannot run docker: {e}"))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
    } else {
        Err(format!(
            "docker {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

pub fn inspect(id: &str, format: &str) -> Result<String, String> {
    run(&["inspect", "--format", format, id])
}

/// A container that is removed when dropped, even if a check fails midway.
pub struct Container {
    pub id: String,
}

impl Container {
    pub fn start(args: &[&str]) -> Result<Self, String> {
        let mut full = vec!["run", "--detach"];
        full.extend_from_slice(args);
        Ok(Self { id: run(&full)? })
    }

    /// Resident memory of the container's main process, read from the host's /proc.
    pub fn rss_bytes(&self) -> Result<u64, String> {
        let pid = inspect(&self.id, "{{.State.Pid}}")?;
        let status = std::fs::read_to_string(format!("/proc/{pid}/status"))
            .map_err(|e| format!("/proc/{pid}/status: {e}"))?;
        parse_vm_rss(&status).ok_or_else(|| format!("no VmRSS for pid {pid}"))
    }
}

impl Drop for Container {
    fn drop(&mut self) {
        let _ = run(&["rm", "--force", &self.id]);
    }
}

fn parse_vm_rss(status: &str) -> Option<u64> {
    let line = status.lines().find(|l| l.starts_with("VmRSS:"))?;
    let kib: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
    Some(kib * 1024)
}

#[cfg(test)]
mod tests {
    #[test]
    fn parses_vm_rss() {
        let status = "Name:\tkoma\nVmPeak:\t  9000 kB\nVmRSS:\t    3172 kB\nThreads:\t3\n";
        assert_eq!(super::parse_vm_rss(status), Some(3172 * 1024));
        assert_eq!(super::parse_vm_rss("Name:\tx\n"), None);
    }
}
