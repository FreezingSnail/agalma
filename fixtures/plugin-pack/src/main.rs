//! HP.8 independent plugin package (`agalma-jmr.8`).
//!
//! A real out-of-process plugin executable that speaks the frozen common plugin
//! protocol over the Connect RPC transport (`agalma-ipc`): it announces a
//! `READY` identity, serves `plugin.Describe`/`plugin.Health`/`plugin.Snapshot`/
//! `plugin.Invoke`/`plugin.Reconcile`/`plugin.Cancel`, and (when the hub
//! supplies its callback channel) publishes durable reports back over
//! `hub.PublishReport`.
//!
//! It uses only public contracts (`agalma-contracts`) and the common transport
//! (`agalma-ipc`): no conductor internals, no vendor handle, no private peer
//! storage. The hub learns nothing about this package before discovery.
//!
//! ## Effect journal (independent evidence)
//!
//! Every `plugin.Invoke` appends one JSON line to the file named by
//! `AGALMA_EFFECT_JOURNAL` (created on first effect). The hub never writes this
//! file; it is the plugin-side, independent record the HP.8 proof compares
//! against the hub's durable action receipts and the denied-action cases.
//!
//! ## Launch-channel environment
//!
//! * `AGALMA_HUB_SOCKET` — the hub capability RPC socket (spoke callbacks).
//! * `AGALMA_PLUGIN_REPORT_GRANT` — the per-launch `hub.PublishReport` grant.
//! * `AGALMA_PLUGIN_BINDING` — the pinned `BindingPin/v1` JSON for this launch.
//! * `AGALMA_EFFECT_JOURNAL` — independent plugin-side effect journal path.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use agalma_contracts::task_package::BindingPin;
use agalma_contracts::{
    canonical_digest, ActionIntent, ComponentOperationState, ComponentReceipt, ContractError,
    Durability, HealthState, PluginHealth, PluginOperationStatus, PluginSnapshot,
    PluginSnapshotEntry, ReportPublication, PLUGIN_OPERATION_STATUS_FORMAT, PLUGIN_SNAPSHOT_FORMAT,
    REPORT_PUBLICATION_FORMAT,
};
use agalma_ipc::plugin_server::{plugin_server_info, register_plugin, PluginHandler};
use agalma_ipc::{socket_from_args, HubCallbackClient, Server, Wire};
use serde_json::{json, Value};

/// The package's plugin id (unknown to any hub before discovery).
const PLUGIN_ID: &str = "example.plugin-pack";
/// Fallback implementation version when no installed manifest/binding is present.
const DEFAULT_VERSION: &str = "0.1.0";
/// The report namespace declared by the manifest.
const REPORT_NAMESPACE: &str = "example.plugin-pack.load";
/// The report schema version declared by the manifest.
const REPORT_VERSION: u64 = 1;
/// Comma-separated action ids whose `plugin.Invoke` applies the effect but
/// withholds the terminal report, so the hub parks the operation and settles it
/// later via `plugin.Reconcile` on its pinned generation.
const DELAY_ACTIONS_ENV: &str = "AGALMA_PLUGIN_DELAY_ACTIONS";

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// The pinned `BindingPin/v1` this launch serves.
///
/// Prefers the private launch-channel value (`AGALMA_PLUGIN_BINDING`) when the
/// hub conveys it, and otherwise derives it from this process's own hub-owned
/// installed generation. A spoke announces and reports its *pinned identity*;
/// deriving it locally keeps the fixture executable under the real host without
/// the hub importing plugin code or the fixture guessing a stale generation.
fn launch_binding() -> Option<BindingPin> {
    if let Ok(raw) = std::env::var("AGALMA_PLUGIN_BINDING") {
        if let Ok(binding) = serde_json::from_str::<BindingPin>(&raw) {
            return Some(binding);
        }
    }
    installed_binding()
}

