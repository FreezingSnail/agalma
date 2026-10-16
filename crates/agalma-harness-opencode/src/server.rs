//! OpenCode `HarnessApi` component server (B1.2).
//!
//! Hosts the real [`OpenCodeHarness`] behind the selected wire transport
//! (IPCX.2: `Connect` default, `framed` via `AGALMA_IPC_WIRE`). The server is
//! trusted code: the `opencode serve` child is launched through the
//! [`agalma_contracts::ProcessApi`] boundary, implemented here by
//! [`agalma_process::LocalLauncher`] (no confinement profile).
//!
//! Configuration arrives from the environment (the conductor's `server_command()`
//! / `ProcessBinding` supplies it). The process transport serializes the full
//! explicit launch configuration into [`LAUNCH_CONFIG_ENV`] (see
//! [`LaunchConfig`]); [`config_from_env`] reconstructs the harness config from it
//! verbatim. The legacy per-variable form (`AGALMA_HARNESS_ROOT`,
//! `AGALMA_HARNESS_PROXY_URL`, `AGALMA_HARNESS_PAID_PROXY`, `AGALMA_MODEL`) is
//! kept for direct invocation. Process-backed methods need a Tokio reactor, so
//! the binary runs the serial serve loop inside a multithread runtime.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use agalma_contracts::harness::HarnessApi;
use agalma_contracts::process::ProcessApi;
use agalma_ipc::{register_harness, server_info_from, CommandSpec, Server, Wire};
use agalma_process::LocalLauncher;
use serde_json::{json, Value};

use crate::adapter::{OpenCodeHarness, OpenCodeHarnessConfig};
use crate::config::{build_config, OpenCodeConfigInit};

/// Binary name of the OpenCode component server.
pub const SERVER_BIN: &str = "agalma-harness-opencode-server";
/// Environment variable naming the attempt root.
pub const ROOT_ENV: &str = "AGALMA_HARNESS_ROOT";
/// Environment variable carrying the parent-owned proxy URL.
pub const PROXY_URL_ENV: &str = "AGALMA_HARNESS_PROXY_URL";
/// Environment variable enabling the paid provider override (`1`/`true`).
pub const PAID_ENV: &str = "AGALMA_HARNESS_PAID_PROXY";
/// Environment variable carrying the explicit worker launch configuration (JSON)
/// across the trusted conductor→server boundary (`agalma-tqf`).
///
/// The process transport serializes the caller-supplied launch fields into this
/// variable; the server reconstructs an identical [`OpenCodeHarnessConfig`] from
/// it. Only this explicit, conductor-built configuration crosses the boundary —
/// the parent environment and real provider credentials are never forwarded.
pub const LAUNCH_CONFIG_ENV: &str = "AGALMA_HARNESS_LAUNCH_CONFIG";

/// The explicit launch fields transferred across the process boundary.
///
/// Mirrors exactly the values the conductor hands [`build_config`]: the attempt
/// runtime root, the child environment, and the provider/proxy fields. The
/// server reconstructs its config from these verbatim, so a custom `bd` PATH
/// addition, toolchain homes, and `CARGO_HOME` survive the transport unchanged.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LaunchConfig {
    /// Attempt-owned runtime root.
    pub attempt_dir: PathBuf,
    /// Resolved model for the session.
    pub model: String,
    /// Parent-owned forward-proxy URL (provider egress).
    pub proxy_url: Option<String>,
    /// Whether the paid provider path is active.
    pub paid_proxy: bool,
    /// Explicit opencode binary; `None` resolves `OPENCODE_BIN` then `PATH`.
    pub program: Option<PathBuf>,
    /// Additional child environment (toolchain homes, PATH additions).
    pub extra_env: BTreeMap<String, String>,
}

impl LaunchConfig {
    /// Serialize to the compact JSON carried in [`LAUNCH_CONFIG_ENV`].
    pub fn to_json(&self) -> String {
        json!({
            "attempt_dir": self.attempt_dir.to_string_lossy(),
            "model": &self.model,
            "proxy_url": &self.proxy_url,
            "paid_proxy": self.paid_proxy,
            "program": self
                .program
                .as_ref()
                .map(|program| program.to_string_lossy().into_owned()),
            "extra_env": &self.extra_env,
        })
        .to_string()
    }

    /// Parse the JSON carried in [`LAUNCH_CONFIG_ENV`]; `None` when malformed.
    pub fn from_json(text: &str) -> Option<Self> {
        let value: Value = serde_json::from_str(text).ok()?;
        let string = |key: &str| value.get(key)?.as_str().map(str::to_string);
        Some(LaunchConfig {
            attempt_dir: PathBuf::from(string("attempt_dir")?),
            model: string("model")?,
            proxy_url: value
                .get("proxy_url")
                .and_then(Value::as_str)
                .map(str::to_string),
            paid_proxy: value
                .get("paid_proxy")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            program: value
                .get("program")
                .and_then(Value::as_str)
                .map(PathBuf::from),
            extra_env: value
                .get("extra_env")
                .and_then(Value::as_object)
                .map(|obj| {
                    obj.iter()
                        .filter_map(|(key, value)| {
                            value.as_str().map(|value| (key.clone(), value.to_string()))
                        })
                        .collect()
                })
                .unwrap_or_default(),
        })
    }

