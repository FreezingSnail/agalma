//! HP.3 common plugin registry (`agalma-jmr.3`).
//!
//! The registry is the hub's authoritative record of independently installed
//! plugin generations. It owns:
//!
//! * **Hub-owned immutable installed generations** ([`HubInstallStore`]): a
//!   validated discovery package is copied into hub-owned storage and rehashed
//!   on install, so editing or deleting the discovery directory cannot change a
//!   pinned process.
//! * **Monotonic binding generations** ([`PluginRegistry`]): package,
//!   configuration or capability-scope changes allocate a new generation;
//!   generations are never reused after rollback.
//! * **Desired state** persisted through an injected [`DesiredStateStore`]
//!   (HP.5 attaches the selected-ledger durable implementation; the in-memory
//!   store here is a test double). Desired state must survive a hub restart.
//! * **Observed lifecycle** ([`LifecycleState`]) kept separate from the desired
//!   state and from the pinned binding generation.
//! * **Drain/removal refusal**: a generation with unresolved pinned operations
//!   is not removable; the active generation must be retired (replacement
//!   activated) first.
//! * **Launch-epoch fencing**: a restart reissues the same logical generation
//!   under a new launch epoch; observations tagged with a superseded epoch are
//!   ignored rather than applied.
//! * **SUN_LEN-safe socket paths** ([`plugin_socket_path`]) for long state dirs.
//!
//! This module performs only filesystem work for immutable installation; it
//! never launches a child or speaks the plugin RPC (see [`crate::plugin_supervisor`]).

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use agalma_contracts::{ContractError, FailureCode, LedgerApi, PackageArtifactRef, PluginInstallation, PluginManifest};

use crate::plugin_manifest::{
    package_inventory_digest, verify_artifact_ref, DiscoveredPackage, PLUGIN_MANIFEST_FILE,
};

/// Observed lifecycle state of one installed plugin generation.
///
/// Separate from the pinned binding generation and from the desired state: a
/// process crash changes the observation, never the pin.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LifecycleState {
    /// Installed and pinned; no process owned.
    Installed,
    /// A process has been spawned; readiness not yet established.
    Starting,
    /// Handshake identity, Describe compatibility, grants and Health succeeded.
    Ready,
    /// Draining: accepts reconcile/cancel/read for its pinned operations.
    Draining,
    /// Startup or health failure; a restart may be attempted under backoff.
    Failed,
    /// Retired: no longer accepts new work.
    Retired,
}

/// Desired state that must survive a hub restart.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DesiredState {
    /// The hub should keep this plugin running.
    Running,
    /// The hub should not start this plugin.
    Stopped,
}

/// How a registry entry came to exist.
///
/// Built-in conductor components are represented in the registry so the control
/// plane can see them, but registration does not claim their implementation has
/// migrated to the plugin protocol — that remains CV migration work.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ComponentOrigin {
    /// Discovered from a configured plugin root and installed as a generation.
    Discovered,
    /// A built-in conductor component represented without an install.
    BuiltIn,
}

/// One hub-owned immutable installed generation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InstalledGeneration {
    pub plugin_id: String,
    /// Monotonic per-plugin logical binding generation (never reused).
    pub generation: u64,
    pub package_version: String,
    /// Sealed-inventory digest of the installed copy (verified on install).
    pub package_digest: String,
    pub protocol_major: u64,
    /// Digest of the pinned `PluginInstallation/v1` configuration object.
    pub config_digest: String,
    /// Frozen pinned configuration ref from the installation (validated against
    /// the installed package bytes, never the operator's mutable current value).
    pub configuration: PackageArtifactRef,
    /// Digest of the `PluginInstallation/v1` record, when one was supplied.
    pub installation_digest: String,
    pub launch_profile: String,
    /// Capabilities the package requested (`PluginManifest.requested_capabilities`).
    pub requested_capabilities: Vec<String>,
    /// Capabilities the installation actually grants (approved scope).
    pub approved_capabilities: Vec<String>,
    pub required_services: Vec<String>,
    /// Hub-owned immutable package directory.
    pub installed_dir: PathBuf,
    /// Executable inside [`Self::installed_dir`].
    pub entrypoint: PathBuf,
    /// Declared argument vector (never a shell string).
    pub argv: Vec<String>,
    /// The validated manifest of the installed copy (rehashed on stage/launch).
    pub manifest: PluginManifest,
}

impl InstalledGeneration {
    /// Rehash the hub-owned installed copy and compare it to the pinned digest.
    ///
    /// The frozen contract requires rehash on install, stage **and** launch; any
    /// byte change in the installed package (mode, size, content) fails with
    /// [`FailureCode::IntegrityMismatch`].
    pub fn rehash(&self) -> Result<String, ContractError> {
        let recomputed = package_inventory_digest(&self.installed_dir, &self.manifest)?;
        if recomputed != self.package_digest {
            return Err(FailureCode::IntegrityMismatch.failure(
                &self.plugin_id,
                format!(
                    "installed generation {} rehashes to {recomputed}, pinned {}",
                    self.generation, self.package_digest
                ),
            ));
        }
        Ok(recomputed)
    }
}

/// Persistence seam for desired plugin state.
///
/// HP.5 attaches the selected-ledger durable implementation. The hub calls
/// [`DesiredStateStore::save`] on every desired-state change and
/// [`DesiredStateStore::load`] on startup, so a restart restores which plugins
/// were meant to run.
pub trait DesiredStateStore {
    /// Load every recorded desired state.
    fn load(&mut self) -> Result<BTreeMap<String, DesiredState>, ContractError>;
    /// Record the desired state for one plugin.
    fn save(&mut self, plugin_id: &str, desired: DesiredState) -> Result<(), ContractError>;
    /// The durable registry checkpoint when this store is backed by the
    /// selected ledger (HP.5); `None` for in-memory doubles.
    ///
    /// The host reads it once at assembly to restore installed generations and
    /// their monotonic ceiling **before** discovery can allocate any generation,
    /// so a restart never reuses or redirects a binding identity.
    fn checkpoint(&self) -> Option<&RegistryCheckpoint> {
        None
    }
}

