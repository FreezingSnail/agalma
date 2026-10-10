//! H3 static model-routing integration tests (`agalma-zxi.4`).
//!
//! Deterministic: no network, no bd, no model calls. Each test uses its own
//! state dir under `target/test-runs/routing-*` and records real
//! `route.model` decisions in a scratch `SqliteLedger`.

use std::fs;
use std::path::{Path, PathBuf};

use agalma_conductor::routing::{self, RouteClass, RouteOrigin};
use agalma_contracts::{LedgerApi, OperationId};
use agalma_ledger::SqliteLedger;

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crate lives under <root>/crates/")
        .to_path_buf()
}

fn fresh(name: &str) -> PathBuf {
    let dir = root().join("target").join("test-runs").join(name);
    // WARNING: deletes files, but only under `target/test-runs/` (disposable).
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("create run dir");
    dir
}

#[test]
fn builder_route_is_free_first_and_recorded() {
    let state_dir = fresh("routing-builder");
    let selection = routing::resolve_route(
        &state_dir,
        "task-1",
        "builder",
        agalma_contracts::TaskFamily::Fix,
        false,
        false,
        None,
    )
    .expect("resolve");
    assert_eq!(selection.class, RouteClass::Builder);
    assert_eq!(selection.model, "opencode/big-pickle");
    assert_eq!(selection.origin, RouteOrigin::Static);
    assert!(!selection.is_paid());

    // Recorded in the ledger decisions table under the deterministic op id.
    let ledger = SqliteLedger::open(state_dir.join("ledger.sqlite")).expect("open ledger");
    let op = OperationId::new("op:route:task-1:builder:free");
    let outcome = ledger.decision_outcome(&op).expect("read outcome");
    let outcome = outcome.expect("route decision recorded");
    match outcome {
        agalma_contracts::DecisionOutcome::Applied {
            chosen: Some(chosen),
            ..
        } => assert_eq!(chosen, "opencode/big-pickle"),
        other => panic!("unexpected outcome {other:?}"),
    }

    // Resolution is durable: a second call reuses the recorded outcome.
    let again = routing::resolve_route(
        &state_dir,
        "task-1",
        "builder",
        agalma_contracts::TaskFamily::Fix,
        false,
        false,
        None,
    )
    .expect("resolve again");
    assert_eq!(again.model, "opencode/big-pickle");
}

#[test]
fn verifier_skips_paid_without_opt_in_and_uses_it_when_allowed() {
    let free_dir = fresh("routing-verifier-free");
    let free = routing::resolve_route(
        &free_dir,
        "task-v",
        "verifier",
        agalma_contracts::TaskFamily::Feature,
        false,
        true,
        None,
    )
    .expect("resolve free");
    assert_eq!(free.class, RouteClass::Verifier);
    assert_eq!(
        free.model, "opencode/big-pickle",
        "free fallback without opt-in"
    );

    let paid_dir = fresh("routing-verifier-paid");
    let paid = routing::resolve_route(
        &paid_dir,
        "task-v",
        "verifier",
        agalma_contracts::TaskFamily::Feature,
        true,
        true,
        None,
    )
    .expect("resolve paid");
    assert_eq!(paid.model, "opencode-go/claude-haiku-5-5");
    assert!(paid.is_paid());
}

#[test]
fn paid_opt_in_without_a_credential_falls_back_to_free_and_records_reason() {
    let state_dir = fresh("routing-paid-no-token");
    let selection = routing::resolve_route(
        &state_dir,
        "task-nt",
        "verifier",
        agalma_contracts::TaskFamily::Feature,
        true,
        false,
        None,
    )
    .expect("resolve");
    assert_eq!(
        selection.model, "opencode/big-pickle",
        "no token -> free fallback"
    );
    assert!(!selection.is_paid());

    // The fallback reason is recorded on the route decision context.
    let conn = rusqlite::Connection::open(state_dir.join("ledger.sqlite")).expect("open ledger");
    let request_json: String = conn
        .query_row(
            "SELECT request_json FROM decisions WHERE operation_id=?1",
            ["op:route:task-nt:verifier:free"],
            |row| row.get(0),
        )
        .expect("route decision recorded");
    let request: serde_json::Value = serde_json::from_str(&request_json).expect("decision json");
    assert_eq!(request["context"]["allow_paid"], true);
    assert_eq!(request["context"]["paid_available"], false);
    assert_eq!(request["context"]["paid_active"], false);
    assert!(
        request["context"]["fallback_reason"].is_string(),
        "fallback reason recorded: {}",
        request["context"]
    );
}

#[test]
fn override_wins_and_is_recorded_as_override() {
    let state_dir = fresh("routing-override");
    let selection = routing::resolve_route(
        &state_dir,
        "task-o",
        "builder",
        agalma_contracts::TaskFamily::Fix,
        true,
        true,
        Some("custom/model-x"),
    )
    .expect("resolve override");
    assert_eq!(selection.model, "custom/model-x");
    assert_eq!(selection.origin, RouteOrigin::Override);
    assert!(
        selection.spec.is_none(),
        "unknown model has no table metadata"
    );
}

#[test]
fn planner_class_is_static_and_records_no_model() {
    let state_dir = fresh("routing-planner");
    let selection = routing::resolve_route(
        &state_dir,
        "task-p",
        "planner",
        agalma_contracts::TaskFamily::Feature,
        true,
        true,
        Some("should-not-be-used"),
    )
    .expect("resolve planner");
    assert_eq!(selection.class, RouteClass::Planner);
    assert_eq!(selection.model, "static");
    assert_eq!(selection.origin, RouteOrigin::Baseline);
}
