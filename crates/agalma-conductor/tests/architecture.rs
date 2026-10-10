//! Architecture lint: implementation crates must not reference each other.
//!
//! Every crate under `crates/` that is an implementation (`agalma-ledger`,
//! `agalma-execution`, `agalma-sandbox`, `agalma-workspace`,
//! `agalma-harness-opencode`) may depend on `agalma-contracts` but never on
//! another implementation crate. Dependency direction is
//! `conductor -> implementations -> contracts`.
//!
//! The scan reads Rust sources under each implementation crate's `src/`, strips
//! comments, and fails if another implementation crate's snake-case name appears
//! as a token. This mirrors the design rule in `docs/m0-skeleton.md`: workspace
//! membership plus a source scan that fails on cross-impl imports.

use std::fs;
use std::path::{Path, PathBuf};

/// Implementation crates subject to the isolation rule.
const IMPL_CRATES: &[&str] = &[
    "agalma-ledger",
    "agalma-execution",
    "agalma-sandbox",
    "agalma-workspace",
    "agalma-harness-opencode",
    "agalma-harness-reference",
    "agalma-decision",
    "agalma-taskqueue",
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

#[test]
fn comment_stripper_ignores_comments_but_keeps_code() {
    let source = "// agalma_ledger\n/* agalma_execution */ use agalma_sandbox;\n";
    let stripped = strip_comments(source);
    assert!(!contains_ident(&stripped, "agalma_ledger"));
    assert!(!contains_ident(&stripped, "agalma_execution"));
    assert!(contains_ident(&stripped, "agalma_sandbox"));
}