    /// Reconstruct a harness config, applying the shared paid-provider rule.
    ///
    /// `build_config` (not a raw field copy) is used so the dummy-key rule stays
    /// single-sourced; the conductor already ran the same rule, so this is an
    /// identity for a conductor-built config.
    pub fn to_config(&self) -> OpenCodeHarnessConfig {
        let mut config = build_config(OpenCodeConfigInit {
            attempt_dir: self.attempt_dir.clone(),
            model: self.model.clone(),
            proxy_url: self.proxy_url.clone(),
            paid_proxy: self.paid_proxy,
            extra_env: self.extra_env.clone(),
        });
        config.program = self.program.clone();
        config
    }
}

/// Launch spec for the OpenCode server.
pub fn server_command() -> CommandSpec {
    CommandSpec::new(SERVER_BIN)
}

/// Assemble the server's harness config from the process environment.
pub fn config_from_env(root: PathBuf) -> OpenCodeHarnessConfig {
    config_from_lookup(root, |key| std::env::var(key).ok())
}

/// Assemble the server's harness config, reading each variable through `lookup`.
///
/// Split from [`config_from_env`] so reconstruction is testable without mutating
/// the process environment. When the conductor transferred a [`LaunchConfig`] in
/// [`LAUNCH_CONFIG_ENV`], it is used verbatim (the same fields the in-process
/// transport would use); otherwise the legacy per-variable reconstruction derives
/// a root-scoped config (a run-local `CARGO_HOME`/`CARGO_TARGET_DIR` and a single
/// [`EXTRA_RW_ENV`] root).
pub fn config_from_lookup(
    root: PathBuf,
    lookup: impl Fn(&str) -> Option<String>,
) -> OpenCodeHarnessConfig {
    if let Some(launch) = lookup(LAUNCH_CONFIG_ENV).and_then(|json| LaunchConfig::from_json(&json))
    {
        return launch.to_config();
    }
    let proxy_url = lookup(PROXY_URL_ENV).filter(|url| !url.is_empty());
    let paid_proxy = matches!(lookup(PAID_ENV).as_deref(), Some("1") | Some("true"));
    let model = lookup("AGALMA_MODEL").unwrap_or_default();
    let mut extra_env = BTreeMap::new();
    extra_env.insert(
        "CARGO_HOME".to_string(),
        root.join("cargo-home").to_string_lossy().into_owned(),
    );
    extra_env.insert(
        "CARGO_TARGET_DIR".to_string(),
        root.join("agent-target").to_string_lossy().into_owned(),
    );
    build_config(OpenCodeConfigInit {
        attempt_dir: root.join("attempt"),
        model,
        proxy_url,
        paid_proxy,
        extra_env,
    })
}

/// Build the OpenCode server around an injected process launcher.
pub fn build_server(launcher: Box<dyn ProcessApi + Send>, config: OpenCodeHarnessConfig) -> Server {
    let harness = OpenCodeHarness::new(launcher, config);
    let describe = harness.describe();
    let mut server = Server::new(server_info_from(&describe));
    register_harness(&mut server, harness);
    server
}

/// Resolve the attempt root: [`ROOT_ENV`] when set, else `<socket parent>`.
pub fn root_for(socket: &Path) -> PathBuf {
    std::env::var_os(ROOT_ENV)
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            socket
                .parent()
                .unwrap_or_else(|| Path::new("."))
                .join("opencode-harness")
        })
}

/// Wire the OpenCode harness server speaks on its serve boundary.
///
/// The inbound serve loop [`agalma_ipc::serve_harness`] reads
/// [`agalma_ipc::WIRE_ENV`] with a `Connect` default — the IPCX convention the
/// peer component servers follow. `AGALMA_IPC_WIRE=framed` restores the B1
/// length-prefixed transport.
///
/// Split out (like [`config_from_lookup`]) so the selection is testable without
/// mutating the process environment.
pub fn wire_from_env() -> Wire {
    Wire::from_env(Wire::Connect)
}

