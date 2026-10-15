//! SF.7 wave-1 acceptance: deterministic request-budget admission.
//!
//! Two layers:
//!
//! * **Pure** deterministic generated-sequence properties over
//!   [`agalma_contracts::budget::SpendState`] (fixed SplitMix64 seed, bounded
//!   case/step budget): no over-admission, duplicate-settlement idempotence,
//!   unknown-liability retention, cancellation/no-refund, cash/quota separation.
//! * **Fake-provider** behavior against a real loopback upstream: an
//!   over-budget request is refused (`402`) and the upstream is **never**
//!   contacted; a durable-commit failure refuses before send; an in-budget
//!   request is reserved, sent once, and settled from authoritative usage.
//!
//! Scratch dirs live under `target/test-runs/` (never `/tmp`) and are retained
//! for review evidence (`ScratchDir::keep`). All offline; no live provider call.

#![allow(clippy::too_many_arguments)]

use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use agalma_conductor::budget::{
    load_execution_budget, role_key, AdmissionScope, BudgetGate, ExecutionBudget, ExecutionGates,
    GateAdmission, PaidRequestGate, ReservationLedger,
};
use agalma_conductor::provider_proxy::{AllowList, InjectConfig, ProviderProxy, ProxyConfig};
use agalma_contracts::budget::{
    BudgetError, Nusd, ProviderRequestProfile, Reservation, ReservationState, SpendState,
    UsageObservation,
};
use agalma_contracts::{ContractError, ExecutionId, Role};
use agalma_ledger::SqliteLedger;
use agalma_testkit::ScratchDir;
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// Test scratch retained on disk for review evidence (`ScratchDir::keep`).
fn scratch(name: &str) -> ScratchDir {
    let mut dir = ScratchDir::new(name);
    dir.keep();
    dir
}

// ---------------------------------------------------------------------------
// Common fixtures
// ---------------------------------------------------------------------------

const EXECUTION: &str = "exec:sf-budget:1";
const PACKAGE: &str = "sha256:package";
const RESERVATION_NUSD: u64 = 14_336_000;

fn scope(role: Role) -> AdmissionScope {
    AdmissionScope::new(EXECUTION, PACKAGE, role)
}

fn reservation(id: &str, role: Role, liability: Nusd, quota: u64) -> Reservation {
    Reservation {
        request_id: id.to_string(),
        execution_id: EXECUTION.to_string(),
        package_digest: PACKAGE.to_string(),
        role,
        profile_digest: "sha256:profile".to_string(),
        max_liability_nusd: liability,
        quota_units: quota,
        state: ReservationState::Reserved,
        charged_nusd: None,
        created_at_unix_ms: 1,
    }
}

/// A Send durable port that keeps records in memory and can inject a commit
/// failure (regression coverage for the durable-before-send invariant).
#[derive(Default)]
struct MemoryLedger {
    records: Vec<Reservation>,
    fail: bool,
}

impl ReservationLedger for MemoryLedger {
    fn load_records(
        &mut self,
        _execution: &ExecutionId,
    ) -> Result<Vec<Reservation>, ContractError> {
        Ok(self.records.clone())
    }
    fn commit_record(
        &mut self,
        _execution: &ExecutionId,
        record: &Reservation,
    ) -> Result<(), ContractError> {
        if self.fail {
            return Err(ContractError::KnownFailure(
                "injected commit failure".to_string(),
            ));
        }
        self.records.push(record.clone());
        Ok(())
    }
}

fn memory_budget(execution: &str, allowance: Nusd) -> Arc<ExecutionBudget> {
    ExecutionBudget::open(
        Box::new(MemoryLedger::default()),
        ExecutionId::new(execution),
        Some(allowance),
        None,
    )
    .expect("open memory budget")
}

fn memory_gate(execution: &str, allowance: Nusd, role: Role) -> Arc<BudgetGate> {
    Arc::new(BudgetGate::new(
        AdmissionScope::new(execution, PACKAGE, role),
        ProviderRequestProfile::fake_byte_token(),
        memory_budget(execution, allowance),
    ))
}

fn request_body(max_tokens: u64) -> String {
    json!({ "model": "fake-byte-token", "max_tokens": max_tokens }).to_string()
}

// ---------------------------------------------------------------------------
// Deterministic generator (SplitMix64), fixed seed and bounded budget
// ---------------------------------------------------------------------------

const PROP_SEED: u64 = 0x5F7_0B17_0000_0001;
const PROP_CASES: u64 = 160;
const PROP_STEPS: usize = 32;

struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Rng(seed)
    }
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next_u64() % n
    }
}

// ---------------------------------------------------------------------------
// Pure properties
// ---------------------------------------------------------------------------

/// No sequence of proposals ever admits more cash than the allowance, and the
/// separate quota bucket is never mixed into the cash figure.
#[test]
fn property_no_over_admission() {
    let mut rng = Rng::new(PROP_SEED);
    let mut admitted_cases = 0u64;
    for case in 0..PROP_CASES {
        let allowance = rng.below(40_000_000) + 1;
        let quota_cap = rng.below(8);
        let mut state = SpendState {
            allowance_nusd: Some(allowance),
            quota_cap_units: Some(quota_cap),
            ..Default::default()
        };
        for step in 0..PROP_STEPS {
            let liability = rng.below(16_000_000) + 1;
            let quota = rng.below(3);
            let proposed =
                reservation(&format!("c{case}-s{step}"), Role::Builder, liability, quota);
            match state.reserve(proposed.clone()) {
                Ok(_) => {
                    admitted_cases += 1;
                    assert!(
                        state.cash_spent_nusd() + state.outstanding_nusd() <= allowance,
                        "cash committed {} exceeds allowance {allowance}",
                        state.cash_spent_nusd() + state.outstanding_nusd()
                    );
                    assert!(
                        state.quota_committed_units() <= quota_cap,
                        "quota committed {} exceeds cap {quota_cap}",
                        state.quota_committed_units()
                    );
                }
                Err(BudgetError::OverBudget { .. }) => {
                    let proposed_total = state
                        .cash_spent_nusd()
                        .saturating_add(state.outstanding_nusd())
                        .saturating_add(liability);
                    assert!(
                        proposed_total > allowance,
                        "refused although it fit: {proposed_total} <= {allowance}"
                    );
                }
                Err(BudgetError::OverQuota { .. }) => {
                    assert!(state.quota_committed_units() + quota > quota_cap);
                }
                Err(other) => panic!("unexpected reservation error: {other}"),
            }
        }
    }
    assert!(
        admitted_cases > 0,
        "the generated budget must admit some work"
    );
}

