//! Reap heavy execution run directories once an execution is terminal
//! (`agalma-36y`).
//!
//! A run directory (`<state>/runs/<exec>`) accumulates heavyweight temp trees
//! over an execution's lifetime: the checkout clone, the agent `target/`, the
//! verify sandbox homes/tmp, the isolated cargo home, and the OpenCode XDG
//! runtime dirs. They are pure scratch: nothing reads them after the execution
//! reaches a terminal state, and together they reach gigabytes.
//!
//! This module deletes exactly those scratch subdirectories — best-effort and
//! symlink-safe — while preserving the small forensic evidence a postmortem
//! needs: `artifacts/` (digest, acceptance captures, diagnosis), `stop/`
//! (survivor-termination proof), `opencode-log-*`, `effects.log`, the phase
//! markers, and any other small file.
//!
//! Reaping is skipped entirely when a live worker record (`*.worker.json`)
//! remains in a phase directory: that execution may still have a live process
//! writing to its scratch, so deletion waits for a later sweep.

use std::path::Path;

use agalma_contracts::{ExecutionId, ExecutionPhase, ExecutionState, LedgerApi};

use crate::ledger_process::LedgerHandle;
use crate::phases::run_root;

/// Heavy scratch subdirectories under a run root, safe to delete on terminal.
///
/// Mirrors the union of the workspace checkout (`checkout`), the confined
/// build (`agent-target`, `cargo-home`), the verify sandbox (`verify`,
/// `verify-tmp`, `verify-home`), and the OpenCode runtime state
/// (`xdg-*`, `tmp`, `protected`, `sock`, `work`). The phase marker dirs
/// `build/` and `intake/` are deliberately absent, as are `artifacts/`,
/// `stop/`, `opencode-log-*`, `effects.log`, `checkout.json`, and `prompts/`.
pub const REAP_SUBDIRS: [&str; 14] = [
    "checkout",
    "agent-target",
    "verify",
    "cargo-home",
    "verify-tmp",
    "verify-home",
    "tmp",
    "xdg-config",
    "xdg-data",
    "xdg-cache",
    "xdg-state",
    "protected",
    "sock",
    "work",
];

/// Phase directories scanned for a live worker record before reaping.
const PHASE_DIRS: [&str; 5] = ["intake", "checkout", "build", "verify", "integrate"];

/// Outcome of one execution's reap attempt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReapReport {
    /// Execution whose run dir was reaped.
    pub execution: ExecutionId,
    /// Approximate bytes reclaimed (recursive metadata walk taken before delete).
    pub bytes: u64,
    /// Names of the scratch subdirectories actually removed.
    pub removed: Vec<String>,
    /// Set when reaping was skipped; carries the reason.
    pub skipped: Option<String>,
}

/// Terminal predicate for an execution state/phase pair.
///
/// Identical to `work::recover_inflight` (`Completed | Parked | Failed` state,
/// or `Done | Parked` phase).
pub fn is_terminal(phase: ExecutionPhase, state: ExecutionState) -> bool {
    matches!(
        state,
        ExecutionState::Completed | ExecutionState::Parked | ExecutionState::Failed
    ) || matches!(phase, ExecutionPhase::Done | ExecutionPhase::Parked)
}

/// Reap one execution's heavy scratch, best-effort. Never returns an error.
///
/// Skips (deletes nothing) when any `*.worker.json` exists under a phase
/// directory, signifying a possibly-live worker. For every present
/// [`REAP_SUBDIRS`] entry, sums its size by metadata, then deletes it: a
/// directory with `remove_dir_all`, anything else (a stray file or symlink)
/// with `remove_file` without following the link.
pub fn reap_execution(state_dir: &Path, execution: &ExecutionId) -> ReapReport {
    let root = run_root(state_dir, execution);
    let mut report = ReapReport {
        execution: execution.clone(),
        bytes: 0,
        removed: Vec::new(),
        skipped: None,
    };

    if let Some(reason) = live_worker(&root) {
        report.skipped = Some(reason);
        return report;
    }

    for sub in REAP_SUBDIRS {
        let path = root.join(sub);
        let Ok(metadata) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        report.bytes += size_bytes(&path, &metadata);
        let removed = if metadata.is_dir() {
            std::fs::remove_dir_all(&path).is_ok()
        } else {
            // A symlink or stray file: unlink only — never traverse a link.
            std::fs::remove_file(&path).is_ok()
        };
        if removed {
            report.removed.push(sub.to_string());
        }
    }
    report
}

