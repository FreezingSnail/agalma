//! OpenCode provider credential handling for the paid proxy (`agalma-csw.2`).
//!
//! The token is read by the **parent** process (the conductor/proxy) from the
//! OpenCode auth store and injected into the upstream request by
//! [`crate::provider_proxy`]. It never reaches the sandbox, is never logged,
//! and is never written to the ledger or a digest. Only a **non-cryptographic**
//! fingerprint, the provider host, and a timestamp are audited as a
//! `provider_auth` execution event.
//!
//! Missing file/token disables the paid path; the reason is recorded with the
//! audit event and surfaced on the route decision.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

use agalma_contracts::{CommitBatch, ContractError, ExecutionEvent, ExecutionId, LedgerApi};
use agalma_ledger::SqliteLedger;
use serde_json::json;

/// Environment override for the OpenCode auth file path (tests, non-standard
/// installs). Defaults to `$XDG_DATA_HOME/opencode/auth.json` or
/// `$HOME/.local/share/opencode/auth.json`.
pub const AUTH_PATH_ENV: &str = "AGALMA_OPENCODE_AUTH";

/// Environment override for the paid upstream base URL.
pub const PROVIDER_BASE_ENV: &str = "AGALMA_PROVIDER_BASE";

/// Default paid upstream base: OpenCode Go (subscription) chat endpoint.
pub const DEFAULT_PROVIDER_BASE: &str = "https://opencode.ai/zen/go/v1";

/// Synthetic execution id for the global (execution-less) `provider_auth` audit
/// event. The ledger accepts events without a state row; this keeps the audit
/// append-only without inventing a task.
pub const PROVIDER_AUTH_EXECUTION: &str = "exec:provider-auth";

/// Environment flag unlocking paid routes (mirrors `routing::ALLOW_PAID_ENV`).
pub const ALLOW_PAID_ENV: &str = "AGALMA_ALLOW_PAID";

/// Resolve the OpenCode auth store path.
pub fn default_auth_path() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os(AUTH_PATH_ENV) {
        if !path.is_empty() {
            return Some(PathBuf::from(path));
        }
    }
    if let Some(xdg) = std::env::var_os("XDG_DATA_HOME") {
        if !xdg.is_empty() {
            return Some(PathBuf::from(xdg).join("opencode").join("auth.json"));
        }
    }
    std::env::var_os("HOME").map(|home| {
        PathBuf::from(home)
            .join(".local")
            .join("share")
            .join("opencode")
            .join("auth.json")
    })
}

/// A stable, non-cryptographic fingerprint of a token (`DefaultHasher`, 16 hex
/// digits). Deliberately weak: it must not allow recovering the secret and must
/// not be treated as a credential. Enough to correlate audit rows for the same
/// token across runs.
pub fn fingerprint(token: &str) -> String {
    let mut hasher = DefaultHasher::new();
    token.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

/// Read the OpenCode bearer token from `path`.
///
/// Accepts the auth store shape `{ "<provider>": { "type": "api", "key": "…" } }`.
/// Prefers `opencode-go`, then `opencode`, then any top-level entry carrying a
/// non-empty string `key`. The returned string is the secret: callers must not
/// log or persist it.
pub fn read_token(path: &Path) -> Result<String, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("read auth store {}: {e}", path.display()))?;
    let value: serde_json::Value = serde_json::from_str(&text)
        .map_err(|e| format!("parse auth store {}: {e}", path.display()))?;
    let obj = value
        .as_object()
        .ok_or_else(|| "auth store is not a JSON object".to_string())?;
    let mut order: Vec<&String> = Vec::new();
    for preferred in ["opencode-go", "opencode"] {
        if let Some(key) = obj.get_key_value(preferred) {
            order.push(key.0);
        }
    }
    for key in obj.keys() {
        if !order.contains(&key) {
            order.push(key);
        }
    }
    for provider in order {
        if let Some(key) = obj
            .get(provider)
            .and_then(|v| v.get("key"))
            .and_then(|v| v.as_str())
        {
            if !key.is_empty() {
                return Ok(key.to_string());
            }
        }
    }
    Err("no API key found in auth store".to_string())
}

/// Audit state for the paid provider credential. Recorded as a `provider_auth`
/// event; never contains the token.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthState {
    /// Whether a token was found and the paid path is enabled.
    pub present: bool,
    /// Non-cryptographic token fingerprint (hex); `None` when absent.
    pub fingerprint: Option<String>,
    /// Upstream provider host the paid route targets (e.g. `opencode.ai`).
    pub host: String,
    /// Why the paid path is disabled, when it is.
    pub reason: Option<String>,
    /// When the credential was read (unix ms).
    pub recorded_at_unix_ms: u64,
}

impl AuthState {
    /// Paid path enabled for `host` with `token`.
    pub fn enabled(host: impl Into<String>, token: &str) -> Self {
        AuthState {
            present: true,
            fingerprint: Some(fingerprint(token)),
            host: host.into(),
            reason: None,
            recorded_at_unix_ms: now_ms(),
        }
    }

    /// Paid path disabled for `host`, with a reason.
    pub fn disabled(host: impl Into<String>, reason: impl Into<String>) -> Self {
        AuthState {
            present: false,
            fingerprint: None,
            host: host.into(),
            reason: Some(reason.into()),
            recorded_at_unix_ms: now_ms(),
        }
    }

