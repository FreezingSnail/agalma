//! Harness binding resolution and persistence (M1.7).
//!
//! `AGALMA_HARNESS=opencode|reference|reference-fail` (or `agalma work --harness
//! <id>`) selects the implementation bound to the build phase. A [`BindingRecord`]
//! (S0d shape) is resolved and persisted **at the start of each operation**; an
//! in-flight or recovered operation reuses its pinned binding, while a new
//! attempt resolves the current configuration. This is the boundary the
//! architecture names: a harness change takes effect between attempts, never
//! mid-operation.

use std::cell::RefCell;
use std::collections::BTreeSet;
use std::rc::Rc;

use agalma_contracts::harness::{Describe, API_VERSION, HARNESS_API};
use agalma_contracts::{
    BindingId, BindingRecord, BindingState, CommitBatch, ContractError, ExecutionEvent,
    ExecutionId, LedgerApi, OperationId,
};
use agalma_harness_reference::{self as reference, ReferenceProfile};

/// Environment variable selecting the harness implementation.
pub const HARNESS_ENV: &str = "AGALMA_HARNESS";
/// Event kind carrying a persisted binding pin.
pub const BINDING_EVENT_KIND: &str = "harness_binding";

/// Static implementation identity for the live OpenCode adapter. Vendor
/// details stay in `agalma-harness-opencode`; this is only the binding name.
pub const OPENCODE_IMPLEMENTATION: &str = "opencode";
/// Version reported for the OpenCode binding.
pub const OPENCODE_VERSION: &str = "opencode-adapter/v1";

/// Concrete harness implementations a binding can select.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HarnessKind {
    OpenCode,
    Reference,
    ReferenceFail,
}

impl HarnessKind {
    /// Stable selector string (`--harness` / `AGALMA_HARNESS`).
    pub fn as_str(self) -> &'static str {
        match self {
            HarnessKind::OpenCode => "opencode",
            HarnessKind::Reference => "reference",
            HarnessKind::ReferenceFail => "reference-fail",
        }
    }

    /// Implementation name written into the binding record.
    pub fn implementation(self) -> &'static str {
        match self {
            HarnessKind::OpenCode => OPENCODE_IMPLEMENTATION,
            HarnessKind::Reference => reference::implementation(ReferenceProfile::Success),
            HarnessKind::ReferenceFail => reference::implementation(ReferenceProfile::Red),
        }
    }

    /// Map a recorded implementation name back to a kind.
    pub fn from_implementation(name: &str) -> Option<HarnessKind> {
        match name {
            OPENCODE_IMPLEMENTATION => Some(HarnessKind::OpenCode),
            "reference" => Some(HarnessKind::Reference),
            "reference-fail" => Some(HarnessKind::ReferenceFail),
            _ => None,
        }
    }

    fn describe(self) -> Describe {
        match self {
            HarnessKind::OpenCode => Describe {
                api: HARNESS_API.to_string(),
                api_version: API_VERSION,
                impl_name: OPENCODE_IMPLEMENTATION.to_string(),
                impl_version: OPENCODE_VERSION.to_string(),
                capabilities: BTreeSet::new(),
            },
            HarnessKind::Reference => reference::describe_for(ReferenceProfile::Success),
            HarnessKind::ReferenceFail => reference::describe_for(ReferenceProfile::Red),
        }
    }
}

/// `Describe` result for a kind (binding identity and advertised capabilities).
pub fn describe_for(kind: HarnessKind) -> Describe {
    kind.describe()
}

/// Mutable selection shared by the conductor and its activities. Tests flip the
/// cell between attempts; the binary fixes it once from flag/env.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HarnessChoice {
    OpenCode,
    Reference,
    ReferenceFail,
}

impl HarnessChoice {
    pub fn kind(self) -> HarnessKind {
        match self {
            HarnessChoice::OpenCode => HarnessKind::OpenCode,
            HarnessChoice::Reference => HarnessKind::Reference,
            HarnessChoice::ReferenceFail => HarnessKind::ReferenceFail,
        }
    }
}

/// Shared handle to the current harness selection.
pub type HarnessHandle = Rc<RefCell<HarnessChoice>>;

/// Wrap a choice in a shared handle.
pub fn harness_handle(choice: HarnessChoice) -> HarnessHandle {
    Rc::new(RefCell::new(choice))
}

/// Resolve the selection from an explicit `--harness` flag or `AGALMA_HARNESS`.
pub fn resolve_selection(flag: Option<&str>) -> Result<HarnessChoice, ContractError> {
    let raw = flag
        .map(str::to_string)
        .or_else(|| std::env::var(HARNESS_ENV).ok());
    match raw.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        None | Some("opencode") => Ok(HarnessChoice::OpenCode),
        Some("reference") => Ok(HarnessChoice::Reference),
        Some("reference-fail") => Ok(HarnessChoice::ReferenceFail),
        Some(other) => Err(ContractError::KnownFailure(format!(
            "unknown harness {other:?} (expected opencode|reference|reference-fail)"
        ))),
    }
}

/// The result of binding resolution: the durable record plus any requested
/// optional capability that fell back to a declared supported plan.
#[derive(Clone, Debug, PartialEq)]
pub struct BindPlan {
    pub binding: BindingRecord,
    /// Optional capabilities requested but not advertised; the conductor uses
    /// the S0d declared plan (a fresh session with an explicit handoff).
    pub fallback: Vec<String>,
}

