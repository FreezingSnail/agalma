//! HP.2 plugin-package discovery (`agalma-jmr.2`).
//!
//! Discovery is the hub's read-only view of independently installed executable
//! packages: it enumerates the configured plugin roots, resolves each package's
//! canonical entrypoint, validates its `PluginManifest/v1` declarations against
//! the sealed file inventory, and returns deterministic descriptors to the
//! registry. It never launches a child, evaluates a shell command string, or
//! imports a plugin's code — discovery only reads the filesystem.
//!
//! ## Root policy (D1)
//!
//! Roots come from the common CLI/config path ([`crate::config::Config`]). No
//! configured root yields an empty discovery set. An explicitly *required* root
//! that is missing refuses discovery. An explicitly *optional* root that is
//! missing contributes no packages and reports a visible diagnostic, and
//! discovery must not create that directory. Permission errors, non-directory
//! roots, and invalid packages fail even under an optional root.
//!
//! ## Package digest
//!
//! `PluginManifest.package_digest` covers an ordered inventory of the sealed
//! package: every regular file below the package directory (except the manifest
//! file itself) contributes `{path, mode, sha256}` with `path` a `/`-separated
//! package-relative path, and the inventory is sorted by UTF-8 byte order. The
//! canonical manifest excluding its own `package_digest` field is bound into the
//! same document, so a manifest edit also changes the package digest. The digest
//! is `sha256` of the canonical encoding of
//! `{"inventory":[…],"manifest":{…}}`. A symlink that escapes the package is
//! refused, and a *directory* symlink is refused outright so the recursive walk
//! cannot cycle through an ancestor link. `config_schema`,
//! `default_configuration` and every `report_schemas[].schema` reference are
//! resolved as package-relative paths and verified against their pinned byte
//! count and SHA-256 digest, so discovery never hands the registry an unverified
//! artifact identity.
//!
//! HP.3 copies validated packages into hub-owned immutable storage and rehashes
//! them at install/stage/launch using [`package_inventory_digest`].

use std::collections::BTreeMap;
use std::fs;
use std::path::{Component, Path, PathBuf};

use serde_json::{json, Value};

use agalma_contracts::{
    canonical_json, sha256_digest, validate_digest, ContractError, FailureCode, PackageArtifactRef,
    PluginManifest, MANIFEST_DOCUMENT_LIMIT_BYTES,
};

/// Manifest file name inside one package directory.
pub const PLUGIN_MANIFEST_FILE: &str = "plugin-manifest.json";

/// Frozen `PluginManifest/v1` `maxItems` bound for every declared list.
pub const MAX_DECLARED_ITEMS: usize = 100;

/// One configured plugin discovery root and whether it must exist.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PluginRoot {
    /// Configured root directory (never created by discovery).
    pub path: PathBuf,
    /// When true a missing root yields an empty contribution and a diagnostic;
    /// when false a missing root refuses discovery.
    pub optional: bool,
}

impl PluginRoot {
    /// A required root: a missing directory refuses discovery.
    pub fn required(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            optional: false,
        }
    }

    /// An optional root: a missing directory contributes nothing.
    pub fn optional(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            optional: true,
        }
    }
}

/// A discovered, validated package descriptor (no process is started).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiscoveredPackage {
    pub plugin_id: String,
    pub package_version: String,
    /// Verified `sha256:<64 hex>` sealed-inventory digest.
    pub package_digest: String,
    pub protocol_major: u64,
    /// Configured root that supplied this package.
    pub root: PathBuf,
    /// Canonical (symlink-resolved) package directory.
    pub package_dir: PathBuf,
    /// Canonical (symlink-resolved) executable path inside the package.
    pub entrypoint_path: PathBuf,
    /// The validated manifest, intact.
    pub manifest: PluginManifest,
}

/// A non-fatal discovery observation for an explicitly optional root.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiscoveryDiagnostic {
    pub root: PathBuf,
    pub message: String,
}

/// Deterministic discovery result: ordered packages plus optional-root
/// diagnostics.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DiscoveryReport {
    /// Packages ordered by `plugin_id` (UTF-8 byte order), then package dir.
    pub packages: Vec<DiscoveredPackage>,
    pub diagnostics: Vec<DiscoveryDiagnostic>,
}

