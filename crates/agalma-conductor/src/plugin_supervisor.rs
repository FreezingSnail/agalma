//! HP.3 common plugin supervisor (`agalma-jmr.3`).
//!
//! One supervisor owns every discovered plugin child and its bounded socket.
//! It reuses the trusted-component lifecycle lessons from
//! [`crate::harness_process`]: the child is spawned as a process-group leader,
//! its `READY {json}` handshake is read under a timeout, a compatibility gate
//! runs **before** readiness, and any startup failure tears down the whole
//! owned process group.
//!
//! ## Boundaries
//!
//! * The supervisor owns the process; [`crate::plugin_registry::PluginRegistry`]
//!   owns the recorded lifecycle. The supervisor records observations through
//!   the registry, so a crash changes the observation, never the pinned binding
//!   generation.
//! * Readiness is **health-gated**: an identity-compatible `READY` moves the
//!   entry to `Starting`; only a successful [`HealthProbe`] transitions it to
//!   `Ready`. The real RPC probe is an HP.4 seam; the injected fake proves the
//!   gate deterministically.
//! * Time is injected through [`Clock`] so backoff and the drain deadline are
//!   testable without sleeping; the launch epoch is allocated by the registry
//!   and fences stale observations.
//! * A restart reissues the **same** logical generation under a new launch
//!   epoch: pinned work is never redirected to a replacement.
//!
//! The supervisor never speaks the plugin RPC and never schedules factory work;
//! it only spawns, gates, records, backs off and cleans up.

use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::process::Child;
use std::rc::Rc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use agalma_contracts::harness::StopEvidence;
use agalma_contracts::{ContractError, FailureCode, PackageArtifactRef, PluginManifest};
use agalma_ipc::{CommandSpec, ServerInfo};

use crate::harness_process::{spawn_supervised, stop_supervised};
use crate::plugin_manifest::package_inventory_digest;
use crate::plugin_registry::{LifecycleState, PluginRegistry};

/// Injected wall clock. Production uses [`SystemClock`]; tests advance a
/// [`ManualClock`] so backoff, restart dues and drain deadlines are
/// deterministic.
pub trait Clock {
    /// Milliseconds since the Unix epoch (or an arbitrary test origin).
    fn now_ms(&self) -> u64;
}

/// The production clock.
#[derive(Debug, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_ms(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_millis() as u64)
            .unwrap_or(0)
    }
}

/// A manually advanced clock for deterministic tests.
#[derive(Debug)]
pub struct ManualClock {
    now: Cell<u64>,
}

impl ManualClock {
    /// A clock reading `start` until advanced.
    pub fn new(start: u64) -> Self {
        ManualClock {
            now: Cell::new(start),
        }
    }

    /// Advance the clock by `by` milliseconds.
    pub fn advance(&self, by: u64) {
        self.now.set(self.now.get() + by);
    }

    /// Set the clock to an absolute value.
    pub fn set(&self, to: u64) {
        self.now.set(to);
    }
}

impl Clock for ManualClock {
    fn now_ms(&self) -> u64 {
        self.now.get()
    }
}

/// Exponential restart backoff, evaluated with the injected clock.
///
/// `delay(attempt) = min(base_ms * factor^attempt, max_ms)`, so attempt `0`
/// (first restart after one crash) waits `base_ms`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BackoffPolicy {
    pub base_ms: u64,
    pub factor: u64,
    pub max_ms: u64,
}

impl Default for BackoffPolicy {
    fn default() -> Self {
        BackoffPolicy {
            base_ms: 250,
            factor: 2,
            max_ms: 30_000,
        }
    }
}

impl BackoffPolicy {
    /// The delay before the restart attempt numbered `attempt` (0-based).
    pub fn delay_ms(&self, attempt: u32) -> u64 {
        let mut delay = self.base_ms;
        for _ in 0..attempt.min(32) {
            delay = delay.saturating_mul(self.factor);
            if delay >= self.max_ms {
                return self.max_ms;
            }
        }
        delay.min(self.max_ms)
    }
}