/// The durable registry checkpoint held in the selected ledger's
/// [`crate::plugin_reports::REGISTRY_NAMESPACE`] component namespace.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RegistryCheckpoint {
    /// Desired state per plugin id (survives a hub restart).
    pub desired: BTreeMap<String, DesiredState>,
    /// Active generation per plugin id (what the registry last ran).
    pub active_generations: BTreeMap<String, u64>,
    /// Highest generation ever allocated per plugin id.
    pub max_generations: BTreeMap<String, u64>,
}

/// Durable [`DesiredStateStore`] backed by the selected ledger component store.
///
/// Every save is one atomic `ComponentCommit/v1` carrying the updated registry
/// checkpoint; a restart reconstructs the desired map from that checkpoint. No
/// private file database or direct SQL is involved.
pub struct LedgerDesiredStateStore {
    ledger: crate::ledger_process::LedgerHandle,
    revision: u64,
    state: RegistryCheckpoint,
}

impl LedgerDesiredStateStore {
    /// Open the durable store, loading the existing registry checkpoint.
    pub fn open(
        ledger: crate::ledger_process::LedgerHandle,
    ) -> Result<LedgerDesiredStateStore, ContractError> {
        let (revision, state) = read_registry_checkpoint(&ledger)?;
        Ok(LedgerDesiredStateStore {
            ledger,
            revision,
            state,
        })
    }

    /// The current durable checkpoint.
    pub fn checkpoint(&self) -> &RegistryCheckpoint {
        &self.state
    }

    /// The selected ledger backing this store.
    pub fn ledger(&self) -> &crate::ledger_process::LedgerHandle {
        &self.ledger
    }

    /// Persist an arbitrary registry checkpoint update atomically.
    ///
    /// The durable ledger is the source of truth: the current checkpoint is
    /// re-read before mutating so a host-assembly generation write and a later
    /// desired-state save cannot fight over a stale cached revision.
    pub fn update(
        &mut self,
        mutate: impl FnOnce(&mut RegistryCheckpoint),
    ) -> Result<(), ContractError> {
        let (revision, mut current) = read_registry_checkpoint(&self.ledger)?;
        mutate(&mut current);
        let checkpoint = serde_json::to_value(&current).map_err(|e| {
            agalma_contracts::ContractError::KnownFailure(format!(
                "registry checkpoint json: {e}"
            ))
        })?;
        let commit_id = format!("registry-checkpoint:{}", revision + 1);
        let mut commit = agalma_contracts::ComponentCommit {
            format: agalma_contracts::COMPONENT_COMMIT_FORMAT.to_string(),
            namespace: crate::plugin_reports::REGISTRY_NAMESPACE.to_string(),
            commit_id,
            input_digest: String::new(),
            expected_revision: revision,
            checkpoint: Some(checkpoint),
            events: Vec::new(),
            operations: Vec::new(),
            receipts: Vec::new(),
        };
        commit.input_digest = commit.computed_input_digest()?;
        let anchor = self.ledger.component_commit(&commit)?;
        self.revision = anchor.revision;
        self.state = current;
        Ok(())
    }
}

impl DesiredStateStore for LedgerDesiredStateStore {
    fn load(&mut self) -> Result<BTreeMap<String, DesiredState>, ContractError> {
        Ok(self.state.desired.clone())
    }

    fn checkpoint(&self) -> Option<&RegistryCheckpoint> {
        Some(&self.state)
    }

    fn save(&mut self, plugin_id: &str, desired: DesiredState) -> Result<(), ContractError> {
        if self.state.desired.get(plugin_id) == Some(&desired) {
            return Ok(());
        }
        let plugin = plugin_id.to_string();
        self.update(move |checkpoint| {
            checkpoint.desired.insert(plugin, desired);
        })
    }
}

/// Load the durable registry checkpoint from `ledger` (revision 0 / empty when
/// none has been written yet).
pub fn read_registry_checkpoint(
    ledger: &crate::ledger_process::LedgerHandle,
) -> Result<(u64, RegistryCheckpoint), ContractError> {
    let row = ledger.component_read(crate::plugin_reports::REGISTRY_NAMESPACE)?;
    let revision = row.as_ref().map(|r| r.revision).unwrap_or(0);
    let state = match row.and_then(|r| r.checkpoint) {
        Some(checkpoint) => serde_json::from_value(checkpoint).map_err(|e| {
            agalma_contracts::ContractError::KnownFailure(format!(
                "registry checkpoint is invalid: {e}"
            ))
        })?,
        None => RegistryCheckpoint::default(),
    };
    Ok((revision, state))
}

/// Persist the registry's active/max generations into the durable checkpoint.
///
/// Called after host assembly so a later report-ingestor open and the control
/// plane can see which generation each plugin is running.
pub fn persist_registry_generations(
    ledger: &mut crate::ledger_process::LedgerHandle,
    registry: &PluginRegistry,
) -> Result<(), ContractError> {
    let (revision, mut state) = read_registry_checkpoint(ledger)?;
    let mut changed = false;
    for plugin_id in registry.plugin_ids() {
        let Some(entry) = registry.entry(plugin_id) else {
            continue;
        };
        let active = entry
            .active_generation
            .or(entry.pending_generation)
            .unwrap_or(0);
        if active != 0 && state.active_generations.get(plugin_id) != Some(&active) {
            state.active_generations.insert(plugin_id.to_string(), active);
            changed = true;
        }
        if state.max_generations.get(plugin_id).copied().unwrap_or(0) < entry.max_generation {
            state
                .max_generations
                .insert(plugin_id.to_string(), entry.max_generation);
            changed = true;
        }
    }
    if !changed {
        return Ok(());
    }
    let checkpoint = serde_json::to_value(&state).map_err(|e| {
        agalma_contracts::ContractError::KnownFailure(format!("registry checkpoint json: {e}"))
    })?;
    let commit_id = format!("registry-checkpoint:{}", revision + 1);
    let mut commit = agalma_contracts::ComponentCommit {
        format: agalma_contracts::COMPONENT_COMMIT_FORMAT.to_string(),
        namespace: crate::plugin_reports::REGISTRY_NAMESPACE.to_string(),
        commit_id,
        input_digest: String::new(),
        expected_revision: revision,
        checkpoint: Some(checkpoint),
        events: Vec::new(),
        operations: Vec::new(),
        receipts: Vec::new(),
    };
    commit.input_digest = commit.computed_input_digest()?;
    ledger.component_commit(&commit)?;
    Ok(())
}