/// Applying the same complete observation twice never double counts.
#[test]
fn property_duplicate_settlement_is_idempotent() {
    let mut rng = Rng::new(PROP_SEED ^ 0x11);
    let mut settled_cases = 0u64;
    for case in 0..PROP_CASES {
        let mut state = SpendState {
            allowance_nusd: Some(1_000_000_000),
            quota_cap_units: None,
            ..Default::default()
        };
        for step in 0..PROP_STEPS {
            let liability = rng.below(5_000_000) + 1;
            let id = format!("c{case}-s{step}");
            state
                .reserve(reservation(&id, Role::Builder, liability, 0))
                .expect("in budget");
            state.mark_sent(&id).expect("sent");
            let charged = rng.below(liability + 1);
            state.settle(&id, Some(charged)).expect("settle");
            let after_first = state.clone();
            state.settle(&id, Some(charged)).expect("duplicate settle");
            state.settle(&id, Some(9_999_999)).expect("later settle");
            assert_eq!(state, after_first, "settlement is idempotent");
            settled_cases += 1;
        }
    }
    assert!(settled_cases > 0);
}

/// Partial/missing usage always retains the full liability (never becomes zero)
/// and suppresses any further paid dispatch in the scope until reconciled.
#[test]
fn property_unknown_liability_is_retained() {
    let mut rng = Rng::new(PROP_SEED ^ 0x22);
    let mut unknown_cases = 0u64;
    for case in 0..PROP_CASES {
        for step in 0..PROP_STEPS {
            let mut state = SpendState {
                allowance_nusd: Some(1_000_000_000),
                quota_cap_units: None,
                ..Default::default()
            };
            let liability = rng.below(5_000_000) + 1;
            let id = format!("c{case}-s{step}");
            state
                .reserve(reservation(&id, Role::Verifier, liability, 0))
                .expect("in budget");
            state.mark_sent(&id).expect("sent");
            state.settle(&id, None).expect("unknown settle");
            let record = &state.reservations[&id];
            assert_eq!(record.state, ReservationState::Unconfirmed);
            assert_eq!(record.outstanding_nusd(), liability);
            assert_eq!(state.outstanding_nusd(), liability);
            let next = reservation(&format!("{id}-next"), Role::Builder, 1, 0);
            assert_eq!(
                state.reserve(next),
                Err(BudgetError::UnknownUsage),
                "unknown usage suppresses new admissions"
            );
            unknown_cases += 1;
        }
    }
    assert!(unknown_cases > 0);
}

/// Cancellation releases only never-sent work; it can never refund an incurred
/// charge held by a sent/unconfirmed reservation.
#[test]
fn property_cancellation_cannot_refund_incurred_charges() {
    let mut rng = Rng::new(PROP_SEED ^ 0x33);
    for case in 0..PROP_CASES {
        for step in 0..PROP_STEPS {
            let mut state = SpendState {
                allowance_nusd: Some(1_000_000_000),
                quota_cap_units: None,
                ..Default::default()
            };
            let liability = rng.below(5_000_000) + 1;
            let id = format!("c{case}-s{step}");
            state
                .reserve(reservation(&id, Role::Builder, liability, 0))
                .expect("in budget");
            let sent = rng.below(2) == 1;
            if sent {
                state.mark_sent(&id).expect("sent");
            }
            let before = state.outstanding_nusd();
            state.cancel(&id).expect("cancel");
            if sent {
                assert_eq!(
                    state.outstanding_nusd(),
                    before,
                    "a sent charge is retained after cancellation"
                );
            } else {
                assert_eq!(
                    state.outstanding_nusd(),
                    before - liability,
                    "an unsent reservation is released"
                );
            }
        }
    }
}

/// Cash and quota are tracked in their own units and never summed.
#[test]
fn separate_quota_accounting_and_unknown_cap() {
    let mut state = SpendState {
        allowance_nusd: Some(1_000_000_000),
        quota_cap_units: Some(5),
        ..Default::default()
    };
    state
        .reserve(reservation("a", Role::Builder, RESERVATION_NUSD, 3))
        .expect("cash and quota both fit");
    assert_eq!(state.cash_spent_nusd(), 0);
    assert_eq!(state.outstanding_nusd(), RESERVATION_NUSD);
    assert_eq!(state.quota_committed_units(), 3);

    let mut unknown = SpendState {
        allowance_nusd: Some(1_000_000_000),
        quota_cap_units: None,
        ..Default::default()
    };
    assert_eq!(
        unknown.reserve(reservation("b", Role::Builder, 1, 1)),
        Err(BudgetError::UnknownQuota)
    );

    let mut boundary = SpendState {
        allowance_nusd: Some(1_000_000_000),
        quota_cap_units: Some(2),
        ..Default::default()
    };
    boundary
        .reserve(reservation("c", Role::Builder, 1, 2))
        .expect("boundary-equal quota admitted");
    assert!(matches!(
        boundary.reserve(reservation("d", Role::Builder, 1, 1)),
        Err(BudgetError::OverQuota { .. })
    ));
}

/// Boundary-equal **cash** allowance is admitted; one nanoUSD over is refused.
#[test]
fn boundary_equal_cash_allowance() {
    let exact = ProviderRequestProfile::fake_byte_token()
        .reservation_nusd()
        .unwrap();
    let mut state = SpendState {
        allowance_nusd: Some(exact),
        quota_cap_units: None,
        ..Default::default()
    };
    state
        .reserve(reservation("r", Role::Builder, exact, 0))
        .expect("exactly the allowance fits");
    let mut over = SpendState {
        allowance_nusd: Some(exact),
        quota_cap_units: None,
        ..Default::default()
    };
    assert!(matches!(
        over.reserve(reservation("r", Role::Builder, exact + 1, 0)),
        Err(BudgetError::OverBudget { .. })
    ));
}

/// Unknown allowance denies admission outright.
#[test]
fn unknown_allowance_denies() {
    let mut state = SpendState::default();
    assert_eq!(
        state.reserve(reservation("r", Role::Builder, 1, 0)),
        Err(BudgetError::UnknownAllowance)
    );
}

// ---------------------------------------------------------------------------
// Cross-role shared-cap concurrency (durable-before-send findings)
// ---------------------------------------------------------------------------

