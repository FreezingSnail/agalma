//! Hardcoded-task descriptor parsing (M0.7).
//!
//! The fixture ships `task.yaml` with the four documented keys:
//!
//! ```yaml
//! id: fix-answer
//! acceptance: [cargo test]
//! max_attempts: 2
//! role: builder
//! ```
//!
//! M0 needs no general YAML engine (`serde_yaml` is not an approved crate), so
//! this module hand-rolls a strict reader for exactly those keys. Unknown keys
//! are ignored (forward-compatible); the required keys must be present. The
//! parser is pure and unit-tested.

use std::fmt;
use std::path::Path;

/// Parsed hardcoded-task descriptor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TaskDescriptor {
    /// Fixture task id (becomes the `TaskId`).
    pub id: String,
    /// Acceptance commands, e.g. `["cargo test"]`.
    pub acceptance: Vec<String>,
    /// Bounded build/verify attempts.
    pub max_attempts: u32,
    /// Agent role for the builder session.
    pub role: String,
}

impl TaskDescriptor {
    /// The default descriptor used when a fixture omits `max_attempts`/`role`.
    pub fn defaults_for(id: impl Into<String>) -> Self {
        TaskDescriptor {
            id: id.into(),
            acceptance: vec!["cargo test".to_string()],
            max_attempts: 2,
            role: "builder".to_string(),
        }
    }
}

/// Why a descriptor could not be parsed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TaskParseError(pub String);

impl fmt::Display for TaskParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "task descriptor: {}", self.0)
    }
}

impl std::error::Error for TaskParseError {}

/// Parse the fixture descriptor text.
pub fn parse_task_descriptor(text: &str) -> Result<TaskDescriptor, TaskParseError> {
    let mut id: Option<String> = None;
    let mut acceptance: Option<Vec<String>> = None;
    let mut max_attempts: Option<u32> = None;
    let mut role: Option<String> = None;

    for (line_no, raw) in text.lines().enumerate() {
        let line = strip_comment(raw).trim();
        if line.is_empty() {
            continue;
        }
        let (key, value) = line.split_once(':').ok_or_else(|| {
            TaskParseError(format!("line {}: missing ':' in {line:?}", line_no + 1))
        })?;
        let key = key.trim();
        let value = value.trim();
        match key {
            "id" => id = Some(unquote(value).to_string()),
            "acceptance" => acceptance = Some(parse_list(value)?),
            "max_attempts" => {
                max_attempts = Some(
                    value
                        .parse::<u32>()
                        .map_err(|e| TaskParseError(format!("max_attempts {value:?}: {e}")))?,
                )
            }
            "role" => role = Some(unquote(value).to_string()),
            // Unknown keys are ignored so the descriptor can grow without
            // breaking M0.
            _ => {}
        }
    }

    let id = id.ok_or_else(|| TaskParseError("missing required key `id`".to_string()))?;
    let acceptance = acceptance
        .ok_or_else(|| TaskParseError("missing required key `acceptance`".to_string()))?;
    if acceptance.is_empty() {
        return Err(TaskParseError(
            "`acceptance` must list at least one command".to_string(),
        ));
    }
    Ok(TaskDescriptor {
        id,
        acceptance,
        max_attempts: max_attempts.unwrap_or(2),
        role: role.unwrap_or_else(|| "builder".to_string()),
    })
}

/// Load `task.yaml` from a fixture directory.
pub fn load_task_descriptor(fixture: &Path) -> Result<TaskDescriptor, TaskParseError> {
    let path = fixture.join("task.yaml");
    let text = std::fs::read_to_string(&path)
        .map_err(|e| TaskParseError(format!("cannot read {}: {e}", path.display())))?;
    parse_task_descriptor(&text)
}

/// Remove a trailing `#` comment (outside quotes).
fn strip_comment(line: &str) -> &str {
    let mut in_single = false;
    let mut in_double = false;
    for (idx, c) in line.char_indices() {
        match c {
            '\'' if !in_double => in_single = !in_single,
            '"' if !in_single => in_double = !in_double,
            '#' if !in_single && !in_double => return &line[..idx],
            _ => {}
        }
    }
    line
}

/// Strip one layer of matching single or double quotes.
fn unquote(value: &str) -> &str {
    let bytes = value.as_bytes();
    if bytes.len() >= 2 {
        let first = bytes[0];
        let last = bytes[bytes.len() - 1];
        if (first == b'"' && last == b'"') || (first == b'\'' && last == b'\'') {
            return &value[1..value.len() - 1];
        }
    }
    value
}

/// Parse a flow-style list `[a, b, c]`; also accept a single bare scalar.
fn parse_list(value: &str) -> Result<Vec<String>, TaskParseError> {
    let inner = value
        .strip_prefix('[')
        .and_then(|v| v.strip_suffix(']'))
        .ok_or_else(|| TaskParseError(format!("acceptance must be a `[...]` list: {value:?}")))?;
    let items: Vec<String> = inner
        .split(',')
        .map(|item| unquote(item.trim()).to_string())
        .filter(|item| !item.is_empty())
        .collect();
    Ok(items)
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str =
        "id: fix-answer\nacceptance: [cargo test]\nmax_attempts: 2\nrole: builder\n";

    #[test]
    fn parses_the_documented_descriptor() {
        let descriptor = parse_task_descriptor(FIXTURE).expect("parse");
        assert_eq!(
            descriptor,
            TaskDescriptor {
                id: "fix-answer".to_string(),
                acceptance: vec!["cargo test".to_string()],
                max_attempts: 2,
                role: "builder".to_string(),
            }
        );
    }

    #[test]
    fn parses_multiple_acceptance_commands_and_quotes() {
        let text = "id: \"fix-answer\"\nacceptance: ['cargo test', 'cargo fmt --check']\n";
        let descriptor = parse_task_descriptor(text).expect("parse");
        assert_eq!(descriptor.id, "fix-answer");
        assert_eq!(
            descriptor.acceptance,
            vec!["cargo test".to_string(), "cargo fmt --check".to_string()]
        );
    }

    #[test]
    fn ignores_comments_and_unknown_keys_and_defaults_optionals() {
        let text =
            "# fixture task\nid: fix-answer\nacceptance: [cargo test]  # red seed\nfuture_key: 7\n";
        let descriptor = parse_task_descriptor(text).expect("parse");
        assert_eq!(descriptor.max_attempts, 2);
        assert_eq!(descriptor.role, "builder");
    }

    #[test]
    fn missing_or_empty_acceptance_is_rejected() {
        assert!(parse_task_descriptor("id: x\n").is_err());
        assert!(parse_task_descriptor("id: x\nacceptance: []\n").is_err());
    }

    #[test]
    fn bad_max_attempts_is_rejected() {
        let err = parse_task_descriptor("id: x\nacceptance: [cargo test]\nmax_attempts: nope\n")
            .expect_err("reject");
        assert!(err.to_string().contains("max_attempts"));
    }

    #[test]
    fn loads_the_real_fixture() {
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("fixtures")
            .join("target-template");
        let descriptor = load_task_descriptor(&fixture).expect("load fixture");
        assert_eq!(descriptor.id, "fix-answer");
        assert_eq!(descriptor.acceptance, vec!["cargo test".to_string()]);
        assert_eq!(descriptor.max_attempts, 2);
        assert_eq!(descriptor.role, "builder");
    }
}