/// In-memory desired-state store (test double; HP.5 supplies the durable one).
#[derive(Clone, Debug, Default)]
pub struct InMemoryDesiredStateStore {
    map: BTreeMap<String, DesiredState>,
}

impl InMemoryDesiredStateStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Seed a desired state without going through [`DesiredStateStore::save`].
    pub fn seed(&mut self, plugin_id: impl Into<String>, desired: DesiredState) {
        self.map.insert(plugin_id.into(), desired);
    }
}

impl DesiredStateStore for InMemoryDesiredStateStore {
    fn load(&mut self) -> Result<BTreeMap<String, DesiredState>, ContractError> {
        Ok(self.map.clone())
    }

    fn save(&mut self, plugin_id: &str, desired: DesiredState) -> Result<(), ContractError> {
        self.map.insert(plugin_id.to_string(), desired);
        Ok(())
    }
}

/// Hub-owned immutable generation storage (`<state_dir>/plugin-installed`).
pub struct HubInstallStore {
    root: PathBuf,
}

impl HubInstallStore {
    /// Construct the store rooted under `state_dir`.
    pub fn new(state_dir: &Path) -> Self {
        HubInstallStore {
            root: state_dir.join("plugin-installed"),
        }
    }

    /// Root directory holding every installed generation.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Where generation `generation` of `plugin_id` is installed.
    pub fn generation_dir(&self, plugin_id: &str, generation: u64) -> PathBuf {
        self.root
            .join(sanitized_dir_name(plugin_id))
            .join(format!("g{generation}"))
    }

    /// Copy `package` into hub-owned immutable storage and rehash the copy.
    ///
    /// Refuses an inventory digest mismatch, a manifest byte mismatch, an
    /// entrypoint escaping the installed copy, or an installation configuration
    /// ref that does not verify against the installed package bytes. The
    /// generation directory is rewritten atomically from the hub's point of
    /// view: it is removed and recreated before the copy.
    pub fn install(
        &self,
        package: &DiscoveredPackage,
        generation: u64,
        installation: &PluginInstallation,
    ) -> Result<InstalledGeneration, ContractError> {
        let id = package.plugin_id.as_str();
        let dest = self.generation_dir(id, generation);
        if dest.exists() {
            fs::remove_dir_all(&dest).map_err(|err| {
                FailureCode::StateUnavailable.failure(
                    dest.display().to_string(),
                    format!("cannot clear prior installed generation: {err}"),
                )
            })?;
        }
        let package_dir = dest.join("package");
        fs::create_dir_all(&package_dir).map_err(|err| {
            FailureCode::StateUnavailable.failure(
                package_dir.display().to_string(),
                format!("cannot create installed package dir: {err}"),
            )
        })?;
        copy_tree(&package.package_dir, &package_dir)?;
        let installed =
            seal_installed_generation(&package_dir, package, generation, installation)?;
        // Persist the exact pinned installation beside the immutable copy (outside
        // `package/`, so it is not part of the sealed inventory) so a restart
        // restores this generation's own configuration ref, grants, launch
        // profile and digest instead of re-deriving a replacement identity.
        write_installation_pin(&dest, installation)?;
        Ok(installed)
    }

