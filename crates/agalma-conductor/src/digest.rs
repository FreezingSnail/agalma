//! Per-execution digest (M1.4, `agalma-52k.5`).
//!
//! The ledger is the source of truth for what happened; the digest is a derived,
//! human- and machine-readable rollup of one execution:
//!
//! - task/execution ids and the model;
//! - base/candidate/result SHAs and the files the candidate touched
//!   (`git diff --name-status`);
//! - every acceptance command with its exit code, duration, and output artifact;
//! - repairs: attempt count and the verifier diagnosis artifacts;
//! - cost: tokens in/out and USD summed from `usage` execution events;
//! - wall time and the terminal verdict.
//!
//! [`record`] writes `<run>/artifacts/digest.json` and persists a `digests`
//! (schema v2) row via [`LedgerApi::record_digest`]. The one-line
//! [`Digest::summary_line`] is projected into the bd issue on done/parked and
//! printed by `agalma digest`.

use std::path::Path;
use std::process::Command;

use agalma_contracts::{
    ArtifactRef, Checkout, ContractError, DigestRecord, ExecutionId, LedgerApi, OperationId, Task,
};
use agalma_ledger::SqliteLedger;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::phases::run_root;

/// One acceptance command's recorded outcome.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CommandOutcome {
    /// 1-based attempt the command ran under.
    pub attempt: u32,
    pub command: String,
    pub exit_code: i32,
    pub duration_ms: u64,
    /// Output artifact path (`artifacts/accept@<attempt>-<n>.txt`).
    pub artifact: String,
}

/// A tracked file the candidate changed (`git diff --name-status`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileTouched {
    /// Git status letter (`A`, `M`, `D`, `R…`).
    pub status: String,
    pub path: String,
}

/// Cost/usage derived from `usage` execution events.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct CostSummary {
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub usd: f64,
}

/// Repair activity for the execution.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepairSummary {
    /// Attempts observed (build/verify operation receipts).
    pub attempts: u32,
    /// Verifier diagnosis artifacts written for red attempts.
    pub diagnosis_artifacts: Vec<String>,
}

/// Terminal verdict of the execution.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Verdict {
    Green,
    Red,
}

impl Verdict {
    /// Stable label (`green`/`red`).
    pub fn as_str(self) -> &'static str {
        match self {
            Verdict::Green => "green",
            Verdict::Red => "red",
        }
    }
}

/// One per-execution digest (the schema v2 `digests` summary).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Digest {
    pub execution_id: String,
    pub task_id: String,
    pub model: String,
    pub base_sha: Option<String>,
    pub candidate_sha: Option<String>,
    pub result_sha: Option<String>,
    pub files_touched: Vec<FileTouched>,
    pub commands: Vec<CommandOutcome>,
    pub repairs: RepairSummary,
    pub cost: CostSummary,
    pub wall_ms: u64,
    pub verdict: Verdict,
}

impl Digest {
    /// One-line summary for the bd projection comment and the CLI.
    pub fn summary_line(&self) -> String {
        format!(
            "digest execution={} task={} verdict={} attempts={} files={} tokens={}/{} usd={:.6} wall_ms={}",
            self.execution_id,
            self.task_id,
            self.verdict.as_str(),
            self.repairs.attempts,
            self.files_touched.len(),
            self.cost.tokens_in,
            self.cost.tokens_out,
            self.cost.usd,
            self.wall_ms,
        )
    }
}

/// Build a digest for `execution`, write `<run>/artifacts/digest.json`, and
/// persist a `digests` row (version = prior rows + 1).
pub fn record(
    state_dir: &Path,
    execution: &ExecutionId,
    task: &Task,
    model: &str,
    verdict: Verdict,
) -> Result<Digest, ContractError> {
    let mut ledger = SqliteLedger::open(state_dir.join("ledger.sqlite"))?;
    let digest = build(&ledger, state_dir, execution, task, model, verdict)?;

    let artifacts = run_root(state_dir, execution).join("artifacts");
    std::fs::create_dir_all(&artifacts).map_err(|e| {
        ContractError::KnownFailure(format!("create artifacts {}: {e}", artifacts.display()))
    })?;
    let path = artifacts.join("digest.json");
    let text = serde_json::to_string_pretty(&digest).map_err(json_err)?;
    std::fs::write(&path, text)
        .map_err(|e| ContractError::KnownFailure(format!("write {}: {e}", path.display())))?;

    let version = ledger.digests_for_execution(execution)?.len() as u32 + 1;
    ledger.record_digest(&DigestRecord {
        execution_id: execution.clone(),
        version,
        summary: serde_json::to_value(&digest).map_err(json_err)?,
        artifact_ref: Some(ArtifactRef::derive("digest")),
        recorded_at_unix_ms: now_ms(),
    })?;
    Ok(digest)
}

