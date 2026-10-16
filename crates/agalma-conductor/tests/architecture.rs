//! Architecture lint: implementation crates must not reference each other.
//!
//! Every crate under `crates/` that is an implementation (`agalma-ledger`,
//! `agalma-execution`, `agalma-workspace`,
//! `agalma-harness-opencode`, `agalma-kernel`) may depend on `agalma-contracts`
//! but never on another implementation crate. Dependency direction is
//! `conductor -> implementations -> contracts`.
//!
//! The scan reads Rust sources under each implementation crate's `src/`, strips
//! comments, and fails if another implementation crate's snake-case name appears
//! as a token. This mirrors the design rule in `docs/m0-skeleton.md`: workspace
//! membership plus a source scan that fails on cross-impl imports.
//!
//! Per `docs/b1-extraction.md` D6, the check is extended to the Cargo.toml edge:
//! no implementation crate may declare another implementation crate as a normal
//! dependency. Dev-/build-dependencies are out of scope (tests legitimately wire
//! implementations together). `agalma-ipc` and `agalma-process` are
//! infrastructure, not implementation crates, so they are deliberately absent
//! from `IMPL_CRATES` and may be depended on by implementations. `agalma-process`
//! is the local process launcher (`docs/sandbox-removal-design.md` §10).
//!
//! A B1.2 temporary carve-out for the harness→confinement edge was removed at
//! B1.3, when that boundary moved behind a typed client; both checks are strict
//! again, and the confinement layer was deleted in P4.
//!
//! IPCX.1 (`agalma-ebg.2`) adds two more checks: the ConnectRPC stack
//! (`connectrpc`/`buffa`) may be a normal dependency only of `agalma-ipc`, and
//! extracted implementation crates must not import conductor internals.
//!
//! IPCX.8 (`agalma-ebg.9`) strengthens the Connect confinement to the full
//! stack (`connectrpc`/`buffa` plus the build-time `connectrpc-build`/
//! `buffa-codegen`), pins that no crate but `agalma-ipc` *uses* the stack in
//! source (not just in the manifest), and pins that no implementation crate
//! declares a Cargo dependency on `agalma-conductor`.

use std::fs;
use std::path::{Path, PathBuf};

/// Implementation crates subject to the isolation rule.
const IMPL_CRATES: &[&str] = &[
    "agalma-ledger",
    "agalma-execution",
    "agalma-workspace",
    "agalma-harness-opencode",
    "agalma-harness-reference",
    "agalma-decision",
    "agalma-taskqueue",
    "agalma-kernel",
];

fn workspace_root() -> PathBuf {
    // CARGO_MANIFEST_DIR = <root>/crates/agalma-conductor
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("conductor must live under <root>/crates/")
        .to_path_buf()
}

fn rust_files(dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    collect_rust_files(dir, &mut files);
    files
}

fn collect_rust_files(dir: &Path, files: &mut Vec<PathBuf>) {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_rust_files(&path, files);
        } else if path.extension().and_then(|ext| ext.to_str()) == Some("rs") {
            files.push(path);
        }
    }
}

/// True when `haystack` contains `ident` as a whole token (not a substring).
fn contains_ident(haystack: &str, ident: &str) -> bool {
    haystack
        .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .any(|token| token == ident)
}

/// A char literal starts at `bytes[i] == '\''`; distinguish it from a lifetime.
fn is_char_literal(bytes: &[u8], i: usize) -> bool {
    if i + 2 >= bytes.len() {
        return false;
    }
    if bytes[i + 1] == b'\\' {
        return i + 3 < bytes.len() && bytes[i + 3] == b'\'';
    }
    bytes[i + 2] == b'\''
}

/// Remove line and block comments and string/char literal contents so that the
/// token scan only sees real identifiers.
fn strip_comments(src: &str) -> String {
    let bytes = src.as_bytes();
    let mut out = String::with_capacity(src.len());
    let mut i = 0usize;
    let mut block_depth = 0usize;
    let mut in_line = false;
    let mut in_string = false;
    let mut in_char = false;
    let mut escaped = false;

    while i < bytes.len() {
        let c = bytes[i];

        if in_line {
            if c == b'\n' {
                in_line = false;
                out.push('\n');
            }
            i += 1;
            continue;
        }

        if block_depth > 0 {
            if c == b'/' && bytes.get(i + 1) == Some(&b'*') {
                block_depth += 1;
                i += 2;
            } else if c == b'*' && bytes.get(i + 1) == Some(&b'/') {
                block_depth -= 1;
                i += 2;
            } else {
                i += 1;
            }
            continue;
        }

        if in_string || in_char {
            let closing = if in_string { b'"' } else { b'\'' };
            out.push(c as char);
            if escaped {
                escaped = false;
            } else if c == b'\\' {
                escaped = true;
            } else if c == closing {
                in_string = false;
                in_char = false;
            }
            i += 1;
            continue;
        }

        if c == b'/' && bytes.get(i + 1) == Some(&b'/') {
            in_line = true;
            i += 2;
        } else if c == b'/' && bytes.get(i + 1) == Some(&b'*') {
            block_depth = 1;
            i += 2;
        } else if c == b'"' {
            in_string = true;
            out.push('"');
            i += 1;
        } else if c == b'\'' && is_char_literal(bytes, i) {
            in_char = true;
            out.push('\'');
            i += 1;
        } else {
            out.push(c as char);
            i += 1;
        }
    }

    out
}