impl DiscoveryReport {
    /// Discovered plugin IDs in report order.
    pub fn plugin_ids(&self) -> Vec<&str> {
        self.packages
            .iter()
            .map(|package| package.plugin_id.as_str())
            .collect()
    }

    /// The descriptor for `plugin_id`, if discovered.
    pub fn package(&self, plugin_id: &str) -> Option<&DiscoveredPackage> {
        self.packages
            .iter()
            .find(|package| package.plugin_id == plugin_id)
    }
}

/// Discover and validate every package under `roots`.
///
/// Fails (before any effect) on a missing required root, a non-directory root, a
/// permission error, an invalid/duplicate/cyclic package, or a digest mismatch.
/// A missing optional root contributes nothing and produces a diagnostic.
pub fn discover_plugins(roots: &[PluginRoot]) -> Result<DiscoveryReport, ContractError> {
    let mut report = DiscoveryReport::default();
    let mut packages: Vec<DiscoveredPackage> = Vec::new();

    for root in roots {
        let metadata = match fs::metadata(&root.path) {
            Ok(metadata) => Some(metadata),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound && root.optional => {
                report.diagnostics.push(DiscoveryDiagnostic {
                    root: root.path.clone(),
                    message: "optional plugin root is missing; contributing no packages".to_string(),
                });
                continue;
            }
            Err(err) => {
                return Err(FailureCode::StateUnavailable.failure(
                    root.path.display().to_string(),
                    format!("plugin root is unavailable: {err}"),
                ))
            }
        };
        let metadata = metadata.expect("metadata present unless optional-missing");
        if !metadata.is_dir() {
            return Err(FailureCode::InvalidRecord.failure(
                root.path.display().to_string(),
                "plugin root is not a directory",
            ));
        }

        let root_dir = resolve_dir(&root.path, "plugin root")?;
        for package_dir in read_package_dirs(&root.path, &root_dir)? {
            packages.push(load_package(root, &root_dir, &package_dir)?);
        }
    }

    packages.sort_by(|a, b| {
        a.plugin_id
            .as_bytes()
            .cmp(b.plugin_id.as_bytes())
            .then_with(|| a.package_dir.as_os_str().cmp(b.package_dir.as_os_str()))
    });
    refuse_duplicate_ids(&packages)?;
    refuse_dependency_cycles(&packages)?;

    report.packages = packages;
    Ok(report)
}

/// Candidate package directories directly under `root`, sorted by file name.
///
/// Non-directory entries are ignored; a symlink is followed only when it stays
/// inside the root.
fn read_package_dirs(
    declared_root: &Path,
    root_dir: &Path,
) -> Result<Vec<PathBuf>, ContractError> {
    let entries = fs::read_dir(declared_root).map_err(|err| {
        FailureCode::StateUnavailable.failure(
            declared_root.display().to_string(),
            format!("cannot read plugin root: {err}"),
        )
    })?;
    let mut dirs = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|err| {
            FailureCode::StateUnavailable.failure(
                declared_root.display().to_string(),
                format!("cannot read plugin root entry: {err}"),
            )
        })?;
        let file_type = entry.file_type().map_err(|err| {
            FailureCode::StateUnavailable.failure(
                entry.path().display().to_string(),
                format!("cannot stat plugin root entry: {err}"),
            )
        })?;
        if file_type.is_symlink() {
            let resolved = resolve_dir(&entry.path(), "package directory")?;
            if !resolved.starts_with(root_dir) {
                return Err(FailureCode::ScopeDenied.failure(
                    entry.path().display().to_string(),
                    "package directory symlink escapes its plugin root",
                ));
            }
            dirs.push(resolved);
        } else if file_type.is_dir() {
            dirs.push(entry.path());
        }
        // Plain files and other entry kinds are not packages.
    }
    dirs.sort_by(|a, b| a.file_name().cmp(&b.file_name()));
    Ok(dirs)
}

