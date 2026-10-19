//! UI.1e production package sealing (`agalma seal-package`).
//!
//! Copies a source package plus a built binary into a configured plugin root,
//! pins the manifest's sealed-inventory digest, then verifies the result through
//! the same discovery path the hub uses. Every refusal happens before the
//! destination is touched; a failed verification leaves the sealed bytes and
//! reports a non-zero exit.
//!
//! The source package is validated (manifest present, parseable and
//! version-gated; entrypoint package-relative and safe) before any filesystem
//! effect. Staging happens at `<root>/<plugin_id>.seal-tmp-<pid>`, the source
//! tree is copied except any `bin/` directory, the binary is placed at the
//! manifest entrypoint, and the digest is recomputed over the staged inventory
//! by [`package_inventory_digest`]. Only then is the staged tree swapped into
//! `<root>/<plugin_id>`.

use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::process::ExitCode;

use agalma_contracts::{ContractError, FailureCode, PluginManifest, MANIFEST_DOCUMENT_LIMIT_BYTES};

use crate::cli::SealPackageArgs;
use crate::plugin_manifest::{
    discover_plugins, package_inventory_digest, PluginRoot, PLUGIN_MANIFEST_FILE,
};

/// A successfully sealed package: identity plus the verified digest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SealedPackage {
    pub plugin_id: String,
    pub package_digest: String,
}

/// Run `agalma seal-package`: print `SEALED <plugin_id> <package_digest>` on
/// success, or a non-zero exit with the error on stderr.
pub fn run_seal_package(args: &SealPackageArgs) -> ExitCode {
    match seal_package(&args.package, &args.binary, &args.root) {
        Ok(sealed) => {
            println!("SEALED {} {}", sealed.plugin_id, sealed.package_digest);
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("agalma seal-package: {err}");
            ExitCode::FAILURE
        }
    }
}

/// Seal `package` plus `binary` into `root/<plugin_id>` and verify by discovery.
pub fn seal_package(
    package: &Path,
    binary: &Path,
    root: &Path,
) -> Result<SealedPackage, ContractError> {
    // Source validation: nothing below this block may create or modify the
    // destination.
    let metadata = fs::metadata(package).map_err(|err| match err.kind() {
        io::ErrorKind::NotFound => FailureCode::InvalidRecord.failure(
            package.display().to_string(),
            "package directory does not exist",
        ),
        _ => state_error(package, "stat package directory", err),
    })?;
    if !metadata.is_dir() {
        return Err(FailureCode::InvalidRecord.failure(
            package.display().to_string(),
            "package path is not a directory",
        ));
    }

    let manifest = read_manifest(package)?;
    validate_install_name(&manifest.plugin_id)?;
    validate_entrypoint_path(&manifest.entrypoint.path, &manifest.plugin_id)?;

    let binary_metadata = fs::metadata(binary).map_err(|err| match err.kind() {
        io::ErrorKind::NotFound => FailureCode::InvalidRecord
            .failure(binary.display().to_string(), "binary does not exist"),
        _ => state_error(binary, "stat binary", err),
    })?;
    if !binary_metadata.is_file() {
        return Err(FailureCode::InvalidRecord
            .failure(binary.display().to_string(), "binary is not a regular file"));
    }

    match fs::metadata(root) {
        Ok(metadata) if metadata.is_dir() => {}
        Ok(_) => {
            return Err(FailureCode::InvalidRecord.failure(
                root.display().to_string(),
                "plugin root exists and is not a directory",
            ))
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => {}
        Err(err) => return Err(state_error(root, "stat plugin root", err)),
    }

    fs::create_dir_all(root).map_err(|err| state_error(root, "create plugin root", err))?;

    let staging_path = root.join(format!(
        "{}.seal-tmp-{}",
        manifest.plugin_id,
        std::process::id()
    ));
    // A stale staging tree from an earlier interrupted run must not leak files
    // into this seal.
    remove_path(&staging_path)?;
    let mut staging = Staging::new(staging_path.clone());

    copy_package_tree(package, &staging_path)?;

    let entrypoint = staging_path.join(&manifest.entrypoint.path);
    if let Some(parent) = entrypoint.parent() {
        fs::create_dir_all(parent)
            .map_err(|err| state_error(parent, "create entrypoint directory", err))?;
    }
    fs::copy(binary, &entrypoint)
        .map_err(|err| state_error(&entrypoint, "copy binary to entrypoint", err))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&entrypoint, fs::Permissions::from_mode(0o755))
            .map_err(|err| state_error(&entrypoint, "set entrypoint mode", err))?;
    }

    // The inventory digest binds the canonical manifest excluding
    // `package_digest`, so zero the field first and then write the recomputed
    // value.
    let mut sealed_manifest = manifest.clone();
    sealed_manifest.package_digest = zero_digest();
    write_manifest(&staging_path, &sealed_manifest)?;
    let package_digest = package_inventory_digest(&staging_path, &sealed_manifest)?;
    sealed_manifest.package_digest = package_digest.clone();
    write_manifest(&staging_path, &sealed_manifest)?;

    let final_dir = root.join(&manifest.plugin_id);
    remove_path(&final_dir)?;
    fs::rename(&staging_path, &final_dir)
        .map_err(|err| state_error(&final_dir, "swap staged package into place", err))?;
    staging.disarm();

    verify_sealed(root, &manifest.plugin_id, &package_digest)?;

    Ok(SealedPackage {
        plugin_id: manifest.plugin_id,
        package_digest,
    })
}

