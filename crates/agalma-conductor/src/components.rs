//! Per-component vision for the TUI read model (TUI.3, `agalma-3kr.4`).
//!
//! Derives one [`ComponentSnapshot`] per supervised component boundary from the
//! same read model the TUI already consumes (the TUI.1 snapshot): the component
//! binding registry (`LedgerApi::bindings`), per-operation binding pins
//! (`harness_binding` events), the boundary transports (the `AGALMA_*_TRANSPORT`
//! env selectors), the supervised server sockets under the state dir, and the
//! provider/watchdog health signals (`provider_health` events).
//!
//! Read-only and std-only: it opens no sockets, spawns nothing, and writes
//! nothing. The supervised server **pid** is not durable (it lives only in the
//! in-memory supervisor), so liveness is a proxy: the boundary socket file under
//! the state dir is present. This matches the v1 decision that there is no
//! `Health` type — liveness proxies only (`docs/tui-design.md`).

use std::collections::BTreeMap;
use std::path::Path;

use agalma_contracts::{BindingState, ExecutionRecord, LedgerApi};
use serde::Serialize;

/// Event kind carrying a provider/watchdog health signal.
pub const PROVIDER_HEALTH_KIND: &str = "provider_health";

/// The boundary transport selectors, one per supervised component.
///
/// Derived from the same env selectors the conductor binds with
/// (`AGALMA_LEDGER_TRANSPORT`, `AGALMA_HARNESS_TRANSPORT`, …). The sandbox
/// boundary is always a supervised process server (no in-process variant).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Transports {
    pub ledger: &'static str,
    pub harness: &'static str,
    pub sandbox: &'static str,
    pub workspace: &'static str,
    pub taskqueue: &'static str,
    pub execution: &'static str,
}

impl Transports {
    /// Read every boundary selector from the process environment.
    pub fn from_env() -> Self {
        Transports {
            ledger: crate::ledger_process::LedgerTransport::from_env().as_str(),
            harness: crate::harness_process::HarnessTransport::from_env().as_str(),
            sandbox: "process",
            workspace: crate::workspace_process::WorkspaceTransport::from_env().as_str(),
            taskqueue: crate::taskqueue_process::TaskQueueTransport::from_env().as_str(),
            execution: crate::execution_process::ExecutionTransport::from_env().as_str(),
        }
    }

    /// Every boundary in-process (the default; used by tests).
    pub fn all_in_process() -> Self {
        Transports {
            ledger: "in-process",
            harness: "in-process",
            sandbox: "process",
            workspace: "in-process",
            taskqueue: "in-process",
            execution: "in-process",
        }
    }

    /// Every boundary in-process except the given selectors set to `process`.
    pub fn with_process(ledger: bool, harness: bool) -> Self {
        let mut transports = Transports::all_in_process();
        if ledger {
            transports.ledger = "process";
        }
        if harness {
            transports.harness = "process";
        }
        transports
    }
}

/// One component row in the read model.
#[derive(Debug, Clone, Serialize)]
pub struct ComponentSnapshot {
    /// Stable component name (`ledger`, `harness`, `sandbox`, …).
    pub component: String,
    /// Component API name (`LedgerApi`, `HarnessApi`, …).
    pub api: String,
    /// API major version from the newest binding; `null` when unbound.
    pub api_version: Option<u32>,
    /// Boundary transport mode (`in-process` | `process`).
    pub transport: String,
    /// Socket file the supervised server binds (a glob for per-session sockets).
    pub socket: Option<String>,
    /// Liveness proxy: the socket file is present under the state dir.
    pub socket_present: bool,
    /// Component binding records, newest first.
    pub bindings: Vec<BindingView>,
    /// `provider_health` signals recorded for this component (harness only).
    pub stalls: u32,
    /// Reason of the most recent health signal, if any.
    pub last_health_reason: Option<String>,
}

/// One component binding record in the read model.
#[derive(Debug, Clone, Serialize)]
pub struct BindingView {
    pub binding_id: String,
    pub implementation: String,
    pub implementation_version: String,
    pub api_version: u32,
    pub generation: u64,
    /// `Debug` name of [`agalma_contracts::BindingState`] (`Active`, …).
    pub state: String,
    pub capabilities: Vec<String>,
    /// Operation ids whose binding pin points at this record.
    pub pinned_operations: Vec<String>,
}

