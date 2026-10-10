//! Component-server launch specs (decision D2).
//!
//! Each implementation crate exports a `server_command()` constructor returning
//! one of these. The conductor resolves [`CommandSpec::program`] relative to its
//! own executable (the server binaries are siblings in the Cargo target
//! directory) and spawns it as a supervised child process. This is registry
//! data, not policy branching on vendor names.

use std::collections::BTreeMap;

/// Launch spec for one component server binary.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandSpec {
    /// Binary name (e.g. `agalma-harness-reference-server`). Resolved to a path
    /// by the caller relative to the conductor executable.
    pub program: String,
    /// Positional arguments (e.g. `--socket <path>`).
    pub args: Vec<String>,
    /// Environment overrides merged over the inherited environment.
    pub env: BTreeMap<String, String>,
}

impl CommandSpec {
    /// Start a spec for `program` with no args or env.
    pub fn new(program: impl Into<String>) -> Self {
        CommandSpec {
            program: program.into(),
            args: Vec::new(),
            env: BTreeMap::new(),
        }
    }

    /// Append one positional argument.
    pub fn arg(mut self, arg: impl Into<String>) -> Self {
        self.args.push(arg.into());
        self
    }

    /// Append several positional arguments.
    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }

    /// Set one environment override.
    pub fn env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.insert(key.into(), value.into());
        self
    }
}