/// Deterministic sequential interleave across roles sharing one execution
/// budget: the joint commitment never exceeds the allowance, every admitted
/// request is attributed, and each admission committed durably.
#[test]
fn property_cross_role_shared_cap() {
    let roles = [
        Role::Builder,
        Role::Verifier,
        Role::Planner,
        Role::Reviewer,
        Role::Evaluator,
    ];
    let mut rng = Rng::new(PROP_SEED ^ 0x44);
    let mut multi_role_admits = 0u64;
    for case in 0..PROP_CASES {
        let allowance = (rng.below(4) + 1) * RESERVATION_NUSD;
        let execution = format!("exec:cr:{case}");
        let shared = memory_budget(&execution, allowance);
        let gates: Vec<BudgetGate> = roles
            .iter()
            .map(|role| {
                BudgetGate::new(
                    AdmissionScope::new(execution.clone(), PACKAGE, *role),
                    ProviderRequestProfile::fake_byte_token(),
                    Arc::clone(&shared),
                )
            })
            .collect();
        let mut admitted = 0u64;
        for step in 0..(PROP_STEPS * roles.len()) {
            let gate = &gates[(rng.below(roles.len() as u64) as usize + step) % gates.len()];
            match gate.inspect(
                "POST",
                "/chat/completions",
                &request_body(64).into_bytes(),
                1,
            ) {
                Ok(agalma_conductor::budget::GateAdmission::Billable(_)) => admitted += 1,
                Ok(agalma_conductor::budget::GateAdmission::NonBillable) => unreachable!(),
                Err(BudgetError::OverBudget { .. }) => {}
                Err(other) => panic!("unexpected admission error: {other}"),
            }
            let snap = shared.snapshot();
            assert!(
                snap.cash_spent_nusd() + snap.outstanding_nusd() <= allowance,
                "cross-role commitment {} exceeds {allowance}",
                snap.cash_spent_nusd() + snap.outstanding_nusd()
            );
        }
        assert_eq!(
            admitted * RESERVATION_NUSD,
            shared.snapshot().outstanding_nusd(),
            "every admitted request is attributed"
        );
        if admitted > 1 {
            multi_role_admits += 1;
        }
    }
    assert!(
        multi_role_admits > 0,
        "the generated budget must admit across roles"
    );
}

/// Concurrent roles/attempts sharing one budget cannot jointly over-admit: the
/// shared atomic admission serialises them so exactly `allowance / reservation`
/// requests survive.
#[test]
fn concurrent_cross_role_admission_never_exceeds_allowance() {
    let allowance = RESERVATION_NUSD * 3;
    let shared = memory_budget(EXECUTION, allowance);
    let roles = [
        Role::Builder,
        Role::Verifier,
        Role::Planner,
        Role::Reviewer,
        Role::Evaluator,
    ];

    let handles: Vec<std::thread::JoinHandle<u64>> = roles
        .iter()
        .map(|role| {
            let gate = Arc::new(BudgetGate::new(
                scope(*role),
                ProviderRequestProfile::fake_byte_token(),
                Arc::clone(&shared),
            ));
            std::thread::spawn(move || {
                let mut admitted = 0u64;
                for _ in 0..64 {
                    match gate.inspect(
                        "POST",
                        "/chat/completions",
                        &request_body(64).into_bytes(),
                        1,
                    ) {
                        Ok(agalma_conductor::budget::GateAdmission::Billable(_)) => admitted += 1,
                        Ok(agalma_conductor::budget::GateAdmission::NonBillable) => unreachable!(),
                        Err(BudgetError::OverBudget { .. }) => break,
                        Err(other) => panic!("unexpected admission error: {other}"),
                    }
                }
                admitted
            })
        })
        .collect();

    let admitted: u64 = handles.into_iter().map(|h| h.join().expect("thread")).sum();
    assert_eq!(
        admitted, 3,
        "exactly allowance/reservation requests admit across all roles"
    );
    let snap = shared.snapshot();
    assert_eq!(snap.cash_spent_nusd(), 0);
    assert_eq!(snap.outstanding_nusd(), allowance);
}

/// Durable reservations checkpoint through the **bound ledger API** and are
/// rebuilt after a restart; a crash/timeout retains the outstanding liability.
#[test]
fn durable_reservations_rebuild_through_bound_ledger() {
    let dir = scratch("sf-budget-rebuild");
    let execution = ExecutionId::new(EXECUTION);
    let ledger = SqliteLedger::open(dir.join("ledger.sqlite")).expect("open bound ledger");
    let shared = ExecutionBudget::open(Box::new(ledger), execution.clone(), Some(20_000_000), None)
        .expect("open budget");

    let gate = BudgetGate::new(
        scope(Role::Builder),
        ProviderRequestProfile::fake_byte_token(),
        Arc::clone(&shared),
    );
    let ticket = match gate
        .inspect("POST", "/chat/completions", request_body(512).as_bytes(), 1)
        .expect("admitted")
    {
        agalma_conductor::budget::GateAdmission::Billable(t) => t,
        other => panic!("expected billable, got {other:?}"),
    };
    gate.mark_sent(&ticket);

    let ledger2 = SqliteLedger::open(dir.join("ledger.sqlite")).expect("reopen bound ledger");
    let reborn = load_execution_budget(Box::new(ledger2), &execution, Some(20_000_000), None)
        .expect("rebuild");
    assert_eq!(
        reborn.snapshot().outstanding_nusd(),
        RESERVATION_NUSD,
        "the crash/restart retains the outstanding reservation"
    );
    assert_eq!(
        reborn
            .snapshot()
            .reservations
            .values()
            .filter(|r| r.state == ReservationState::Sent)
            .count(),
        1,
        "the sent state survived restart"
    );
}

// ---------------------------------------------------------------------------
// Fake-provider proxy tests
// ---------------------------------------------------------------------------

/// A fake loopback upstream that counts accepted connections and answers each
/// request with a fixed JSON body.
struct FakeUpstream {
    addr: SocketAddr,
    accepted: Arc<AtomicUsize>,
    handle: tokio::task::JoinHandle<()>,
}

impl FakeUpstream {
    async fn spawn(body: &'static str) -> FakeUpstream {
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind upstream");
        let addr = listener.local_addr().expect("upstream addr");
        let accepted = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&accepted);
        let handle = tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                counter.fetch_add(1, Ordering::SeqCst);
                let _ = read_http_request(&mut socket).await;
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.shutdown().await;
            }
        });
        FakeUpstream {
            addr,
            accepted,
            handle,
        }
    }
}