/// Derive a digest from the ledger, the recorded checkout, and the candidate's
/// Git state.
fn build(
    ledger: &SqliteLedger,
    state_dir: &Path,
    execution: &ExecutionId,
    task: &Task,
    model: &str,
    verdict: Verdict,
) -> Result<Digest, ContractError> {
    let events = ledger.events(execution)?;
    let root = run_root(state_dir, execution);
    let checkout = read_checkout(&root);
    let base_sha = checkout.as_ref().map(|c| c.base_sha.clone());
    let candidate_sha = checkout.as_ref().and_then(|c| {
        rev_parse(
            Path::new(&c.path),
            &format!("refs/heads/{}", c.candidate_branch),
        )
    });
    let result_sha = ledger
        .operation_receipt(&OperationId::derive(execution, "integrate"))?
        .and_then(|receipt| {
            receipt
                .result
                .pointer("/result/result_sha")
                .and_then(Value::as_str)
                .map(str::to_string)
        });

    let files_touched = match (&checkout, &base_sha, &candidate_sha) {
        (Some(c), Some(base), Some(candidate)) => {
            diff_name_status(Path::new(&c.path), base, candidate)
        }
        _ => Vec::new(),
    };

    let (commands, attempts) = collect_commands(ledger, execution)?;
    let repairs = RepairSummary {
        attempts,
        diagnosis_artifacts: diagnosis_artifacts(&root, attempts),
    };

    let mut cost = CostSummary::default();
    let mut wall_ms = 0u64;
    for event in &events {
        if event.kind != "usage" {
            continue;
        }
        cost.tokens_in += event
            .payload
            .get("tokens_in")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        cost.tokens_out += event
            .payload
            .get("tokens_out")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        cost.usd += event
            .payload
            .get("cost_usd")
            .and_then(Value::as_f64)
            .unwrap_or(0.0);
        wall_ms += event
            .payload
            .get("wall_ms")
            .and_then(Value::as_u64)
            .unwrap_or(0);
    }

    Ok(Digest {
        execution_id: execution.as_str().to_string(),
        task_id: task.task_id.as_str().to_string(),
        model: model.to_string(),
        base_sha,
        candidate_sha,
        result_sha,
        files_touched,
        commands,
        repairs,
        cost,
        wall_ms,
        verdict,
    })
}

/// Collect per-command outcomes across verify attempts (ordered by attempt).
fn collect_commands(
    ledger: &SqliteLedger,
    execution: &ExecutionId,
) -> Result<(Vec<CommandOutcome>, u32), ContractError> {
    let mut commands = Vec::new();
    let mut attempts = 0u32;
    for attempt in 1u32.. {
        let receipt = ledger.operation_receipt(&OperationId::derive(
            execution,
            &format!("verify@{attempt}"),
        ))?;
        let Some(receipt) = receipt else {
            break;
        };
        attempts = attempt;
        let empty = Vec::new();
        let entries = receipt
            .result
            .pointer("/result/command_results")
            .and_then(Value::as_array)
            .unwrap_or(&empty);
        for entry in entries {
            if let Ok(outcome) = serde_json::from_value::<CommandOutcome>(entry.clone()) {
                commands.push(outcome);
            }
        }
    }
    Ok((commands, attempts))
}

/// Diagnosis artifacts present for attempts `1..=attempts`.
fn diagnosis_artifacts(root: &Path, attempts: u32) -> Vec<String> {
    let mut artifacts = Vec::new();
    for attempt in 1..=attempts {
        let path = root
            .join("artifacts")
            .join(format!("diagnosis@{attempt}.md"));
        if path.exists() {
            artifacts.push(path.to_string_lossy().into_owned());
        }
    }
    artifacts
}

fn read_checkout(root: &Path) -> Option<Checkout> {
    let text = std::fs::read_to_string(root.join("checkout.json")).ok()?;
    serde_json::from_str(&text).ok()
}

fn rev_parse(repo: &Path, rev: &str) -> Option<String> {
    let out = git(repo, &["rev-parse", rev])?;
    let sha = out.trim().to_string();
    if sha.is_empty() {
        None
    } else {
        Some(sha)
    }
}

fn diff_name_status(repo: &Path, base: &str, candidate: &str) -> Vec<FileTouched> {
    let Some(text) = git(repo, &["diff", "--name-status", base, candidate]) else {
        return Vec::new();
    };
    text.lines()
        .filter_map(|line| {
            let mut parts = line.split('\t');
            let status = parts.next()?.trim().to_string();
            let path = parts.next()?.trim().to_string();
            if status.is_empty() || path.is_empty() {
                return None;
            }
            Some(FileTouched { status, path })
        })
        .collect()
}

/// Run `git` in `repo`, returning trimmed stdout or `None` on failure.
fn git(repo: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .arg("--no-pager")
        .args(args)
        .current_dir(repo)
        .env("GIT_PAGER", "cat")
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn json_err(err: serde_json::Error) -> ContractError {
    ContractError::KnownFailure(format!("digest json: {err}"))
}

fn now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