/// Read, parse, bound, and version-gate `<package_dir>/plugin-manifest.json`.
fn read_manifest(package_dir: &Path) -> Result<PluginManifest, ContractError> {
    let manifest_path = package_dir.join(PLUGIN_MANIFEST_FILE);
    let bytes = match fs::read(&manifest_path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            return Err(FailureCode::InvalidRecord.failure(
                package_dir.display().to_string(),
                format!("package is missing `{PLUGIN_MANIFEST_FILE}`"),
            ))
        }
        Err(err) => return Err(state_error(&manifest_path, "read package manifest", err)),
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

/// Refuse a `plugin_id` that cannot be used as one package directory name
/// without escaping the configured root.
fn validate_install_name(plugin_id: &str) -> Result<(), ContractError> {
    let declared = Path::new(plugin_id);
    let normal_components = declared
        .components()
        .all(|component| matches!(component, Component::Normal(_)));
    let no_empty_segments = !plugin_id.is_empty() && !plugin_id.split('/').any(str::is_empty);
    if declared.is_absolute() || !normal_components || !no_empty_segments {
        return Err(FailureCode::ScopeDenied.failure(
            plugin_id,
            "plugin_id cannot be used as a package directory name",
        ));
    }
    Ok(())
}

/// Refuse an entrypoint path that is absolute, has an empty segment, or
/// contains a `..` component.
fn validate_entrypoint_path(path: &str, plugin_id: &str) -> Result<(), ContractError> {
    if path.is_empty() {
        return Err(
            FailureCode::InvalidRecord.failure(plugin_id, "entrypoint.path must not be empty")
        );
    }
    let declared = Path::new(path);
    let escapes = declared.is_absolute()
        || declared.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        });
    let empty_segment = path.split('/').any(str::is_empty);
    if escapes || empty_segment {
        return Err(FailureCode::ScopeDenied.failure(
            plugin_id,
            format!("entrypoint.path {path:?} is not a safe package-relative path"),
        ));
    }
    Ok(())
}

/// The 64-zero placeholder digest the manifest carries before sealing.
fn zero_digest() -> String {
    format!("sha256:{}", "0".repeat(64))
}

/// Write the manifest pretty-printed inside `package_dir`.
fn write_manifest(package_dir: &Path, manifest: &PluginManifest) -> Result<(), ContractError> {
    let bytes = serde_json::to_vec_pretty(manifest).map_err(|err| {
        FailureCode::InvalidRecord.failure(
            manifest.plugin_id.as_str(),
            format!("cannot serialize manifest: {err}"),
        )
    })?;
    fs::write(package_dir.join(PLUGIN_MANIFEST_FILE), bytes)
        .map_err(|err| state_error(package_dir, "write sealed manifest", err))
}

/// Recursively copy `source` into `dest`, except any `bin/` directory.
fn copy_package_tree(source: &Path, dest: &Path) -> Result<(), ContractError> {
    let source_root = fs::canonicalize(source)
        .map_err(|err| state_error(source, "resolve package directory", err))?;
    fs::create_dir_all(dest).map_err(|err| state_error(dest, "create staging directory", err))?;
    copy_dir_contents(&source_root, &source_root, dest)
}

