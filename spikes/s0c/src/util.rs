//! Spike utilities: paths, time, crash injection, hashing, evidence report.

use std::path::{Path, PathBuf};

pub fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Crate root = spikes/s0c (never a temp directory).
pub fn manifest_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

pub fn runs_root() -> PathBuf {
    manifest_dir().join("runs")
}

pub fn case_dir(name: &str) -> PathBuf {
    runs_root().join(name)
}

/// Delete and recreate a case's run directory.
///
/// WARNING: This deletes files. The target is confined to spikes/s0c/runs/,
/// which is gitignored and holds only regenerated spike artifacts. It never
/// touches anything outside the spike tree and never uses a system temp dir.
pub fn reset_dir(p: &Path) -> std::io::Result<()> {
    println!(
        "WARNING: deleting and recreating spike run directory at {} (confined to spikes/s0c/runs/).",
        p.display()
    );
    if p.exists() {
        std::fs::remove_dir_all(p)?;
    }
    std::fs::create_dir_all(p)
}

/// Abort the process when `SPIKE_CRASH_AT` selects `point`.
pub fn maybe_crash(point: &str) {
    if let Ok(v) = std::env::var("SPIKE_CRASH_AT") {
        if v == point {
            eprintln!("[crash-inject] deliberate abort at injection point '{point}'");
            std::process::abort();
        }
    }
}

pub fn fnv1a64(s: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100_0000_01b3);
    }
    h
}

pub fn input_hash(inputs: &str) -> String {
    format!("{:016x}", fnv1a64(inputs))
}

pub struct Report {
    pub case: String,
    pub checks: Vec<(bool, String, String)>,
    pub notes: Vec<String>,
}

impl Report {
    pub fn new(case: &str) -> Self {
        Report {
            case: case.to_string(),
            checks: Vec::new(),
            notes: Vec::new(),
        }
    }

    pub fn check(&mut self, name: &str, pass: bool, evidence: impl Into<String>) {
        self.checks.push((pass, name.to_string(), evidence.into()));
    }

    pub fn note(&mut self, s: impl Into<String>) {
        self.notes.push(s.into());
    }

    pub fn passed(&self) -> bool {
        !self.checks.is_empty() && self.checks.iter().all(|(p, _, _)| *p)
    }

    pub fn render(&self) -> String {
        let mut out = format!("=== case {} ===\n", self.case);
        for n in &self.notes {
            out.push_str(&format!("note: {n}\n"));
        }
        for (pass, name, ev) in &self.checks {
            out.push_str(&format!(
                "{} {name} :: {ev}\n",
                if *pass { "PASS" } else { "FAIL" }
            ));
        }
        out.push_str(&format!(
            "VERDICT: {}\n",
            if self.passed() { "PASS" } else { "FAIL" }
        ));
        out
    }

    /// Print and persist the evidence file under the case run directory.
    pub fn emit(&self, dir: &Path) -> std::io::Result<()> {
        let text = self.render();
        print!("{text}");
        std::fs::create_dir_all(dir)?;
        std::fs::write(dir.join("evidence.txt"), &text)?;
        Ok(())
    }
}
