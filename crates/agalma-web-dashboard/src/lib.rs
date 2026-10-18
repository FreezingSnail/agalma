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
//! opens no ledger/database and exposes no mutation route beyond the declared
//! factory control routes. The browser read surface is `GET /api/v1/overview`,
//! `GET /api/v1/components`, and — only when the private capability descriptor
//! grants it — the factory `GET /api/v1/factory`, `GET
//! /api/v1/factory/actions/{action_id}` and the narrow `POST
//! /api/v1/factory/pause` / `POST /api/v1/factory/resume` controls. Report-detail
//! and artifact routes remain intentionally absent.
//!
//! Authority: the per-launch grants stay inside this process. A read-only launch
//! mounts no factory route; the operator session/CSRF surface exists only when
//! an actual explicit live `factory.control/v1` binding is present. Viewing is
//! observational and grants no factory control.

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
