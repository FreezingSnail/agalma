//! Package configuration (`DashboardConfig/v1`).
//!
//! The dashboard binds loopback only, defaults to an OS-assigned port, and
//! refreshes no faster than once a second. Configuration is a small strict JSON
//! object: unknown keys, malformed values, foreign bind addresses and an
//! out-of-range refresh interval are refused before any socket exists.
//!
//! Production startup resolves the private installed path/digest pin supplied
//! by the host. The argument-based resolver remains for local config parsing;
//! the executable does not use it as a fallback.

use std::fmt;
use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The only accepted bind address (design: loopback `127.0.0.1` only).
pub const LOOPBACK_ADDRESS: &str = "127.0.0.1";
/// Default port: the operating system selects an available ephemeral port.
pub const DEFAULT_PORT: u16 = 0;
/// Default refresh interval in seconds.
pub const DEFAULT_REFRESH_INTERVAL_SECS: u16 = 1;
/// Smallest accepted refresh interval in seconds.
pub const MIN_REFRESH_INTERVAL_SECS: u16 = 1;
/// Largest accepted refresh interval in seconds.
pub const MAX_REFRESH_INTERVAL_SECS: u16 = 60;
/// The packaged default configuration, matching `package/objects/default-config.json`.
pub const DEFAULT_CONFIG_JSON: &str =
    r#"{"address":"127.0.0.1","port":0,"refresh_interval_secs":1}"#;

const KNOWN_KEYS: &[&str] = &["address", "port", "refresh_interval_secs"];

/// One validated dashboard configuration.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DashboardConfig {
    /// Bind address; must be [`LOOPBACK_ADDRESS`].
    #[serde(default = "default_address")]
    pub address: String,
    /// Bind port; `0` asks the operating system for an ephemeral port.
    #[serde(default = "default_port")]
    pub port: u16,
    /// Poll interval in seconds (`1..=60`).
    #[serde(default = "default_refresh_interval")]
    pub refresh_interval_secs: u16,
}

fn default_address() -> String {
    LOOPBACK_ADDRESS.to_string()
}

fn default_port() -> u16 {
    DEFAULT_PORT
}

fn default_refresh_interval() -> u16 {
    DEFAULT_REFRESH_INTERVAL_SECS
}

impl Default for DashboardConfig {
    fn default() -> Self {
        Self {
            address: default_address(),
            port: default_port(),
            refresh_interval_secs: default_refresh_interval(),
        }
    }
}

impl DashboardConfig {
    /// Refuse a foreign address or an out-of-range refresh interval.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.address != LOOPBACK_ADDRESS {
            return Err(ConfigError::ForeignAddress(self.address.clone()));
        }
        if !(MIN_REFRESH_INTERVAL_SECS..=MAX_REFRESH_INTERVAL_SECS)
            .contains(&self.refresh_interval_secs)
        {
            return Err(ConfigError::RefreshIntervalOutOfRange(
                self.refresh_interval_secs,
            ));
        }
        Ok(())
    }

    /// The loopback listener configuration the process binds.
    pub fn listener_config(&self) -> agalma_ipc::ListenerConfig {
        agalma_ipc::ListenerConfig::loopback(self.port)
    }

    /// The canonical configuration value used for the component binding digest.
    pub fn canonical_value(&self) -> Result<Value, ConfigError> {
        serde_json::to_value(self).map_err(|error| ConfigError::Malformed(error.to_string()))
    }
}

/// A refused configuration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConfigError {
    /// The document is not a JSON object of the expected types.
    Malformed(String),
    /// The document carries a key outside `DashboardConfig/v1`.
    UnknownField(String),
    /// The bind address is not the accepted loopback address.
    ForeignAddress(String),
    /// The refresh interval is outside `1..=60`.
    RefreshIntervalOutOfRange(u16),
    /// The private installed configuration path/digest pin is invalid.
    InvalidInstalledPin,
    /// The installed configuration could not be read.
    InstalledConfigUnavailable,
    /// Installed configuration bytes do not match the pinned digest.
    InstalledConfigDigestMismatch,
    /// Installed configuration bytes are not UTF-8.
    MalformedInstalledConfig,
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigError::Malformed(detail) => write!(f, "malformed dashboard configuration: {detail}"),
            ConfigError::UnknownField(field) => {
                write!(f, "unknown dashboard configuration field {field:?}")
            }
            ConfigError::ForeignAddress(address) => write!(
                f,
                "dashboard configuration address {address:?} is not the loopback address {LOOPBACK_ADDRESS}"
            ),
            ConfigError::RefreshIntervalOutOfRange(interval) => write!(
                f,
                "dashboard refresh interval {interval} is outside {MIN_REFRESH_INTERVAL_SECS}..={MAX_REFRESH_INTERVAL_SECS}"
            ),
            ConfigError::InvalidInstalledPin => {
                f.write_str("invalid private installed configuration pin")
            }
            ConfigError::InstalledConfigUnavailable => {
                f.write_str("installed dashboard configuration is unavailable")
            }
            ConfigError::InstalledConfigDigestMismatch => {
                f.write_str("installed dashboard configuration digest mismatch")
            }
            ConfigError::MalformedInstalledConfig => {
                f.write_str("installed dashboard configuration is malformed")
            }
        }
    }
}

impl std::error::Error for ConfigError {}

/// Parse and validate one configuration document.
pub fn parse_config(text: &str) -> Result<DashboardConfig, ConfigError> {
    let value: Value =
        serde_json::from_str(text).map_err(|error| ConfigError::Malformed(error.to_string()))?;
    let object = value
        .as_object()
        .ok_or_else(|| ConfigError::Malformed("configuration must be a JSON object".to_string()))?;
    for key in object.keys() {
        if !KNOWN_KEYS.contains(&key.as_str()) {
            return Err(ConfigError::UnknownField(key.clone()));
        }
    }
    let config: DashboardConfig =
        serde_json::from_value(value).map_err(|error| ConfigError::Malformed(error.to_string()))?;
    config.validate()?;
    Ok(config)
}

/// Read and verify the installed configuration using the private pinned path
/// and exact artifact digest supplied by the supervising hub.
pub fn resolve_installed_config(
    path: &Path,
    expected_digest: &str,
) -> Result<DashboardConfig, ConfigError> {
    if !agalma_contracts::is_sha256(expected_digest) || !path.is_absolute() {
        return Err(ConfigError::InvalidInstalledPin);
    }
    let bytes = std::fs::read(path).map_err(|_| ConfigError::InstalledConfigUnavailable)?;
    if agalma_contracts::sha256_digest(&bytes) != expected_digest {
        return Err(ConfigError::InstalledConfigDigestMismatch);
    }
    let text = std::str::from_utf8(&bytes).map_err(|_| ConfigError::MalformedInstalledConfig)?;
    parse_config(text)
}