    /// Load the credential from `auth_path` (or the default path) for `host`.
    pub fn load(auth_path: Option<&Path>, host: &str) -> Self {
        let path = match auth_path {
            Some(path) => Some(path.to_path_buf()),
            None => default_auth_path(),
        };
        let Some(path) = path else {
            return AuthState::disabled(host, "no HOME/XDG_DATA_HOME to locate auth store");
        };
        match read_token(&path) {
            Ok(token) => AuthState::enabled(host, &token),
            Err(err) => AuthState::disabled(host, err),
        }
    }

    /// The audit payload (token-free).
    pub fn payload(&self) -> serde_json::Value {
        json!({
            "present": self.present,
            "fingerprint": self.fingerprint,
            "host": self.host,
            "reason": self.reason,
            "recorded_at_unix_ms": self.recorded_at_unix_ms,
        })
    }
}

/// Persist a `provider_auth` audit event to the ledger (append-only). Contains
/// the fingerprint, host, timestamp, and (when disabled) the reason — never the
/// token.
pub fn record_audit(state_dir: &Path, audit: &AuthState) -> Result<(), ContractError> {
    let mut ledger = SqliteLedger::open(state_dir.join("ledger.sqlite"))?;
    let event = ExecutionEvent {
        execution_id: ExecutionId::new(PROVIDER_AUTH_EXECUTION),
        sequence: 0,
        kind: "provider_auth".to_string(),
        payload: audit.payload(),
        recorded_at_unix_ms: audit.recorded_at_unix_ms,
    };
    ledger.commit(CommitBatch {
        expected_revision: None,
        state: None,
        events: vec![event],
        receipts: vec![],
        intents: vec![],
    })?;
    Ok(())
}

/// Extract the host from a URL (`https://host:port/path` → `host`).
pub fn host_of(base_url: &str) -> String {
    let after_scheme = base_url
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(base_url);
    let authority = after_scheme
        .split(['/', '?', '#'])
        .next()
        .unwrap_or(after_scheme);
    authority
        .rsplit_once(':')
        .filter(|(host, port)| !host.is_empty() && port.chars().all(|c| c.is_ascii_digit()))
        .map(|(host, _)| host.to_string())
        .unwrap_or_else(|| authority.to_string())
}

fn now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_auth(name: &str, body: &str) -> PathBuf {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("target")
            .join("test-runs")
            .join(format!("provider-auth-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let path = dir.join("auth.json");
        std::fs::write(&path, body).expect("write auth");
        path
    }

    #[test]
    fn reads_preferred_provider_key() {
        let path = temp_auth(
            "preferred",
            r#"{"opencode":{"type":"api","key":"zen-token"},"opencode-go":{"type":"api","key":"go-token"}}"#,
        );
        assert_eq!(read_token(&path).unwrap(), "go-token");
    }

    #[test]
    fn falls_back_to_any_entry_with_a_key() {
        let path = temp_auth(
            "fallback",
            r#"{"other":{"type":"api","key":"other-token"}}"#,
        );
        assert_eq!(read_token(&path).unwrap(), "other-token");
    }

    #[test]
    fn missing_token_is_an_error() {
        let path = temp_auth("missing", r#"{"opencode-go":{"type":"api"}}"#);
        assert!(read_token(&path).is_err());
    }

    #[test]
    fn fingerprint_is_stable_distinct_and_not_the_token() {
        let a = fingerprint("secret-token");
        assert_eq!(a, fingerprint("secret-token"));
        assert_ne!(a, fingerprint("other-token"));
        assert_eq!(a.len(), 16);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert!(!a.contains("secret"));
    }

    #[test]
    fn auth_state_payload_never_contains_the_token() {
        let state = AuthState::enabled("opencode.ai", "super-secret");
        let text = serde_json::to_string(&state.payload()).unwrap();
        assert!(!text.contains("super-secret"), "{text}");
        assert!(text.contains("opencode.ai"));
        assert!(state.present);
    }

    #[test]
    fn audit_event_is_recorded_without_the_token() {
        use agalma_contracts::LedgerApi;
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("target")
            .join("test-runs")
            .join(format!("provider-auth-audit-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir");
        let state = AuthState::enabled("opencode.ai", "audit-secret-token");
        record_audit(&dir, &state).expect("record audit");
        let ledger = SqliteLedger::open(dir.join("ledger.sqlite")).expect("open ledger");
        let events = ledger
            .events(&ExecutionId::new(PROVIDER_AUTH_EXECUTION))
            .expect("events");
        let event = events
            .iter()
            .find(|e| e.kind == "provider_auth")
            .expect("provider_auth event");
        assert_eq!(event.payload["present"], true);
        assert_eq!(event.payload["host"], "opencode.ai");
        assert!(event.payload["fingerprint"].is_string());
        assert!(
            !serde_json::to_string(&event.payload)
                .unwrap()
                .contains("audit-secret-token"),
            "token must not be persisted"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn host_of_parses_scheme_and_port() {
        assert_eq!(host_of("https://opencode.ai/zen/go/v1"), "opencode.ai");
        assert_eq!(host_of("http://127.0.0.1:8080/v1"), "127.0.0.1");
        assert_eq!(host_of("opencode.ai"), "opencode.ai");
    }
}