/// The health gate that must pass before a spawned plugin becomes `Ready`.
///
/// HP.4 attaches the real capability-scoped RPC probe; the fake in tests
/// returns a scripted result. Implementations must not mutate the registry.
pub trait HealthProbe {
    /// `Ok(true)` when the child behind `epoch` is healthy.
    fn probe(&mut self, plugin_id: &str, epoch: u64) -> Result<bool, ContractError>;
}

/// Everything needed to launch one installed plugin generation.
#[derive(Clone, Debug)]
pub struct LaunchRequest {
    pub plugin_id: String,
    pub generation: u64,
    /// Absolute executable inside the hub-owned installed generation.
    pub program: PathBuf,
    /// Declared argument vector (never a shell string).
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    /// SUN_LEN-safe socket path the child must serve.
    pub socket: PathBuf,
    /// `READY` identity the child must announce.
    pub expected_api: String,
    pub protocol_major: u64,
    /// Frozen pinned configuration ref carried from the installed generation.
    pub configuration: PackageArtifactRef,
    pub config_digest: String,
    pub launch_profile: String,
    /// Capabilities the installation grants (the launch scope).
    pub approved_capabilities: Vec<String>,
    /// Fields for the launch rehash: hub-owned installed dir, pinned package
    /// digest and validated manifest. Launch rehashes these before spawning.
    pub installed_dir: PathBuf,
    pub package_digest: String,
    pub manifest: PluginManifest,
    pub required_capabilities: BTreeSet<String>,
    /// Services a plugin must find `Ready` before it may start.
    pub required_services: Vec<String>,
}

impl LaunchRequest {
    /// Build a request from an installed generation and its socket.
    pub fn from_installed(
        installed: &crate::plugin_registry::InstalledGeneration,
        socket: PathBuf,
    ) -> Self {
        let mut args = installed.argv.clone();
        args.push("--socket".to_string());
        args.push(socket.to_string_lossy().into_owned());
        LaunchRequest {
            plugin_id: installed.plugin_id.clone(),
            generation: installed.generation,
            program: installed.entrypoint.clone(),
            args,
            env: BTreeMap::new(),
            socket,
            expected_api: "plugin".to_string(),
            protocol_major: installed.protocol_major,
            configuration: installed.configuration.clone(),
            config_digest: installed.config_digest.clone(),
            launch_profile: installed.launch_profile.clone(),
            approved_capabilities: installed.approved_capabilities.clone(),
            installed_dir: installed.installed_dir.clone(),
            package_digest: installed.package_digest.clone(),
            manifest: installed.manifest.clone(),
            required_capabilities: installed.approved_capabilities.iter().cloned().collect(),
            required_services: installed.required_services.clone(),
        }
    }
}

/// A spawned, identity-gated plugin process.
#[derive(Debug)]
pub struct RunningChild {
    pub pid: u32,
    pub epoch: u64,
    pub generation: u64,
    pub socket: PathBuf,
    child: Child,
    stopped: bool,
}

impl RunningChild {
    /// Whether the owned child has exited (reaping it if so).
    fn exited(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(Some(_)))
    }

    /// Stop the owned process group and report cleanup evidence. Idempotent.
    fn stop(&mut self) -> StopEvidence {
        if self.stopped {
            return StopEvidence {
                process_group_gone: true,
                detail: "plugin child already stopped".to_string(),
            };
        }
        let evidence = stop_supervised(&mut self.child);
        self.stopped = true;
        evidence
    }
}

impl Drop for RunningChild {
    fn drop(&mut self) {
        // Safety net: a supervisor that is dropped without an explicit shutdown
        // still tears down its owned process group.
        let _ = self.stop();
    }
}

/// Outcome of a successful spawn (identity gate passed, `Starting`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Spawned {
    pub plugin_id: String,
    pub generation: u64,
    pub epoch: u64,
    pub pid: u32,
}

/// The scheduled restart of one crashed plugin.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RestartPlan {
    /// 1-based crash count for this plugin.
    pub attempt: u32,
    pub delay_ms: u64,
    /// Absolute clock reading at which the restart becomes due.
    pub due_ms: u64,
    /// Cleanup evidence for the crashed group.
    pub stop: StopEvidence,
}

