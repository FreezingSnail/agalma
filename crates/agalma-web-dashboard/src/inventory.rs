//! Compiled browser-asset inventory (WEB.4).
//!
//! `build.rs` emits [`DASHBOARD_ASSETS`] from the committed `assets/` tree. This
//! module turns that table into the WEB.3 [`AssetInventory`] the transport
//! serves. Requests are answered only from this explicit inventory: the running
//! process performs no filesystem lookup, so no request can escape it.
//!
//! The root request `/` resolves to `index.html` when a later task commits that
//! asset, and otherwise falls back to the package's `health.html`. Adding an
//! asset therefore never requires editing this file or the server.

use agalma_ipc::{AssetInventory, WebHttpError};

include!(concat!(env!("OUT_DIR"), "/dashboard_assets.rs"));

/// The package-level health page request path.
pub const HEALTH_PAGE: &str = "/health.html";
/// The future shell entry point; root prefers it when present.
pub const INDEX_PAGE: &str = "/index.html";

/// Build the explicit inventory from the compiled table.
pub fn asset_inventory() -> Result<AssetInventory, WebHttpError> {
    let mut inventory = AssetInventory::new();
    for (path, content_type, bytes) in DASHBOARD_ASSETS {
        inventory.insert(path, content_type, bytes.to_vec())?;
    }
    Ok(inventory)
}

/// One compiled asset by request path.
pub fn compiled_asset(path: &str) -> Option<(&'static str, &'static [u8])> {
    DASHBOARD_ASSETS
        .iter()
        .find(|(candidate, _, _)| *candidate == path)
        .map(|(_, content_type, bytes)| (*content_type, *bytes))
}

/// Every compiled request path, in the deterministic compiled order.
pub fn compiled_paths() -> impl Iterator<Item = &'static str> {
    DASHBOARD_ASSETS
        .iter()
        .map(|(path, _, _)| *path)
}

/// Whether the compiled inventory contains `path`.
pub fn contains(path: &str) -> bool {
    DASHBOARD_ASSETS
        .iter()
        .any(|(candidate, _, _)| *candidate == path)
}
