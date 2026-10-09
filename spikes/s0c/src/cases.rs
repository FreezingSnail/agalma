//! The six crash-matrix cases. Each returns a `Report` in the parent
//! (observing) role; child roles abort at their injection point.

use std::path::PathBuf;
use std::process::{Command, ExitStatus, Stdio};

use crate::executor::{Delivery, Executor, STATE_BUILDING, STATE_PARKED, STATE_VERIFYING};
use crate::util::{case_dir, reset_dir, Report};

fn paths(case: &str) -> (PathBuf, PathBuf) {
    let dir = case_dir(case);
    (dir.join("ledger.sqlite"), dir.join("effects.log"))
}

fn ex(case: &str) -> Executor {
    let (db, effects) = paths(case);
    Executor::new(db, effects)
}

/// Spawn this same binary as a child with the injection point selected.
fn spawn_child(case: &str, crash_at: &str) -> Result<ExitStatus, String> {
    let exe = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
    Command::new(exe)
        .arg(case)
        .env("SPIKE_CHILD", "1")
        .env("SPIKE_CRASH_AT", crash_at)
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .map_err(|e| format!("spawn child: {e}"))
}

fn crashed(st: &ExitStatus) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if st.signal() == Some(6) {
            return true;
        }
    }
    !st.success()
}

// ---------------------------------------------------------------------------
// Case 1 — crash after transition commit, before dispatch.
// ---------------------------------------------------------------------------

pub fn case1_child() -> Result<(), String> {
    let e = ex("case1");
    e.init()?;
    e.start("task-1", "exec-1")?;
    e.advance("exec-1", STATE_BUILDING, "build", "{\"n\":1}")?;
    // Injection: the atomic transition commit has returned; dispatch has not run.
    crate::util::maybe_crash("after_transition_commit");
    Ok(())
}

pub fn case1_parent() -> Result<Report, String> {
    reset_dir(&case_dir("case1")).map_err(|e| format!("reset case1 dir: {e}"))?;
    let mut r = Report::new("case1");
    let st = spawn_child("case1", "after_transition_commit")?;
    r.note(format!("child exit status: {st:?}"));
    r.check(
        "crash_after_commit_before_dispatch",
        crashed(&st),
        format!("{st:?}"),
    );

    let e = ex("case1");
    let c = e.compat();
    r.check("schema_compatible", c.schema_ok, &c.detail);

    let (derived, projected) = e.reconstruct_state("exec-1")?;
    r.check(
        "state_reconstructed_from_events",
        derived == projected,
        format!("derived={derived} projected={projected}"),
    );
    r.check(
        "projected_state_is_building",
        projected == STATE_BUILDING,
        &projected,
    );

    let pending = e.pending(Some("exec-1"))?;
    r.check(
        "pending_intent_recovered",
        pending.len() == 1,
        format!("{} unconsumed intent(s)", pending.len()),
    );

    let op = pending[0].op_id.clone();
    let d = e.deliver(&op)?;
    r.check(
        "recovered_dispatch_executed",
        matches!(d, Delivery::Executed(_)),
        format!("{d:?}"),
    );
    r.check(
        "effect_count_1",
        e.effect_count() == 1,
        format!("{} effect(s)", e.effect_count()),
    );

    let d2 = e.deliver(&op)?;
    r.check(
        "duplicate_returns_recorded",
        matches!(d2, Delivery::Recorded(_)),
        format!("{d2:?}"),
    );
    r.check(
        "duplicate_no_second_effect",
        e.effect_count() == 1,
        format!("{} effect(s)", e.effect_count()),
    );
    Ok(r)
}

// ---------------------------------------------------------------------------
// Case 2 — duplicate delivery after completion returns the recorded result.
// ---------------------------------------------------------------------------

pub fn case2() -> Result<Report, String> {
    reset_dir(&case_dir("case2")).map_err(|e| format!("reset case2 dir: {e}"))?;
    let mut r = Report::new("case2");
    let e = ex("case2");
    e.init()?;
    e.start("task-2", "e1")?;
    let op = e.advance("e1", STATE_BUILDING, "build", "{\"n\":1}")?;

    let d1 = e.deliver(&op)?;
    r.check(
        "first_delivery_executed",
        matches!(d1, Delivery::Executed(_)),
        format!("{d1:?}"),
    );
    r.check(
        "effect_count_1",
        e.effect_count() == 1,
        format!("{} effect(s)", e.effect_count()),
    );

    let d2 = e.deliver(&op)?;
    let recorded = matches!(d2, Delivery::Recorded(_));
    r.check("duplicate_returns_recorded", recorded, format!("{d2:?}"));
    r.check(
        "duplicate_no_second_effect",
        e.effect_count() == 1,
        format!("{} effect(s)", e.effect_count()),
    );

    // Same operation ID with different inputs must be rejected.
    let conflict = e.advance("e1", STATE_BUILDING, "build", "{\"n\":2}");
    r.check(
        "conflicting_inputs_rejected",
        conflict.is_err(),
        conflict.err().unwrap_or_else(|| "no error".into()),
    );
    Ok(r)
}

