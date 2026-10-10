//! Protected-pack integrity: SHA-256 + `MANIFEST.sha256` verification.
//!
//! The recovery fixture pack (`fixtures/recovery-pack/`) is pinned by a
//! `MANIFEST.sha256` listing every pack file (relative path, forward slashes)
//! and its SHA-256 digest. [`verify_pack`] recomputes every hash and fails on
//! any change, addition, or removal. Candidates cannot edit the fixture pack
//! (or this test) without the lint failing.
//!
//! `serde` derive is deliberately avoided so the crate needs no extra
//! dependency; entries are built and parsed with `serde_json::Value`.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

/// Manifest file name inside the pack root (excluded from its own listing).
pub const MANIFEST_FILE: &str = "MANIFEST.sha256";

/// Absolute path to the protected fixture pack.
pub fn pack_root() -> PathBuf {
    // CARGO_MANIFEST_DIR = <root>/crates/agalma-kernel
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("fixtures")
        .join("recovery-pack")
}

// ---------------------------------------------------------------------------
// SHA-256 (self-contained; no external crate is approved for hashing)
// ---------------------------------------------------------------------------

const K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

/// SHA-256 digest of `bytes`.
pub fn sha256(bytes: &[u8]) -> [u8; 32] {
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];

    let bit_len = (bytes.len() as u64).wrapping_mul(8);
    let mut msg = bytes.to_vec();
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_be_bytes());

    for chunk in msg.chunks_exact(64) {
        let mut w = [0u32; 64];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([
                chunk[i * 4],
                chunk[i * 4 + 1],
                chunk[i * 4 + 2],
                chunk[i * 4 + 3],
            ]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }

        let (mut a, mut b, mut c, mut d) = (h[0], h[1], h[2], h[3]);
        let (mut e, mut f, mut g, mut hh) = (h[4], h[5], h[6], h[7]);

        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let t1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);

            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }

        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
        h[5] = h[5].wrapping_add(f);
        h[6] = h[6].wrapping_add(g);
        h[7] = h[7].wrapping_add(hh);
    }

    let mut out = [0u8; 32];
    for (i, word) in h.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&word.to_be_bytes());
    }
    out
}

/// Lowercase hex digest of `bytes`.
pub fn sha256_hex(bytes: &[u8]) -> String {
    let digest = sha256(bytes);
    let mut out = String::with_capacity(64);
    for byte in digest {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

// ---------------------------------------------------------------------------
// Manifest parse / verify
// ---------------------------------------------------------------------------

fn is_hex64(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Parse manifest text into `relative-path -> sha256-hex`.
///
/// Format: one `<64-hex>  <relative-path>` line per file (two spaces), matching
/// `sha256sum` output. Blank lines are ignored. Invalid lines are returned as
/// issues rather than silently skipped.
pub fn parse_manifest(text: &str) -> Result<BTreeMap<String, String>, Vec<String>> {
    let mut entries = BTreeMap::new();
    let mut issues = Vec::new();
    for (n, line) in text.lines().enumerate() {
        let line = line.trim_end();
        if line.trim().is_empty() {
            continue;
        }
        let Some((hash, path)) = line.split_once(char::is_whitespace) else {
            issues.push(format!("line {}: not `<hash>  <path>`: {line:?}", n + 1));
            continue;
        };
        let path = path.trim_start().to_string();
        if !is_hex64(hash) {
            issues.push(format!("line {}: bad sha256 digest: {hash:?}", n + 1));
            continue;
        }
        if path.is_empty() {
            issues.push(format!("line {}: empty path", n + 1));
            continue;
        }
        if path == MANIFEST_FILE {
            issues.push(format!(
                "line {}: manifest must not list itself ({MANIFEST_FILE})",
                n + 1
            ));
            continue;
        }
        if entries
            .insert(path.clone(), hash.to_ascii_lowercase())
            .is_some()
        {
            issues.push(format!("line {}: duplicate path {path:?}", n + 1));
        }
    }
    if issues.is_empty() {
        Ok(entries)
    } else {
        Err(issues)
    }
}

/// Every pack file (relative, forward-slash path), excluding the manifest.
pub fn collect_pack_files(root: &Path) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    walk(root, root, &mut out);
    out
}

fn walk(root: &Path, dir: &Path, out: &mut BTreeSet<String>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk(root, &path, out);
        } else if let Ok(rel) = path.strip_prefix(root) {
            let rel = rel.to_string_lossy().replace('\\', "/");
            if rel != MANIFEST_FILE {
                out.insert(rel);
            }
        }
    }
}

/// Recompute every manifest hash and compare against disk.
///
/// Returns `Err` with one message per problem (changed file, missing file,
/// unlisted/additional file, or a malformed manifest).
pub fn verify_pack(root: &Path) -> Result<(), Vec<String>> {
    let manifest_path = root.join(MANIFEST_FILE);
    let text = match fs::read_to_string(&manifest_path) {
        Ok(text) => text,
        Err(err) => return Err(vec![format!("cannot read {MANIFEST_FILE}: {err}")]),
    };

    let entries = parse_manifest(&text)?;

    let mut issues = Vec::new();
    let on_disk = collect_pack_files(root);

    if entries.is_empty() {
        issues.push(format!("{MANIFEST_FILE} lists no files"));
    }

    for (rel, expected) in &entries {
        let path = root.join(rel);
        match fs::read(&path) {
            Ok(bytes) => {
                let actual = sha256_hex(&bytes);
                if &actual != expected {
                    issues.push(format!(
                        "changed file: {rel} (expected {expected}, got {actual})"
                    ));
                }
            }
            Err(_) => issues.push(format!("missing file listed in manifest: {rel}")),
        }
    }

    for rel in &on_disk {
        if !entries.contains_key(rel) {
            issues.push(format!("unlisted file present (addition): {rel}"));
        }
    }

    if issues.is_empty() {
        Ok(())
    } else {
        Err(issues)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_known_vectors() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            sha256_hex(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
    }

    #[test]
    fn parse_manifest_rejects_malformed_lines() {
        assert!(parse_manifest("*bad*\n").is_err());
        assert!(
            parse_manifest(&format!("{}  a\n{}  a\n", "0".repeat(64), "1".repeat(64))).is_err()
        );
        assert!(parse_manifest(&format!("{}  {}\n", "0".repeat(64), MANIFEST_FILE)).is_err());
    }
}