/// A canonical component boundary: name, API, and socket file.
struct ComponentSpec {
    name: &'static str,
    api: &'static str,
    /// Exact socket file name, or a file-name prefix when `glob` is set.
    socket: &'static str,
    /// Whether `socket` is a prefix glob (per-session harness sockets).
    glob: bool,
}

/// The supervised component boundaries, in a stable display order.
const COMPONENTS: &[ComponentSpec] = &[
    ComponentSpec {
        name: "ledger",
        api: "LedgerApi",
        socket: crate::ledger_process::LEDGER_SOCKET_NAME,
        glob: false,
    },
    ComponentSpec {
        name: "harness",
        api: agalma_contracts::harness::HARNESS_API,
        socket: "harness",
        glob: true,
    },
    ComponentSpec {
        name: "sandbox",
        api: "SandboxApi",
        socket: crate::sandbox_process::SANDBOX_SOCKET_NAME,
        glob: false,
    },
    ComponentSpec {
        name: "workspace",
        api: "WorkspaceApi",
        socket: crate::workspace_process::WORKSPACE_SOCKET_NAME,
        glob: false,
    },
    ComponentSpec {
        name: "taskqueue",
        api: "TaskQueueApi",
        socket: crate::taskqueue_process::TASKQUEUE_SOCKET_NAME,
        glob: false,
    },
    ComponentSpec {
        name: "execution",
        api: "ExecutionApi",
        socket: crate::execution_process::EXECUTION_SOCKET_NAME,
        glob: false,
    },
];

