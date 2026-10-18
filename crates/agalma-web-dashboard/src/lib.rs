//! Independent local browser-dashboard plugin package (WEB.4 foundation).
//!
//! The package supplies an executable, a sealed manifest/configuration contract
//! under `package/`, committed browser assets compiled into a deterministic
//! inventory, and a common-plugin process that:
//!
//! - speaks `plugin.Describe`/`plugin.Health`/`plugin.Snapshot`;
//! - serves the loopback HTTP read adapter built on the WEB.3
//!   [`agalma_ipc::web_http`] transport;
//! - reads hub projections through the scoped `control-plane.read/v1`
//!   capability with the WEB.3 typed client.
//!
//! Boundaries: this crate depends only on public contracts and IPC. It never
//! imports the conductor, a spoke implementation, or another supervisor; it
//! opens no ledger/database and exposes no mutation route. The browser read
//! surface is `GET /api/v1/overview` and `GET /api/v1/components`; report-detail
//! and artifact routes remain intentionally absent.
//!
//! Authority: the per-launch grant stays inside this process. Viewing is
//! observational and grants no factory control. No POST route, operator
//! session, or effect authority is created here.

pub mod asset_walk;
pub mod config;
pub mod inventory;
pub mod plugin;
pub mod server;

/// Stable plugin identity used by the package manifest, component binding and
/// common-plugin identity.
pub const PLUGIN_ID: &str = "agalma.web-dashboard";
/// Package/implementation version (the workspace package version).
pub const PACKAGE_VERSION: &str = env!("CARGO_PKG_VERSION");
/// The common plugin protocol major this process announces.
pub const PLUGIN_PROTOCOL_MAJOR: u32 = agalma_ipc::PLUGIN_PROTOCOL_MAJOR;
/// The single scoped capability the summary adapter requires.
pub const READ_CAPABILITY: &str = agalma_contracts::CONTROL_PLANE_READ_CAPABILITY;

pub use config::{DashboardConfig, ConfigError};
pub use plugin::{DashboardHandler, RuntimeStatus, StatusSnapshot};
pub use server::{DashboardError, DashboardServer, LaunchInputs};