/// A single common supervisor for every discovered plugin child.
pub struct PluginSupervisor {
    clock: Rc<dyn Clock>,
    backoff: BackoffPolicy,
    ready_timeout: Duration,
    running: BTreeMap<String, RunningChild>,
    /// Crash count since the last Ready, per plugin.
    attempts: BTreeMap<String, u32>,
    /// Earliest clock reading at which a crashed plugin may restart.
    restart_due: BTreeMap<String, u64>,
}

impl PluginSupervisor {
    /// Construct a supervisor with injected time and backoff.
    pub fn new(clock: Rc<dyn Clock>, backoff: BackoffPolicy, ready_timeout: Duration) -> Self {
        PluginSupervisor {
            clock,
            backoff,
            ready_timeout,
            running: BTreeMap::new(),
            attempts: BTreeMap::new(),
            restart_due: BTreeMap::new(),
        }
    }

    /// The injected clock's current reading.
    pub fn now_ms(&self) -> u64 {
        self.clock.now_ms()
    }

    /// Whether a child is currently owned for `plugin_id`.
    pub fn is_running(&self, plugin_id: &str) -> bool {
        self.running.contains_key(plugin_id)
    }

    /// The owned child's pid, if any.
    pub fn pid(&self, plugin_id: &str) -> Option<u32> {
        self.running.get(plugin_id).map(|child| child.pid)
    }

    /// The owned child's launch epoch, if any.
    pub fn epoch(&self, plugin_id: &str) -> Option<u64> {
        self.running.get(plugin_id).map(|child| child.epoch)
    }

    /// Spawn and identity-gate one plugin child.
    ///
    /// Dependency readiness is checked first: if any required service is not
    /// `Ready` in `registry`, the start is refused before a process exists.
    /// On any later failure the whole owned process group is torn down before
    /// the error returns.
    pub fn spawn(
        &mut self,
        registry: &mut PluginRegistry,
        request: &LaunchRequest,
    ) -> Result<Spawned, ContractError> {
        let id = request.plugin_id.as_str();
        if self.running.contains_key(id) {
            return Err(FailureCode::IdentityConflict.failure(
                id,
                "a child is already owned for this plugin",
            ));
        }
        self.check_dependencies(registry, request)?;

        // Frozen contract: rehash on install, stage and launch. A tampered
        // installed generation must never be launched.
        if let Err(err) = rehash_request(request) {
            let now = self.clock.now_ms();
            registry.mark_failed(id, format!("launch rehash refused: {err:?}"), now)?;
            return Err(err);
        }

        let now = self.clock.now_ms();
        let epoch = registry.mark_starting(id, request.generation, now)?;

        let mut spec = CommandSpec::new(request.program.to_string_lossy().into_owned())
            .args(request.args.clone());
        for (key, value) in &request.env {
            spec.env.insert(key.clone(), value.clone());
        }
        let (guard, info) = match spawn_supervised(&spec, &request.program, &request.socket, self.ready_timeout)
        {
            Ok(pair) => pair,
            Err(error) => {
                registry.mark_failed(id, format!("startup failed: {error}"), now)?;
                self.schedule_restart(id);
                return Err(FailureCode::StateUnavailable.failure(id, error));
            }
        };

        if let Err(reason) = identity_gate(&info, request) {
            // The guard still owns the group: dropping it tears the tree down.
            drop(guard);
            registry.mark_failed(id, format!("READY identity refused: {reason}"), now)?;
            self.schedule_restart(id);
            return Err(FailureCode::IdentityConflict.failure(id, reason));
        }

        let child = guard.into_child();
        let pid = child.id();
        self.running.insert(
            id.to_string(),
            RunningChild {
                pid,
                epoch,
                generation: request.generation,
                socket: request.socket.clone(),
                child,
                stopped: false,
            },
        );
        Ok(Spawned {
            plugin_id: id.to_string(),
            generation: request.generation,
            epoch,
            pid,
        })
    }

