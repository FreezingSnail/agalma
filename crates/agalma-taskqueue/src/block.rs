//! Strict fenced-block task parser.
//!
//! A task description carries a single ```` ```yaml ```` fence whose body is a
//! hand-rolled strict subset of YAML: scalar `key: value` pairs plus a string
//! list for `acceptance`. No YAML crate is used (none is approved); the grammar
//! is exactly the keys documented in `docs/m1-workloop.md`:
//!
//! ```yaml
//! directive: directive/v0
//! target: agalma
//! ref: main
//! acceptance:
//!   - sh -c 'true'
//! budget_usd: 2.00
//! priority: normal
//! family: fix
//! max_attempts: 2
//! ```
//!
//! Outcomes:
//! - no ```` ```yaml ```` fence → `Ok(None)`; the issue is simply not a task
//!   (the adapter counts it in the reconcile report);
//! - a malformed block → `Err(`[`BlockError`]`)` carrying the offending
//!   description line number and a message.
//!
//! Unknown keys, duplicate keys, out-of-set `priority`/`family`, non-positive
//! `max_attempts`, and an empty `acceptance` list are all malformed.

use std::fmt;

use agalma_contracts::{TaskFamily, TaskPriority};

/// The opening fence that marks a task block.
const OPEN_FENCE: &str = "```yaml";
/// The closing fence.
const CLOSE_FENCE: &str = "```";

/// Parsed task block (before issue identity is attached).
#[derive(Clone, Debug, PartialEq)]
pub struct TaskBlock {
    pub directive: String,
    pub target: String,
    pub base_ref: String,
    pub acceptance: Vec<String>,
    pub budget_usd: Option<f64>,
    pub priority: TaskPriority,
    pub family: TaskFamily,
    pub max_attempts: u32,
    /// The raw block body exactly as authored, without the fence lines.
    pub raw_block: String,
}

/// A malformed task block, located by 1-based line number in the description.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlockError {
    /// 1-based line number in the issue description.
    pub line: usize,
    /// What went wrong.
    pub message: String,
}

impl fmt::Display for BlockError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "task block line {}: {}", self.line, self.message)
    }
}

impl std::error::Error for BlockError {}

/// Extract and parse the fenced task block from an issue description.
///
/// Returns `Ok(None)` when there is no ```` ```yaml ```` fence, `Ok(Some)` when
/// a block parsed fully, and `Err` for a present-but-malformed block.
pub fn parse_task_block(description: &str) -> Result<Option<TaskBlock>, BlockError> {
    let lines: Vec<&str> = description.lines().collect();
    let Some(open) = lines.iter().position(|l| l.trim() == OPEN_FENCE) else {
        return Ok(None);
    };
    let body_start = open + 1;
    let Some(close) = lines
        .iter()
        .enumerate()
        .skip(body_start)
        .find_map(|(i, l)| (l.trim() == CLOSE_FENCE).then_some(i))
    else {
        return Err(BlockError {
            line: open + 1,
            message: format!("unterminated `{OPEN_FENCE}` fence"),
        });
    };

    let body = &lines[body_start..close];
    // 1-based line number of the first body line.
    let first_line_no = body_start + 1;
    let parsed = parse_body(body, first_line_no)?;

    Ok(Some(TaskBlock {
        directive: parsed.directive,
        target: parsed.target,
        base_ref: parsed.base_ref,
        acceptance: parsed.acceptance,
        budget_usd: parsed.budget_usd,
        priority: parsed.priority,
        family: parsed.family,
        max_attempts: parsed.max_attempts,
        raw_block: body.join("\n"),
    }))
}

/// Fully-resolved block fields.
struct Parsed {
    directive: String,
    target: String,
    base_ref: String,
    acceptance: Vec<String>,
    budget_usd: Option<f64>,
    priority: TaskPriority,
    family: TaskFamily,
    max_attempts: u32,
}