    /// Enumerate the generation numbers present in hub-owned storage for
    /// `plugin_id` (ascending, deduplicated). A missing plugin directory yields
    /// an empty list.
    ///
    /// Used by the restart restore path to find every historical immutable
    /// generation so a new discovery never reallocates a used generation number.
    pub fn installed_generations(&self, plugin_id: &str) -> Result<Vec<u64>, ContractError> {
        let dir = self.root.join(sanitized_dir_name(plugin_id));
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(err) => {
                return Err(FailureCode::StateUnavailable.failure(
                    dir.display().to_string(),
                    format!("cannot read installed generations: {err}"),
                ))
            }
        };
        let mut generations = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|err| {
                FailureCode::StateUnavailable.failure(
                    dir.display().to_string(),
                    format!("cannot read installed generation entry: {err}"),
                )
            })?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            let Some(rest) = name.strip_prefix('g') else { continue };
            let Ok(generation) = rest.parse::<u64>() else { continue };
            if generation == 0 {
                continue;
            }
            let is_dir = entry
                .file_type()
                .map_err(|err| {
                    FailureCode::StateUnavailable.failure(
                        entry.path().display().to_string(),
                        format!("cannot stat installed generation: {err}"),
                    )
                })?
                .is_dir();
            if is_dir {
                generations.push(generation);
            }
        }
        generations.sort_unstable();
        generations.dedup();
        Ok(generations)
    }

    /// Load and rehash a hub-owned installed generation as a
    /// [`DiscoveredPackage`], reconstructing the sealed package identity from the
    /// immutable copy. `Ok(None)` when no such generation is stored.
    ///
    /// The manifest and sealed-inventory digest are re-verified against the
    /// stored bytes, so a tampered generation fails with
    /// [`FailureCode::IntegrityMismatch`] rather than being restored.
    pub fn load_installed_package(
        &self,
        plugin_id: &str,
        generation: u64,
    ) -> Result<Option<DiscoveredPackage>, ContractError> {
        let generation_dir = self.generation_dir(plugin_id, generation);
        let package_dir = generation_dir.join("package");
        if !package_dir.is_dir() {
            return Ok(None);
        }
        let manifest_bytes = fs::read(package_dir.join(PLUGIN_MANIFEST_FILE)).map_err(|err| {
            FailureCode::IntegrityMismatch.failure(
                plugin_id,
                format!(
                    "installed generation {generation} is missing `{PLUGIN_MANIFEST_FILE}`: {err}"
                ),
            )
        })?;
        let manifest: PluginManifest = serde_json::from_slice(&manifest_bytes).map_err(|err| {
            FailureCode::IntegrityMismatch.failure(
                plugin_id,
                format!("installed generation {generation} manifest no longer parses: {err}"),
            )
        })?;
        // The stored manifest must name the same plugin as the directory and the
        // durable checkpoint: a mismatched id would otherwise let a corrupted
        // generation masquerade as another plugin's immutable copy.
        if manifest.plugin_id != plugin_id {
            return Err(FailureCode::IdentityConflict.failure(
                plugin_id,
                format!(
                    "installed generation {generation} manifest is for `{}`, not `{plugin_id}`",
                    manifest.plugin_id
                ),
            ));
        }
        manifest.version_gate()?;
        manifest.validate_package_digest_shape()?;
        let recomputed = package_inventory_digest(&package_dir, &manifest)?;
        if recomputed != manifest.package_digest {
            return Err(FailureCode::IntegrityMismatch.failure(
                plugin_id,
                format!(
                    "installed generation {generation} rehashes to {recomputed}, manifest pins {}",
                    manifest.package_digest
                ),
            ));
        }
        let entrypoint_path = resolve_inside(&package_dir, &manifest.entrypoint.path, plugin_id)?;
        Ok(Some(DiscoveredPackage {
            plugin_id: plugin_id.to_string(),
            package_version: manifest.package_version.clone(),
            package_digest: manifest.package_digest.clone(),
            protocol_major: manifest.protocol_major,
            root: generation_dir,
            package_dir,
            entrypoint_path,
            manifest,
        }))
    }

    /// The exact pinned installation persisted for a hub-owned generation, when
    /// one is present.
    ///
    /// The pin is stored beside the immutable package copy (not inside it), so it
    /// survives a restart without perturbing the sealed inventory digest. A
    /// missing pin is `Ok(None)`; an unreadable/corrupt file is a hard error
    /// (never silently ignored).
    pub fn load_installation(
        &self,
        plugin_id: &str,
        generation: u64,
    ) -> Result<Option<PluginInstallation>, ContractError> {
        let path = self
            .generation_dir(plugin_id, generation)
            .join(INSTALLATION_PIN_FILE);
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(err) => {
                return Err(FailureCode::StateUnavailable.failure(
                    path.display().to_string(),
                    format!("cannot read installation pin: {err}"),
                ))
            }
        };
        let installation: PluginInstallation = serde_json::from_slice(&bytes).map_err(|err| {
            FailureCode::InvalidRecord.failure(
                plugin_id,
                format!("installed generation {generation} installation pin no longer parses: {err}"),
            )
        })?;
        Ok(Some(installation))
    }
}

/// File name of the persisted pinned installation beside a hub-owned package
/// copy (`<generation_dir>/installation.json`), outside the sealed `package/`
/// inventory.
pub const INSTALLATION_PIN_FILE: &str = "installation.json";

/// Persist the exact pinned [`PluginInstallation`] for one hub-owned generation.
fn write_installation_pin(
    generation_dir: &Path,
    installation: &PluginInstallation,
) -> Result<(), ContractError> {
    let path = generation_dir.join(INSTALLATION_PIN_FILE);
    let bytes = serde_json::to_vec_pretty(installation).map_err(|err| {
        FailureCode::InvalidRecord.failure(
            &installation.plugin_id,
            format!("cannot serialize installation pin: {err}"),
        )
    })?;
    fs::write(&path, bytes).map_err(|err| {
        FailureCode::StateUnavailable.failure(
            path.display().to_string(),
            format!("cannot persist installation pin: {err}"),
        )
    })
}

/// Build the immutable [`InstalledGeneration`] metadata for a hub-owned package
/// copy at `package_dir`, re-verifying the manifest bytes, the sealed-inventory
/// digest, the pinned configuration ref and the entrypoint.
///
/// Shared by [`HubInstallStore::install`] (fresh install) and the restart
/// restore path in [`crate::composition`], so a reconstructed generation carries
/// exactly the same verification as a freshly installed one.
pub(crate) fn seal_installed_generation(
    package_dir: &Path,
    package: &DiscoveredPackage,
    generation: u64,
    installation: &PluginInstallation,
) -> Result<InstalledGeneration, ContractError> {
    let id = package.plugin_id.as_str();
    let copied_manifest = fs::read(package_dir.join(PLUGIN_MANIFEST_FILE)).map_err(|err| {
        FailureCode::IntegrityMismatch.failure(
            id,
            format!("installed copy is missing `{PLUGIN_MANIFEST_FILE}`: {err}"),
        )
    })?;
    let manifest: PluginManifest = serde_json::from_slice(&copied_manifest).map_err(|err| {
        FailureCode::IntegrityMismatch.failure(
            id,
            format!("installed manifest no longer parses: {err}"),
        )
    })?;
    if manifest != package.manifest {
        return Err(FailureCode::IntegrityMismatch.failure(
            id,
            "installed manifest differs from the validated discovery manifest",
        ));
    }
    let recomputed = package_inventory_digest(package_dir, &manifest)?;
    if recomputed != package.package_digest {
        return Err(FailureCode::IntegrityMismatch.failure(
            id,
            format!(
                "installed copy digest {recomputed} does not match discovered {}",
                package.package_digest
            ),
        ));
    }

    // The pinned installation configuration must verify against the stored
    // package bytes: the generation carries this frozen ref, not the manifest's
    // default configuration.
    verify_artifact_ref(
        package_dir,
        id,
        "installation.configuration",
        &installation.configuration,
    )?;

    let entrypoint = resolve_inside(package_dir, &manifest.entrypoint.path, id)?;
    Ok(InstalledGeneration {
        plugin_id: package.plugin_id.clone(),
        generation,
        package_version: package.package_version.clone(),
        package_digest: package.package_digest.clone(),
        protocol_major: package.protocol_major,
        config_digest: installation.configuration.digest.clone(),
        configuration: installation.configuration.clone(),
        installation_digest: installation.installation_digest.clone(),
        launch_profile: installation.launch_profile.clone(),
        requested_capabilities: manifest.requested_capabilities.clone(),
        approved_capabilities: installation.approved_capabilities.clone(),
        required_services: manifest.required_services.clone(),
        installed_dir: package_dir.to_path_buf(),
        entrypoint,
        argv: manifest.entrypoint.argv.clone(),
        manifest,
    })
}

