//! Conductor configuration: state directory and model resolution.

use std::path::{Path, PathBuf};

use crate::cli::RunArgs;

/// Default model identifier (cheap free model used in S0d).
pub const DEFAULT_MODEL: &str = "opencode/mimo-v2.6-flash-free";

/// The M0 phase plan, in order.
pub const PHASE_PLAN: [&str; 6] = [
    "intake",
    "checkout",
    "build",
    "verify",
    "integrate",
    "done | parked",
];

/// Resolved configuration for one `run` invocation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    pub fixture: PathBuf,
    pub state_dir: PathBuf,
    pub model: String,
}

impl Config {
    /// Resolve configuration from CLI arguments, environment, and defaults.
    ///
    /// Precedence: explicit flag > environment variable > default.
    pub fn resolve(args: &RunArgs) -> Result<Self, String> {
        Ok(Config {
            fixture: args.fixture.clone(),
            state_dir: resolve_state_dir(args.state_dir.as_deref())?,
            model: resolve_model(args.model.as_deref()),
        })
    }
}

/// Resolve the state directory.
///
/// Precedence: `--state-dir` > `AGALMA_STATE_DIR` > `~/Library/Application
/// Support/agalma`.
pub fn resolve_state_dir(explicit: Option<&Path>) -> Result<PathBuf, String> {
    if let Some(dir) = explicit {
        return Ok(dir.to_path_buf());
    }
    if let Some(dir) = std::env::var_os("AGALMA_STATE_DIR") {
        if !dir.is_empty() {
            return Ok(PathBuf::from(dir));
        }
    }
    let home = std::env::var_os("HOME")
        .ok_or_else(|| "HOME is not set; pass --state-dir or set AGALMA_STATE_DIR".to_string())?;
    Ok(PathBuf::from(home)
        .join("Library")
        .join("Application Support")
        .join("agalma"))
}

/// Resolve the model identifier.
///
/// Precedence: `--model` > `AGALMA_MODEL` > [`DEFAULT_MODEL`].
pub fn resolve_model(explicit: Option<&str>) -> String {
    if let Some(model) = explicit {
        if !model.is_empty() {
            return model.to_string();
        }
    }
    if let Ok(model) = std::env::var("AGALMA_MODEL") {
        if !model.is_empty() {
            return model;
        }
    }
    DEFAULT_MODEL.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_state_dir_wins() {
        let dir = resolve_state_dir(Some(Path::new("/var/tmp/agalma-state"))).unwrap();
        assert_eq!(dir, PathBuf::from("/var/tmp/agalma-state"));
    }

    #[test]
    fn explicit_model_wins() {
        assert_eq!(resolve_model(Some("custom/model")), "custom/model");
    }

    #[test]
    fn empty_model_falls_back_to_default() {
        // An empty flag value must not shadow the default.
        assert_eq!(resolve_model(Some("")), resolve_model(None),);
    }
}
