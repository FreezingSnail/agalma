//! Shared `OpenCodeHarnessConfig` construction (B1.2).
//!
//! Both the conductor's build/verifier paths and the component server assemble
//! attempt-scoped configuration the same way. Factoring it here keeps the
//! provider-override rule in one place instead of duplicating it across the
//! process boundary.

use std::collections::BTreeMap;
use std::path::PathBuf;

use crate::adapter::{OpenCodeHarnessConfig, DEFAULT_PAID_PROVIDER_ID, DUMMY_PROVIDER_KEY};

/// Inputs to [`build_config`]; the per-slice values both call sites already have.
#[derive(Clone, Debug)]
pub struct OpenCodeConfigInit {
    /// Rendered, fail-closed sandbox profile.
    pub profile_path: PathBuf,
    /// Attempt-owned read/write root.
    pub attempt_dir: PathBuf,
    /// Protected-tests root (readable, not writable).
    pub protected_dir: PathBuf,
    /// Parent-owned Unix-socket directory.
    pub sock_dir: PathBuf,
    /// Resolved model for the session.
    pub model: String,
    /// Parent-owned forward-proxy URL (provider egress).
    pub proxy_url: Option<String>,
    /// Whether the paid provider path is active (points the provider at the
    /// loopback proxy with a dummy key).
    pub paid_proxy: bool,
    /// Extra read-only roots for the confined worker.
    pub extra_ro_roots: Vec<PathBuf>,
    /// Extra read/write roots (at most one honoured), e.g. a target repo the
    /// session edits while its runtime state stays under `attempt_dir`.
    pub extra_rw_roots: Vec<PathBuf>,
    /// Additional child environment (toolchain homes, PATH additions).
    pub extra_env: BTreeMap<String, String>,
}

/// Assemble an [`OpenCodeHarnessConfig`] from explicit inputs.
///
/// The paid-provider override is applied only when the model's provider matches
/// [`DEFAULT_PAID_PROVIDER_ID`]; the real credential never enters the sandbox.
pub fn build_config(init: OpenCodeConfigInit) -> OpenCodeHarnessConfig {
    let OpenCodeConfigInit {
        profile_path,
        attempt_dir,
        protected_dir,
        sock_dir,
        model,
        proxy_url,
        paid_proxy,
        extra_ro_roots,
        extra_rw_roots,
        extra_env,
    } = init;

    let mut config = OpenCodeHarnessConfig::new(profile_path, attempt_dir, protected_dir, sock_dir);
    config.model = Some(model.clone());
    config.proxy_url = proxy_url.clone();
    if paid_proxy {
        if let Some(url) = proxy_url.filter(|url| !url.is_empty()) {
            let provider = model
                .split('/')
                .next()
                .filter(|provider| !provider.is_empty())
                .unwrap_or("");
            if provider == DEFAULT_PAID_PROVIDER_ID {
                config.provider_base_url = Some(url);
                config.provider_id = Some(provider.to_string());
                config.provider_api_key = Some(DUMMY_PROVIDER_KEY.to_string());
            }
        }
    }
    config.extra_ro_roots = extra_ro_roots;
    config.extra_rw_roots = extra_rw_roots;
    config.extra_env = extra_env;
    config
}

#[cfg(test)]
mod tests {
    use super::*;

    fn init(model: &str, paid: bool) -> OpenCodeConfigInit {
        OpenCodeConfigInit {
            profile_path: PathBuf::from("worker.sb"),
            attempt_dir: PathBuf::from("attempt"),
            protected_dir: PathBuf::from("protected"),
            sock_dir: PathBuf::from("sock"),
            model: model.to_string(),
            proxy_url: Some("http://127.0.0.1:41234".to_string()),
            paid_proxy: paid,
            extra_ro_roots: Vec::new(),
            extra_rw_roots: Vec::new(),
            extra_env: BTreeMap::new(),
        }
    }

    #[test]
    fn paid_proxy_override_only_for_paid_provider() {
        let paid = build_config(init("opencode-go/deepseek", true));
        assert_eq!(
            paid.provider_base_url.as_deref(),
            Some("http://127.0.0.1:41234")
        );
        assert_eq!(paid.provider_api_key.as_deref(), Some(DUMMY_PROVIDER_KEY));

        let free = build_config(init("opencode/big-pickle", true));
        assert!(free.provider_base_url.is_none(), "free route keeps tunnel");

        let off = build_config(init("opencode-go/deepseek", false));
        assert!(off.provider_base_url.is_none(), "paid path disabled");
    }
}