/// Registry entry observed state plus pinned generations and unresolved pins.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RegistryEntry {
    pub plugin_id: String,
    pub origin: ComponentOrigin,
    pub desired: DesiredState,
    pub observed: LifecycleState,
    /// Current launch epoch (increments on every restart/replacement).
    pub launch_epoch: u64,
    /// Generation currently serving new work.
    pub active_generation: Option<u64>,
    /// Candidate generation staged but not yet activated.
    pub pending_generation: Option<u64>,
    /// Highest generation ever allocated (monotonic; never reused).
    pub max_generation: u64,
    pub generations: BTreeMap<u64, InstalledGeneration>,
    /// Unresolved pinned operation ids per generation.
    pub pins: BTreeMap<u64, BTreeSet<String>>,
    pub last_error: Option<String>,
    pub observed_at_ms: u64,
}

impl RegistryEntry {
    fn new(plugin_id: String) -> Self {
        RegistryEntry {
            plugin_id,
            origin: ComponentOrigin::Discovered,
            desired: DesiredState::Stopped,
            observed: LifecycleState::Installed,
            launch_epoch: 0,
            active_generation: None,
            pending_generation: None,
            max_generation: 0,
            generations: BTreeMap::new(),
            pins: BTreeMap::new(),
            last_error: None,
            observed_at_ms: 0,
        }
    }

    /// Whether `generation` has unresolved pinned operations.
    pub fn has_unresolved_pins(&self, generation: u64) -> bool {
        self.pins
            .get(&generation)
            .is_some_and(|set| !set.is_empty())
    }
}

/// The hub's plugin registry.
#[derive(Clone, Debug, Default)]
pub struct PluginRegistry {
    entries: BTreeMap<String, RegistryEntry>,
}