/// Dependency names declared in one `table` (e.g. `[dependencies]`,
/// `[build-dependencies]`) of a Cargo.toml.
///
/// Only the named table is parsed; feature tables and other tables are ignored.
/// Dependency keys are taken verbatim (renames via `package = "..."` are not
/// expected in this workspace).
fn dependency_table_names(manifest: &str, table: &str) -> Vec<String> {
    let header = format!("[{table}]");
    let mut deps = Vec::new();
    let mut in_table = false;
    for raw in manifest.lines() {
        let line = raw.trim();
        if line.starts_with('[') {
            in_table = line == header;
            continue;
        }
        if !in_table || line.is_empty() || line.starts_with('#') {
            continue;
        }
        let key = line
            .split('=')
            .next()
            .unwrap_or("")
            .trim()
            .trim_matches('"');
        if !key.is_empty() {
            deps.push(key.to_string());
        }
    }
    deps
}

/// Normal (`[dependencies]`) dependency names declared in a Cargo.toml.
///
/// `[dev-dependencies]` and `[build-dependencies]` are ignored (tests
/// legitimately wire implementations together; build scripts are not runtime
/// edges). See [`dependency_table_names`].
fn normal_dependency_names(manifest: &str) -> Vec<String> {
    dependency_table_names(manifest, "dependencies")
}

/// Build (`[build-dependencies]`) dependency names declared in a Cargo.toml.
fn build_dependency_names(manifest: &str) -> Vec<String> {
    dependency_table_names(manifest, "build-dependencies")
}

#[test]
fn implementation_crates_do_not_depend_on_each_other_in_cargo_toml() {
    let root = workspace_root();
    let mut violations = Vec::new();

    for crate_name in IMPL_CRATES {
        let manifest = root.join("crates").join(crate_name).join("Cargo.toml");
        let text = fs::read_to_string(&manifest).unwrap_or_default();
        for dep in normal_dependency_names(&text) {
            if IMPL_CRATES.contains(&dep.as_str()) {
                violations.push(format!(
                    "{crate_name} declares a normal dependency on implementation crate {dep}"
                ));
            }
        }
    }

    assert!(
        violations.is_empty(),
        "implementation crates must not depend on each other:\n{}",
        violations.join("\n")
    );
}