impl Drop for FakeUpstream {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

/// Read an HTTP request head, then its `Content-Length` body.
async fn read_http_request(socket: &mut TcpStream) -> Vec<u8> {
    let mut buf = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        match socket.read(&mut byte).await {
            Ok(0) | Err(_) => return buf,
            Ok(_) => {
                buf.push(byte[0]);
                if buf.ends_with(b"\r\n\r\n") {
                    break;
                }
            }
        }
    }
    let text = String::from_utf8_lossy(&buf).to_ascii_lowercase();
    let len = text
        .split("\r\n")
        .find_map(|l| l.strip_prefix("content-length:"))
        .and_then(|v| v.trim().parse::<usize>().ok())
        .unwrap_or(0);
    if len > 0 {
        let mut body = vec![0u8; len];
        let _ = socket.read_exact(&mut body).await;
        buf.extend_from_slice(&body);
    }
    buf
}

/// A proxy with injection pointed at `upstream`, allowlisting loopback, and the
/// given admission gate.
fn gated_proxy(upstream: SocketAddr, gate: Arc<BudgetGate>) -> ProviderProxy {
    let mut config = ProxyConfig::with_injection(
        InjectConfig::from_base(
            &format!("http://127.0.0.1:{}/zen/go/v1", upstream.port()),
            "fake-secret-token",
        )
        .expect("inject config"),
    );
    config.allow = AllowList::new(vec!["127.0.0.1".to_string()]);
    let dynamic: Arc<dyn PaidRequestGate> = gate;
    config.budget = Some(dynamic);
    ProviderProxy::start(config).expect("start proxy")
}

async fn send_forward(proxy: &ProviderProxy, body: &str) -> Vec<u8> {
    let mut client = TcpStream::connect(("127.0.0.1", proxy.port()))
        .await
        .expect("connect proxy");
    let request = format!(
        "POST /chat/completions HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: {}\r\n\r\n{}",
        body.len(),
        body
    );
    client
        .write_all(request.as_bytes())
        .await
        .expect("write request");
    let mut raw = Vec::new();
    client.read_to_end(&mut raw).await.expect("read response");
    raw
}

/// An over-budget request is refused by the parent proxy and the provider is
/// never contacted.
#[tokio::test]
async fn over_budget_never_reaches_upstream() {
    let upstream = FakeUpstream::spawn("{}").await;
    let gate = memory_gate(EXECUTION, 10_000_000, Role::Builder);
    let proxy = gated_proxy(upstream.addr, Arc::clone(&gate));

    let raw = send_forward(&proxy, &request_body(1024)).await;
    let text = String::from_utf8_lossy(&raw);
    assert!(
        text.starts_with("HTTP/1.1 402"),
        "expected 402, got {text:?}"
    );
    assert_eq!(
        upstream.accepted.load(Ordering::SeqCst),
        0,
        "provider never contacted"
    );
    assert_eq!(gate.snapshot().outstanding_nusd(), 0, "nothing reserved");
    proxy.stop().await;
}

/// A durable-commit failure refuses the request before send and leaves no
/// applied reservation (regression for the durable-before-send invariant).
#[tokio::test]
async fn commit_failure_refuses_before_upstream() {
    let upstream = FakeUpstream::spawn("{}").await;
    let budget = ExecutionBudget::open(
        Box::new(MemoryLedger {
            records: Vec::new(),
            fail: true,
        }),
        ExecutionId::new(EXECUTION),
        Some(20_000_000),
        None,
    )
    .expect("open failing budget");
    let gate = Arc::new(BudgetGate::new(
        scope(Role::Builder),
        ProviderRequestProfile::fake_byte_token(),
        Arc::clone(&budget),
    ));
    let proxy = gated_proxy(upstream.addr, Arc::clone(&gate));

    let raw = send_forward(&proxy, &request_body(1024)).await;
    let text = String::from_utf8_lossy(&raw);
    assert!(
        text.starts_with("HTTP/1.1 402"),
        "expected 402, got {text:?}"
    );
    assert_eq!(
        upstream.accepted.load(Ordering::SeqCst),
        0,
        "provider never contacted"
    );
    assert_eq!(
        budget.snapshot().outstanding_nusd(),
        0,
        "failed commit applies nothing"
    );
    proxy.stop().await;
}