    /// Run the health gate and, when healthy, mark the plugin `Ready`.
    ///
    /// Returns `true` once the plugin is Ready. An unhealthy or failed probe
    /// leaves the entry `Starting` (readiness is never inferred from a socket or
    /// from `READY` alone).
    pub fn confirm_ready(
        &mut self,
        registry: &mut PluginRegistry,
        plugin_id: &str,
        probe: &mut dyn HealthProbe,
    ) -> Result<bool, ContractError> {
        let (epoch, generation) = {
            let child = self.running.get(plugin_id).ok_or_else(|| {
                FailureCode::InvalidRecord.failure(plugin_id, "no child is owned for this plugin")
            })?;
            (child.epoch, child.generation)
        };
        if !probe.probe(plugin_id, epoch)? {
            return Ok(false);
        }
        let now = self.clock.now_ms();
        registry.mark_ready(plugin_id, generation, now)?;
        self.attempts.remove(plugin_id);
        self.restart_due.remove(plugin_id);
        Ok(true)
    }

    /// Record an explicit health observation for `epoch` through the registry.
    ///
    /// A superseded epoch is refused by the registry's launch-epoch fence; the
    /// return value is `true` only when the observation applied.
    pub fn observe_health(
        &mut self,
        registry: &mut PluginRegistry,
        plugin_id: &str,
        epoch: u64,
        ready: bool,
        detail: impl Into<String>,
    ) -> Result<bool, ContractError> {
        let now = self.clock.now_ms();
        registry.record_health(plugin_id, epoch, ready, detail, now)
    }

    /// Handle the unexpected exit of `plugin_id`'s child.
    ///
    /// Tears down whatever remains of the owned group, records the failure,
    /// computes the injected backoff and returns the restart plan. The logical
    /// generation is unchanged: the replacement will reuse the pinned
    /// generation under a fresh launch epoch.
    pub fn handle_crash(
        &mut self,
        registry: &mut PluginRegistry,
        plugin_id: &str,
    ) -> Result<RestartPlan, ContractError> {
        let mut child = self.running.remove(plugin_id).ok_or_else(|| {
            FailureCode::InvalidRecord.failure(plugin_id, "no child is owned for this plugin")
        })?;
        let stop = child.stop();
        let attempt = {
            let entry = self.attempts.entry(plugin_id.to_string()).or_insert(0);
            *entry += 1;
            *entry
        };
        let delay_ms = self.backoff.delay_ms(attempt - 1);
        let now = self.clock.now_ms();
        let due_ms = now.saturating_add(delay_ms);
        registry.mark_failed(
            plugin_id,
            format!("child exited (crash #{attempt}); restart due in {delay_ms}ms"),
            now,
        )?;
        self.restart_due.insert(plugin_id.to_string(), due_ms);
        Ok(RestartPlan {
            attempt,
            delay_ms,
            due_ms,
            stop,
        })
    }

    /// Plugins whose scheduled restart is due at `now_ms`, in byte order.
    pub fn due_restarts(&self, now_ms: u64) -> Vec<String> {
        self.restart_due
            .iter()
            .filter(|(_, due)| **due <= now_ms)
            .map(|(id, _)| id.clone())
            .collect()
    }

    /// Take the due restarts at `now_ms` (clearing their schedule).
    pub fn take_due_restarts(&mut self, now_ms: u64) -> Vec<String> {
        let due = self.due_restarts(now_ms);
        for id in &due {
            self.restart_due.remove(id);
        }
        due
    }

    /// Detect owned children that have exited and schedule their restarts.
    ///
    /// The production loop calls this each tick so an unexpected child exit is
    /// observed (not just a previously scheduled restart drained). Each exited
    /// child runs [`Self::handle_crash`], so its group is torn down, the failure
    /// is recorded and backoff is scheduled against the injected clock.
    pub fn reap_exited(
        &mut self,
        registry: &mut PluginRegistry,
    ) -> Result<Vec<RestartPlan>, ContractError> {
        let mut exited = Vec::new();
        for (id, child) in self.running.iter_mut() {
            if child.exited() {
                exited.push(id.clone());
            }
        }
        let mut plans = Vec::with_capacity(exited.len());
        for id in exited {
            plans.push(self.handle_crash(registry, &id)?);
        }
        Ok(plans)
    }

    /// Stop one owned child and return its cleanup evidence.
    pub fn stop(&mut self, plugin_id: &str) -> Option<StopEvidence> {
        self.restart_due.remove(plugin_id);
        let mut child = self.running.remove(plugin_id)?;
        Some(child.stop())
    }

