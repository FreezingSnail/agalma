//! Deterministic content hashing for operation ids and decisions.
//!
//! FNV-1a over canonical JSON is a *conflict detector* (reusing an id with
//! different contents), not a security primitive; the pinned dependency set has
//! no hash crate.

use serde::Serialize;

/// FNV-1a 64-bit hash.
pub(crate) fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for &byte in bytes {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// `fnv1a64:<hex>` content hash of `value`'s canonical JSON.
pub(crate) fn content_hash<T: Serialize>(value: &T) -> String {
    let bytes = serde_json::to_vec(value).unwrap_or_default();
    format!("fnv1a64:{:016x}", fnv1a64(&bytes))
}
