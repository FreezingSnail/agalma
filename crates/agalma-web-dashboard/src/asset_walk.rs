// Deterministic, confined asset walk shared by the build script and tests.
//
// This module is `std`-only so `build.rs` can `include!` it verbatim while the
// library compiles it as a normal module. The walk:
//
// - enumerates every regular file below an asset root recursively;
// - derives one plain absolute request path (`/health.html`) per file;
// - refuses `..`/absolute/non-UTF-8 components, control characters, quotes,
//   backslashes, duplicate request paths, escaping symlinks and directory
//   symlinks;
// - returns the entries sorted by request path, so the compiled inventory is
//   byte-for-byte deterministic for identical committed assets.
//
// A request can therefore only ever address an entry recorded here; there is
// no runtime filesystem lookup and no way for a request path to escape the
// asset root.

use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::path::{Component, Path};

/// Why an asset tree or one of its entries was refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AssetWalkError {
    /// A filesystem read failed.
    Io(String),
    /// A path, link, or component is not a safe asset location.
    UnsafePath(String),
    /// Two source entries map onto the same request path.
    DuplicatePath(String),
}

impl fmt::Display for AssetWalkError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AssetWalkError::Io(detail) => write!(f, "asset I/O failed: {detail}"),
            AssetWalkError::UnsafePath(detail) => write!(f, "unsafe asset path: {detail}"),
            AssetWalkError::DuplicatePath(path) => {
                write!(f, "asset request path {path:?} is declared more than once")
            }
        }
    }
}

impl std::error::Error for AssetWalkError {}

/// One committed asset: its request path, source-relative path, MIME type and
/// exact bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AssetEntry {
    /// Plain absolute request path, for example `/health.html`.
    pub request_path: String,
    /// `/`-separated path relative to the asset root.
    pub relative_path: String,
    /// Declared content type.
    pub content_type: &'static str,
    /// Exact committed bytes.
    pub bytes: Vec<u8>,
}

/// The content type for one asset path, chosen by extension.
pub fn content_type_for(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|extension| extension.to_str())
        .map(|extension| extension.to_ascii_lowercase())
        .as_deref()
    {
        Some("html") | Some("htm") => "text/html; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("js") | Some("mjs") => "text/javascript; charset=utf-8",
        Some("json") => "application/json",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("ico") => "image/x-icon",
        Some("txt") => "text/plain; charset=utf-8",
        Some("wasm") => "application/wasm",
        _ => "application/octet-stream",
    }
}

/// Collect every regular file below `root`, sorted by request path.
pub fn collect_assets(root: &Path) -> Result<Vec<AssetEntry>, AssetWalkError> {
    let resolved_root = fs::canonicalize(root).map_err(|error| {
        AssetWalkError::Io(format!("cannot resolve asset root {}: {error}", root.display()))
    })?;
    let mut entries = BTreeMap::new();
    walk(&resolved_root, &resolved_root, &mut entries)?;
    Ok(entries.into_values().collect())
}