    /// Stop every owned child, returning cleanup evidence per plugin.
    pub fn stop_all(&mut self) -> Vec<(String, StopEvidence)> {
        let ids: Vec<String> = self.running.keys().cloned().collect();
        let mut evidence = Vec::with_capacity(ids.len());
        for id in ids {
            if let Some(stop) = self.stop(&id) {
                evidence.push((id, stop));
            }
        }
        self.restart_due.clear();
        evidence
    }

    /// Refuse a start whose required services are not `Ready`.
    fn check_dependencies(
        &self,
        registry: &PluginRegistry,
        request: &LaunchRequest,
    ) -> Result<(), ContractError> {
        for service in &request.required_services {
            let ready = registry
                .entry(service)
                .is_some_and(|entry| entry.observed == LifecycleState::Ready);
            if !ready {
                return Err(FailureCode::MissingCapability.failure(
                    &request.plugin_id,
                    format!("required service `{service}` is not ready"),
                ));
            }
        }
        Ok(())
    }

    /// Record a crash attempt and schedule the backoff restart.
    fn schedule_restart(&mut self, plugin_id: &str) {
        let attempt = {
            let entry = self.attempts.entry(plugin_id.to_string()).or_insert(0);
            *entry += 1;
            *entry
        };
        let delay = self.backoff.delay_ms(attempt - 1);
        let due = self.clock.now_ms().saturating_add(delay);
        self.restart_due.insert(plugin_id.to_string(), due);
    }
}

impl Drop for PluginSupervisor {
    fn drop(&mut self) {
        // Own every child for its whole lifetime: dropping the supervisor stops
        // the whole owned group rather than leaking processes.
        let _ = self.stop_all();
    }
}

/// Rehash the hub-owned installed copy referenced by `request` (launch gate).
fn rehash_request(request: &LaunchRequest) -> Result<(), ContractError> {
    let recomputed = package_inventory_digest(&request.installed_dir, &request.manifest)?;
    if recomputed != request.package_digest {
        return Err(FailureCode::IntegrityMismatch.failure(
            &request.plugin_id,
            format!(
                "launch rehash {recomputed} != pinned package {}",
                request.package_digest
            ),
        ));
    }
    Ok(())
}

/// The `READY` compatibility gate: the announced identity must match the
/// installed generation's plugin id, API and protocol major, and offer every
/// approved capability.
fn identity_gate(info: &ServerInfo, request: &LaunchRequest) -> Result<(), String> {
    if info.api != request.expected_api {
        return Err(format!(
            "announced api `{}` != expected `{}`",
            info.api, request.expected_api
        ));
    }
    if info.impl_name != request.plugin_id {
        return Err(format!(
            "announced impl `{}` != installed plugin `{}`",
            info.impl_name, request.plugin_id
        ));
    }
    if info.api_version as u64 != request.protocol_major {
        return Err(format!(
            "announced protocol major {} != installed {}",
            info.api_version, request.protocol_major
        ));
    }
    let missing: Vec<&str> = request
        .required_capabilities
        .iter()
        .filter(|capability| !info.capabilities.contains(*capability))
        .map(String::as_str)
        .collect();
    if !missing.is_empty() {
        return Err(format!("missing approved capabilities: {missing:?}"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_is_bounded_and_exponential() {
        let policy = BackoffPolicy {
            base_ms: 100,
            factor: 2,
            max_ms: 1000,
        };
        assert_eq!(policy.delay_ms(0), 100);
        assert_eq!(policy.delay_ms(1), 200);
        assert_eq!(policy.delay_ms(2), 400);
        assert_eq!(policy.delay_ms(3), 800);
        assert_eq!(policy.delay_ms(4), 1000);
        assert_eq!(policy.delay_ms(40), 1000);
    }

    #[test]
    fn manual_clock_advances() {
        let clock = ManualClock::new(10);
        assert_eq!(clock.now_ms(), 10);
        clock.advance(5);
        assert_eq!(clock.now_ms(), 15);
        clock.set(0);
        assert_eq!(clock.now_ms(), 0);
    }
}