/// Derive the pinned binding from this executable's hub-owned installed
/// location.
///
/// The host installs each generation at
/// `<state_dir>/plugin-installed/<plugin>/g<generation>/package/` and launches
/// `<...>/package/<entrypoint>` (here `bin/plugin-pack`). The package's own
/// sealed manifest (`plugin-manifest.json`, a sibling of `bin/`) pins the
/// default configuration whose digest **is** the generation's `config_digest`
/// (configured-root auto-install pins exactly the manifest default). The
/// generation is the `g<generation>` directory name. Returning the exact pinned
/// identity lets the spoke publish and report without a private hub channel.
fn installed_binding() -> Option<BindingPin> {
    let manifest = installed_manifest()?;
    let package = installed_package_dir()?;
    let generation = package
        .parent()?
        .file_name()?
        .to_str()?
        .strip_prefix('g')?
        .parse::<u64>()
        .ok()?;
    let config_digest = manifest
        .get("default_configuration")?
        .get("digest")?
        .as_str()?
        .to_string();
    let implementation_version = manifest.get("package_version")?.as_str()?.to_string();
    Some(BindingPin {
        binding_id: PLUGIN_ID.to_string(),
        api: "plugin".to_string(),
        api_version: 1,
        implementation_version,
        generation,
        config_digest,
    })
}

/// This process's `<...>/package` directory inside its hub-owned install.
fn installed_package_dir() -> Option<PathBuf> {
    std::env::current_exe()
        .ok()?
        .parent()?
        .parent()
        .map(PathBuf::from)
}

/// This generation's sealed `plugin-manifest.json`, when present.
fn installed_manifest() -> Option<Value> {
    let package = installed_package_dir()?;
    serde_json::from_slice(&std::fs::read(package.join("plugin-manifest.json")).ok()?).ok()
}