impl PluginRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// All plugin ids, in UTF-8 byte order.
    pub fn plugin_ids(&self) -> Vec<&str> {
        self.entries.keys().map(String::as_str).collect()
    }

    /// The entry for `plugin_id`, if registered.
    pub fn entry(&self, plugin_id: &str) -> Option<&RegistryEntry> {
        self.entries.get(plugin_id)
    }

    /// The mutable entry for `plugin_id`, if registered.
    pub fn entry_mut(&mut self, plugin_id: &str) -> Option<&mut RegistryEntry> {
        self.entries.get_mut(plugin_id)
    }

    /// Represent a built-in conductor component without installing a package.
    ///
    /// Returns `false` when `component_id` already exists (idempotent). This is
    /// representation only: a built-in has no installed generation and is never
    /// launched by the plugin supervisor.
    pub fn register_builtin(&mut self, component_id: &str, now_ms: u64) -> bool {
        if self.entries.contains_key(component_id) {
            return false;
        }
        let mut entry = RegistryEntry::new(component_id.to_string());
        entry.origin = ComponentOrigin::BuiltIn;
        entry.observed = LifecycleState::Ready;
        entry.observed_at_ms = now_ms;
        self.entries.insert(component_id.to_string(), entry);
        true
    }

    /// Install a validated generation, allocating the next monotonic generation.
    ///
    /// The allocated generation is `max_generation + 1`; it is never reused,
    /// including after a removal or rollback. `installation` is the operator's
    /// frozen `PluginInstallation/v1` pin (validated by the caller against the
    /// discovered package); the installed generation carries its frozen
    /// configuration ref, launch profile and approved grants.
    pub fn install(
        &mut self,
        store: &HubInstallStore,
        package: &DiscoveredPackage,
        installation: &PluginInstallation,
    ) -> Result<u64, ContractError> {
        let entry = self
            .entries
            .entry(package.plugin_id.clone())
            .or_insert_with(|| RegistryEntry::new(package.plugin_id.clone()));
        entry.origin = ComponentOrigin::Discovered;
        let generation = entry.max_generation + 1;
        let installed = store.install(package, generation, installation)?;
        entry.max_generation = generation;
        entry.generations.insert(generation, installed);
        Ok(generation)
    }

    /// Install `package`, or reuse the existing generation that already holds an
    /// identical sealed package **and** pinned installation.
    ///
    /// Returns the generation and whether it was reused. A hub restart with the
    /// same package/config/grants therefore keeps its prior binding generation,
    /// while a changed package (or a changed pin) allocates `max_generation + 1`,
    /// which is never reused. Identity of an existing generation is the sealed
    /// package digest plus the pinned installation digest — the same data the
    /// binding pin and action receipts are bound to.
    pub fn install_or_reuse(
        &mut self,
        store: &HubInstallStore,
        package: &DiscoveredPackage,
        installation: &PluginInstallation,
    ) -> Result<(u64, bool), ContractError> {
        let entry = self
            .entries
            .entry(package.plugin_id.clone())
            .or_insert_with(|| RegistryEntry::new(package.plugin_id.clone()));
        entry.origin = ComponentOrigin::Discovered;
        let reused = entry.generations.iter().find_map(|(generation, installed)| {
            (installed.package_digest == package.package_digest
                && installed.installation_digest == installation.installation_digest
                && installed.config_digest == installation.configuration.digest
                && installed.approved_capabilities == installation.approved_capabilities
                && installed.launch_profile == installation.launch_profile)
                .then_some(*generation)
        });
        if let Some(generation) = reused {
            return Ok((generation, true));
        }
        let generation = entry.max_generation + 1;
        let installed = store.install(package, generation, installation)?;
        entry.max_generation = generation;
        entry.generations.insert(generation, installed);
        Ok((generation, false))
    }

    /// Ensure an entry exists for `plugin_id` and return it mutably (restart
    /// restore path: a persisted plugin the configured roots no longer contain
    /// must still be represented, and its generation must not be reused).
    pub fn ensure_entry(&mut self, plugin_id: &str) -> &mut RegistryEntry {
        self.entries
            .entry(plugin_id.to_string())
            .or_insert_with(|| RegistryEntry::new(plugin_id.to_string()))
    }

    /// Insert a reconstructed immutable generation during restart restore.
    ///
    /// Raises the entry's monotonic ceiling to the restored generation so a later
    /// install can never reallocate it.
    pub fn restore_generation(&mut self, installed: InstalledGeneration) {
        let plugin_id = installed.plugin_id.clone();
        let generation = installed.generation;
        let entry = self.ensure_entry(&plugin_id);
        entry.origin = ComponentOrigin::Discovered;
        entry.max_generation = entry.max_generation.max(generation);
        entry.generations.insert(generation, installed);
    }

    /// Apply the durable generation bounds for `plugin_id` after restoring its
    /// generations.
    ///
    /// `max_generation` is the persisted monotonic ceiling (a new install
    /// allocates strictly above it). `active` is restored only when that
    /// generation was actually reconstructed, so a missing immutable copy never
    /// produces a phantom active generation.
    pub fn restore_bounds(&mut self, plugin_id: &str, max_generation: u64, active: Option<u64>) {
        let entry = self.ensure_entry(plugin_id);
        entry.max_generation = entry.max_generation.max(max_generation);
        if let Some(generation) = active {
            if entry.generations.contains_key(&generation) {
                entry.active_generation = Some(generation);
            }
        }
    }

    /// Record `generation` as the candidate staged for activation.
    ///
    /// Rehashes the hub-owned installed copy first (frozen contract: rehash on
    /// install, stage and launch); a tampered installed generation is refused.
    pub fn stage(&mut self, plugin_id: &str, generation: u64) -> Result<(), ContractError> {
        let entry = self.require(plugin_id)?;
        let Some(installed) = entry.generations.get(&generation) else {
            return Err(FailureCode::InvalidRecord.failure(
                plugin_id,
                format!("cannot stage unknown generation {generation}"),
            ));
        };
        installed.rehash()?;
        entry.pending_generation = Some(generation);
        Ok(())
    }

    /// Activate a staged generation: it becomes the active generation.
    ///
    /// The prior active generation is marked [`LifecycleState::Draining`]; new
    /// work switches to the candidate, while the old generation keeps its pins.
    pub fn activate(&mut self, plugin_id: &str, generation: u64) -> Result<(), ContractError> {
        let entry = self.require(plugin_id)?;
        if !entry.generations.contains_key(&generation) {
            return Err(FailureCode::InvalidRecord.failure(
                plugin_id,
                format!("cannot activate unknown generation {generation}"),
            ));
        }
        if let Some(previous) = entry.active_generation {
            if previous != generation {
                entry.observed = LifecycleState::Draining;
            }
        }
        entry.active_generation = Some(generation);
        entry.pending_generation = None;
        Ok(())
    }

    /// Set the desired state and persist it through `store`.
    pub fn set_desired(
        &mut self,
        plugin_id: &str,
        desired: DesiredState,
        store: &mut dyn DesiredStateStore,
        now_ms: u64,
    ) -> Result<(), ContractError> {
        store.save(plugin_id, desired)?;
        let entry = self
            .entries
            .entry(plugin_id.to_string())
            .or_insert_with(|| RegistryEntry::new(plugin_id.to_string()));
        entry.desired = desired;
        entry.observed_at_ms = now_ms;
        Ok(())
    }

    /// Load desired state from `store` into every registered entry.
    pub fn load_desired(
        &mut self,
        store: &mut dyn DesiredStateStore,
    ) -> Result<BTreeMap<String, DesiredState>, ContractError> {
        let desired = store.load()?;
        for (plugin_id, state) in &desired {
            if let Some(entry) = self.entries.get_mut(plugin_id) {
                entry.desired = *state;
            }
        }
        Ok(desired)
    }

    /// The desired state recorded for `plugin_id` (defaults to `Stopped`).
    pub fn desired(&self, plugin_id: &str) -> DesiredState {
        self.entries
            .get(plugin_id)
            .map(|entry| entry.desired)
            .unwrap_or(DesiredState::Stopped)
    }

    /// Mark startup begun under a new launch epoch.
    ///
    /// The candidate generation stays **pending** while it starts: the frozen
    /// contract activates a staged generation only after it is Ready and
    /// activation is receipted, so startup must not switch new work onto it.
    pub fn mark_starting(
        &mut self,
        plugin_id: &str,
        generation: u64,
        now_ms: u64,
    ) -> Result<u64, ContractError> {
        let entry = self.require(plugin_id)?;
        entry.launch_epoch += 1;
        entry.observed = LifecycleState::Starting;
        entry.pending_generation = Some(generation);
        entry.observed_at_ms = now_ms;
        entry.last_error = None;
        Ok(entry.launch_epoch)
    }

    /// Mark readiness after the supervisor's READY/Describe/Health gates pass.
    ///
    /// This is the pending-to-active transition: the Ready candidate becomes the
    /// active generation and any staged candidate is cleared.
    pub fn mark_ready(
        &mut self,
        plugin_id: &str,
        generation: u64,
        now_ms: u64,
    ) -> Result<(), ContractError> {
        let entry = self.require(plugin_id)?;
        entry.observed = LifecycleState::Ready;
        entry.active_generation = Some(generation);
        entry.pending_generation = None;
        entry.observed_at_ms = now_ms;
        entry.last_error = None;
        Ok(())
    }

    /// Mark a failure with a diagnostic.
    pub fn mark_failed(
        &mut self,
        plugin_id: &str,
        reason: impl Into<String>,
        now_ms: u64,
    ) -> Result<(), ContractError> {
        let entry = self.require(plugin_id)?;
        entry.observed = LifecycleState::Failed;
        entry.last_error = Some(reason.into());
        entry.observed_at_ms = now_ms;
        Ok(())
    }

    /// Begin draining a generation. Only the active generation may drain.
    pub fn begin_drain(
        &mut self,
        plugin_id: &str,
        generation: u64,
        now_ms: u64,
    ) -> Result<(), ContractError> {
        let entry = self.require(plugin_id)?;
        if entry.active_generation != Some(generation) {
            return Err(FailureCode::StaleGeneration.failure(
                plugin_id,
                format!("generation {generation} is not the active generation"),
            ));
        }
        entry.observed = LifecycleState::Draining;
        entry.observed_at_ms = now_ms;
        Ok(())
    }

    /// Retire a generation: it accepts no new work and is no longer active.
    pub fn retire(
        &mut self,
        plugin_id: &str,
        generation: u64,
        now_ms: u64,
    ) -> Result<(), ContractError> {
        let entry = self.require(plugin_id)?;
        if entry.active_generation == Some(generation) {
            entry.active_generation = None;
        }
        if entry.pending_generation == Some(generation) {
            entry.pending_generation = None;
        }
        entry.observed = LifecycleState::Retired;
        entry.observed_at_ms = now_ms;
        Ok(())
    }

    /// Pin one unresolved operation to `generation`.
    pub fn pin_operation(
        &mut self,
        plugin_id: &str,
        generation: u64,
        operation_id: impl Into<String>,
    ) -> Result<(), ContractError> {
        let entry = self.require(plugin_id)?;
        if !entry.generations.contains_key(&generation) {
            return Err(FailureCode::StaleGeneration.failure(
                plugin_id,
                format!("cannot pin to unknown generation {generation}"),
            ));
        }
        entry
            .pins
            .entry(generation)
            .or_default()
            .insert(operation_id.into());
        Ok(())
    }

    /// Resolve (complete) one pinned operation on `generation`.
    pub fn resolve_pin(
        &mut self,
        plugin_id: &str,
        generation: u64,
        operation_id: &str,
    ) -> Result<(), ContractError> {
        let entry = self.require(plugin_id)?;
        if let Some(set) = entry.pins.get_mut(&generation) {
            set.remove(operation_id);
        }
        Ok(())
    }

    /// Remove an installed generation.
    ///
    /// Refuses (`EffectUnconfirmed`) while the generation still has unresolved
    /// pinned operations, and (`RevisionConflict`) while it is the active
    /// generation — the replacement must be activated and this one retired
    /// first. A removed generation number is never reallocated.
    pub fn remove(
        &mut self,
        plugin_id: &str,
        generation: u64,
        now_ms: u64,
    ) -> Result<(), ContractError> {
        let entry = self.require(plugin_id)?;
        if entry.has_unresolved_pins(generation) {
            return Err(FailureCode::EffectUnconfirmed.failure(
                plugin_id,
                format!("generation {generation} has unresolved pinned operations"),
            ));
        }
        if entry.active_generation == Some(generation) {
            return Err(FailureCode::RevisionConflict.failure(
                plugin_id,
                format!("generation {generation} is active; retire it before removal"),
            ));
        }
        if entry.generations.remove(&generation).is_none() {
            return Err(FailureCode::InvalidRecord.failure(
                plugin_id,
                format!("generation {generation} is not installed"),
            ));
        }
        entry.pins.remove(&generation);
        entry.observed_at_ms = now_ms;
        Ok(())
    }

    /// Apply a health observation from launch `epoch`.
    ///
    /// An observation tagged with an epoch older than the entry's current
    /// launch epoch is ignored: a superseded process cannot mark a replacement
    /// Ready or Failed (launch-epoch fencing).
    pub fn record_health(
        &mut self,
        plugin_id: &str,
        epoch: u64,
        ready: bool,
        detail: impl Into<String>,
        now_ms: u64,
    ) -> Result<bool, ContractError> {
        let entry = self.require(plugin_id)?;
        if epoch < entry.launch_epoch {
            return Ok(false);
        }
        entry.observed = if ready {
            LifecycleState::Ready
        } else {
            LifecycleState::Failed
        };
        entry.last_error = if ready { None } else { Some(detail.into()) };
        entry.observed_at_ms = now_ms;
        Ok(true)
    }

    fn require(&mut self, plugin_id: &str) -> Result<&mut RegistryEntry, ContractError> {
        self.entries.get_mut(plugin_id).ok_or_else(|| {
            FailureCode::InvalidRecord.failure(plugin_id, "plugin is not registered")
        })
    }
}

