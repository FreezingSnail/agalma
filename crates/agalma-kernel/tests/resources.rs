//! B0.6 resource gate: measure the **seed executor** baseline
//! (`agalma-execution`) with a reproducible, deterministic harness.
//!
//! `docs/durable-execution-bootstrap.md` §"Acceptance and evaluation": compare a
//! candidate's resource measurements with the seed executor (idle memory/CPU,
//! restart time, dispatch latency, database growth) and establish numeric
//! regression limits from the measured seed; correctness remains a veto.
//!
//! The measurements are written to
//! `target/test-runs/b0-resources/seed-baseline.json` (never `/tmp`). Run:
//!
//! ```sh
//! cargo test -p agalma-kernel --offline --test resources -- --nocapture
//! ```
//!
//! The harness is intentionally a *measurement*, not a hard gate: resident
//! memory and CPU vary by host, so the test asserts the numbers were collected
//! and records them for `docs/b0-exit-evidence.md` to freeze limits against.

mod support;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use agalma_contracts::{ExecutionApi, ExecutionState, StepOutcome, TaskId};
use agalma_execution::{Executor, ExecutorConfig, ScriptedActivity, EXECUTION_DEFINITION_VERSION};
use agalma_ledger::SqliteLedger;
use serde_json::json;

/// Number of seed executions driven for dispatch-latency and DB-growth.
const TASKS: usize = 25;

fn target_dir() -> PathBuf {
    std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target"))
}

fn run_dir() -> PathBuf {
    let dir = target_dir().join("test-runs").join("b0-resources");
    std::fs::create_dir_all(&dir).expect("create run dir");
    dir
}