/// Load, validate, and inventory-verify one package directory.
fn load_package(
    root: &PluginRoot,
    root_dir: &Path,
    package_dir: &Path,
) -> Result<DiscoveredPackage, ContractError> {
    let resolved_dir = resolve_dir(package_dir, "package directory")?;
    if !resolved_dir.starts_with(root_dir) {
        return Err(FailureCode::ScopeDenied.failure(
            package_dir.display().to_string(),
            "package directory resolves outside its plugin root",
        ));
    }
    let manifest = load_manifest(&resolved_dir)?;
    validate_declarations(&manifest)?;
    verify_artifact_refs(&resolved_dir, &manifest)?;
    let entrypoint_path = resolve_entrypoint(&resolved_dir, &manifest)?;
    let recomputed = package_inventory_digest(&resolved_dir, &manifest)?;
    if recomputed != manifest.package_digest {
        return Err(FailureCode::IntegrityMismatch.failure(
            &manifest.plugin_id,
            format!(
                "package digest {} does not match recomputed inventory digest {recomputed}",
                manifest.package_digest
            ),
        ));
    }
    Ok(DiscoveredPackage {
        plugin_id: manifest.plugin_id.clone(),
        package_version: manifest.package_version.clone(),
        package_digest: manifest.package_digest.clone(),
        protocol_major: manifest.protocol_major,
        root: root.path.clone(),
        package_dir: resolved_dir,
        entrypoint_path,
        manifest,
    })
}

/// Read, parse, bound, and version-gate `<package_dir>/plugin-manifest.json`.
fn load_manifest(package_dir: &Path) -> Result<PluginManifest, ContractError> {
    let manifest_path = package_dir.join(PLUGIN_MANIFEST_FILE);
    let bytes = match fs::read(&manifest_path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Err(FailureCode::InvalidRecord.failure(
                package_dir.display().to_string(),
                format!("package is missing `{PLUGIN_MANIFEST_FILE}`"),
            ))
        }
        Err(err) => {
            return Err(FailureCode::StateUnavailable.failure(
                manifest_path.display().to_string(),
                format!("cannot read package manifest: {err}"),
            ))
        }
    };
    if bytes.len() as u64 > MANIFEST_DOCUMENT_LIMIT_BYTES {
        return Err(FailureCode::InvalidRecord.failure(
            package_dir.display().to_string(),
            format!(
                "manifest is {} bytes, exceeding the {MANIFEST_DOCUMENT_LIMIT_BYTES}-byte limit",
                bytes.len()
            ),
        ));
    }
    let manifest: PluginManifest = serde_json::from_slice(&bytes).map_err(|err| {
        FailureCode::InvalidRecord.failure(
            package_dir.display().to_string(),
            format!("malformed manifest: {err}"),
        )
    })?;
    manifest.version_gate()?;
    manifest.validate_package_digest_shape()?;
    manifest.check_document_bound()?;
    Ok(manifest)
}