fn parse_body(body: &[&str], first_line_no: usize) -> Result<Parsed, BlockError> {
    let mut directive: Option<String> = None;
    let mut target: Option<String> = None;
    let mut base_ref: Option<String> = None;
    let mut acceptance: Vec<String> = Vec::new();
    let mut acceptance_seen = false;
    let mut budget_usd: Option<f64> = None;
    let mut priority: Option<TaskPriority> = None;
    let mut family: Option<TaskFamily> = None;
    let mut max_attempts: Option<u32> = None;
    let mut seen: Vec<&str> = Vec::new();
    let mut in_acceptance = false;

    for (i, raw) in body.iter().enumerate() {
        let line_no = first_line_no + i;
        let trimmed = raw.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }

        if let Some(item) = trimmed.strip_prefix("- ") {
            if !in_acceptance {
                return Err(err(
                    line_no,
                    format!("list item outside `acceptance`: {trimmed:?}"),
                ));
            }
            let item = unquote(item.trim());
            if item.is_empty() {
                return Err(err(line_no, "empty acceptance item"));
            }
            acceptance.push(item.to_string());
            continue;
        }
        if trimmed == "-" {
            return Err(err(line_no, "empty acceptance item"));
        }

        // Any non-item line ends the acceptance list.
        in_acceptance = false;

        let Some((key, value)) = trimmed.split_once(':') else {
            return Err(err(
                line_no,
                format!("expected `key: value`, found {trimmed:?}"),
            ));
        };
        let key = key.trim();
        let value = value.trim();
        if seen.contains(&key) {
            return Err(err(line_no, format!("duplicate key `{key}`")));
        }

        match key {
            "directive" => {
                directive = Some(scalar(key, value, line_no)?);
                seen.push(key);
            }
            "target" => {
                target = Some(scalar(key, value, line_no)?);
                seen.push(key);
            }
            "ref" => {
                base_ref = Some(scalar(key, value, line_no)?);
                seen.push(key);
            }
            "acceptance" => {
                acceptance_seen = true;
                seen.push(key);
                if value.is_empty() {
                    in_acceptance = true;
                } else {
                    acceptance = parse_inline_list(value).map_err(|m| err(line_no, m))?;
                }
            }
            "budget_usd" => {
                let v = value.parse::<f64>().map_err(|e| {
                    err(
                        line_no,
                        format!("budget_usd must be a number, got {value:?}: {e}"),
                    )
                })?;
                if !v.is_finite() || v < 0.0 {
                    return Err(err(
                        line_no,
                        format!("budget_usd must be a non-negative finite number, got {value:?}"),
                    ));
                }
                budget_usd = Some(v);
                seen.push(key);
            }
            "priority" => {
                priority = Some(TaskPriority::parse(value).ok_or_else(|| {
                    err(
                        line_no,
                        format!(
                            "priority {value:?} not one of {}",
                            allowed(&TaskPriority::ALLOWED.map(TaskPriority::as_str))
                        ),
                    )
                })?);
                seen.push(key);
            }
            "family" => {
                family = Some(TaskFamily::parse(value).ok_or_else(|| {
                    err(
                        line_no,
                        format!(
                            "family {value:?} not one of {}",
                            allowed(&TaskFamily::ALLOWED.map(TaskFamily::as_str))
                        ),
                    )
                })?);
                seen.push(key);
            }
            "max_attempts" => {
                let v = value.parse::<u32>().map_err(|e| {
                    err(
                        line_no,
                        format!("max_attempts must be an integer, got {value:?}: {e}"),
                    )
                })?;
                if v == 0 {
                    return Err(err(line_no, "max_attempts must be >= 1"));
                }
                max_attempts = Some(v);
                seen.push(key);
            }
            other => return Err(err(line_no, format!("unknown key `{other}`"))),
        }
    }

    let missing = |key: &str| err(first_line_no, format!("missing required key `{key}`"));
    let directive = directive.ok_or_else(|| missing("directive"))?;
    let target = target.ok_or_else(|| missing("target"))?;
    let base_ref = base_ref.ok_or_else(|| missing("ref"))?;
    let priority = priority.ok_or_else(|| missing("priority"))?;
    let family = family.ok_or_else(|| missing("family"))?;
    let max_attempts = max_attempts.ok_or_else(|| missing("max_attempts"))?;
    if !acceptance_seen {
        return Err(missing("acceptance"));
    }
    if acceptance.is_empty() {
        return Err(err(
            first_line_no,
            "acceptance must list at least one command",
        ));
    }

    Ok(Parsed {
        directive,
        target,
        base_ref,
        acceptance,
        budget_usd,
        priority,
        family,
        max_attempts,
    })
}

fn err(line: usize, message: impl Into<String>) -> BlockError {
    BlockError {
        line,
        message: message.into(),
    }
}

fn allowed(values: &[&str]) -> String {
    values
        .iter()
        .map(|v| format!("`{v}`"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Parse a scalar: strip one layer of matching quotes; reject empty.
fn scalar(key: &str, value: &str, line_no: usize) -> Result<String, BlockError> {
    let value = unquote(value);
    if value.is_empty() {
        return Err(err(line_no, format!("`{key}` must not be empty")));
    }
    Ok(value.to_string())
}

/// Parse a flow-style list `[a, b, c]`.
fn parse_inline_list(value: &str) -> Result<Vec<String>, String> {
    let inner = value
        .strip_prefix('[')
        .and_then(|v| v.strip_suffix(']'))
        .ok_or_else(|| {
            format!("`acceptance` must be a block list or `[...]` flow list, found {value:?}")
        })?;
    Ok(inner
        .split(',')
        .map(|item| unquote(item.trim()).to_string())
        .filter(|item| !item.is_empty())
        .collect())
}

/// Strip one layer of matching single or double quotes.
fn unquote(value: &str) -> &str {
    let bytes = value.as_bytes();
    if bytes.len() >= 2 {
        let (first, last) = (bytes[0], bytes[bytes.len() - 1]);
        if (first == b'"' && last == b'"') || (first == b'\'' && last == b'\'') {
            return &value[1..value.len() - 1];
        }
    }
    value
}