// ---------------------------------------------------------------------------
// Case 3 — restart reconstructs state from history; pending reconciled.
// ---------------------------------------------------------------------------

pub fn case3_child() -> Result<(), String> {
    let e = ex("case3");
    e.init()?;
    e.start("task-3", "e1")?;
    let op1 = e.advance("e1", STATE_BUILDING, "build", "{\"n\":1}")?;
    e.deliver(&op1)?; // completes build
    e.advance("e1", STATE_VERIFYING, "verify", "{\"n\":2}")?;
    crate::util::maybe_crash("after_transition_commit");
    Ok(())
}

pub fn case3_parent() -> Result<Report, String> {
    reset_dir(&case_dir("case3")).map_err(|e| format!("reset case3 dir: {e}"))?;
    let mut r = Report::new("case3");
    let st = spawn_child("case3", "after_transition_commit")?;
    r.note(format!("child exit status: {st:?}"));
    r.check(
        "crash_after_verify_transition",
        crashed(&st),
        format!("{st:?}"),
    );

    let e = ex("case3");
    let (derived, projected) = e.reconstruct_state("e1")?;
    r.check(
        "history_replay_matches_projection",
        derived == projected,
        format!("derived={derived} projected={projected}"),
    );
    r.check(
        "reconstructed_state_verifying",
        derived == STATE_VERIFYING,
        &derived,
    );

    let pending = e.pending(Some("e1"))?;
    r.check(
        "only_verify_pending",
        pending.len() == 1 && pending[0].op_id.contains("verify"),
        format!("{:?}", pending.iter().map(|i| &i.op_id).collect::<Vec<_>>()),
    );
    r.check(
        "build_effect_not_repeated",
        e.effect_count() == 1,
        format!("{} effect(s) before verify dispatch", e.effect_count()),
    );

    let d = e.deliver(&pending[0].op_id)?;
    r.check(
        "pending_verify_reconciled",
        matches!(d, Delivery::Executed(_)),
        format!("{d:?}"),
    );
    r.check(
        "effect_count_2_after_verify",
        e.effect_count() == 2,
        format!("{} effect(s)", e.effect_count()),
    );
    r.check(
        "no_pending_left",
        e.pending(Some("e1"))?.is_empty(),
        format!("{:?}", e.pending(Some("e1"))?),
    );
    Ok(r)
}

// ---------------------------------------------------------------------------
// Case 4 — crash after external effect, before completion record.
// ---------------------------------------------------------------------------

pub fn case4_child() -> Result<(), String> {
    let e = ex("case4");
    e.init()?;
    e.start("task-4", "e1")?;
    let op = e.advance("e1", STATE_BUILDING, "build", "{\"n\":1}")?;
    // This performs the effect then aborts (injection inside `deliver`).
    let _ = e.deliver(&op)?;
    Ok(())
}

pub fn case4_parent() -> Result<Report, String> {
    reset_dir(&case_dir("case4")).map_err(|e| format!("reset case4 dir: {e}"))?;
    let mut r = Report::new("case4");
    let st = spawn_child("case4", "after_effect_before_completion")?;
    r.note(format!("child exit status: {st:?}"));
    r.check(
        "crash_after_effect_before_completion",
        crashed(&st),
        format!("{st:?}"),
    );

    let e = ex("case4");
    let op = "op:e1:build";
    r.check(
        "external_effect_survived",
        e.effect_present(op) && e.effect_count() == 1,
        format!("{} effect(s), present={}", e.effect_count(), e.effect_present(op)),
    );
    r.check(
        "completion_not_recorded",
        e.op_state(op)?.as_deref() != Some("completed"),
        format!("state={:?}", e.op_state(op)?),
    );

    let d = e.deliver(op)?;
    r.check(
        "recovery_recognized_effect",
        matches!(d, Delivery::Reconciled(_)),
        format!("{d:?}"),
    );
    r.check(
        "effect_not_repeated",
        e.effect_count() == 1,
        format!("{} effect(s)", e.effect_count()),
    );
    r.check(
        "operation_now_completed",
        e.op_state(op)?.as_deref() == Some("completed"),
        format!("state={:?}", e.op_state(op)?),
    );
    Ok(r)
}