/// Structural validation of the declared entrypoint, argument vector,
/// capability/service declarations, report schemas, views and actions.
///
/// Enforces every applicable frozen `PluginManifest/v1` constraint:
/// `minLength`/`minItems`/`maxItems` per list, non-empty strings, `pattern` for
/// ids, integer bounds, and the required `report_schemas`/`views` minimum of one.
/// Uniqueness of identity-bearing declaration keys is additionally enforced so a
/// package cannot shadow one entry with another.
fn validate_declarations(manifest: &PluginManifest) -> Result<(), ContractError> {
    let id = manifest.plugin_id.as_str();
    validate_plugin_id(id)?;
    if manifest.package_version.is_empty() {
        return Err(FailureCode::InvalidRecord.failure(
            id,
            "package_version must not be empty",
        ));
    }

    // Entrypoint: package-relative, no absolute/`..`, non-empty launcher.
    let path = manifest.entrypoint.path.as_str();
    if path.is_empty() {
        return Err(FailureCode::InvalidRecord.failure(
            id,
            "entrypoint.path must not be empty",
        ));
    }
    let declared = Path::new(path);
    if declared.is_absolute() || declared.components().any(|c| c == Component::ParentDir) {
        return Err(FailureCode::ScopeDenied.failure(
            id,
            format!("entrypoint.path {path:?} is not package-relative"),
        ));
    }
    if manifest.entrypoint.launch_profile.is_empty() {
        return Err(FailureCode::InvalidRecord.failure(
            id,
            "entrypoint.launch_profile must not be empty",
        ));
    }
    if manifest.entrypoint.argv.len() > MAX_DECLARED_ITEMS {
        return Err(bound_failure(id, "entrypoint.argv"));
    }

    // Offered domain APIs: bounded list, non-empty names, major >= 1, and
    // bounded, non-empty capability names per API.
    if manifest.domain_apis.len() > MAX_DECLARED_ITEMS {
        return Err(bound_failure(id, "domain_apis"));
    }
    let mut seen_apis = BTreeMap::new();
    for domain in &manifest.domain_apis {
        if domain.api.is_empty() || domain.major < 1 {
            return Err(FailureCode::InvalidRecord.failure(
                id,
                "domain API requires a non-empty name and major >= 1",
            ));
        }
        if domain.capabilities.len() > MAX_DECLARED_ITEMS {
            return Err(bound_failure(id, "domain_apis[].capabilities"));
        }
        for capability in &domain.capabilities {
            if capability.is_empty() {
                return Err(FailureCode::InvalidRecord.failure(
                    id,
                    "domain API capability must not be empty",
                ));
            }
        }
        if seen_apis.insert((domain.api.clone(), domain.major), ()).is_some() {
            return Err(FailureCode::InvalidRecord.failure(
                id,
                format!("duplicate domain API declaration {}/{}", domain.api, domain.major),
            ));
        }
    }

    // Capability and service declarations: bounded, non-empty, unique.
    require_bounded_unique_nonempty(id, "requested_capabilities", &manifest.requested_capabilities)?;
    require_bounded_unique_nonempty(id, "required_services", &manifest.required_services)?;
    require_bounded_unique_nonempty(id, "optional_services", &manifest.optional_services)?;
    for service in &manifest.required_services {
        if manifest.optional_services.contains(service) {
            return Err(FailureCode::InvalidRecord.failure(
                id,
                format!("service {service:?} is declared both required and optional"),
            ));
        }
    }

    // Report schemas: at least one, bounded, non-empty namespace, version >= 1.
    if manifest.report_schemas.is_empty() {
        return Err(FailureCode::InvalidRecord.failure(
            id,
            "report_schemas must declare at least one report schema",
        ));
    }
    if manifest.report_schemas.len() > MAX_DECLARED_ITEMS {
        return Err(bound_failure(id, "report_schemas"));
    }
    let mut seen_schemas = BTreeMap::new();
    for schema in &manifest.report_schemas {
        if schema.namespace.is_empty() || schema.version < 1 {
            return Err(FailureCode::InvalidRecord.failure(
                id,
                "report schema requires a non-empty namespace and version >= 1",
            ));
        }
        if seen_schemas
            .insert((schema.namespace.clone(), schema.version), ())
            .is_some()
        {
            return Err(FailureCode::InvalidRecord.failure(
                id,
                format!(
                    "duplicate report schema {}/{}",
                    schema.namespace, schema.version
                ),
            ));
        }
    }

    // Views: at least one, bounded, id pattern, non-empty text, bounded fields.
    if manifest.views.is_empty() {
        return Err(FailureCode::InvalidRecord.failure(
            id,
            "views must declare at least one view",
        ));
    }
    if manifest.views.len() > MAX_DECLARED_ITEMS {
        return Err(bound_failure(id, "views"));
    }
    let mut seen_views = BTreeMap::new();
    for view in &manifest.views {
        validate_view_id(&view.view_id)?;
        if view.schema_namespace.is_empty() || view.title.is_empty() || view.schema_version < 1 {
            return Err(FailureCode::InvalidRecord.failure(
                id,
                "view requires a non-empty schema_namespace/title and schema_version >= 1",
            ));
        }
        if view.fields.len() < 1 {
            return Err(FailureCode::InvalidRecord.failure(
                id,
                "view must declare at least one field",
            ));
        }
        if view.fields.len() > MAX_DECLARED_ITEMS {
            return Err(bound_failure(id, "views[].fields"));
        }
        for field in &view.fields {
            if field.is_empty() {
                return Err(FailureCode::InvalidRecord.failure(
                    id,
                    "view field must not be empty",
                ));
            }
        }
        if seen_views.insert(view.view_id.clone(), ()).is_some() {
            return Err(FailureCode::InvalidRecord.failure(
                id,
                format!("duplicate view_id {:?}", view.view_id),
            ));
        }
    }

    // Actions: bounded, id pattern, non-empty text, positive deadline.
    if manifest.actions.len() > MAX_DECLARED_ITEMS {
        return Err(bound_failure(id, "actions"));
    }
    let mut seen_actions = BTreeMap::new();
    for action in &manifest.actions {
        validate_action_id(&action.action_id)?;
        if action.input_schema.is_empty() || action.required_capability.is_empty() {
            return Err(FailureCode::InvalidRecord.failure(
                id,
                "action requires a non-empty input_schema and required_capability",
            ));
        }
        if action.default_deadline_ms < 1 {
            return Err(FailureCode::InvalidRecord.failure(
                id,
                "action.default_deadline_ms must be >= 1",
            ));
        }
        if let Some(confirmation) = &action.human_confirmation {
            if confirmation.is_empty() {
                return Err(FailureCode::InvalidRecord.failure(
                    id,
                    "action.human_confirmation must be null or a non-empty string",
                ));
            }
        }
        if seen_actions.insert(action.action_id.clone(), ()).is_some() {
            return Err(FailureCode::InvalidRecord.failure(
                id,
                format!("duplicate action_id {:?}", action.action_id),
            ));
        }
    }

    // State compatibility: non-empty write namespace, bounded read list.
    if manifest.state_compatibility.writes.is_empty() {
        return Err(FailureCode::InvalidRecord.failure(
            id,
            "state_compatibility.writes must not be empty",
        ));
    }
    require_bounded_unique_nonempty(
        id,
        "state_compatibility.reads",
        &manifest.state_compatibility.reads,
    )?;
    Ok(())
}