/// An in-budget request is reserved, sent exactly once, and settled from
/// authoritative usage; role attribution is recorded.
#[tokio::test]
async fn within_budget_request_is_sent_once_and_settled() {
    let upstream =
        FakeUpstream::spawn(r#"{"usage":{"prompt_tokens":100,"completion_tokens":50}}"#).await;
    let gate = memory_gate(EXECUTION, 20_000_000, Role::Verifier);
    let proxy = gated_proxy(upstream.addr, Arc::clone(&gate));

    let raw = send_forward(&proxy, &request_body(1024)).await;
    let text = String::from_utf8_lossy(&raw);
    assert!(
        text.starts_with("HTTP/1.1 200"),
        "expected 200, got {text:?}"
    );
    assert_eq!(
        upstream.accepted.load(Ordering::SeqCst),
        1,
        "sent exactly once"
    );

    let snapshot = gate.snapshot();
    assert_eq!(snapshot.outstanding_nusd(), 0, "complete usage settled it");
    assert_eq!(snapshot.cash_spent_nusd(), 500_000);
    assert!(
        snapshot
            .reservations
            .values()
            .all(|r| r.role == Role::Verifier),
        "role spend attributed to the pinned verifier scope"
    );
    proxy.stop().await;
}

/// A response without authoritative usage retains the full liability (it never
/// becomes zero) and suppresses further paid dispatch until reconciled.
#[tokio::test]
async fn missing_usage_retains_full_liability_and_suppresses_dispatch() {
    let upstream = FakeUpstream::spawn(r#"{"choices":[]}"#).await;
    let gate = memory_gate(EXECUTION, 1_000_000_000, Role::Builder);
    let proxy = gated_proxy(upstream.addr, Arc::clone(&gate));

    let raw = send_forward(&proxy, &request_body(1024)).await;
    assert!(String::from_utf8_lossy(&raw).starts_with("HTTP/1.1 200"));
    let snapshot = gate.snapshot();
    assert_eq!(snapshot.outstanding_nusd(), RESERVATION_NUSD);
    assert_eq!(snapshot.cash_spent_nusd(), 0);

    let second = send_forward(&proxy, &request_body(1024)).await;
    assert!(
        String::from_utf8_lossy(&second).starts_with("HTTP/1.1 402"),
        "unknown usage suppresses paid dispatch"
    );
    assert_eq!(upstream.accepted.load(Ordering::SeqCst), 1);
    proxy.stop().await;
}

/// A declared output bound above the reviewed limit is denied before send.
#[tokio::test]
async fn over_limit_output_is_denied_before_send() {
    let upstream = FakeUpstream::spawn("{}").await;
    let gate = memory_gate(EXECUTION, 20_000_000, Role::Builder);
    let proxy = gated_proxy(upstream.addr, Arc::clone(&gate));

    let raw = send_forward(&proxy, &request_body(4096)).await;
    assert!(String::from_utf8_lossy(&raw).starts_with("HTTP/1.1 402"));
    assert_eq!(upstream.accepted.load(Ordering::SeqCst), 0);
    proxy.stop().await;
}

/// Non-billable metadata passes through without reserving.
#[tokio::test]
async fn metadata_passes_without_reserving() {
    let upstream = FakeUpstream::spawn(r#"{"data":[]}"#).await;
    let gate = memory_gate(EXECUTION, 1_000, Role::Builder);
    let proxy = gated_proxy(upstream.addr, Arc::clone(&gate));

    let mut client = TcpStream::connect(("127.0.0.1", proxy.port()))
        .await
        .expect("connect proxy");
    client
        .write_all(b"GET /models HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n")
        .await
        .expect("write metadata request");
    client.shutdown().await.expect("half-close after request");
    let mut raw = Vec::new();
    client.read_to_end(&mut raw).await.expect("read response");
    assert!(
        String::from_utf8_lossy(&raw).starts_with("HTTP/1.1 200"),
        "metadata is forwarded"
    );
    assert_eq!(gate.snapshot().outstanding_nusd(), 0);
    proxy.stop().await;
}

/// Role attribution and request identity are recorded per reservation.
#[test]
fn role_spend_attribution_is_recorded() {
    let mut state = SpendState {
        allowance_nusd: Some(100_000_000),
        quota_cap_units: None,
        ..Default::default()
    };
    for (role, liability) in [
        (Role::Builder, RESERVATION_NUSD),
        (Role::Verifier, 1_000_000),
        (Role::Evaluator, 2_000_000),
    ] {
        let record = reservation(&format!("req-{}", role_key(role)), role, liability, 0);
        state.reserve(record).expect("in budget");
    }
    let mut roles: Vec<Role> = state.reservations.values().map(|r| r.role).collect();
    roles.sort_by_key(|r| role_key(*r));
    assert_eq!(roles, vec![Role::Builder, Role::Evaluator, Role::Verifier]);
}

/// A duplicate reservation id with changed liability conflicts (never silently
/// overwrites); an identical repeat is an idempotent no-op.
#[test]
fn duplicate_reservation_identity_is_idempotent_or_conflicting() {
    let mut state = SpendState {
        allowance_nusd: Some(100_000_000),
        quota_cap_units: None,
        ..Default::default()
    };
    let first = reservation("dup", Role::Builder, 1_000, 0);
    state.reserve(first.clone()).expect("first reserve");
    let again = state.reserve(first.clone()).expect("identical repeat");
    assert_eq!(again.max_liability_nusd, 1_000);
    assert_eq!(state.reservations.len(), 1, "no double count");
    let changed = reservation("dup", Role::Builder, 2_000, 0);
    assert_eq!(
        state.reserve(changed),
        Err(BudgetError::Conflict("dup".to_string()))
    );
}

// ---------------------------------------------------------------------------
// agalma-c31: production role attribution, fail-closed profile, restart-safe ids
// ---------------------------------------------------------------------------

/// A durable reservation port whose records survive across simulated restarts
/// (a shared vector), so a fresh `ExecutionGates::open` rebuilds the same
/// durable state an earlier process left behind.
#[derive(Clone, Default)]
struct SharedLedger {
    records: Arc<std::sync::Mutex<Vec<Reservation>>>,
}

impl ReservationLedger for SharedLedger {
    fn load_records(
        &mut self,
        _execution: &ExecutionId,
    ) -> Result<Vec<Reservation>, ContractError> {
        Ok(self.records.lock().expect("shared ledger").clone())
    }
    fn commit_record(
        &mut self,
        _execution: &ExecutionId,
        record: &Reservation,
    ) -> Result<(), ContractError> {
        self.records
            .lock()
            .expect("shared ledger")
            .push(record.clone());
        Ok(())
    }
}

/// Production constructor: per-role gates over one shared execution budget.
fn production_gates(
    ledger: impl ReservationLedger + 'static,
    execution: &str,
    allowance: Nusd,
    roles: &[Role],
) -> Arc<ExecutionGates> {
    ExecutionGates::open(
        Box::new(ledger),
        ExecutionId::new(execution),
        PACKAGE,
        Some(allowance),
        None,
        ProviderRequestProfile::fake_byte_token(),
        roles,
    )
    .expect("open production gates")
}

/// A real proxy whose paid admission runs through the **production** per-role
/// gate (not a hand-built gate), pointed at `upstream`.
fn role_gated_proxy(
    upstream: SocketAddr,
    gates: &Arc<ExecutionGates>,
    role: Role,
) -> ProviderProxy {
    let gate = gates.gate(role).expect("role gate constructed");
    let mut config = ProxyConfig::with_injection(
        InjectConfig::from_base(
            &format!("http://127.0.0.1:{}/zen/go/v1", upstream.port()),
            "fake-secret-token",
        )
        .expect("inject config"),
    );
    config.allow = AllowList::new(vec!["127.0.0.1".to_string()]);
    let gate_arc: Arc<BudgetGate> = Arc::clone(gate);
    let dynamic: Arc<dyn PaidRequestGate> = gate_arc;
    config.budget = Some(dynamic);
    ProviderProxy::start(config).expect("start proxy")
}

/// Production builder and verifier requests are attributed to their own roles
/// while sharing one execution allowance.
#[tokio::test]
async fn production_gates_attribute_builder_and_verifier_separately() {
    let upstream =
        FakeUpstream::spawn(r#"{"usage":{"prompt_tokens":100,"completion_tokens":50}}"#).await;
    let gates = production_gates(
        MemoryLedger::default(),
        EXECUTION,
        4 * RESERVATION_NUSD,
        &[Role::Builder, Role::Verifier],
    );
    let builder = role_gated_proxy(upstream.addr, &gates, Role::Builder);
    let verifier = role_gated_proxy(upstream.addr, &gates, Role::Verifier);

    let b = send_forward(&builder, &request_body(512)).await;
    assert!(
        String::from_utf8_lossy(&b).starts_with("HTTP/1.1 200"),
        "builder admitted: {:?}",
        String::from_utf8_lossy(&b)
    );
    let v = send_forward(&verifier, &request_body(512)).await;
    assert!(
        String::from_utf8_lossy(&v).starts_with("HTTP/1.1 200"),
        "verifier admitted: {:?}",
        String::from_utf8_lossy(&v)
    );

    let snapshot = gates.budget().snapshot();
    let mut roles: Vec<Role> = snapshot.reservations.values().map(|r| r.role).collect();
    roles.sort_by_key(|r| role_key(*r));
    assert_eq!(
        roles,
        vec![Role::Builder, Role::Verifier],
        "each request is attributed to the proxy's own role"
    );
    assert_eq!(snapshot.outstanding_nusd(), 0);
    assert_eq!(snapshot.cash_spent_nusd(), 1_000_000);
    assert_eq!(upstream.accepted.load(Ordering::SeqCst), 2);

    builder.stop().await;
    verifier.stop().await;
}

/// Both roles share one allowance: the builder's admitted spend pushes the
/// verifier over the shared cap, and the verifier request never reaches upstream.
#[tokio::test]
async fn production_gates_share_allowance_across_roles() {
    let upstream =
        FakeUpstream::spawn(r#"{"usage":{"prompt_tokens":100,"completion_tokens":50}}"#).await;
    let gates = production_gates(
        MemoryLedger::default(),
        EXECUTION,
        RESERVATION_NUSD,
        &[Role::Builder, Role::Verifier],
    );
    let builder = role_gated_proxy(upstream.addr, &gates, Role::Builder);
    let verifier = role_gated_proxy(upstream.addr, &gates, Role::Verifier);

    let first = send_forward(&builder, &request_body(512)).await;
    assert!(String::from_utf8_lossy(&first).starts_with("HTTP/1.1 200"));

    let second = send_forward(&verifier, &request_body(512)).await;
    assert!(
        String::from_utf8_lossy(&second).starts_with("HTTP/1.1 402"),
        "shared allowance denies the second role: {:?}",
        String::from_utf8_lossy(&second)
    );
    assert_eq!(
        upstream.accepted.load(Ordering::SeqCst),
        1,
        "the denied verifier request never reaches upstream"
    );
    let snapshot = gates.budget().snapshot();
    assert_eq!(snapshot.reservations.len(), 1);
    assert_eq!(
        snapshot
            .reservations
            .values()
            .filter(|r| r.role == Role::Builder)
            .count(),
        1
    );

    builder.stop().await;
    verifier.stop().await;
}

/// Recovery-path regression (`agalma-c31`): the shared production constructor
/// that recovery (`recover_inflight`, `build_recovery_executor`) and fresh
/// dispatch both use yields a **distinct** verifier proxy, and a verifier request
/// through it is billed to the verifier. A recovered paid run must not route
/// verifier traffic through the builder proxy.
#[tokio::test]
async fn recovery_role_proxies_attribute_verifier_to_verifier() {
    let upstream =
        FakeUpstream::spawn(r#"{"usage":{"prompt_tokens":100,"completion_tokens":50}}"#).await;
    let mut base = ProxyConfig::with_injection(
        InjectConfig::from_base(
            &format!("http://127.0.0.1:{}/zen/go/v1", upstream.addr.port()),
            "fake-secret-token",
        )
        .expect("inject config"),
    );
    base.allow = AllowList::new(vec!["127.0.0.1".to_string()]);

    let proxies = agalma_conductor::work::execution_role_proxies(
        Box::new(MemoryLedger::default()),
        EXECUTION,
        PACKAGE,
        Some(4 * RESERVATION_NUSD),
        ProviderRequestProfile::fake_byte_token(),
        base,
    )
    .expect("recovery role proxies");
    assert_ne!(
        proxies.builder_url(),
        proxies.verifier_url(),
        "the verifier must not be routed through the builder proxy"
    );

    // The exact routing contract recovery uses: with role proxies the verifier
    // gets its own URL; without them it falls back to the shared free path.
    let (rb, rv) = agalma_conductor::work::executor_proxy_urls(Some(&proxies), None);
    assert_eq!(rb.as_deref(), Some(proxies.builder_url().as_str()));
    assert_eq!(
        rv.as_deref(),
        Some(proxies.verifier_url().as_str()),
        "recovery routes the verifier to the verifier proxy"
    );
    let (fb, fv) = agalma_conductor::work::executor_proxy_urls(None, Some("http://free".into()));
    assert_eq!(fb.as_deref(), Some("http://free"));
    assert_eq!(fv, None, "no verifier proxy on the free path");

    let b = send_forward(proxies.builder(), &request_body(512)).await;
    assert!(
        String::from_utf8_lossy(&b).starts_with("HTTP/1.1 200"),
        "builder admitted: {:?}",
        String::from_utf8_lossy(&b)
    );
    let v = send_forward(proxies.verifier(), &request_body(512)).await;
    assert!(
        String::from_utf8_lossy(&v).starts_with("HTTP/1.1 200"),
        "verifier admitted: {:?}",
        String::from_utf8_lossy(&v)
    );

    let snapshot = proxies.budget().snapshot();
    let mut roles: Vec<Role> = snapshot.reservations.values().map(|r| r.role).collect();
    roles.sort_by_key(|r| role_key(*r));
    assert_eq!(
        roles,
        vec![Role::Builder, Role::Verifier],
        "verifier spend is attributed to the verifier"
    );
    assert_eq!(upstream.accepted.load(Ordering::SeqCst), 2);
}

/// Property: for any generated sequence of role assignments, each admitted
/// request is attributed to the role of the gate that admitted it — attribution
/// is a function of the pinned gate, never shared mutable state.
///
/// Independent invariant: for every `Ok(Billable(ticket))`, the reservation
/// recorded under `ticket.request_id` has `role == the requesting role`.
#[test]
fn property_role_attribution_follows_the_requesting_role() {
    let roles = [
        Role::Builder,
        Role::Verifier,
        Role::Planner,
        Role::Reviewer,
        Role::Evaluator,
    ];
    let mut rng = Rng::new(PROP_SEED ^ 0x77);
    let mut admitted = 0u64;
    let mut multi_role_cases = 0u64;
    for case in 0..PROP_CASES {
        let exec = format!("exec:attr:{case}");
        let gates = production_gates(
            MemoryLedger::default(),
            &exec,
            (PROP_STEPS as u64 + 1) * RESERVATION_NUSD,
            &roles,
        );
        let mut seen_roles = std::collections::BTreeSet::new();
        for _ in 0..PROP_STEPS {
            let role = roles[rng.below(roles.len() as u64) as usize];
            let gate = gates.gate(role).expect("serialized gate resident");
            match gate.inspect("POST", "/chat/completions", &request_body(64).into_bytes(), 1) {
                Ok(GateAdmission::Billable(ticket)) => {
                    let snapshot = gates.budget().snapshot();
                    let record = snapshot
                        .reservations
                        .get(&ticket.request_id)
                        .expect("admitted reservation is recorded");
                    assert_eq!(
                        record.role, role,
                        "attribution must follow the requesting role {}",
                        role_key(role)
                    );
                    seen_roles.insert(role_key(role));
                    admitted += 1;
                }
                Ok(GateAdmission::NonBillable) => unreachable!("chat-completions is billable"),
                Err(other) => panic!("unexpected admission error: {other}"),
            }
        }
        if seen_roles.len() > 1 {
            multi_role_cases += 1;
        }
    }
    assert!(admitted > 0, "the generated roles must admit some work");
    assert!(
        multi_role_cases > 0,
        "the generated cases must exercise multiple roles in one execution"
    );
}

/// Property: a generated absent/invalid paid profile always denies paid
/// admission, so the production path (`task_role_proxies`) never reaches proxy
/// construction; only a well-formed, in-validity, priced profile is admitted.
///
/// Independent invariant: `resolve_paid_profile(true, input)` is `Ok(Some(_))`
/// iff the input is exactly a readable, schema-valid, `n == 1`, priced,
/// non-expired profile; every other generated input is `Err` (no ungated paid
/// proxy). The expected verdict is derived from the profile specification, not
/// from the resolver implementation.
#[test]
fn property_invalid_or_missing_paid_profile_never_yields_ungated_proxy() {
    let mut rng = Rng::new(PROP_SEED ^ 0x88);
    let dir = scratch("paid-profile-prop");
    let mut invalid_cases = 0u64;
    let mut valid_cases = 0u64;
    let mut checked = 0u64;

    for case in 0..PROP_CASES {
        let kind = rng.below(10);
        let mut profile = ProviderRequestProfile::fake_byte_token();
        let mut path: Option<String> = None;
        // `None` expected verdict == deny.
        let expect_valid = matches!(kind, 8 | 9);

        match kind {
            0 => {} // missing path
            1 => path = Some("   ".to_string()),
            2 => {
                path = Some(
                    dir.join(format!("absent-{case}.json"))
                        .to_string_lossy()
                        .into_owned(),
                )
            }
            3 => {
                let p = dir.join(format!("malformed-{case}.json"));
                std::fs::write(&p, format!("{{ not-json {case}").as_bytes()).expect("write");
                path = Some(p.to_string_lossy().into_owned());
            }
            4 => {
                let mut value = serde_json::to_value(&profile).expect("encode");
                value
                    .as_object_mut()
                    .expect("object")
                    .remove("model");
                let p = dir.join(format!("missing-field-{case}.json"));
                std::fs::write(&p, serde_json::to_vec(&value).expect("encode")).expect("write");
                path = Some(p.to_string_lossy().into_owned());
            }
            5 => profile.n = 2, // unsupported n
            6 => {
                profile.valid_from_unix_ms = 0;
                profile.valid_until_unix_ms = 1; // expired
            }
            7 => {
                profile.input_limit_tokens = u64::MAX;
                profile.max_input_price_per_million_nusd = u64::MAX; // unknown price
            }
            8 => {} // valid fake profile
            9 => {
                // Boundary-valid interval: the whole open interval up to u64::MAX.
                profile.valid_from_unix_ms = 0;
                profile.valid_until_unix_ms = u64::MAX;
            }
            _ => unreachable!(),
        }

        // Any non-`None`/empty path with an invalid profile is serialized to a
        // real file, so the resolver must reject on content, not on absence.
        if path.is_none() && kind != 0 && kind != 1 {
            let p = dir.join(format!("case-{case}.json"));
            std::fs::write(&p, serde_json::to_vec(&profile).expect("encode")).expect("write");
            path = Some(p.to_string_lossy().into_owned());
        }

        let outcome = agalma_conductor::work::resolve_paid_profile(true, path.as_deref());
        if expect_valid {
            let resolved = outcome.expect("valid profile must resolve");
            assert!(
                resolved.is_some(),
                "a valid profile must yield the reviewed profile"
            );
            valid_cases += 1;
        } else {
            assert!(
                outcome.is_err(),
                "invalid/missing profile (kind {kind}) must deny, got {outcome:?}"
            );
            invalid_cases += 1;
        }
        // No paid credential: no profile is required and no proxy is gated.
        assert_eq!(
            agalma_conductor::work::resolve_paid_profile(false, path.as_deref())
                .expect("free path never errors"),
            None
        );
        checked += 1;
    }
    assert!(checked > 0);
    assert!(valid_cases > 0, "the generator must produce valid profiles");
    assert!(
        invalid_cases > 0,
        "the generator must produce invalid/missing profiles"
    );
}

/// Duplicate send/settle observations for one ticket never double count and
/// never change the pinned role attribution.
#[test]
fn duplicate_observation_through_production_gate_is_stable() {
    let gates = production_gates(
        MemoryLedger::default(),
        EXECUTION,
        4 * RESERVATION_NUSD,
        &[Role::Verifier],
    );
    let gate = gates.gate(Role::Verifier).expect("verifier gate");
    let ticket = match gate
        .inspect("POST", "/chat/completions", &request_body(64).into_bytes(), 1)
        .expect("admitted")
    {
        GateAdmission::Billable(t) => t,
        other => panic!("expected billable, got {other:?}"),
    };
    gate.mark_sent(&ticket);
    let usage = UsageObservation {
        prompt_tokens: Some(100),
        completion_tokens: Some(50),
        reported_cash_nusd: None,
    };
    gate.settle_usage(&ticket, &usage);
    gate.settle_usage(&ticket, &usage);
    gate.mark_sent(&ticket);

    let snapshot = gate.snapshot();
    assert_eq!(snapshot.cash_spent_nusd(), 500_000, "settled once");
    assert_eq!(snapshot.outstanding_nusd(), 0);
    assert_eq!(snapshot.reservations.len(), 1);
    assert_eq!(snapshot.reservations[&ticket.request_id].role, Role::Verifier);
}

/// An absent, unreadable, invalid, or expired paid profile denies paid
/// admission; there is no ungated fallback. No paid credential (`paid_available
/// == false`) leaves the free path unchanged.
#[test]
fn missing_or_invalid_paid_profile_denies_ungated_fallback() {
    use agalma_conductor::work::resolve_paid_profile;

    // No paid dispatch available -> free path, no profile needed.
    assert_eq!(resolve_paid_profile(false, None).expect("free path"), None);

    // Paid dispatch with no profile path at all -> deny (no ungated fallback).
    assert!(resolve_paid_profile(true, None).is_err());
    assert!(resolve_paid_profile(true, Some("   ")).is_err());

    // Unreadable / invalid / expired profiles -> deny.
    assert!(resolve_paid_profile(true, Some("/no/such/paid-profile.json")).is_err());

    let dir = scratch("paid-profile-c31");
    std::fs::write(dir.join("invalid.json"), b"{ not json").expect("write invalid");
    assert!(resolve_paid_profile(true, Some(dir.join("invalid.json").to_str().unwrap())).is_err());

    let mut expired = ProviderRequestProfile::fake_byte_token();
    expired.valid_from_unix_ms = 0;
    expired.valid_until_unix_ms = 1;
    std::fs::write(
        dir.join("expired.json"),
        serde_json::to_vec(&expired).expect("encode expired"),
    )
    .expect("write expired");
    assert!(resolve_paid_profile(true, Some(dir.join("expired.json").to_str().unwrap())).is_err());

    // A valid reviewed profile is admitted.
    let valid = ProviderRequestProfile::fake_byte_token();
    std::fs::write(
        dir.join("valid.json"),
        serde_json::to_vec(&valid).expect("encode valid"),
    )
    .expect("write valid");
    let got = resolve_paid_profile(true, Some(dir.join("valid.json").to_str().unwrap()))
        .expect("valid profile")
        .expect("some profile");
    assert_eq!(got.model, "fake-byte-token");
}

/// After restart, a fresh request must not reuse a durable request id (which
/// would make `reserve` return the existing reservation as an idempotent
/// success and resend the billable request). The next id is derived from the
/// durable maximum, not the reservation count.
#[test]
fn restart_next_id_exceeds_durable_maximum_and_never_reuses() {
    // Durable state with a same-role id (`builder:2`) and a gap below it: a
    // count-based seed of 2 would reissue `builder:2` and turn a fresh request
    // into a resend of the existing reservation.
    let ledger = SharedLedger::default();
    {
        let mut records = ledger.records.lock().expect("seed ledger");
        records.push(reservation(
            &format!("budget:{EXECUTION}:verifier:1"),
            Role::Verifier,
            RESERVATION_NUSD,
            0,
        ));
        records.push(reservation(
            &format!("budget:{EXECUTION}:builder:2"),
            Role::Builder,
            RESERVATION_NUSD,
            0,
        ));
    }
    let gates = production_gates(
        ledger.clone(),
        EXECUTION,
        4 * RESERVATION_NUSD,
        &[Role::Builder, Role::Verifier],
    );
    assert_eq!(
        gates.budget().snapshot().reservations.len(),
        2,
        "durable state rebuilt across the restart"
    );
    let builder = gates.gate(Role::Builder).expect("builder gate");
    let ticket = match builder
        .inspect("POST", "/chat/completions", &request_body(64).into_bytes(), 1)
        .expect("admitted")
    {
        GateAdmission::Billable(t) => t,
        other => panic!("expected billable, got {other:?}"),
    };
    assert_ne!(
        ticket.request_id,
        format!("budget:{EXECUTION}:builder:2"),
        "must not reuse a durable id"
    );
    assert!(
        ticket.request_id.ends_with(":3"),
        "fresh id must exceed the durable maximum: {}",
        ticket.request_id
    );
    let snapshot = gates.budget().snapshot();
    assert_eq!(
        snapshot.reservations.len(),
        3,
        "a fresh reservation is added, never reused"
    );
    assert_eq!(snapshot.outstanding_nusd(), 3 * RESERVATION_NUSD);
}

/// Property: for any durable set of request ids (including sparse/gappy ones),
/// the next generated id never collides with a durable id and its sequence
/// strictly exceeds the durable maximum.
#[test]
fn property_fresh_request_id_never_collides_with_durable_ids() {
    let mut rng = Rng::new(PROP_SEED ^ 0x55);
    let mut checked = 0u64;
    for case in 0..PROP_CASES {
        let exec = format!("exec:prop:{case}");
        let n = rng.below(6) + 1;
        let mut seeded: Vec<String> = Vec::new();
        let mut max_seq: Option<u64> = None;
        for _ in 0..n {
            let seq = rng.below(40);
            let role = if rng.below(2) == 0 {
                Role::Builder
            } else {
                Role::Verifier
            };
            let id = format!("budget:{exec}:{}:{seq}", role_key(role));
            if !seeded.contains(&id) {
                seeded.push(id);
            }
            max_seq = Some(max_seq.map_or(seq, |m| m.max(seq)));
        }
        let max_seq = max_seq.expect("at least one seeded id");

        let ledger = SharedLedger::default();
        {
            let mut records = ledger.records.lock().expect("seed ledger");
            for id in &seeded {
                records.push(Reservation {
                    request_id: id.clone(),
                    execution_id: exec.clone(),
                    package_digest: PACKAGE.to_string(),
                    role: Role::Builder,
                    profile_digest: "sha256:profile".to_string(),
                    max_liability_nusd: RESERVATION_NUSD,
                    quota_units: 0,
                    state: ReservationState::Reserved,
                    charged_nusd: None,
                    created_at_unix_ms: 1,
                });
            }
        }
        let allowance = (seeded.len() as u64 + 2) * RESERVATION_NUSD;
        let gates = production_gates(
            ledger.clone(),
            &exec,
            allowance,
            &[Role::Builder, Role::Verifier],
        );
        let gate = gates.gate(Role::Builder).expect("builder gate");
        let ticket = match gate
            .inspect("POST", "/chat/completions", &request_body(64).into_bytes(), 1)
            .expect("admitted")
        {
            GateAdmission::Billable(t) => t,
            other => panic!("expected billable, got {other:?}"),
        };
        assert!(
            !seeded.contains(&ticket.request_id),
            "fresh id collided with a durable id: {}",
            ticket.request_id
        );
        let seq: u64 = ticket
            .request_id
            .rsplit(':')
            .next()
            .expect("seq suffix")
            .parse()
            .expect("numeric seq");
        assert!(
            seq > max_seq,
            "fresh seq {seq} must exceed durable max {max_seq}"
        );
        checked += 1;
    }
    assert!(checked > 0);
}
