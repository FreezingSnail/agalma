//! Sandbox profile rendering and fail-closed pre-validation.
//!
//! The launcher must never pass an unvalidated profile to `sandbox-exec`. A
//! missing file, empty document, unbalanced parentheses, absent
//! `(version ...)`/`(deny default)`, or a `(param "...")` that the launcher
//! does not supply is a hard error *before* any confined process is created
//! (S0b condition 1; probe row10).
//!
//! This module is pure: it reads no files and spawns nothing, so it is unit
//! testable without a sandbox.

use std::collections::BTreeSet;
use std::fmt;

/// Bundled product confine profile (from S0b; `(deny default)`).
pub const PRODUCT_PROFILE: &str = include_str!("../profiles/worker.sb");

/// Profile parameter: attempt-owned read/write root.
pub const PARAM_ATTEMPT: &str = "ATTEMPT";
/// Profile parameter: protected-tests root (read-only).
pub const PARAM_PROTECTED: &str = "PROTECTED";
/// Profile parameter: parent-owned Unix-socket directory.
pub const PARAM_SOCKDIR: &str = "SOCKDIR";
/// Profile parameter: extra read-only root (slot 1).
pub const PARAM_EXTRA_RO: &str = "EXTRA_RO";
/// Profile parameter: extra read-only root (slot 2).
pub const PARAM_EXTRA_RO2: &str = "EXTRA_RO2";
/// Profile parameter: extra read-only root (slot 3).
pub const PARAM_EXTRA_RO3: &str = "EXTRA_RO3";

/// Number of extra read-only root slots the launcher always renders.
pub const EXTRA_RO_SLOTS: usize = 3;

/// Inert placeholder for an unused extra read-only slot.
///
/// `sandbox-exec` rejects an empty `(subpath ...)` pattern at profile-compile
/// time, so an unused slot must carry a harmless existing path instead. `/dev/null`
/// is already read-permitted by the product profile, so it can never widen
/// access; the slot is inert.
pub const INERT_EXTRA_RO: &str = "/dev/null";

/// Every parameter the launcher supplies. A profile that references any other
/// name can never be fully substituted and must fail closed.
pub const SUPPLIED_PARAMS: [&str; 6] = [
    PARAM_ATTEMPT,
    PARAM_PROTECTED,
    PARAM_SOCKDIR,
    PARAM_EXTRA_RO,
    PARAM_EXTRA_RO2,
    PARAM_EXTRA_RO3,
];

/// Why a profile was rejected. Rendered by the launcher as a `KnownFailure`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProfileError {
    /// The profile file was empty or only whitespace/comments.
    Empty,
    /// The profile has no `(version ...)` declaration.
    MissingVersion,
    /// The profile has no `(deny default)` policy.
    NotDenyDefault,
    /// Parentheses are unbalanced (a syntax error `sandbox-exec` would reject).
    UnbalancedParens,
    /// A referenced parameter is not one the launcher supplies.
    UnknownParam(String),
}

impl fmt::Display for ProfileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProfileError::Empty => write!(f, "sandbox profile is empty"),
            ProfileError::MissingVersion => {
                write!(f, "sandbox profile has no (version ...) declaration")
            }
            ProfileError::NotDenyDefault => {
                write!(
                    f,
                    "sandbox profile is not deny-by-default: missing (deny default)"
                )
            }
            ProfileError::UnbalancedParens => {
                write!(f, "sandbox profile has unbalanced parentheses")
            }
            ProfileError::UnknownParam(name) => write!(
                f,
                "sandbox profile references unsubstituted parameter {name:?}; \
                 the launcher only supplies {SUPPLIED_PARAMS:?}"
            ),
        }
    }
}

impl std::error::Error for ProfileError {}

/// Validate a rendered profile's source. See module docs for the exact rules.
pub fn validate(source: &str) -> Result<(), ProfileError> {
    let code = strip_comments(source);
    if code.trim().is_empty() {
        return Err(ProfileError::Empty);
    }
    if !code.contains("(version") {
        return Err(ProfileError::MissingVersion);
    }
    if !code.contains("(deny default)") {
        return Err(ProfileError::NotDenyDefault);
    }
    if !parens_balanced(&code) {
        return Err(ProfileError::UnbalancedParens);
    }
    for name in referenced_params(&code) {
        if !SUPPLIED_PARAMS.contains(&name.as_str()) {
            return Err(ProfileError::UnknownParam(name));
        }
    }
    Ok(())
}