/// A typed failure for a list/argument-vector exceeding its frozen bound.
fn bound_failure(id: &str, field: &str) -> ContractError {
    FailureCode::InvalidRecord.failure(
        id,
        format!("{field} exceeds the frozen {MAX_DECLARED_ITEMS}-item bound"),
    )
}

/// Resolve and verify `config_schema`, `default_configuration`, and every
/// `report_schemas[].schema` reference against its package-relative path, file
/// bytes, and pinned digest.
fn verify_artifact_refs(package_dir: &Path, manifest: &PluginManifest) -> Result<(), ContractError> {
    let id = manifest.plugin_id.as_str();
    verify_artifact_ref(package_dir, id, "config_schema", &manifest.config_schema)?;
    verify_artifact_ref(
        package_dir,
        id,
        "default_configuration",
        &manifest.default_configuration,
    )?;
    for schema in &manifest.report_schemas {
        verify_artifact_ref(package_dir, id, "report_schemas[].schema", &schema.schema)?;
    }
    Ok(())
}

/// Verify one `ArtifactRef`: relative safe path, present regular file inside the
/// package, and bytes matching the pinned byte count and SHA-256 digest.
fn verify_artifact_ref(
    package_dir: &Path,
    id: &str,
    label: &str,
    reference: &PackageArtifactRef,
) -> Result<(), ContractError> {
    // Shape first, so a malformed digest is `InvalidRecord` even if the file is
    // also missing or escaped.
    validate_digest(&reference.digest, &reference.path)?;
    if reference.media_type.is_empty() {
        return Err(FailureCode::InvalidRecord.failure(
            id,
            format!("{label} media_type must not be empty"),
        ));
    }
    let candidate = relative_join(package_dir, &reference.path, id, label)?;
    let resolved = match fs::canonicalize(&candidate) {
        Ok(path) => path,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Err(FailureCode::InvalidRecord.failure(
                id,
                format!("{label} path {:?} does not exist", reference.path),
            ))
        }
        Err(err) => {
            return Err(FailureCode::StateUnavailable.failure(
                candidate.display().to_string(),
                format!("cannot resolve {label}: {err}"),
            ))
        }
    };
    if !resolved.starts_with(package_dir) {
        return Err(FailureCode::ScopeDenied.failure(
            id,
            format!("{label} path {:?} escapes the package directory", reference.path),
        ));
    }
    let metadata = fs::metadata(&resolved).map_err(|err| {
        FailureCode::StateUnavailable.failure(
            resolved.display().to_string(),
            format!("cannot stat {label}: {err}"),
        )
    })?;
    if !metadata.is_file() {
        return Err(FailureCode::InvalidRecord.failure(
            id,
            format!("{label} path {:?} is not a regular file", reference.path),
        ));
    }
    let bytes = fs::read(&resolved).map_err(|err| {
        FailureCode::StateUnavailable.failure(
            resolved.display().to_string(),
            format!("cannot read {label}: {err}"),
        )
    })?;
    reference.verify(&bytes)?;
    Ok(())
}

