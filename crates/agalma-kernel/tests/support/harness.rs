//! Crash-injection + fake-effect harness contract for the B0 recovery pack.
//!
//! This module is the stable, documented contract that B0.2+ recovery tests
//! consume unchanged. It has three parts:
//!
//! 1. **Crash-injection hooks.** Reuses the established `AGALMA_*_CRASH_AT`
//!    convention (`crates/agalma-execution/tests/crash_matrix.rs`,
//!    `crates/agalma-ledger/src/tests.rs`): a child test process sets
//!    [`CRASH_ENV`] to a named point, the code under test calls [`crash_hook`]
//!    at every persistence/effect boundary, and the matching call aborts the
//!    process (`SIGABRT`). The parent then reopens the same isolated ledger and
//!    asserts recovery. Callers reference the `pub const` point names, never
//!    string literals.
//! 2. **Fake activity handle.** [`FakeActivity`] mirrors the M1 activity model
//!    (`execute` / `effect_present` / `reconcile`) but records every effect to
//!    an append-only [`EffectJournal`] (one NDJSON file per case) instead of
//!    performing real I/O. [`RecordingActivity`] is the shipped double.
//! 3. **Isolated state.** [`open_isolated_ledger`] opens a fresh SQLite ledger
//!    under `target/test-runs/recovery-*` (never a system temp directory).
//!
//! # Child re-exec
//!
//! To inject a crash the parent re-execs its own test binary as a child with
//! [`CHILD_ENV`] set; the child arms the hook and aborts. See
//! [`spawn_self_as_child`] / [`child_case`] / [`assert_sigabrt`].

#![allow(dead_code)] // consumed by B0.2+ recovery tests, not all at once

use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use agalma_contracts::OperationId;
use agalma_execution::EffectProbe;
use agalma_ledger::SqliteLedger;

// ---------------------------------------------------------------------------
// Crash-injection hooks
// ---------------------------------------------------------------------------

/// Environment variable a child process sets to a crash point name.
pub const CRASH_ENV: &str = "AGALMA_KERNEL_CRASH_AT";

/// Environment variable marking a re-exec'd child test process (value = case).
pub const CHILD_ENV: &str = "KERNEL_RECOVERY_CHILD";

/// Before the atomic ledger commit; nothing durable yet.
pub const BEFORE_PERSISTENCE: &str = "before_persistence";
/// After the transition commit returned, before dispatch (fixture case 1).
pub const AFTER_TRANSITION_COMMIT: &str = "after_transition_commit";
/// Immediately before handing work to an adapter/handler.
pub const BEFORE_DISPATCH: &str = "before_dispatch";
/// After dispatch returned, before the completion record.
pub const AFTER_DISPATCH: &str = "after_dispatch";
/// After the external effect landed, before completion (fixture cases 2/4).
pub const AFTER_EFFECT_BEFORE_COMPLETION: &str = "after_effect_before_completion";
/// After the decision/fallback outcome commit (fixture case 3).
pub const AFTER_DECISION_COMMIT: &str = "after_decision_commit";
/// After a merge landed but before its completion record (fixture case 4).
pub const AFTER_MERGE_BEFORE_COMPLETION: &str = "after_merge_before_completion";
/// After a retry deadline row committed (fixture case 6).
pub const AFTER_TIMER_COMMIT: &str = "after_timer_commit";
/// After a directive version row committed (fixture case 7).
pub const AFTER_DIRECTIVE_COMMIT: &str = "after_directive_commit";

/// Abort the process when `point` matches the armed [`CRASH_ENV`] value.
///
/// Code under test calls this at every persistence/effect boundary. A no-op
/// when the env var is unset or names a different point, so it is safe to leave
/// in the hot path.
pub fn crash_hook(point: &str) {
    if std::env::var(CRASH_ENV).as_deref() == Ok(point) {
        std::process::abort();
    }
}

/// Arm the crash hook at `point` for this process.
pub fn arm(point: &str) {
    std::env::set_var(CRASH_ENV, point);
}

/// Disarm the crash hook.
pub fn disarm() {
    std::env::remove_var(CRASH_ENV);
}

// ---------------------------------------------------------------------------
// Isolated state (run dirs under target/test-runs/, never /tmp)
// ---------------------------------------------------------------------------

