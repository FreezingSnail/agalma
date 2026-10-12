//! Reference `HarnessApi` component server (B1.2).
//!
//! Hosts a [`ReferenceHarness`] behind the framed transport so the conductor can
//! bind the reference implementation across a process boundary. The deterministic
//! profile is selected by `AGALMA_HARNESS_PROFILE` (`success` | `red` | `fail`,
//! default `success`); the scenario directory may be pinned by
//! `AGALMA_HARNESS_SCENARIO_DIR` (default: a `reference-<profile>` directory
//! beside the socket).

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};

use agalma_ipc::{register_harness, server_info_from, CommandSpec, Server, Wire};

use crate::adapter::{ReferenceHarness, ReferenceHarnessConfig};
use crate::{describe_for, ReferenceProfile};

/// Binary name of the reference component server.
pub const SERVER_BIN: &str = "agalma-harness-reference-server";
/// Environment variable selecting the reference profile.
pub const PROFILE_ENV: &str = "AGALMA_HARNESS_PROFILE";
/// Environment variable pinning the scenario directory.
pub const SCENARIO_DIR_ENV: &str = "AGALMA_HARNESS_SCENARIO_DIR";

/// Stable env value for a profile.
pub fn profile_env_value(profile: ReferenceProfile) -> &'static str {
    match profile {
        ReferenceProfile::Success => "success",
        ReferenceProfile::Red => "red",
    }
}

/// Resolve a profile from [`PROFILE_ENV`]; anything unrecognized is `Success`.
pub fn profile_from_env() -> ReferenceProfile {
    match std::env::var(PROFILE_ENV).ok().as_deref() {
        Some("red") | Some("fail") | Some("reference-fail") => ReferenceProfile::Red,
        _ => ReferenceProfile::Success,
    }
}

/// Launch spec for the reference server bound to `profile`.
pub fn server_command(profile: ReferenceProfile) -> CommandSpec {
    CommandSpec::new(SERVER_BIN).env(PROFILE_ENV, profile_env_value(profile))
}

/// Build the reference server: the announced identity plus every harness method.
pub fn build_server(profile: ReferenceProfile, scenario_dir: PathBuf) -> Server {
    let harness = ReferenceHarness::new(ReferenceHarnessConfig {
        profile,
        scenario_dir,
        scenario: None,
    });
    let describe = describe_for(profile);
    let mut server = Server::new(server_info_from(&describe));
    register_harness(&mut server, harness);
    server
}

/// Wire the reference harness server serves on.
///
/// IPCX convention: [`agalma_ipc::WIRE_ENV`] selects the wire with a `Connect`
/// default; `AGALMA_IPC_WIRE=framed` restores the B1 length-prefixed transport.
/// Aligned with the sandbox/peer component servers (`agalma-3ej`).
pub fn wire_from_env() -> Wire {
    Wire::from_env(Wire::Connect)
}

/// Bind `socket` and serve `profile` over the wire selected by
/// [`agalma_ipc::WIRE_ENV`] (`connect` default, `framed` optional) until stopped.
pub fn serve(socket: &Path, profile: ReferenceProfile, scenario_dir: PathBuf) -> io::Result<()> {
    serve_wire(socket, profile, scenario_dir, wire_from_env())
}

/// Bind `socket` and serve `profile` over the selected wire transport (IPCX.2).
pub fn serve_wire(
    socket: &Path,
    profile: ReferenceProfile,
    scenario_dir: PathBuf,
    wire: Wire,
) -> io::Result<()> {
    let server = build_server(profile, scenario_dir);
    agalma_ipc::serve_harness(server, socket, wire)
}

/// Scenario directory for a server: [`SCENARIO_DIR_ENV`] when set, else
/// `<socket parent>/reference-<profile>`.
pub fn scenario_dir_for(socket: &Path, profile: ReferenceProfile) -> PathBuf {
    if let Some(dir) = std::env::var_os(SCENARIO_DIR_ENV) {
        return PathBuf::from(dir);
    }
    let parent = socket.parent().unwrap_or_else(|| Path::new("."));
    parent.join(format!("reference-{}", profile_env_value(profile)))
}

/// Full argv/env entry point used by the server binary.
pub fn run_from_args<I>(args: I) -> Result<(), String>
where
    I: IntoIterator<Item = String>,
{
    let socket = agalma_ipc::socket_from_args(args)?;
    let profile = profile_from_env();
    let scenario_dir = scenario_dir_for(&socket, profile);
    if let Some(parent) = socket.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("create socket dir {}: {e}", parent.display()))?;
    }
    let wire = wire_from_env();
    serve_wire(&socket, profile, scenario_dir, wire)
        .map_err(|e| format!("serve {}: {e}", socket.display()))
}

/// The environment a reference server announces for `profile` (tests/docs).
pub fn server_env(profile: ReferenceProfile) -> BTreeMap<String, String> {
    server_command(profile).env
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_names_the_reference_server_bin() {
        let spec = server_command(ReferenceProfile::Red);
        assert_eq!(spec.program, SERVER_BIN);
        assert_eq!(spec.env.get(PROFILE_ENV).map(String::as_str), Some("red"));
    }

    #[test]
    fn server_announces_the_reference_identity_for_each_method() {
        let server = build_server(ReferenceProfile::Success, PathBuf::from("scenario-test"));
        assert_eq!(server.info().impl_name, "reference");
        agalma_ipc::ensure_all_handlers(&server).expect("all handlers registered");
    }
}