/// The implementation version this launch announces.
///
/// Matches the pinned binding's `implementation_version` (from the launch
/// channel or the installed manifest's `package_version`), so a 0.2.0 package
/// announces `0.2.0` and not a hardcoded default.
fn reported_version() -> String {
    if let Some(binding) = launch_binding() {
        return binding.implementation_version;
    }
    installed_manifest()
        .and_then(|manifest| {
            manifest
                .get("package_version")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_else(|| DEFAULT_VERSION.to_string())
}

/// Action ids whose invoke withholds the terminal report (see [`DELAY_ACTIONS_ENV`]).
fn delay_actions() -> Vec<String> {
    std::env::var(DELAY_ACTIONS_ENV)
        .ok()
        .map(|raw| {
            raw.split(',')
                .map(|entry| entry.trim().to_string())
                .filter(|entry| !entry.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

fn now_or(zero: u64) -> u64 {
    let n = now_ms();
    if n == 0 {
        zero
    } else {
        n
    }
}

/// Append one independent effect-journal line (best effort; the proof reads it).
fn journal_effect(record: &Value) {
    let Some(path) = std::env::var_os("AGALMA_EFFECT_JOURNAL") else {
        return;
    };
    let path = PathBuf::from(path);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(mut line) = serde_json::to_string(record) {
        line.push('\n');
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
        {
            use std::io::Write;
            let _ = file.write_all(line.as_bytes());
        }
    }
}

/// Whether an effect for `operation_id` was already applied (reconcile marker).
fn effect_applied(operation_id: &str) -> bool {
    let Some(path) = std::env::var_os("AGALMA_EFFECT_JOURNAL") else {
        return false;
    };
    let Ok(text) = std::fs::read_to_string(PathBuf::from(path)) else {
        return false;
    };
    text.lines()
        .any(|line| line.contains(&format!("\"operation_id\":\"{operation_id}\"")))
}

fn completed_status(intent: &ActionIntent, outcome: Value) -> PluginOperationStatus {
    let receipt = ComponentReceipt {
        operation_id: format!(
            "{}:{}",
            intent.binding.generation, intent.operation_id
        ),
        input_digest: intent.input_digest.clone(),
        binding: intent.binding.clone(),
        outcome,
        completed_at_unix_ms: now_or(1),
    };
    PluginOperationStatus {
        format: PLUGIN_OPERATION_STATUS_FORMAT.to_string(),
        plugin_id: intent.plugin_id.clone(),
        binding: intent.binding.clone(),
        operation_id: intent.operation_id.clone(),
        input_digest: intent.input_digest.clone(),
        state: ComponentOperationState::Completed,
        receipt: Some(receipt),
    }
}

/// A non-terminal status: the effect was applied but the terminal report is
/// withheld, so the hub parks the operation on its pinned generation and settles
/// it via `plugin.Reconcile`.
fn accepted_status(intent: &ActionIntent) -> PluginOperationStatus {
    PluginOperationStatus {
        format: PLUGIN_OPERATION_STATUS_FORMAT.to_string(),
        plugin_id: intent.plugin_id.clone(),
        binding: intent.binding.clone(),
        operation_id: intent.operation_id.clone(),
        input_digest: intent.input_digest.clone(),
        state: ComponentOperationState::Accepted,
        receipt: None,
    }
}

/// The spoke's common protocol handler.
struct PackHandler;

impl PluginHandler for PackHandler {
    fn describe(&self) -> Result<Value, ContractError> {
        Ok(json!({
            "api": "plugin",
            "protocol_major": agalma_ipc::PLUGIN_PROTOCOL_MAJOR,
            "plugin_id": PLUGIN_ID,
            "implementation_version": reported_version(),
            "capabilities": ["ledger.read", "report.publish"],
        }))
    }

    fn health(&self) -> Result<Value, ContractError> {
        let binding = launch_binding().unwrap_or(BindingPin {
            binding_id: PLUGIN_ID.to_string(),
            api: "plugin".to_string(),
            api_version: 1,
            implementation_version: reported_version(),
            generation: 0,
            config_digest: String::new(),
        });
        let health = PluginHealth {
            format: "PluginHealth/v1".to_string(),
            plugin_id: PLUGIN_ID.to_string(),
            binding,
            state: HealthState::Ready,
            detail: "plugin-pack serving".to_string(),
            checked_at_unix_ms: now_or(1),
        };
        serde_json::to_value(health).map_err(|e| ContractError::KnownFailure(e.to_string()))
    }

    fn snapshot(&self) -> Result<Value, ContractError> {
        let binding = launch_binding();
        let entry = PluginSnapshotEntry {
            schema_namespace: REPORT_NAMESPACE.to_string(),
            schema_version: REPORT_VERSION,
            payload: json!({"items": 0, "ready": true}),
            payload_digest: canonical_digest(&json!({"items": 0, "ready": true}))?,
        };
        let snapshot = PluginSnapshot {
            format: PLUGIN_SNAPSHOT_FORMAT.to_string(),
            plugin_id: PLUGIN_ID.to_string(),
            binding: binding.unwrap_or(BindingPin {
                binding_id: PLUGIN_ID.to_string(),
                api: "plugin".to_string(),
                api_version: 1,
                implementation_version: reported_version(),
                generation: 0,
                config_digest: String::new(),
            }),
            watermark: 1,
            snapshots: vec![entry],
            snapshot_digest: String::new(),
        };
        serde_json::to_value(snapshot).map_err(|e| ContractError::KnownFailure(e.to_string()))
    }

    fn invoke(&self, request: Value) -> Result<Value, ContractError> {
        let intent: ActionIntent = serde_json::from_value(request)
            .map_err(|e| ContractError::KnownFailure(format!("action intent: {e}")))?;
        // The effect: one independent journal line. The hub cannot write this.
        journal_effect(&json!({
            "action_id": intent.action_id,
            "operation_id": intent.operation_id,
            "plugin_id": intent.plugin_id,
            "generation": intent.binding.generation,
            "input": intent.input,
            "at_unix_ms": now_or(1),
        }));
        let status = if delay_actions().iter().any(|action| *action == intent.action_id) {
            // Applied but not yet reported terminal: the hub parks it pinned to
            // this generation and settles it via `plugin.Reconcile`.
            accepted_status(&intent)
        } else {
            completed_status(&intent, json!({"action": intent.action_id, "applied": true}))
        };
        serde_json::to_value(status).map_err(|e| ContractError::KnownFailure(e.to_string()))
    }

    fn reconcile(&self, request: Value) -> Result<Value, ContractError> {
        let intent: ActionIntent = serde_json::from_value(request)
            .map_err(|e| ContractError::KnownFailure(format!("action intent: {e}")))?;
        let status = if effect_applied(&intent.operation_id) {
            completed_status(&intent, json!({"action": intent.action_id, "applied": true}))
        } else {
            PluginOperationStatus {
                format: PLUGIN_OPERATION_STATUS_FORMAT.to_string(),
                plugin_id: intent.plugin_id.clone(),
                binding: intent.binding.clone(),
                operation_id: intent.operation_id.clone(),
                input_digest: intent.input_digest.clone(),
                state: ComponentOperationState::Accepted,
                receipt: None,
            }
        };
        serde_json::to_value(status).map_err(|e| ContractError::KnownFailure(e.to_string()))
    }

    fn cancel(&self, request: Value) -> Result<Value, ContractError> {
        let intent: ActionIntent = serde_json::from_value(request)
            .map_err(|e| ContractError::KnownFailure(format!("action intent: {e}")))?;
        let status = PluginOperationStatus {
            format: PLUGIN_OPERATION_STATUS_FORMAT.to_string(),
            plugin_id: intent.plugin_id.clone(),
            binding: intent.binding.clone(),
            operation_id: intent.operation_id.clone(),
            input_digest: intent.input_digest.clone(),
            state: ComponentOperationState::Cancelled,
            receipt: Some(ComponentReceipt {
                operation_id: format!(
                    "{}:{}",
                    intent.binding.generation, intent.operation_id
                ),
                input_digest: intent.input_digest.clone(),
                binding: intent.binding.clone(),
                outcome: json!({"outcome": "cancelled"}),
                completed_at_unix_ms: now_or(1),
            }),
        };
        serde_json::to_value(status).map_err(|e| ContractError::KnownFailure(e.to_string()))
    }
}

/// Publish one durable report to the hub, retrying while the hub socket comes up.
fn publish_report() {
    let (Some(hub), Some(grant)) = (
        std::env::var_os("AGALMA_HUB_SOCKET"),
        std::env::var("AGALMA_PLUGIN_REPORT_GRANT").ok(),
    ) else {
        return;
    };
    let Some(binding) = launch_binding() else {
        return;
    };
    let hub = PathBuf::from(hub);
    let payload = json!({"items": 1, "ready": true});
    let Ok(payload_digest) = canonical_digest(&payload) else {
        return;
    };
    let publication = ReportPublication {
        format: REPORT_PUBLICATION_FORMAT.to_string(),
        plugin_id: PLUGIN_ID.to_string(),
        binding: binding.clone(),
        schema_namespace: REPORT_NAMESPACE.to_string(),
        schema_version: REPORT_VERSION,
        sequence: 1,
        reported_at_unix_ms: now_or(1),
        execution_id: None,
        durability: Durability::Durable,
        payload,
        payload_digest,
    };
    let request = json!({
        "grant": grant,
        "plugin_id": PLUGIN_ID,
        "generation": binding.generation,
        "publication": publication,
    });
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(_) => return,
    };
    let _ = runtime.block_on(async move {
        let mut client = HubCallbackClient::connect(&hub).await?;
        client.publish_report(request).await
    });
}

fn main() {
    let socket = match socket_from_args(std::env::args().skip(1)) {
        Ok(socket) => socket,
        Err(err) => {
            eprintln!("plugin-pack: {err}");
            std::process::exit(2);
        }
    };

    // Publish the startup durable report on its own thread so the `READY`
    // handshake is not delayed by hub-callback retries.
    std::thread::spawn(publish_report);

    let mut server = Server::new(plugin_server_info(
        PLUGIN_ID,
        &reported_version(),
        ["ledger.read".to_string(), "report.publish".to_string()],
    ));
    register_plugin(&mut server, Arc::new(PackHandler));

    // `serve_wire` prints the single `READY {json}` handshake line, installs the
    // parent-death stdin watch, and serves the Connect transport (the same
    // framing the hub's `PluginClient` speaks).
    if let Err(err) = agalma_ipc::serve_wire(server, &socket, Wire::Connect) {
        eprintln!("plugin-pack: serve: {err}");
        std::process::exit(1);
    }
}