/// Reap every terminal execution in the ledger (boot sweep).
///
/// Applies the same terminal predicate as [`is_terminal`]; reaps each match and
/// emits one summary line per execution. Best-effort: a ledger read failure
/// yields an empty report list.
pub fn reap_terminal(state_dir: &Path, ledger: &LedgerHandle) -> Vec<ReapReport> {
    let records = match ledger.executions() {
        Ok(records) => records,
        Err(err) => {
            eprintln!("agalma: reap sweep: cannot read executions: {err}");
            return Vec::new();
        }
    };
    let mut reports = Vec::new();
    for record in records {
        if !is_terminal(record.phase, record.state) {
            continue;
        }
        let report = reap_execution(state_dir, &record.execution_id);
        eprintln!("agalma: reap {}", summary_line(&report));
        reports.push(report);
    }
    reports
}

/// One-line human summary of a reap report (MB reclaimed, or skip reason).
pub fn summary_line(report: &ReapReport) -> String {
    match &report.skipped {
        Some(reason) => format!("{}: skipped ({reason})", report.execution),
        None => {
            let mb = report.bytes as f64 / (1024.0 * 1024.0);
            format!(
                "{}: reclaimed {mb:.2} MB from {} dir(s)",
                report.execution,
                report.removed.len()
            )
        }
    }
}

/// Find a phase directory holding a `*.worker.json` record, if any.
fn live_worker(root: &Path) -> Option<String> {
    for phase in PHASE_DIRS {
        let dir = root.join(phase);
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            if name.to_string_lossy().ends_with(".worker.json") {
                return Some(format!(
                    "live worker record {}/{}",
                    phase,
                    name.to_string_lossy()
                ));
            }
        }
    }
    None
}

/// Recursive metadata-only size walk (no file contents are read).
///
/// Directories sum their children; regular files report their length; symlinks
/// and other special files count as zero (unlinking reclaims only the inode).
fn size_bytes(path: &Path, metadata: &std::fs::Metadata) -> u64 {
    if metadata.is_dir() {
        let mut total = 0u64;
        if let Ok(entries) = std::fs::read_dir(path) {
            for entry in entries.flatten() {
                if let Ok(child) = std::fs::symlink_metadata(entry.path()) {
                    total += size_bytes(&entry.path(), &child);
                }
            }
        }
        total
    } else if metadata.is_file() {
        metadata.len()
    } else {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_predicate_matches_recover_inflight() {
        assert!(is_terminal(
            ExecutionPhase::Build,
            ExecutionState::Completed
        ));
        assert!(is_terminal(ExecutionPhase::Verify, ExecutionState::Parked));
        assert!(is_terminal(ExecutionPhase::Build, ExecutionState::Failed));
        assert!(is_terminal(ExecutionPhase::Done, ExecutionState::Running));
        assert!(is_terminal(ExecutionPhase::Parked, ExecutionState::Running));
        assert!(!is_terminal(ExecutionPhase::Build, ExecutionState::Running));
    }

    #[test]
    fn heavy_and_keep_sets_are_disjoint() {
        for keep in ["artifacts", "stop", "build", "intake", "prompts"] {
            assert!(!REAP_SUBDIRS.contains(&keep), "{keep} must never be reaped");
        }
    }
}