/// Byte budget for a plugin socket path, leaving headroom under the smallest
/// platform `sun_path` (`104` on Linux / `104` on macOS).
pub const SOCKET_PATH_LIMIT: usize = 100;

/// A SUN_LEN-safe socket path for one plugin launch.
///
/// Uses `<state_dir>/plugin-sockets/<slug>-e<epoch>.sock` when that fits
/// [`SOCKET_PATH_LIMIT`]; under a long state dir it falls back to a short
/// hashed name under a stable per-user base, so the path always fits.
pub fn plugin_socket_path(state_dir: &Path, plugin_id: &str, epoch: u64) -> PathBuf {
    let slug = slug(plugin_id, 24);
    let direct = state_dir
        .join("plugin-sockets")
        .join(format!("{slug}-e{epoch}.sock"));
    if direct.as_os_str().len() <= SOCKET_PATH_LIMIT {
        return direct;
    }
    let base = socket_fallback_base(state_dir);
    base.join(format!("{}.sock", short_hash(plugin_id, epoch)))
}

/// Short, stable fallback base for a socket path that cannot fit under a long
/// state dir. Derived from the state dir so two hubs do not collide, and rooted
/// in the platform temp dir (a socket is not durable evidence). Kept compact so
/// the hashed fallback path always fits [`SOCKET_PATH_LIMIT`] even when the
/// platform temp dir is long.
fn socket_fallback_base(state_dir: &Path) -> PathBuf {
    let digest = agalma_contracts::sha256_digest(state_dir.to_string_lossy().as_bytes());
    let short = &digest[7..19];
    std::env::temp_dir().join("ap").join(short)
}