impl BindPlan {
    /// The S0d declared fallback plan for a missing optional capability.
    pub fn fallback_plan(&self) -> Option<&'static str> {
        if self.fallback.is_empty() {
            None
        } else {
            Some("fresh_session+handoff")
        }
    }
}

/// Resolve a binding for `kind` against the `mvp-baseline` profile (empty
/// required and optional capability sets).
pub fn plan_binding(
    kind: HarnessKind,
    generation: u32,
    model: &str,
) -> Result<BindPlan, ContractError> {
    plan_from_describe(
        &kind.describe(),
        generation,
        model,
        &BTreeSet::new(),
        &BTreeSet::new(),
    )
}

/// Apply the S0d static bind gate: refuse an API/version mismatch or a missing
/// **required** capability; a missing **optional** capability yields a declared
/// fallback plan rather than a bind failure.
pub fn plan_from_describe(
    describe: &Describe,
    generation: u32,
    model: &str,
    required: &BTreeSet<String>,
    optional: &BTreeSet<String>,
) -> Result<BindPlan, ContractError> {
    if describe.api != HARNESS_API {
        return Err(ContractError::Conflict(format!(
            "binding expects API {HARNESS_API}, implementation reports {}",
            describe.api
        )));
    }
    if describe.api_version != API_VERSION {
        return Err(ContractError::Conflict(format!(
            "binding expects API version {API_VERSION}, implementation reports {}",
            describe.api_version
        )));
    }
    for capability in required {
        if !describe.capabilities.contains(capability) {
            return Err(ContractError::UnsupportedCapability(format!(
                "binding {} requires capability {capability} (not advertised)",
                describe.impl_name
            )));
        }
    }
    let fallback: Vec<String> = optional
        .iter()
        .filter(|capability| !describe.capabilities.contains(*capability))
        .cloned()
        .collect();
    let config_hash = hash(&format!(
        "{}|{}|{}",
        describe.impl_name, describe.impl_version, model
    ));
    let binding = BindingRecord {
        binding_id: BindingId::new(format!("binding:{}:{}", describe.impl_name, generation)),
        component_api: HARNESS_API.to_string(),
        api_version: API_VERSION,
        implementation: describe.impl_name.clone(),
        implementation_version: describe.impl_version.clone(),
        capabilities: describe.capabilities.clone(),
        required_capabilities: required.clone(),
        optional_capabilities: optional.clone(),
        config_hash,
        generation: u64::from(generation),
        state: BindingState::Active,
    };
    Ok(BindPlan { binding, fallback })
}

/// Read the binding pinned for `operation` (first recorded pin wins).
pub fn read_pinned_binding<L: LedgerApi + ?Sized>(
    ledger: &L,
    execution: &ExecutionId,
    operation: &OperationId,
) -> Result<Option<BindingRecord>, ContractError> {
    for event in ledger.events(execution)? {
        if event.kind != BINDING_EVENT_KIND {
            continue;
        }
        if event.payload.get("operation_id").and_then(|v| v.as_str()) != Some(operation.as_str()) {
            continue;
        }
        if let Some(value) = event.payload.get("binding") {
            if let Ok(record) = serde_json::from_value::<BindingRecord>(value.clone()) {
                return Ok(Some(record));
            }
        }
    }
    Ok(None)
}

/// Persist a binding for `operation`: the record in the component registry plus
/// a per-operation pin event. No-op when a pin already exists (never rebind an
/// in-flight operation).
#[allow(clippy::too_many_arguments)]
pub fn persist_binding<L: LedgerApi>(
    ledger: &mut L,
    execution: &ExecutionId,
    operation: &OperationId,
    attempt: u32,
    plan: &BindPlan,
) -> Result<(), ContractError> {
    if read_pinned_binding(ledger, execution, operation)?.is_some() {
        return Ok(());
    }
    ledger.upsert_binding(&plan.binding)?;
    let event = ExecutionEvent {
        execution_id: execution.clone(),
        sequence: 0,
        kind: BINDING_EVENT_KIND.to_string(),
        payload: serde_json::json!({
            "operation_id": operation.as_str(),
            "attempt": attempt,
            "binding_id": plan.binding.binding_id.as_str(),
            "implementation": plan.binding.implementation,
            "implementation_version": plan.binding.implementation_version,
            "generation": plan.binding.generation,
            "fallback": plan.fallback,
            "binding": plan.binding,
        }),
        recorded_at_unix_ms: now_ms(),
    };
    ledger.commit(CommitBatch {
        expected_revision: None,
        state: None,
        events: vec![event],
        receipts: vec![],
        intents: vec![],
    })?;
    Ok(())
}

/// Latest binding pin recorded for an execution (status projection).
pub fn latest_binding<L: LedgerApi + ?Sized>(
    ledger: &L,
    execution: &ExecutionId,
) -> Result<Option<BindingRecord>, ContractError> {
    let mut latest = None;
    for event in ledger.events(execution)? {
        if event.kind != BINDING_EVENT_KIND {
            continue;
        }
        if let Some(value) = event.payload.get("binding") {
            if let Ok(record) = serde_json::from_value::<BindingRecord>(value.clone()) {
                latest = Some(record);
            }
        }
    }
    Ok(latest)
}

/// Stable 64-bit FNV-1a hash (hex).
fn hash(text: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in text.as_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

fn now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