// ---------------------------------------------------------------------------
// Case 5 — kill latch survives restart and blocks recovered dispatch.
// ---------------------------------------------------------------------------

pub fn case5_child() -> Result<(), String> {
    let e = ex("case5");
    e.init()?;
    e.start("task-5", "e1")?;
    e.advance("e1", STATE_BUILDING, "build", "{\"n\":1}")?;
    e.set_kill(true, "spike-test")?;
    crate::util::maybe_crash("after_latch_set");
    Ok(())
}

pub fn case5_parent() -> Result<Report, String> {
    reset_dir(&case_dir("case5")).map_err(|e| format!("reset case5 dir: {e}"))?;
    let mut r = Report::new("case5");
    let st = spawn_child("case5", "after_latch_set")?;
    r.note(format!("child exit status: {st:?}"));
    r.check("crash_after_latch_set", crashed(&st), format!("{st:?}"));

    let e = ex("case5");
    r.check(
        "latch_survives_restart",
        e.kill_active()?,
        format!("active={}", e.kill_active()?),
    );

    let pending = e.pending(Some("e1"))?;
    let op = pending[0].op_id.clone();
    let d = e.deliver(&op)?;
    r.check(
        "recovered_dispatch_blocked_by_latch",
        matches!(d, Delivery::Blocked),
        format!("{d:?}"),
    );
    r.check(
        "no_effect_while_latched",
        e.effect_count() == 0,
        format!("{} effect(s)", e.effect_count()),
    );
    r.check(
        "operation_still_incomplete",
        e.op_state(&op)?.as_deref() != Some("completed"),
        format!("state={:?}", e.op_state(&op)?),
    );

    e.set_kill(false, "human-resume")?;
    let d2 = e.deliver(&op)?;
    r.check(
        "dispatch_after_resume_executes",
        matches!(d2, Delivery::Executed(_)),
        format!("{d2:?}"),
    );
    r.check(
        "effect_count_1_after_resume",
        e.effect_count() == 1,
        format!("{} effect(s)", e.effect_count()),
    );
    Ok(r)
}

// ---------------------------------------------------------------------------
// Case 6 — pinned versions stored with results; incompatible versions park.
// ---------------------------------------------------------------------------

pub fn case6() -> Result<Report, String> {
    reset_dir(&case_dir("case6")).map_err(|e| format!("reset case6 dir: {e}"))?;
    let mut r = Report::new("case6");
    let e = ex("case6");
    e.init()?;

    // 6a — result pins the execution-definition version.
    e.start("task-6a", "e1")?;
    let op1 = e.advance("e1", STATE_BUILDING, "build", "{\"n\":1}")?;
    e.deliver(&op1)?;
    let result = e.op_result(&op1)?.unwrap_or_default();
    r.check(
        "result_pins_execution_version",
        result.contains("\"execution_version\":1"),
        &result,
    );
    r.check(
        "schema_version_pinned",
        e.meta("schema_version")?.as_deref() == Some("1"),
        format!("schema_version={:?}", e.meta("schema_version")?),
    );

    // 6b — execution record with an incompatible execution version parks.
    e.start("task-6b", "e2")?;
    let op2 = e.advance("e2", STATE_BUILDING, "build", "{\"n\":2}")?;
    e.force_exec_version("e2", 99)?;
    let d2 = e.deliver(&op2)?;
    r.check(
        "incompatible_execution_version_parks",
        matches!(d2, Delivery::Parked(_)),
        format!("{d2:?}"),
    );
    r.check(
        "incompatible_execution_parks_state",
        e.exec_state("e2")?.as_deref() == Some(STATE_PARKED),
        format!("state={:?}", e.exec_state("e2")?),
    );

    // 6c — incompatible physical schema parks recovered work.
    e.start("task-6c", "e3")?;
    let op3 = e.advance("e3", STATE_BUILDING, "build", "{\"n\":3}")?;
    let before = e.effect_count();
    e.set_meta("schema_version", "99")?;
    let d3 = e.deliver(&op3)?;
    r.check(
        "incompatible_schema_parks",
        matches!(d3, Delivery::Parked(_)),
        format!("{d3:?}"),
    );
    r.check(
        "incompatible_schema_no_effect",
        e.effect_count() == before,
        format!("{} -> {} effect(s)", before, e.effect_count()),
    );
    r.check(
        "compat_reports_incompatible",
        !e.compat().schema_ok,
        e.compat().detail,
    );
    Ok(r)
}