/// Full argv/env entry point used by the server binary.
pub fn run_from_args<I>(args: I) -> Result<(), String>
where
    I: IntoIterator<Item = String>,
{
    let socket = agalma_ipc::socket_from_args(args)?;
    let root = root_for(&socket);
    std::fs::create_dir_all(&root)
        .map_err(|e| format!("create harness root {}: {e}", root.display()))?;
    let config = config_from_env(root);
    let server = build_server(Box::new(LocalLauncher::new()), config);
    let wire = wire_from_env();
    let display = socket.display().to_string();
    agalma_ipc::serve_harness(server, &socket, wire).map_err(|e| format!("serve {display}: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::DUMMY_PROVIDER_KEY;

    #[test]
    fn command_names_the_opencode_server_bin() {
        assert_eq!(server_command().program, SERVER_BIN);
    }

    /// The transferred launch config must survive JSON round-tripping, including
    /// paths containing spaces (toolchain/bd roots on real hosts often do).
    #[test]
    fn launch_config_json_round_trips_paths_with_spaces() {
        let launch = LaunchConfig {
            attempt_dir: PathBuf::from("/state/run dir/attempt"),
            model: "opencode-go/deepseek".to_string(),
            proxy_url: Some("http://127.0.0.1:41234".to_string()),
            paid_proxy: true,
            program: None,
            extra_env: BTreeMap::from([
                (
                    "PATH".to_string(),
                    "/Users/a/.local/bin:/tool chain/bin:/usr/bin:/bin".to_string(),
                ),
                ("CARGO_HOME".to_string(), "/state/cargo home".to_string()),
            ]),
        };
        let json = launch.to_json();
        let decoded = LaunchConfig::from_json(&json).expect("valid launch config json");
        assert_eq!(decoded, launch);
        // Malformed input is rejected, not silently defaulted.
        assert!(LaunchConfig::from_json("not json").is_none());
        assert!(LaunchConfig::from_json("{}").is_none());
    }

    /// A transferred launch config is reconstructed verbatim: custom bd PATH
    /// and `CARGO_HOME` survive, while unrelated parent environment keys are
    /// never consulted.
    #[test]
    fn forwarded_launch_config_is_used_verbatim_and_drops_unrelated_env() {
        let launch = LaunchConfig {
            attempt_dir: PathBuf::from("/state/run/attempt"),
            model: "opencode-go/deepseek".to_string(),
            proxy_url: Some("http://127.0.0.1:41234".to_string()),
            paid_proxy: true,
            program: None,
            extra_env: BTreeMap::from([
                (
                    "PATH".to_string(),
                    "/Users/a/.local/bin:/cargo home/bin:/usr/bin:/bin".to_string(),
                ),
                (
                    "CARGO_HOME".to_string(),
                    "/state/run/cargo-home".to_string(),
                ),
                ("RUSTUP_HOME".to_string(), "/Users/a/.rustup".to_string()),
            ]),
        };
        // The lookup carries a parent secret that must never reach the config.
        let lookup = |key: &str| -> Option<String> {
            match key {
                LAUNCH_CONFIG_ENV => Some(launch.to_json()),
                "AWS_SECRET_ACCESS_KEY" => Some("super-secret".to_string()),
                "OPENCODE_API_KEY" => Some("real-provider-token".to_string()),
                _ => None,
            }
        };
        let config = config_from_lookup(PathBuf::from("/state/run"), lookup);

        assert_eq!(config.attempt_dir, launch.attempt_dir);
        assert_eq!(config.extra_env, launch.extra_env);
        assert_eq!(
            config.extra_env.get("PATH").map(String::as_str),
            Some("/Users/a/.local/bin:/cargo home/bin:/usr/bin:/bin")
        );
        assert_eq!(
            config.extra_env.get("CARGO_HOME").map(String::as_str),
            Some("/state/run/cargo-home")
        );
        // The legacy root-scoped fallbacks must not leak into a transferred config.
        assert!(!config.extra_env.contains_key("CARGO_TARGET_DIR"));
        // Parent environment is never merged in.
        assert!(!config.extra_env.contains_key("AWS_SECRET_ACCESS_KEY"));
        assert!(!config.extra_env.contains_key("OPENCODE_API_KEY"));
        // The paid-provider dummy-key rule still applies on reconstruction.
        assert_eq!(
            config.provider_base_url.as_deref(),
            Some("http://127.0.0.1:41234")
        );
        assert_eq!(config.provider_api_key.as_deref(), Some(DUMMY_PROVIDER_KEY));
    }

    /// Without a transferred launch config the server keeps the legacy
    /// root-scoped reconstruction (direct invocation / older conductors).
    #[test]
    fn absent_launch_config_falls_back_to_root_scoped_reconstruction() {
        let lookup = |key: &str| -> Option<String> {
            match key {
                "AGALMA_MODEL" => Some("opencode/mimo".to_string()),
                _ => None,
            }
        };
        let config = config_from_lookup(PathBuf::from("/state/run"), lookup);
        assert_eq!(
            config.extra_env.get("CARGO_HOME").map(String::as_str),
            Some("/state/run/cargo-home")
        );
        assert_eq!(
            config.extra_env.get("CARGO_TARGET_DIR").map(String::as_str),
            Some("/state/run/agent-target")
        );
        assert_eq!(config.attempt_dir, PathBuf::from("/state/run/attempt"));
    }
}