/// Join a declared package-relative path onto `package_dir`, refusing an
/// absolute path or any `..` component before touching the filesystem.
fn relative_join(
    package_dir: &Path,
    relative: &str,
    id: &str,
    label: &str,
) -> Result<PathBuf, ContractError> {
    if relative.is_empty() {
        return Err(FailureCode::InvalidRecord.failure(
            id,
            format!("{label} path must not be empty"),
        ));
    }
    let declared = Path::new(relative);
    if declared.is_absolute() || declared.components().any(|c| c == Component::ParentDir) {
        return Err(FailureCode::ScopeDenied.failure(
            id,
            format!("{label} path {relative:?} is not package-relative"),
        ));
    }
    Ok(package_dir.join(declared))
}

/// Validate `plugin_id` against the frozen schema pattern
/// `^[a-zA-Z0-9][a-zA-Z0-9._:/-]{0,255}$`.
fn validate_plugin_id(id: &str) -> Result<(), ContractError> {
    validate_identifier(id, "plugin_id")
}

/// Validate a `view_id` against the frozen schema pattern.
fn validate_view_id(id: &str) -> Result<(), ContractError> {
    validate_identifier(id, "view_id")
}

/// Validate an `action_id` against the frozen schema pattern.
fn validate_action_id(id: &str) -> Result<(), ContractError> {
    validate_identifier(id, "action_id")
}

/// Validate an identity string against `^[a-zA-Z0-9][a-zA-Z0-9._:/-]{0,255}$`.
fn validate_identifier(value: &str, label: &str) -> Result<(), ContractError> {
    let mut chars = value.chars();
    let first_ok = chars.next().is_some_and(|c| c.is_ascii_alphanumeric());
    let rest_ok =
        chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | ':' | '/' | '-'));
    if !first_ok || !rest_ok || value.len() > 256 {
        return Err(FailureCode::InvalidRecord.failure(
            value,
            format!("{label} does not match the frozen PluginManifest/v1 pattern"),
        ));
    }
    Ok(())
}

/// Reject a list over its frozen bound, an empty entry, or a repeated name.
fn require_bounded_unique_nonempty(
    id: &str,
    field: &str,
    names: &[String],
) -> Result<(), ContractError> {
    if names.len() > MAX_DECLARED_ITEMS {
        return Err(bound_failure(id, field));
    }
    let mut seen = BTreeMap::new();
    for name in names {
        if name.is_empty() {
            return Err(FailureCode::InvalidRecord.failure(
                id,
                format!("{field} contains an empty entry"),
            ));
        }
        if seen.insert(name.clone(), ()).is_some() {
            return Err(FailureCode::InvalidRecord.failure(
                id,
                format!("{field} repeats {name:?}"),
            ));
        }
    }
    Ok(())
}

/// Resolve the canonical executable path and refuse an escape.
fn resolve_entrypoint(
    package_dir: &Path,
    manifest: &PluginManifest,
) -> Result<PathBuf, ContractError> {
    let candidate = package_dir.join(&manifest.entrypoint.path);
    let resolved = fs::canonicalize(&candidate).map_err(|err| {
        FailureCode::InvalidRecord.failure(
            &manifest.plugin_id,
            format!("entrypoint {} is unavailable: {err}", manifest.entrypoint.path),
        )
    })?;
    if !resolved.starts_with(package_dir) {
        return Err(FailureCode::ScopeDenied.failure(
            &manifest.plugin_id,
            format!(
                "entrypoint {:?} escapes the package directory",
                manifest.entrypoint.path
            ),
        ));
    }
    let metadata = fs::metadata(&resolved).map_err(|err| {
        FailureCode::StateUnavailable.failure(
            &manifest.plugin_id,
            format!("cannot stat entrypoint: {err}"),
        )
    })?;
    if !metadata.is_file() {
        return Err(FailureCode::InvalidRecord.failure(
            &manifest.plugin_id,
            "entrypoint is not a regular file",
        ));
    }
    Ok(resolved)
}