#[test]
fn cargo_toml_scan_ignores_dev_dependencies() {
    let manifest = "\
[dependencies]
agalma-contracts = { path = \"../agalma-contracts\" }
agalma-ipc = { path = \"../agalma-ipc\" }

[dev-dependencies]
agalma-ledger = { path = \"../agalma-ledger\" }
";
    let deps = normal_dependency_names(manifest);
    assert_eq!(deps, vec!["agalma-contracts", "agalma-ipc"]);
    assert!(!deps.contains(&"agalma-ledger".to_string()));
}

#[test]
fn implementation_crates_do_not_reference_each_other() {
    let root = workspace_root();
    let mut violations = Vec::new();

    for crate_name in IMPL_CRATES {
        let self_ident = crate_name.replace('-', "_");
        let src = root.join("crates").join(crate_name).join("src");
        for file in rust_files(&src) {
            let text = fs::read_to_string(&file).unwrap_or_default();
            let code = strip_comments(&text);
            for other in IMPL_CRATES {
                if *other == *crate_name {
                    continue;
                }
                let other_ident = other.replace('-', "_");
                if other_ident != self_ident && contains_ident(&code, &other_ident) {
                    violations.push(format!(
                        "{} references {} ({})",
                        file.display(),
                        other,
                        other_ident
                    ));
                }
            }
        }
    }

    assert!(
        violations.is_empty(),
        "cross-implementation references are forbidden:\n{}",
        violations.join("\n")
    );
}

/// The ConnectRPC stack — the client/server crates `connectrpc`/`buffa` and the
/// build-time `connectrpc-build`/`buffa-codegen` — appears in dependency tables
/// (normal *and* build) only in `agalma-ipc` (design D6, strengthened by
/// IPCX.8): implementation crates reach the transport only through the
/// transport-agnostic `agalma-ipc` boundary.
const CONNECT_STACK: &[&str] = &["connectrpc", "buffa", "connectrpc-build", "buffa-codegen"];

#[test]
fn connect_stack_is_confined_to_agalma_ipc() {
    let crates_dir = workspace_root().join("crates");
    let mut violations = Vec::new();

    let entries = fs::read_dir(&crates_dir).expect("crates dir");
    for entry in entries.flatten() {
        let manifest = entry.path().join("Cargo.toml");
        if !manifest.is_file() {
            continue;
        }
        let crate_name = match entry.file_name().to_str() {
            Some(name) => name.to_string(),
            None => continue,
        };
        let text = fs::read_to_string(&manifest).unwrap_or_default();
        for table in ["dependencies", "build-dependencies"] {
            for dep in dependency_table_names(&text, table) {
                if CONNECT_STACK.contains(&dep.as_str()) && crate_name != "agalma-ipc" {
                    violations.push(format!(
                        "{crate_name} declares a {table} entry on the Connect stack ({dep})"
                    ));
                }
            }
        }
    }

    assert!(
        violations.is_empty(),
        "the ConnectRPC stack must stay confined to agalma-ipc:\n{}",
        violations.join("\n")
    );

    // And the infrastructure crate does own it (normal + build).
    let ipc = fs::read_to_string(crates_dir.join("agalma-ipc/Cargo.toml")).unwrap_or_default();
    let normal = normal_dependency_names(&ipc);
    for dep in ["connectrpc", "buffa"] {
        assert!(normal.contains(&dep.to_string()), "agalma-ipc must own {dep}");
    }
    let build = build_dependency_names(&ipc);
    for dep in ["connectrpc-build", "buffa-codegen"] {
        assert!(
            build.contains(&dep.to_string()),
            "agalma-ipc must own build dependency {dep}"
        );
    }
}

/// The Connect stack must not even be *named* in another crate's source: reaching
/// it through `agalma-ipc` means implementations never spell `connectrpc` /
/// `buffa`, so the manifest-only check above is backed by a source scan (IPCX.8).
#[test]
fn connect_stack_source_is_confined_to_agalma_ipc() {
    let crates_dir = workspace_root().join("crates");
    let mut violations = Vec::new();

    let entries = fs::read_dir(&crates_dir).expect("crates dir");
    for entry in entries.flatten() {
        let src = entry.path().join("src");
        if !src.is_dir() {
            continue;
        }
        let crate_name = match entry.file_name().to_str() {
            Some(name) => name.to_string(),
            None => continue,
        };
        if crate_name == "agalma-ipc" {
            continue;
        }
        for file in rust_files(&src) {
            let text = fs::read_to_string(&file).unwrap_or_default();
            let code = strip_comments(&text);
            for dep in CONNECT_STACK {
                let ident = dep.replace('-', "_");
                if contains_ident(&code, dep) || contains_ident(&code, &ident) {
                    violations.push(format!("{} references the Connect stack ({dep})", file.display()));
                }
            }
        }
    }

    assert!(
        violations.is_empty(),
        "only agalma-ipc may name the Connect stack in source:\n{}",
        violations.join("\n")
    );
}

/// Extracted implementation (server) crates must not import conductor
/// internals: the conductor composes them, never the reverse (design D6).
#[test]
fn implementation_crates_do_not_reference_conductor() {
    let root = workspace_root();
    let mut violations = Vec::new();

    for crate_name in IMPL_CRATES {
        let src = root.join("crates").join(crate_name).join("src");
        for file in rust_files(&src) {
            let text = fs::read_to_string(&file).unwrap_or_default();
            let code = strip_comments(&text);
            if contains_ident(&code, "agalma_conductor") {
                violations.push(format!("{} references agalma_conductor", file.display()));
            }
        }
    }

    assert!(
        violations.is_empty(),
        "implementation crates must not import conductor internals:\n{}",
        violations.join("\n")
    );
}

/// Implementation crates must not declare a Cargo dependency on the conductor
/// either: direction is `conductor -> implementations -> contracts` (IPCX.8
/// pins the manifest half of the same rule the source scan enforces).
#[test]
fn implementation_crates_do_not_depend_on_conductor_in_cargo_toml() {
    let root = workspace_root();
    let mut violations = Vec::new();

    for crate_name in IMPL_CRATES {
        let manifest = root.join("crates").join(crate_name).join("Cargo.toml");
        let text = fs::read_to_string(&manifest).unwrap_or_default();
        for table in ["dependencies", "build-dependencies", "dev-dependencies"] {
            if dependency_table_names(&text, table).iter().any(|d| d == "agalma-conductor") {
                violations.push(format!(
                    "{crate_name} declares a {table} dependency on agalma-conductor"
                ));
            }
        }
    }

    assert!(
        violations.is_empty(),
        "implementation crates must not depend on conductor:\n{}",
        violations.join("\n")
    );
}

#[test]
fn build_dependency_parser_reads_only_build_table() {
    let manifest = "\
[dependencies]
agalma-ipc = { path = \"../agalma-ipc\" }

[build-dependencies]
connectrpc-build = { workspace = true }
serde = { workspace = true }
";
    assert_eq!(
        build_dependency_names(manifest),
        vec!["connectrpc-build", "serde"]
    );
    assert_eq!(normal_dependency_names(manifest), vec!["agalma-ipc"]);
}

#[test]
fn comment_stripper_ignores_comments_but_keeps_code() {
    let source = "// agalma_ledger\n/* agalma_execution */ use agalma_kernel;\n";
    let stripped = strip_comments(source);
    assert!(!contains_ident(&stripped, "agalma_ledger"));
    assert!(!contains_ident(&stripped, "agalma_execution"));
    assert!(contains_ident(&stripped, "agalma_kernel"));
}