/// Copy the regular files and directories of `source` into `dest`.
///
/// A directory named `bin` is skipped at every depth: the entrypoint binary is
/// supplied only by `--binary`. A file symlink is resolved and copied when it
/// stays inside the package; a directory symlink is refused (matching discovery,
/// which forbids them to prevent cycles).
fn copy_dir_contents(source_root: &Path, source: &Path, dest: &Path) -> Result<(), ContractError> {
    let mut entries = fs::read_dir(source)
        .map_err(|err| state_error(source, "read package directory", err))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|err| state_error(source, "read package directory entry", err))?;
    entries.sort_by_key(|entry| entry.file_name());

    for entry in entries {
        let path = entry.path();
        let file_type = entry
            .file_type()
            .map_err(|err| state_error(&path, "stat package entry", err))?;
        let target = dest.join(entry.file_name());

        if entry.file_name().to_str() == Some("bin") {
            let is_directory = file_type.is_dir()
                || (file_type.is_symlink()
                    && fs::metadata(&path).map(|m| m.is_dir()).unwrap_or(false));
            if is_directory {
                continue;
            }
        }

        if file_type.is_symlink() {
            let resolved = fs::canonicalize(&path)
                .map_err(|err| state_error(&path, "resolve package symlink", err))?;
            if !resolved.starts_with(source_root) {
                return Err(FailureCode::ScopeDenied.failure(
                    path.display().to_string(),
                    "package symlink escapes the package directory",
                ));
            }
            let metadata = fs::metadata(&resolved)
                .map_err(|err| state_error(&resolved, "stat package symlink target", err))?;
            if metadata.is_dir() {
                return Err(FailureCode::ScopeDenied.failure(
                    path.display().to_string(),
                    "package directory symlink is not permitted",
                ));
            }
            if !metadata.is_file() {
                return Err(FailureCode::InvalidRecord.failure(
                    path.display().to_string(),
                    "package entry is neither a file nor a directory",
                ));
            }
            fs::copy(&resolved, &target)
                .map_err(|err| state_error(&target, "copy package symlink target", err))?;
        } else if file_type.is_dir() {
            fs::create_dir_all(&target)
                .map_err(|err| state_error(&target, "create package directory", err))?;
            copy_dir_contents(source_root, &path, &target)?;
        } else if file_type.is_file() {
            fs::copy(&path, &target)
                .map_err(|err| state_error(&target, "copy package file", err))?;
        } else {
            return Err(FailureCode::InvalidRecord.failure(
                path.display().to_string(),
                "package entry is neither a file nor a directory",
            ));
        }
    }
    Ok(())
}

/// Remove `path` if present (directory, file, or symlink).
fn remove_path(path: &Path) -> Result<(), ContractError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_dir() => fs::remove_dir_all(path)
            .map_err(|err| state_error(path, "remove existing package", err)),
        Ok(_) => {
            fs::remove_file(path).map_err(|err| state_error(path, "remove existing package", err))
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(state_error(path, "stat existing package", err)),
    }
}

/// Verify the sealed result through the production discovery path: the plugin
/// id must be present and its digest must equal the recomputed one.
fn verify_sealed(root: &Path, plugin_id: &str, expected_digest: &str) -> Result<(), ContractError> {
    let report = discover_plugins(&[PluginRoot::required(root)])?;
    let package = report.package(plugin_id).ok_or_else(|| {
        FailureCode::IntegrityMismatch.failure(
            plugin_id,
            "sealed package was not discovered under the configured root",
        )
    })?;
    if package.package_digest != expected_digest {
        return Err(FailureCode::IntegrityMismatch.failure(
            plugin_id,
            format!(
                "discovered digest {} does not match recomputed inventory digest {expected_digest}",
                package.package_digest
            ),
        ));
    }
    Ok(())
}

/// A staged tree removed on drop unless the swap succeeded.
struct Staging {
    path: PathBuf,
    armed: bool,
}

impl Staging {
    fn new(path: PathBuf) -> Self {
        Self { path, armed: true }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for Staging {
    fn drop(&mut self) {
        if self.armed {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}

/// Map an I/O failure to a typed state failure naming the operation.
fn state_error(path: &Path, action: &str, err: io::Error) -> ContractError {
    FailureCode::StateUnavailable.failure(
        path.display().to_string(),
        format!("cannot {action}: {err}"),
    )
}