/// Derive the per-component panel from the read model.
///
/// `ledger` is `None` when no ledger file is present (the panel still reports
/// transports and socket liveness; bindings and health are empty).
pub fn derive<L: LedgerApi + ?Sized>(
    state_dir: &Path,
    ledger: Option<&L>,
    records: &[ExecutionRecord],
    transports: &Transports,
) -> Vec<ComponentSnapshot> {
    let mut by_api: BTreeMap<String, Vec<BindingView>> = BTreeMap::new();
    let mut pins: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut stalls = 0u32;
    let mut last_health_reason: Option<String> = None;

    if let Some(ledger) = ledger {
        if let Ok(bindings) = ledger.bindings() {
            for binding in bindings {
                by_api
                    .entry(binding.component_api.clone())
                    .or_default()
                    .push(BindingView {
                        binding_id: binding.binding_id.to_string(),
                        implementation: binding.implementation,
                        implementation_version: binding.implementation_version,
                        api_version: binding.api_version,
                        generation: binding.generation,
                        state: format!("{:?}", binding.state),
                        capabilities: binding.capabilities.into_iter().collect(),
                        pinned_operations: Vec::new(),
                    });
            }
        }

        for record in records {
            let Ok(events) = ledger.events(&record.execution_id) else {
                continue;
            };
            for event in events {
                match event.kind.as_str() {
                    kind if kind == crate::harness_binding::BINDING_EVENT_KIND => {
                        if let (Some(binding_id), Some(operation_id)) = (
                            event.payload.get("binding_id").and_then(|v| v.as_str()),
                            event.payload.get("operation_id").and_then(|v| v.as_str()),
                        ) {
                            let entry = pins.entry(binding_id.to_string()).or_default();
                            if !entry.iter().any(|op| op == operation_id) {
                                entry.push(operation_id.to_string());
                            }
                        }
                    }
                    PROVIDER_HEALTH_KIND => {
                        stalls += 1;
                        if let Some(reason) = event.payload.get("reason").and_then(|v| v.as_str()) {
                            last_health_reason = Some(reason.to_string());
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    // Attach per-operation pins to their binding rows.
    for bindings in by_api.values_mut() {
        for binding in bindings.iter_mut() {
            if let Some(operations) = pins.remove(&binding.binding_id) {
                binding.pinned_operations = operations;
            }
        }
    }

    let mut components: Vec<ComponentSnapshot> = COMPONENTS
        .iter()
        .map(|spec| {
            let bindings = by_api.remove(spec.api).unwrap_or_default();
            let api_version = bindings.first().map(|binding| binding.api_version);
            let socket_present = socket_present(state_dir, spec);
            let is_harness = spec.api == agalma_contracts::harness::HARNESS_API;
            ComponentSnapshot {
                component: spec.name.to_string(),
                api: spec.api.to_string(),
                api_version,
                transport: transport_for(spec.name, transports).to_string(),
                socket: Some(socket_label(spec)),
                socket_present,
                bindings,
                stalls: if is_harness { stalls } else { 0 },
                last_health_reason: if is_harness {
                    last_health_reason.clone()
                } else {
                    None
                },
            }
        })
        .collect();

    // Any binding whose API is not a known boundary still shows (future-proof).
    for (api, bindings) in by_api {
        let api_version = bindings.first().map(|binding| binding.api_version);
        components.push(ComponentSnapshot {
            component: api.clone(),
            api,
            api_version,
            transport: "-".to_string(),
            socket: None,
            socket_present: false,
            bindings,
            stalls: 0,
            last_health_reason: None,
        });
    }

    components
}

/// Boundary transport selector for a component name.
fn transport_for(name: &str, transports: &Transports) -> &'static str {
    match name {
        "ledger" => transports.ledger,
        "harness" => transports.harness,
        "sandbox" => transports.sandbox,
        "workspace" => transports.workspace,
        "taskqueue" => transports.taskqueue,
        "execution" => transports.execution,
        _ => "-",
    }
}

/// Human-facing socket label (`sandbox.sock`, `harness*.sock`).
fn socket_label(spec: &ComponentSpec) -> String {
    if spec.glob {
        format!("{}*.sock", spec.socket)
    } else {
        spec.socket.to_string()
    }
}

/// Liveness proxy: a matching socket file exists under the state dir.
fn socket_present(state_dir: &Path, spec: &ComponentSpec) -> bool {
    if spec.glob {
        std::fs::read_dir(state_dir)
            .map(|entries| {
                entries.filter_map(Result::ok).any(|entry| {
                    let name = entry.file_name();
                    let name = name.to_string_lossy();
                    name.starts_with(spec.socket) && name.ends_with(".sock")
                })
            })
            .unwrap_or(false)
    } else {
        state_dir.join(spec.socket).exists()
    }
}

/// Debug label for a binding state (kept next to the view type so callers do not
/// re-derive it).
pub fn binding_state_label(state: BindingState) -> &'static str {
    match state {
        BindingState::Staged => "Staged",
        BindingState::Active => "Active",
        BindingState::Draining => "Draining",
        BindingState::Retired => "Retired",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agalma_contracts::{
        CommitBatch, DispatchIntent, ExecutionId, ExecutionPhase, ExecutionState, OperationId,
        TaskId,
    };
    use agalma_ledger::SqliteLedger;
    use agalma_testkit::ScratchDir;
    use serde_json::json;

    /// Seed a ledger with one execution, a pending operation, a binding pin, and
    /// two `provider_health` stall signals.
    fn seed(dir: &ScratchDir) -> (SqliteLedger, ExecutionId) {
        let exec = ExecutionId::derive(&TaskId::new("task-a"), 1);
        let mut ledger = SqliteLedger::open(dir.join("ledger.sqlite")).expect("open ledger");
        ledger
            .commit(CommitBatch {
                expected_revision: None,
                state: Some(agalma_contracts::ExecutionRecord {
                    execution_id: exec.clone(),
                    task_id: TaskId::new("task-a"),
                    generation: 1,
                    phase: ExecutionPhase::Build,
                    state: ExecutionState::Running,
                    attempt: 2,
                    revision: 0,
                }),
                events: vec![],
                receipts: vec![],
                intents: vec![DispatchIntent {
                    operation_id: OperationId::derive(&exec, "build@2"),
                    execution_id: exec.clone(),
                    payload: json!({}),
                    enqueued_at_unix_ms: 1,
                    consumed: false,
                }],
            })
            .expect("seed execution");
        let plan = crate::harness_binding::plan_binding(
            crate::harness_binding::HarnessKind::Reference,
            1,
            "model",
        )
        .expect("plan binding");
        crate::harness_binding::persist_binding(
            &mut ledger,
            &exec,
            &OperationId::derive(&exec, "build@2"),
            2,
            &plan,
        )
        .expect("persist binding");
        for reason in [
            crate::phases::PollStallReason::Idle,
            crate::phases::PollStallReason::SessionGone,
        ] {
            crate::phases::record_provider_health(
                dir.path(),
                &exec,
                "model",
                "reference@reference/v1",
                2,
                &crate::phases::PollStall {
                    reason,
                    idle_ms: 0,
                    tokens: 0,
                    wall_ms: 0,
                    last_state: String::new(),
                },
            )
            .expect("record health");
        }
        (ledger, exec)
    }

    #[test]
    fn absent_ledger_lists_components_without_bindings() {
        let dir = ScratchDir::new("components-absent");
        let components =
            derive::<SqliteLedger>(dir.path(), None, &[], &Transports::all_in_process());
        assert_eq!(components.len(), COMPONENTS.len());
        let harness = components
            .iter()
            .find(|c| c.component == "harness")
            .expect("harness row");
        assert_eq!(harness.transport, "in-process");
        assert!(harness.bindings.is_empty());
        assert_eq!(harness.stalls, 0);
        assert!(!harness.socket_present);
    }

    #[test]
    fn present_ledger_reports_bindings_and_health() {
        let dir = ScratchDir::new("components-present");
        let (ledger, exec) = seed(&dir);
        let components = derive(
            dir.path(),
            Some(&ledger),
            &ledger.executions().expect("executions"),
            &Transports::with_process(true, true),
        );

        let ledger_row = components
            .iter()
            .find(|c| c.component == "ledger")
            .expect("ledger row");
        assert_eq!(ledger_row.transport, "process");

        let harness = components
            .iter()
            .find(|c| c.component == "harness")
            .expect("harness row");
        assert_eq!(harness.transport, "process");
        assert_eq!(harness.api, "HarnessApi");
        assert_eq!(harness.api_version, Some(0));
        assert_eq!(harness.stalls, 2);
        assert_eq!(harness.last_health_reason.as_deref(), Some("session_gone"));
        assert_eq!(harness.bindings.len(), 1);
        let binding = &harness.bindings[0];
        assert_eq!(binding.implementation, "reference");
        assert_eq!(binding.implementation_version, "reference/v1");
        assert_eq!(binding.generation, 1);
        assert_eq!(binding.state, "Active");
        assert_eq!(
            binding.pinned_operations,
            vec![OperationId::derive(&exec, "build@2").to_string()]
        );
        assert!(binding.capabilities.contains(&"context_hooks".to_string()));
    }

    #[test]
    fn socket_liveness_tracks_files_under_state_dir() {
        let dir = ScratchDir::new("components-socket");
        std::fs::write(
            dir.path().join(crate::sandbox_process::SANDBOX_SOCKET_NAME),
            b"",
        )
        .expect("touch sandbox socket");
        std::fs::write(dir.path().join("harness-build.sock"), b"").expect("touch harness socket");
        let components =
            derive::<SqliteLedger>(dir.path(), None, &[], &Transports::all_in_process());

        let sandbox = components
            .iter()
            .find(|c| c.component == "sandbox")
            .expect("sandbox row");
        assert!(sandbox.socket_present);
        assert_eq!(sandbox.socket.as_deref(), Some("sandbox.sock"));

        let harness = components
            .iter()
            .find(|c| c.component == "harness")
            .expect("harness row");
        assert!(harness.socket_present, "harness*.sock glob should match");
        assert_eq!(harness.socket.as_deref(), Some("harness*.sock"));

        let ledger_row = components
            .iter()
            .find(|c| c.component == "ledger")
            .expect("ledger row");
        assert!(!ledger_row.socket_present);
    }

    #[test]
    fn binding_state_label_matches_states() {
        assert_eq!(binding_state_label(BindingState::Active), "Active");
        assert_eq!(binding_state_label(BindingState::Draining), "Draining");
        assert_eq!(binding_state_label(BindingState::Retired), "Retired");
        assert_eq!(binding_state_label(BindingState::Staged), "Staged");
    }
}