/// A filesystem-safe directory name for `plugin_id` (the frozen id pattern
/// permits `/`, `:`, and other path-significant characters).
pub fn sanitized_dir_name(plugin_id: &str) -> String {
    let slug = slug(plugin_id, 64);
    format!("{slug}-{}", &agalma_contracts::sha256_digest(plugin_id.as_bytes())[7..19])
}

/// Replace path-significant characters and bound the length.
fn slug(value: &str, max: usize) -> String {
    let mut out = String::with_capacity(value.len().min(max));
    for ch in value.chars() {
        if out.len() >= max {
            break;
        }
        if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-') {
            out.push(ch);
        } else {
            out.push('_');
        }
    }
    if out.is_empty() {
        out.push('_');
    }
    out
}

/// A compact 12-hex short hash of `plugin_id` + `epoch`.
fn short_hash(plugin_id: &str, epoch: u64) -> String {
    let digest = agalma_contracts::sha256_digest(format!("{plugin_id}#{epoch}").as_bytes());
    digest[7..19].to_string()
}

/// Resolve `relative` inside `dir`, refusing an escape.
fn resolve_inside(dir: &Path, relative: &str, id: &str) -> Result<PathBuf, ContractError> {
    let candidate = dir.join(relative);
    let resolved = fs::canonicalize(&candidate).map_err(|err| {
        FailureCode::InvalidRecord.failure(
            id,
            format!("installed entrypoint {relative:?} is unavailable: {err}"),
        )
    })?;
    if !resolved.starts_with(dir) {
        return Err(FailureCode::ScopeDenied.failure(
            id,
            format!("installed entrypoint {relative:?} escapes the installed package"),
        ));
    }
    Ok(resolved)
}

/// Recursively copy regular files (and their mode bits) from `src` to `dst`.
fn copy_tree(src: &Path, dst: &Path) -> Result<(), ContractError> {
    for entry in fs::read_dir(src).map_err(|err| {
        FailureCode::StateUnavailable.failure(
            src.display().to_string(),
            format!("cannot read package dir for install: {err}"),
        )
    })? {
        let entry = entry.map_err(|err| {
            FailureCode::StateUnavailable.failure(
                src.display().to_string(),
                format!("cannot read package entry for install: {err}"),
            )
        })?;
        let file_type = entry.file_type().map_err(|err| {
            FailureCode::StateUnavailable.failure(
                entry.path().display().to_string(),
                format!("cannot stat package entry for install: {err}"),
            )
        })?;
        let target = dst.join(entry.file_name());
        if file_type.is_dir() {
            fs::create_dir_all(&target).map_err(|err| {
                FailureCode::StateUnavailable.failure(
                    target.display().to_string(),
                    format!("cannot create install dir: {err}"),
                )
            })?;
            copy_tree(&entry.path(), &target)?;
        } else if file_type.is_file() {
            fs::copy(entry.path(), &target).map_err(|err| {
                FailureCode::StateUnavailable.failure(
                    target.display().to_string(),
                    format!("cannot copy package file: {err}"),
                )
            })?;
            let metadata = entry.metadata().map_err(|err| {
                FailureCode::StateUnavailable.failure(
                    entry.path().display().to_string(),
                    format!("cannot stat copied source: {err}"),
                )
            })?;
            fs::set_permissions(&target, metadata.permissions()).map_err(|err| {
                FailureCode::StateUnavailable.failure(
                    target.display().to_string(),
                    format!("cannot preserve mode bits: {err}"),
                )
            })?;
        } else {
            return Err(FailureCode::ScopeDenied.failure(
                entry.path().display().to_string(),
                "package contains a non-file entry (discovery should have refused it)",
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitized_dir_name_never_escapes_or_collides() {
        assert!(!sanitized_dir_name("../evil").contains('/'));
        assert!(!sanitized_dir_name("a/b").contains('/'));
        assert_ne!(sanitized_dir_name("a/b"), sanitized_dir_name("a_b"));
    }

    #[test]
    fn socket_path_fits_under_long_state_dir() {
        let long = PathBuf::from("/".to_string() + &"x".repeat(220));
        let path = plugin_socket_path(&long, "example.plugin", 3);
        assert!(
            path.as_os_str().len() <= SOCKET_PATH_LIMIT,
            "{} length {}",
            path.display(),
            path.as_os_str().len()
        );
    }
}