fn walk(
    root: &Path,
    directory: &Path,
    out: &mut BTreeMap<String, AssetEntry>,
) -> Result<(), AssetWalkError> {
    let mut children = fs::read_dir(directory)
        .map_err(|error| {
            AssetWalkError::Io(format!("cannot read asset directory {}: {error}", directory.display()))
        })?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| {
            AssetWalkError::Io(format!("cannot read asset directory entry: {error}"))
        })?;
    children.sort_by_key(|entry| entry.file_name());

    for child in children {
        let path = child.path();
        let file_type = child.file_type().map_err(|error| {
            AssetWalkError::Io(format!("cannot stat asset {}: {error}", path.display()))
        })?;

        if file_type.is_symlink() {
            let resolved = fs::canonicalize(&path).map_err(|error| {
                AssetWalkError::UnsafePath(format!(
                    "asset symlink {} cannot be resolved: {error}",
                    path.display()
                ))
            })?;
            if !resolved.starts_with(root) {
                return Err(AssetWalkError::UnsafePath(format!(
                    "asset symlink {} escapes the asset root",
                    path.display()
                )));
            }
            let target = fs::metadata(&resolved).map_err(|error| {
                AssetWalkError::Io(format!(
                    "cannot stat asset symlink target {}: {error}",
                    resolved.display()
                ))
            })?;
            if target.is_dir() {
                return Err(AssetWalkError::UnsafePath(format!(
                    "directory symlink {} is not permitted in the asset inventory",
                    path.display()
                )));
            }
            if !target.is_file() {
                return Err(AssetWalkError::UnsafePath(format!(
                    "asset symlink {} does not target a regular file",
                    path.display()
                )));
            }
            add_entry(root, &resolved, &path, out)?;
        } else if file_type.is_dir() {
            walk(root, &path, out)?;
        } else if file_type.is_file() {
            add_entry(root, &path, &path, out)?;
        } else {
            return Err(AssetWalkError::UnsafePath(format!(
                "asset entry {} is neither a regular file nor a directory",
                path.display()
            )));
        }
    }
    Ok(())
}

fn add_entry(
    root: &Path,
    source: &Path,
    declared: &Path,
    out: &mut BTreeMap<String, AssetEntry>,
) -> Result<(), AssetWalkError> {
    let relative = declared.strip_prefix(root).map_err(|_| {
        AssetWalkError::UnsafePath(format!(
            "asset {} is outside the asset root",
            declared.display()
        ))
    })?;
    let relative_path = slash_path(relative)?;
    let request_path = request_path(&relative_path)?;
    if out.contains_key(&request_path) {
        return Err(AssetWalkError::DuplicatePath(request_path));
    }
    let bytes = fs::read(source).map_err(|error| {
        AssetWalkError::Io(format!("cannot read asset {}: {error}", source.display()))
    })?;
    out.insert(
        request_path.clone(),
        AssetEntry {
            request_path,
            relative_path,
            content_type: content_type_for(declared),
            bytes,
        },
    );
    Ok(())
}

/// Render a confined relative path as `/`-separated UTF-8 text.
fn slash_path(relative: &Path) -> Result<String, AssetWalkError> {
    let mut parts = Vec::new();
    for component in relative.components() {
        match component {
            Component::Normal(part) => {
                let part = part.to_str().ok_or_else(|| {
                    AssetWalkError::UnsafePath(format!(
                        "asset path {} is not valid UTF-8",
                        relative.display()
                    ))
                })?;
                if part.is_empty()
                    || part == "."
                    || part == ".."
                    || part
                        .bytes()
                        .any(|byte| byte.is_ascii_control() || byte == b'"' || byte == b'\\')
                {
                    return Err(AssetWalkError::UnsafePath(format!(
                        "asset path component {part:?} is not permitted"
                    )));
                }
                parts.push(part.to_string());
            }
            _ => {
                return Err(AssetWalkError::UnsafePath(format!(
                    "asset path {} is not a confined relative path",
                    relative.display()
                )))
            }
        }
    }
    if parts.is_empty() {
        return Err(AssetWalkError::UnsafePath(
            "asset path must not be empty".to_string(),
        ));
    }
    Ok(parts.join("/"))
}

/// Derive the plain absolute request path from a confined relative path.
pub fn request_path(relative_path: &str) -> Result<String, AssetWalkError> {
    if relative_path.is_empty()
        || relative_path.starts_with('/')
        || relative_path.contains('\\')
        || relative_path
            .bytes()
            .any(|byte| byte.is_ascii_control() || byte == b'?' || byte == b'#')
    {
        return Err(AssetWalkError::UnsafePath(format!(
            "relative asset path {relative_path:?} is not permitted"
        )));
    }
    for segment in relative_path.split('/') {
        if segment.is_empty() || segment == "." || segment == ".." {
            return Err(AssetWalkError::UnsafePath(format!(
                "relative asset path {relative_path:?} contains an empty, `.` or `..` segment"
            )));
        }
    }
    Ok(format!("/{relative_path}"))
}