/// Remove `;`-to-end-of-line comments, preserving newlines and quoted strings.
fn strip_comments(source: &str) -> String {
    let mut out = String::with_capacity(source.len());
    let mut chars = source.chars().peekable();
    let mut in_string = false;
    let mut escaped = false;
    while let Some(c) = chars.next() {
        if in_string {
            out.push(c);
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_string = false;
            }
        } else if c == '"' {
            in_string = true;
            out.push(c);
        } else if c == ';' {
            // Consume through end of line; keep the newline for line structure.
            for c2 in chars.by_ref() {
                if c2 == '\n' {
                    out.push('\n');
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Whether parentheses balance, ignoring those inside quoted strings.
fn parens_balanced(code: &str) -> bool {
    let mut depth: i32 = 0;
    let mut in_string = false;
    let mut escaped = false;
    for c in code.chars() {
        if in_string {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_string = false;
            }
        } else if c == '"' {
            in_string = true;
        } else if c == '(' {
            depth += 1;
        } else if c == ')' {
            depth -= 1;
            if depth < 0 {
                return false;
            }
        }
    }
    depth == 0
}

/// The names of every `(param "NAME")` referenced by the profile.
fn referenced_params(code: &str) -> BTreeSet<String> {
    let bytes = code.as_bytes();
    let mut out = BTreeSet::new();
    let mut last_ident = String::new();
    let mut i = 0usize;
    while i < bytes.len() {
        let c = bytes[i] as char;
        if c == '"' {
            let start = i + 1;
            let mut j = start;
            while j < bytes.len() && bytes[j] != b'"' {
                j += 1;
            }
            if last_ident == "param" {
                out.insert(code[start..j].to_string());
            }
            last_ident.clear();
            i = j + 1;
        } else if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '*') {
            let start = i;
            let mut j = i;
            while j < bytes.len() {
                let d = bytes[j] as char;
                if d.is_ascii_alphanumeric() || matches!(d, '-' | '_' | '*') {
                    j += 1;
                } else {
                    break;
                }
            }
            last_ident = code[start..j].to_string();
            i = j;
        } else {
            if !c.is_whitespace() {
                last_ident.clear();
            }
            i += 1;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_product_profile_is_valid() {
        validate(PRODUCT_PROFILE).expect("product profile must validate");
    }

    #[test]
    fn bundled_profile_is_deny_default() {
        assert!(PRODUCT_PROFILE.contains("(deny default)"));
    }

    #[test]
    fn bundled_profile_references_only_supplied_params() {
        let params = referenced_params(&strip_comments(PRODUCT_PROFILE));
        for expected in SUPPLIED_PARAMS {
            assert!(
                params.contains(expected),
                "product profile should reference {expected}"
            );
        }
    }

    #[test]
    fn empty_profile_is_rejected() {
        assert_eq!(validate(""), Err(ProfileError::Empty));
        assert_eq!(validate("   \n\t"), Err(ProfileError::Empty));
        assert_eq!(validate(";; only a comment\n"), Err(ProfileError::Empty));
    }

    #[test]
    fn missing_version_is_rejected() {
        assert_eq!(
            validate("(deny default)\n"),
            Err(ProfileError::MissingVersion)
        );
    }

    #[test]
    fn non_deny_default_is_rejected() {
        assert_eq!(
            validate("(version 1)\n(allow default)\n"),
            Err(ProfileError::NotDenyDefault)
        );
    }

    #[test]
    fn unbalanced_parens_are_rejected() {
        let malformed = "(version 1)\n(deny default)\n(allow file-read* (subpath \"/usr\")\n";
        assert_eq!(validate(malformed), Err(ProfileError::UnbalancedParens));
    }

    #[test]
    fn undefined_param_is_rejected() {
        let source = "(version 1)\n(deny default)\n\
                      (allow file-read* (subpath (param \"UNDEFINED_ATTEMPT\")))\n";
        assert_eq!(
            validate(source),
            Err(ProfileError::UnknownParam("UNDEFINED_ATTEMPT".to_string()))
        );
    }

    #[test]
    fn comments_do_not_confuse_param_scanning() {
        let source = "(version 1)\n(deny default)\n\
                      ;; (param \"COMMENTED\")\n\
                      (allow file-read* (subpath (param \"ATTEMPT\")))\n";
        validate(source).expect("commented param must be ignored");
    }

    #[test]
    fn parens_inside_strings_are_ignored() {
        let source = "(version 1)\n(deny default)\n(allow file-read* (literal \"a(b\"))\n";
        validate(source).expect("parentheses in strings must not break balancing");
    }
}