/// Recompute the sealed-inventory digest described in the module docs.
///
/// The manifest file itself is excluded from the inventory (the canonical
/// manifest, minus its `package_digest`, is bound separately), so the digest is
/// independent of the digest value it records.
pub fn package_inventory_digest(
    package_dir: &Path,
    manifest: &PluginManifest,
) -> Result<String, ContractError> {
    let package_dir = resolve_dir(package_dir, "package directory")?;
    let mut inventory = Vec::new();
    walk_inventory(&package_dir, &package_dir, &mut inventory)?;
    inventory.sort_by(|a, b| a.path.as_bytes().cmp(b.path.as_bytes()));

    let mut manifest_value = serde_json::to_value(manifest).map_err(|err| {
        FailureCode::InvalidRecord.failure(
            &manifest.plugin_id,
            format!("cannot serialize manifest: {err}"),
        )
    })?;
    if let Some(object) = manifest_value.as_object_mut() {
        object.remove("package_digest");
    }
    let entries: Vec<Value> = inventory
        .iter()
        .map(|entry| {
            json!({
                "mode": entry.mode,
                "path": entry.path,
                "sha256": entry.sha256,
            })
        })
        .collect();
    let document = json!({
        "inventory": entries,
        "manifest": manifest_value,
    });
    let text = canonical_json(&document)?;
    Ok(sha256_digest(text.as_bytes()))
}

/// One inventory row: `/`-separated package-relative path, unix mode bits, raw
/// SHA-256 of the file bytes.
struct InventoryEntry {
    path: String,
    mode: u64,
    sha256: String,
}

/// Recursively collect regular files under `dir`, refusing escapes and
/// unsupported entry kinds. The manifest file is skipped.
fn walk_inventory(
    package_dir: &Path,
    dir: &Path,
    out: &mut Vec<InventoryEntry>,
) -> Result<(), ContractError> {
    let entries = fs::read_dir(dir).map_err(|err| {
        FailureCode::StateUnavailable.failure(
            dir.display().to_string(),
            format!("cannot read package directory: {err}"),
        )
    })?;
    let mut entries = entries
        .collect::<Result<Vec<_>, _>>()
        .map_err(|err| {
            FailureCode::StateUnavailable.failure(
                dir.display().to_string(),
                format!("cannot read package directory entry: {err}"),
            )
        })?;
    entries.sort_by_key(|entry| entry.file_name());

    for entry in entries {
        let path = entry.path();
        let file_type = entry.file_type().map_err(|err| {
            FailureCode::StateUnavailable.failure(
                path.display().to_string(),
                format!("cannot stat package entry: {err}"),
            )
        })?;
        if file_type.is_symlink() {
            let resolved = fs::canonicalize(&path).map_err(|err| {
                FailureCode::InvalidRecord.failure(
                    path.display().to_string(),
                    format!("cannot resolve package symlink: {err}"),
                )
            })?;
            if !resolved.starts_with(package_dir) {
                return Err(FailureCode::ScopeDenied.failure(
                    path.display().to_string(),
                    "package symlink escapes the package directory",
                ));
            }
            let target = fs::metadata(&resolved).map_err(|err| {
                FailureCode::StateUnavailable.failure(
                    path.display().to_string(),
                    format!("cannot stat package symlink target: {err}"),
                )
            })?;
            if target.is_dir() {
                return Err(FailureCode::ScopeDenied.failure(
                    path.display().to_string(),
                    "package directory symlink is not permitted (prevents cycles and ancestor escapes)",
                ));
            } else if target.is_file() {
                push_file(package_dir, &resolved, &target, out)?;
            } else {
                return Err(FailureCode::InvalidRecord.failure(
                    path.display().to_string(),
                    "package entry is neither a file nor a directory",
                ));
            }
        } else if file_type.is_dir() {
            walk_inventory(package_dir, &path, out)?;
        } else if file_type.is_file() {
            let metadata = entry.metadata().map_err(|err| {
                FailureCode::StateUnavailable.failure(
                    path.display().to_string(),
                    format!("cannot stat package file: {err}"),
                )
            })?;
            push_file(package_dir, &path, &metadata, out)?;
        } else {
            return Err(FailureCode::InvalidRecord.failure(
                path.display().to_string(),
                "package entry is neither a file nor a directory",
            ));
        }
    }
    Ok(())
}