/// `ps -o <field>=` for this process, trimmed (`None` when `ps` unavailable).
fn ps_field(field: &str) -> Option<String> {
    let out = std::process::Command::new("ps")
        .args([
            "-o",
            &format!("{field}="),
            "-p",
            &std::process::id().to_string(),
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let value = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!value.is_empty()).then_some(value)
}

/// Resident set size in KiB, when observable.
fn rss_kb() -> Option<u64> {
    ps_field("rss").and_then(|v| v.parse().ok())
}

/// Accumulated process CPU time as `MM:SS.ss` / `HH:MM:SS.ss` / `DD-HH:MM:SS`,
/// converted to milliseconds.
fn cpu_time_ms() -> Option<f64> {
    let raw = ps_field("cputime")?;
    parse_cputime(&raw)
}

fn parse_cputime(raw: &str) -> Option<f64> {
    let (days, rest) = match raw.split_once('-') {
        Some((d, r)) => (d.parse::<f64>().ok()?, r),
        None => (0.0, raw),
    };
    let parts: Vec<&str> = rest.split(':').collect();
    let (hours, minutes, seconds) = match parts.as_slice() {
        [m, s] => (0.0, m.parse::<f64>().ok()?, s.parse::<f64>().ok()?),
        [h, m, s] => (
            h.parse::<f64>().ok()?,
            m.parse::<f64>().ok()?,
            s.parse::<f64>().ok()?,
        ),
        _ => return None,
    };
    Some(((days * 86_400.0 + hours * 3_600.0 + minutes * 60.0 + seconds) * 1000.0).max(0.0))
}

/// Total bytes of the SQLite database and its WAL/SHM sidecars.
fn db_bytes(path: &Path) -> u64 {
    let mut total = 0;
    for suffix in ["", "-wal", "-shm"] {
        let candidate = PathBuf::from(format!("{}{suffix}", path.display()));
        if let Ok(meta) = std::fs::metadata(&candidate) {
            total += meta.len();
        }
    }
    total
}

/// Seed executor over a fresh ledger and scripted activities (all phases).
fn seed_executor(db: &Path, effects: &Path) -> Executor<SqliteLedger> {
    let mut executor = Executor::new(
        SqliteLedger::open(db).expect("open seed ledger"),
        ExecutorConfig::default(),
    );
    for phase in [
        agalma_contracts::ExecutionPhase::Intake,
        agalma_contracts::ExecutionPhase::Checkout,
        agalma_contracts::ExecutionPhase::Build,
        agalma_contracts::ExecutionPhase::Verify,
        agalma_contracts::ExecutionPhase::Integrate,
    ] {
        executor.add_activity(phase, Box::new(ScriptedActivity::new(effects)));
    }
    executor
}

/// Drive one execution to a terminal/idle outcome, returning the logical step
/// count.
fn drive_to_idle(executor: &mut Executor<SqliteLedger>, task: &TaskId) -> usize {
    let exec = executor.start(task).expect("start seed execution");
    let mut steps = 0usize;
    loop {
        steps += 1;
        match executor.step(&exec).expect("seed step") {
            StepOutcome::Idle { .. } | StepOutcome::Parked { .. } => break,
            StepOutcome::Blocked { .. } => panic!("seed blocked"),
            _ => {}
        }
        if steps > 64 {
            panic!("seed execution did not terminate");
        }
    }
    // Confirm the terminal projection is Completed.
    let status = executor.status(&exec).expect("status");
    assert_eq!(status.state, ExecutionState::Completed);
    steps
}

#[test]
fn seed_executor_resource_baseline() {
    let dir = run_dir();
    let db = dir.join("seed-baseline.sqlite");
    let effects = dir.join("seed-effects.ndjson");
    // WARNING: removes only this run's disposable files under target/test-runs/.
    for path in [&db, &effects] {
        let _ = std::fs::remove_file(path);
    }
    let _ = std::fs::remove_file(dir.join("seed-baseline.sqlite-wal"));
    let _ = std::fs::remove_file(dir.join("seed-baseline.sqlite-shm"));

    // --- Idle baseline: open the seed executor, then observe RSS + CPU. -------
    let mut executor = seed_executor(&db, &effects);
    let idle_rss_kb = rss_kb();
    let cpu_before = cpu_time_ms();
    std::thread::sleep(Duration::from_millis(200));
    let cpu_after = cpu_time_ms();
    let idle_cpu_ms = match (cpu_before, cpu_after) {
        (Some(b), Some(a)) => Some((a - b).max(0.0)),
        _ => None,
    };
    let db_after_open = db_bytes(&db);

    // --- Dispatch latency + database growth over TASKS executions. -----------
    let started = Instant::now();
    let mut total_steps = 0usize;
    for n in 0..TASKS {
        total_steps += drive_to_idle(&mut executor, &TaskId::new(format!("seed-bench-{n}")));
    }
    let elapsed = started.elapsed();
    let dispatch_latency_us = elapsed.as_micros() as f64 / total_steps.max(1) as f64;
    let db_after_tasks = db_bytes(&db);
    let db_growth_bytes_per_op =
        (db_after_tasks.saturating_sub(db_after_open)) as f64 / total_steps.max(1) as f64;

    // --- Restart time: reopen the ledger + executor and recover. -------------
    drop(executor);
    let restart_started = Instant::now();
    {
        let mut reopened = seed_executor(&db, &effects);
        reopened.recover().expect("seed recover");
    }
    let restart_ms = restart_started.elapsed().as_secs_f64() * 1000.0;

    // --- Proposed regression limits (generous host-tolerant multipliers). ----
    let report = json!({
        "kernel": "seed-executor",
        "crate": "agalma-execution",
        "execution_definition_version": EXECUTION_DEFINITION_VERSION,
        "tasks": TASKS,
        "total_steps": total_steps,
        "measurements": {
            "idle_rss_kb": idle_rss_kb,
            "idle_cpu_ms_per_200ms": idle_cpu_ms,
            "restart_ms": restart_ms,
            "dispatch_latency_us": dispatch_latency_us,
            "db_growth_bytes_per_op": db_growth_bytes_per_op,
            "db_after_open_bytes": db_after_open,
            "db_after_tasks_bytes": db_after_tasks,
        },
        "proposed_limits": {
            "idle_rss_kb_max": idle_rss_kb.map(|kb| kb.saturating_mul(2)),
            "restart_ms_max": restart_ms * 3.0,
            "dispatch_latency_us_max": dispatch_latency_us * 4.0,
            "db_growth_bytes_per_op_max": db_growth_bytes_per_op * 2.0 + 1024.0
        },
        "note": "Correctness is a veto; these limits bound resource regressions a candidate may introduce."
    });

    let report_path = dir.join("seed-baseline.json");
    std::fs::write(
        &report_path,
        serde_json::to_string_pretty(&report).expect("serialize report"),
    )
    .expect("write resource report");
    println!(
        "seed resource baseline written to {}",
        report_path.display()
    );
    println!("{report}");

    // The measurement ran and produced positive numbers.
    assert!(total_steps > 0);
    assert!(dispatch_latency_us > 0.0);
    assert!(restart_ms >= 0.0);
    assert!(report_path.exists());
}