fn unix_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock before epoch")
        .as_nanos()
}

fn target_dir() -> PathBuf {
    std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target"))
}

/// A fresh, unique run directory under `target/test-runs/recovery-<case>-*`.
///
/// WARNING: deletes only the freshly-minted unique directory if it somehow
/// exists. The base is confined to the workspace `target/` tree; it never
/// touches a system temp directory.
pub fn recovery_dir(case: &str) -> PathBuf {
    let dir = target_dir().join("test-runs").join(format!(
        "recovery-{case}-{}-{}",
        std::process::id(),
        unix_nanos()
    ));
    if dir.exists() {
        std::fs::remove_dir_all(&dir).expect("reset run dir");
    }
    std::fs::create_dir_all(&dir).expect("create run dir");
    dir
}

/// Open an isolated SQLite ledger under `target/test-runs/recovery-<case>-*`.
///
/// Returns the run directory (holding `ledger.sqlite` and any effect journal)
/// alongside the ledger, so a parent can reopen the same state after a crash
/// child aborts.
pub fn open_isolated_ledger(case: &str) -> (PathBuf, SqliteLedger) {
    let dir = recovery_dir(case);
    let ledger = SqliteLedger::open(dir.join("ledger.sqlite")).expect("open isolated ledger");
    (dir, ledger)
}

// ---------------------------------------------------------------------------
// Effect journal (deterministic fake effect recorder)
// ---------------------------------------------------------------------------

/// One append-only effect-journal record (NDJSON line).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JournalEntry {
    /// Monotonic 0-based sequence within the journal.
    pub seq: u64,
    pub operation_id: String,
    /// `execute` for a performed effect, `reconcile` for a completed survivor.
    pub kind: String,
    /// `applied` (new effect) or `replayed` (idempotent repeat / reconcile).
    pub outcome: String,
    pub recorded_at_unix_ms: u64,
}

/// Append-only NDJSON effect journal (one file per fixture case).
///
/// `execute` is idempotent by operation id: a repeat records no new effect
/// line, mirroring `ScriptedActivity`. Lines are flushed and `sync_all`'d so a
/// crash after the effect leaves the line durably on disk.
#[derive(Clone, Debug)]
pub struct EffectJournal {
    path: PathBuf,
}

impl EffectJournal {
    /// Journal at an explicit path.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        EffectJournal { path: path.into() }
    }

    /// Journal at `<run_dir>/effects/<case_id>.ndjson`.
    pub fn in_run_dir(run_dir: &Path, case_id: &str) -> Self {
        EffectJournal::new(run_dir.join("effects").join(format!("{case_id}.ndjson")))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Whether an `execute` record exists for `operation_id`.
    pub fn effect_present(&self, operation_id: &str) -> bool {
        self.entries()
            .iter()
            .any(|e| e.kind == "execute" && e.operation_id == operation_id)
    }

    /// All entries, in append order.
    pub fn entries(&self) -> Vec<JournalEntry> {
        let Ok(text) = std::fs::read_to_string(&self.path) else {
            return Vec::new();
        };
        text.lines()
            .filter(|l| !l.trim().is_empty())
            .filter_map(parse_entry)
            .collect()
    }

    /// Number of recorded entries.
    pub fn count(&self) -> usize {
        self.entries().len()
    }

    /// Number of `execute` entries.
    pub fn effect_count(&self) -> usize {
        self.entries()
            .iter()
            .filter(|e| e.kind == "execute")
            .count()
    }

    /// Append one record, returning `true` when a new line was written.
    ///
    /// `execute` records are idempotent by operation id (a repeat is a no-op
    /// returning `false`); `reconcile` records always append.
    pub fn record(&self, operation_id: &str, kind: &str, outcome: &str) -> Result<bool, String> {
        if kind == "execute" && self.effect_present(operation_id) {
            return Ok(false);
        }
        let seq = self.entries().len() as u64;
        let line = serde_json::json!({
            "seq": seq,
            "operation_id": operation_id,
            "kind": kind,
            "outcome": outcome,
            "recorded_at_unix_ms": now_ms(),
        })
        .to_string();
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("create journal dir: {e}"))?;
        }
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .map_err(|e| format!("open effect journal: {e}"))?;
        writeln!(file, "{line}").map_err(|e| format!("append effect journal: {e}"))?;
        file.sync_all()
            .map_err(|e| format!("sync effect journal: {e}"))?;
        Ok(true)
    }
}