/// Hash and record `path` unless it is the manifest file.
fn push_file(
    package_dir: &Path,
    path: &Path,
    metadata: &fs::Metadata,
    out: &mut Vec<InventoryEntry>,
) -> Result<(), ContractError> {
    let relative = path.strip_prefix(package_dir).map_err(|_| {
        FailureCode::ScopeDenied.failure(
            path.display().to_string(),
            "package file is outside the package directory",
        )
    })?;
    let relative = slash_path(relative);
    if relative == PLUGIN_MANIFEST_FILE {
        return Ok(());
    }
    let bytes = fs::read(path).map_err(|err| {
        FailureCode::StateUnavailable.failure(
            path.display().to_string(),
            format!("cannot read package file: {err}"),
        )
    })?;
    out.push(InventoryEntry {
        path: relative,
        mode: mode_bits(metadata),
        sha256: sha256_digest(&bytes),
    });
    Ok(())
}

/// Unix permission bits (`0` off-unix, where no mode identity exists).
fn mode_bits(metadata: &fs::Metadata) -> u64 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        (metadata.permissions().mode() & 0o7777) as u64
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        0
    }
}

/// `/`-separated relative path with raw UTF-8 components.
fn slash_path(path: &Path) -> String {
    path.components()
        .map(|component| component.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/")
}

/// Canonicalize a directory, mapping a missing/inaccessible path to a typed
/// failure.
fn resolve_dir(path: &Path, label: &str) -> Result<PathBuf, ContractError> {
    fs::canonicalize(path).map_err(|err| {
        FailureCode::StateUnavailable.failure(
            path.display().to_string(),
            format!("cannot resolve {label}: {err}"),
        )
    })
}

/// Refuse two discovered packages sharing one `plugin_id`, even when identical.
fn refuse_duplicate_ids(packages: &[DiscoveredPackage]) -> Result<(), ContractError> {
    let mut seen: BTreeMap<&str, &Path> = BTreeMap::new();
    for package in packages {
        if let Some(first) = seen.insert(package.plugin_id.as_str(), &package.package_dir) {
            return Err(FailureCode::IdentityConflict.failure(
                &package.plugin_id,
                format!(
                    "duplicate plugin_id discovered at {} and {}",
                    first.display(),
                    package.package_dir.display()
                ),
            ));
        }
    }
    Ok(())
}

/// Refuse a cycle among required service dependencies that name another
/// discovered plugin. A plugin depends on another when the other's `plugin_id`
/// appears in its `required_services`.
fn refuse_dependency_cycles(packages: &[DiscoveredPackage]) -> Result<(), ContractError> {
    let ids: Vec<&str> = packages
        .iter()
        .map(|package| package.plugin_id.as_str())
        .collect();
    let edges: Vec<Vec<usize>> = packages
        .iter()
        .map(|package| {
            package
                .manifest
                .required_services
                .iter()
                .filter_map(|service| ids.iter().position(|id| id == service))
                .collect()
        })
        .collect();

    // Iterative DFS with three colors; `ids` is already sorted by plugin_id, so
    // the reported cycle is deterministic.
    const WHITE: u8 = 0;
    const GREY: u8 = 1;
    const BLACK: u8 = 2;
    let mut color = vec![WHITE; packages.len()];
    for start in 0..packages.len() {
        if color[start] != WHITE {
            continue;
        }
        let mut stack: Vec<(usize, usize)> = vec![(start, 0)];
        color[start] = GREY;
        while let Some((node, next)) = stack.pop() {
            if next < edges[node].len() {
                stack.push((node, next + 1));
                let child = edges[node][next];
                match color[child] {
                    WHITE => {
                        color[child] = GREY;
                        stack.push((child, 0));
                    }
                    GREY => {
                        let mut cycle: Vec<&str> =
                            stack.iter().map(|(index, _)| ids[*index]).collect();
                        cycle.push(ids[child]);
                        return Err(FailureCode::InvalidRecord.failure(
                            ids[start],
                            format!("required dependency cycle: {}", cycle.join(" -> ")),
                        ));
                    }
                    _ => {}
                }
            } else {
                color[node] = BLACK;
            }
        }
    }
    Ok(())
}

/// Verify `manifest.package_digest` shape only (no inventory read).
///
/// Exposed so a caller with a manifest but no package directory (for example a
/// registry read model) can reject a malformed digest before effects.
pub fn validate_package_digest(manifest: &PluginManifest) -> Result<(), ContractError> {
    validate_digest(&manifest.package_digest, &manifest.plugin_id)
}
