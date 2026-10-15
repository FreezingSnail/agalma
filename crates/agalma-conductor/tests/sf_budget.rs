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
//! Scratch dirs live under `target/test-runs/` (never `/tmp`) and remove on
//! success. All offline; no live provider call.

#![allow(clippy::too_many_arguments)]

use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use agalma_conductor::budget::{
    load_execution_budget, role_key, AdmissionScope, BudgetGate, ExecutionBudget, PaidRequestGate,
    ReservationLedger,
};
use agalma_conductor::provider_proxy::{AllowList, InjectConfig, ProviderProxy, ProxyConfig};
use agalma_contracts::budget::{
    BudgetError, Nusd, ProviderRequestProfile, Reservation, ReservationState, SpendState,
};
use agalma_contracts::{ContractError, ExecutionId, Role};
use agalma_ledger::SqliteLedger;
use agalma_testkit::ScratchDir;
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

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
    let dir = ScratchDir::new("sf-budget-rebuild");
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
