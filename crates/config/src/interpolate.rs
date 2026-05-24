//! `{variable}` substitution in config strings.
//!
//! Used at runtime to resolve `{port}`, `{slot}`, `{worktree}`, etc. in
//! `cmd`, `check`, `provision`, env values, and health URLs.
//!
//! See ADR-0012 and the locked-in variable surface from the design grilling.
//!
//! ## Syntax
//!
//! - `{name}` substitutes a known variable.
//! - `{{` is a literal `{`; `}}` is a literal `}`.
//! - An unmatched `{` or an unknown variable name is an error — surfaces
//!   at config-resolution time, not at spawn time, so users see the
//!   problem before a half-broken service tries to start.
//!
//! Devstack does **not** interpolate `$VAR` or `${VAR}` — the shell handles
//! environment variables at spawn time.

use std::collections::HashMap;

use thiserror::Error;

/// The set of values available for substitution. The supervisor populates
/// this from runtime state (slot, port, git, etc.) before invoking
/// [`interpolate`].
#[derive(Debug, Clone, Default)]
pub struct InterpContext {
    values: HashMap<String, String>,
}

impl InterpContext {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add or replace a variable. Returns self for builder-style chaining.
    pub fn set(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.values.insert(key.into(), value.into());
        self
    }

    /// Add a variable in-place.
    pub fn insert(&mut self, key: impl Into<String>, value: impl Into<String>) {
        self.values.insert(key.into(), value.into());
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.values.get(key).map(String::as_str)
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum InterpError {
    #[error("unknown variable {{{name}}} at byte offset {offset}")]
    UnknownVariable { name: String, offset: usize },

    #[error("unclosed '{{' at byte offset {offset}")]
    UnclosedBrace { offset: usize },

    #[error("unexpected '}}' at byte offset {offset} (use '}}}}' for a literal)")]
    UnexpectedCloseBrace { offset: usize },

    #[error("empty variable {{}} at byte offset {offset}")]
    EmptyVariable { offset: usize },
}

/// Substitute `{name}` references in `template` using `ctx`. Returns an
/// error for any unknown variable, unclosed brace, or empty `{}`.
pub fn interpolate(template: &str, ctx: &InterpContext) -> Result<String, InterpError> {
    let mut out = String::with_capacity(template.len());
    let bytes = template.as_bytes();
    let mut i = 0;

    while i < bytes.len() {
        // Shell-style ${VAR} passes through literally — that's the shell's
        // job. Detect `${` as a unit before the generic `{...}` branch.
        if bytes[i] == b'$' {
            if peek(bytes, i + 1) == Some(b'{') {
                let body_start = i + 2;
                let end_offset = template[body_start..]
                    .find('}')
                    .ok_or(InterpError::UnclosedBrace { offset: i })?;
                let end = body_start + end_offset + 1;
                out.push_str(&template[i..end]);
                i = end;
            } else {
                // bare `$` — emit and continue
                out.push('$');
                i += 1;
            }
            continue;
        }
        match bytes[i] {
            b'{' if peek(bytes, i + 1) == Some(b'{') => {
                out.push('{');
                i += 2;
            }
            b'}' if peek(bytes, i + 1) == Some(b'}') => {
                out.push('}');
                i += 2;
            }
            b'}' => return Err(InterpError::UnexpectedCloseBrace { offset: i }),
            b'{' => {
                let start = i + 1;
                let end = template[start..]
                    .find('}')
                    .ok_or(InterpError::UnclosedBrace { offset: i })?;
                let name = &template[start..start + end];
                if name.is_empty() {
                    return Err(InterpError::EmptyVariable { offset: i });
                }
                let value = ctx.get(name).ok_or_else(|| InterpError::UnknownVariable {
                    name: name.to_string(),
                    offset: i,
                })?;
                out.push_str(value);
                i = start + end + 1;
            }
            _ => {
                // Copy until the next '{', '}', or '$' — stopping on '$'
                // ensures the shell-var detection at the top of the loop
                // gets a chance to fire on `${...}`.
                let chunk_start = i;
                while i < bytes.len()
                    && bytes[i] != b'{'
                    && bytes[i] != b'}'
                    && bytes[i] != b'$'
                {
                    i += 1;
                }
                out.push_str(&template[chunk_start..i]);
            }
        }
    }

    Ok(out)
}

fn peek(bytes: &[u8], idx: usize) -> Option<u8> {
    bytes.get(idx).copied()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> InterpContext {
        InterpContext::new()
            .set("port", "8080")
            .set("slot", "0")
            .set("worktree", "/Users/x/repo")
            .set("branch", "main")
    }

    #[test]
    fn plain_string_passes_through_unchanged() {
        let out = interpolate("uv run manage.py runserver", &ctx()).unwrap();
        assert_eq!(out, "uv run manage.py runserver");
    }

    #[test]
    fn substitutes_a_single_variable() {
        let out = interpolate("port={port}", &ctx()).unwrap();
        assert_eq!(out, "port=8080");
    }

    #[test]
    fn substitutes_multiple_variables() {
        let out = interpolate(
            "cmd: 0.0.0.0:{port} (slot {slot}, branch {branch})",
            &ctx(),
        )
        .unwrap();
        assert_eq!(out, "cmd: 0.0.0.0:8080 (slot 0, branch main)");
    }

    #[test]
    fn unknown_variable_is_an_error() {
        let err = interpolate("hi {unknown}", &ctx()).unwrap_err();
        assert!(matches!(err, InterpError::UnknownVariable { ref name, .. } if name == "unknown"));
    }

    #[test]
    fn unknown_variable_carries_offset() {
        let err = interpolate("aaa {unknown}", &ctx()).unwrap_err();
        let InterpError::UnknownVariable { offset, .. } = err else { panic!() };
        assert_eq!(offset, 4);
    }

    #[test]
    fn unclosed_brace_is_an_error() {
        let err = interpolate("hi {port", &ctx()).unwrap_err();
        assert!(matches!(err, InterpError::UnclosedBrace { offset: 3 }));
    }

    #[test]
    fn lonely_close_brace_is_an_error() {
        let err = interpolate("oops } here", &ctx()).unwrap_err();
        assert!(matches!(err, InterpError::UnexpectedCloseBrace { offset: 5 }));
    }

    #[test]
    fn empty_variable_is_an_error() {
        let err = interpolate("hi {}", &ctx()).unwrap_err();
        assert!(matches!(err, InterpError::EmptyVariable { .. }));
    }

    #[test]
    fn double_open_brace_is_a_literal_brace() {
        let out = interpolate("echo {{not_a_var}}", &ctx()).unwrap();
        assert_eq!(out, "echo {not_a_var}");
    }

    #[test]
    fn escaped_braces_dont_consume_real_variables() {
        let out = interpolate("{{{port}}}", &ctx()).unwrap();
        // `{{` -> `{`, then `{port}` -> `8080`, then `}}` -> `}`
        assert_eq!(out, "{8080}");
    }

    #[test]
    fn variable_at_start_of_string() {
        let out = interpolate("{port}/health", &ctx()).unwrap();
        assert_eq!(out, "8080/health");
    }

    #[test]
    fn variable_at_end_of_string() {
        let out = interpolate("listen:{port}", &ctx()).unwrap();
        assert_eq!(out, "listen:8080");
    }

    #[test]
    fn empty_template_returns_empty() {
        let out = interpolate("", &ctx()).unwrap();
        assert_eq!(out, "");
    }

    #[test]
    fn shell_dollar_vars_are_not_interpolated() {
        // `$HOME` and `${PATH}` are the shell's job; devstack passes them
        // through unchanged.
        let out = interpolate("echo $HOME and ${PATH}", &ctx()).unwrap();
        assert_eq!(out, "echo $HOME and ${PATH}");
    }

    #[test]
    fn same_variable_used_multiple_times() {
        let out = interpolate("{port}/{port}/{port}", &ctx()).unwrap();
        assert_eq!(out, "8080/8080/8080");
    }

    #[test]
    fn context_builder_chains() {
        let c = InterpContext::new().set("a", "1").set("b", "2");
        assert_eq!(c.get("a"), Some("1"));
        assert_eq!(c.get("b"), Some("2"));
        assert_eq!(c.get("c"), None);
    }

    #[test]
    fn context_insert_in_place() {
        let mut c = InterpContext::new();
        c.insert("port", "9090");
        assert_eq!(c.get("port"), Some("9090"));
    }
}