fn parse_entry(line: &str) -> Option<JournalEntry> {
    let value: serde_json::Value = serde_json::from_str(line).ok()?;
    Some(JournalEntry {
        seq: value.get("seq")?.as_u64()?,
        operation_id: value.get("operation_id")?.as_str()?.to_string(),
        kind: value.get("kind")?.as_str()?.to_string(),
        outcome: value.get("outcome")?.as_str()?.to_string(),
        recorded_at_unix_ms: value
            .get("recorded_at_unix_ms")
            .and_then(|v| v.as_u64())
            .unwrap_or(0),
    })
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Fake activity handle contract
// ---------------------------------------------------------------------------

/// Result of one fake activity call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FakeOutcome {
    /// `true` when this call applied/recorded the effect; `false` when the
    /// effect was already present (idempotent repeat).
    pub effect_applied: bool,
}

/// Fake activity handle: `execute` runs the effect; `reconcile` completes a
/// survivor without repeating it. Both record to the [`EffectJournal`].
///
/// The real B0.3 activity protocol will wrap this handle; recovery tests
/// register a [`RecordingActivity`] to observe effect vs. replay behavior.
pub trait FakeActivity {
    /// Probe for a prior effect, mirroring the M1 `EffectProbe` semantics.
    fn effect_present(&self, operation_id: &OperationId) -> EffectProbe;

    /// Perform the effect (idempotent by operation id).
    fn execute(&mut self, operation_id: &OperationId) -> Result<FakeOutcome, String>;

    /// Complete a surviving effect without repeating it.
    fn reconcile(&mut self, operation_id: &OperationId) -> Result<FakeOutcome, String>;
}

/// A [`FakeActivity`] backed by an append-only [`EffectJournal`].
pub struct RecordingActivity {
    journal: EffectJournal,
}

impl RecordingActivity {
    pub fn new(journal: EffectJournal) -> Self {
        RecordingActivity { journal }
    }

    /// The backing journal (for effect-count assertions in tests).
    pub fn journal(&self) -> &EffectJournal {
        &self.journal
    }
}

impl FakeActivity for RecordingActivity {
    fn effect_present(&self, operation_id: &OperationId) -> EffectProbe {
        if self.journal.effect_present(operation_id.as_str()) {
            EffectProbe::Present
        } else {
            EffectProbe::Absent
        }
    }

    fn execute(&mut self, operation_id: &OperationId) -> Result<FakeOutcome, String> {
        let applied = self
            .journal
            .record(operation_id.as_str(), "execute", "applied")?;
        Ok(FakeOutcome {
            effect_applied: applied,
        })
    }

    fn reconcile(&mut self, operation_id: &OperationId) -> Result<FakeOutcome, String> {
        self.journal
            .record(operation_id.as_str(), "reconcile", "replayed")?;
        Ok(FakeOutcome {
            effect_applied: false,
        })
    }
}

// ---------------------------------------------------------------------------
// Child re-exec helpers
// ---------------------------------------------------------------------------

/// The child case name when this process is a re-exec'd crash child.
pub fn child_case() -> Option<String> {
    std::env::var(CHILD_ENV).ok()
}

/// Re-exec this test binary as a crash child running `test_name`, with
/// [`CHILD_ENV`] set to `case`.
pub fn spawn_self_as_child(test_name: &str, case: &str) -> ExitStatus {
    let exe = std::env::current_exe().expect("current_exe");
    Command::new(exe)
        .args([test_name, "--exact", "--test-threads=1", "--nocapture"])
        .env(CHILD_ENV, case)
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .expect("spawn crash child")
}

/// Assert the child died from `SIGABRT` (exit code 6), as [`crash_hook`] does.
pub fn assert_sigabrt(status: &ExitStatus) {
    assert!(!status.success(), "child should abort, got {status:?}");
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        assert_eq!(status.signal(), Some(6), "expected SIGABRT, got {status:?}");
    }
}
