//! Validation: name uniqueness, dependency resolution, cycle detection,
//! and a few field-level invariants.

use std::collections::{HashMap, HashSet};

use crate::error::ConfigError;
use crate::stack::{SCHEMA_VERSION, Stack};

/// Run every validation pass on a parsed [`Stack`]. Collects every error
/// rather than failing on the first — config authors should see all problems
/// in one shot.
pub fn validate(stack: &Stack) -> Result<(), Vec<ConfigError>> {
    let mut errors = Vec::new();

    if stack.schema_version != SCHEMA_VERSION {
        errors.push(ConfigError::UnsupportedSchemaVersion {
            found: stack.schema_version,
            expected: SCHEMA_VERSION,
        });
    }

    check_name_collisions(stack, &mut errors);
    check_dependency_targets_exist(stack, &mut errors);
    check_no_cycles(stack, &mut errors);
    check_external_services_have_health(stack, &mut errors);

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

/// A non-fatal advisory: the config parses and [`validate`]s, but something
/// looks likely-unintended. Unlike [`ConfigError`], a lint never blocks
/// `devme up` — it surfaces only through `devme config check`, to catch the
/// class of mistake that otherwise only bites at runtime (a frontend that
/// won't open, a `{port}` left literal).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lint {
    /// The step/service the advisory is about.
    pub target: String,
    /// What looks wrong, in one line.
    pub message: String,
    /// Concrete fix.
    pub hint: String,
}

/// Run every lint pass over a parsed [`Stack`]. Advisory only — the result is
/// always returned (never an error), and an empty vec means "nothing to flag".
pub fn lint(stack: &Stack) -> Vec<Lint> {
    let mut lints = Vec::new();
    check_ambiguous_openability(stack, &mut lints);
    check_port_placeholder_without_port(stack, &mut lints);
    lints
}

/// A service with a port but neither a `url` nor a health check is ambiguous:
/// devme can't tell a web server (browser-openable) from a database (copy a
/// bare `host:port`). It defaults to the latter, so a frontend silently
/// refuses to open. Declaring *either* signal removes the ambiguity — this is
/// the lint that closes the "open says not a web URL" footgun.
fn check_ambiguous_openability(stack: &Stack, lints: &mut Vec<Lint>) {
    for (name, svc) in &stack.service {
        if svc.external {
            continue; // external services already require health (validated).
        }
        if svc.port.is_some() && svc.url.is_none() && svc.health.is_none() {
            lints.push(Lint {
                target: name.clone(),
                message: format!(
                    "service '{name}' has a port but no `url` and no health check — \
                     devme can't tell whether it's browser-openable, so the TUI's `o` \
                     (open) copies a bare host:port instead of opening a browser"
                ),
                hint: "add `url = \"http://{host}:{port}\"` for a web server, \
                       or a health check (e.g. `health = { tcp = \"localhost:{port}\" }`) \
                       for a database / TCP service"
                    .into(),
            });
        }
    }
}

/// `{port}` in a command with no `port` declared is passed through literally —
/// the process gets the string `{port}`, not a number. Almost always a missing
/// `port = { … }`.
fn check_port_placeholder_without_port(stack: &Stack, lints: &mut Vec<Lint>) {
    for (name, svc) in &stack.service {
        if svc.port.is_none() && svc.cmd.contains("{port}") {
            lints.push(Lint {
                target: name.clone(),
                message: format!(
                    "service '{name}' uses `{{port}}` in its command but declares no \
                     `port` — the placeholder is passed through literally"
                ),
                hint: "add `port = { base = <default>, slot_offset = 10 }` \
                       (or `{ fixed = <n> }`)"
                    .into(),
            });
        }
    }
}

fn check_name_collisions(stack: &Stack, errors: &mut Vec<ConfigError>) {
    for name in stack.step.keys() {
        if stack.service.contains_key(name) {
            errors.push(ConfigError::NameCollision { name: name.clone() });
        }
    }
}

fn check_dependency_targets_exist(stack: &Stack, errors: &mut Vec<ConfigError>) {
    let known: HashSet<&str> = stack
        .step
        .keys()
        .chain(stack.service.keys())
        .map(String::as_str)
        .collect();

    let mut check = |from: &str, deps: &[devme_core::Dependency]| {
        for d in deps {
            if !known.contains(d.name.as_str()) {
                errors.push(ConfigError::UnknownDependency {
                    from: from.to_string(),
                    to: d.name.clone(),
                });
            }
        }
    };

    for (name, step) in &stack.step {
        check(name, &step.depends_on);
    }
    for (name, service) in &stack.service {
        check(name, &service.depends_on);
    }
}

fn check_external_services_have_health(stack: &Stack, errors: &mut Vec<ConfigError>) {
    for (name, service) in &stack.service {
        if service.external && service.health.is_none() {
            errors.push(ConfigError::ExternalServiceMissingHealth { name: name.clone() });
        }
    }
}

// --- cycle detection ---

#[derive(Clone, Copy, PartialEq, Eq)]
enum Color {
    White,
    Gray,
    Black,
}

fn check_no_cycles(stack: &Stack, errors: &mut Vec<ConfigError>) {
    // Build adjacency: name -> list of dep names (filtered to known nodes so
    // unknown-dep errors don't spuriously cycle).
    let known: HashSet<&str> = stack
        .step
        .keys()
        .chain(stack.service.keys())
        .map(String::as_str)
        .collect();

    let mut adj: HashMap<&str, Vec<&str>> = HashMap::new();
    for (name, step) in &stack.step {
        adj.insert(
            name.as_str(),
            step.depends_on
                .iter()
                .map(|d| d.name.as_str())
                .filter(|n| known.contains(n))
                .collect(),
        );
    }
    for (name, service) in &stack.service {
        adj.insert(
            name.as_str(),
            service
                .depends_on
                .iter()
                .map(|d| d.name.as_str())
                .filter(|n| known.contains(n))
                .collect(),
        );
    }

    // Three-color DFS. Each gray node is on the current stack; reaching a
    // gray node = cycle. Capture the cycle path for the error message.
    let mut color: HashMap<&str, Color> = adj.keys().map(|k| (*k, Color::White)).collect();
    let mut path: Vec<&str> = Vec::new();
    let mut reported: HashSet<Vec<&str>> = HashSet::new();

    // Visit in declared order so error messages are deterministic.
    for name in stack.step.keys().chain(stack.service.keys()) {
        let n = name.as_str();
        if color.get(n) == Some(&Color::White) {
            dfs(n, &adj, &mut color, &mut path, &mut reported, errors);
        }
    }
}

fn dfs<'a>(
    node: &'a str,
    adj: &HashMap<&'a str, Vec<&'a str>>,
    color: &mut HashMap<&'a str, Color>,
    path: &mut Vec<&'a str>,
    reported: &mut HashSet<Vec<&'a str>>,
    errors: &mut Vec<ConfigError>,
) {
    color.insert(node, Color::Gray);
    path.push(node);

    if let Some(neighbors) = adj.get(node) {
        for &next in neighbors {
            match color.get(next).copied().unwrap_or(Color::White) {
                Color::White => {
                    dfs(next, adj, color, path, reported, errors);
                }
                Color::Gray => {
                    // Cycle: collect path from `next` back to current node.
                    let start = path.iter().position(|&n| n == next).unwrap_or(0);
                    let mut cycle: Vec<&str> = path[start..].to_vec();
                    cycle.push(next);

                    // Canonicalize for dedup (rotate to start at lex-min, then
                    // store) — two reports of the same cycle starting from
                    // different nodes would otherwise both fire.
                    let canonical = canonical_cycle(&cycle);
                    if reported.insert(canonical) {
                        let formatted = cycle.join(" -> ");
                        errors.push(ConfigError::Cycle { cycle: formatted });
                    }
                }
                Color::Black => {}
            }
        }
    }

    path.pop();
    color.insert(node, Color::Black);
}

fn canonical_cycle<'a>(cycle: &[&'a str]) -> Vec<&'a str> {
    // Cycle paths are like ["a", "b", "c", "a"] — drop the trailing repeat,
    // rotate so the lex-min element is first.
    if cycle.len() < 2 {
        return cycle.to_vec();
    }
    let body = &cycle[..cycle.len() - 1];
    let min_idx = body
        .iter()
        .enumerate()
        .min_by_key(|(_, n)| **n)
        .map(|(i, _)| i)
        .unwrap_or(0);
    let mut rotated: Vec<&str> = body[min_idx..].to_vec();
    rotated.extend_from_slice(&body[..min_idx]);
    rotated
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stack::Stack;

    fn parse(toml_str: &str) -> Stack {
        Stack::parse(toml_str).expect("parse")
    }

    #[test]
    fn empty_config_is_valid() {
        let s = parse("schema_version = 1");
        assert!(validate(&s).is_ok());
    }

    #[test]
    fn simple_valid_graph() {
        let s = parse(
            r#"
schema_version = 1

[step.tooling]
check = "command -v uv"

[service.db]
cmd = "docker run postgres"

[service.backend]
cmd = "uv run x"
depends_on = ["tooling", "db"]
"#,
        );
        assert!(validate(&s).is_ok());
    }

    #[test]
    fn name_collision_between_step_and_service() {
        let s = parse(
            r#"
schema_version = 1

[step.db]
check = "true"

[service.db]
cmd = "true"
"#,
        );
        let errs = validate(&s).unwrap_err();
        assert!(matches!(errs.as_slice(), [ConfigError::NameCollision { name }] if name == "db"));
    }

    #[test]
    fn unknown_dependency_is_caught() {
        let s = parse(
            r#"
schema_version = 1

[service.backend]
cmd = "true"
depends_on = ["does_not_exist"]
"#,
        );
        let errs = validate(&s).unwrap_err();
        assert!(
            matches!(errs.as_slice(), [ConfigError::UnknownDependency { from, to }] if from == "backend" && to == "does_not_exist")
        );
    }

    #[test]
    fn unknown_dependency_through_optional_suffix_still_caught() {
        let s = parse(
            r#"
schema_version = 1

[service.backend]
cmd = "true"
depends_on = ["ghost?"]
"#,
        );
        let errs = validate(&s).unwrap_err();
        assert!(matches!(
            errs.as_slice(),
            [ConfigError::UnknownDependency { .. }]
        ));
    }

    #[test]
    fn detects_two_node_cycle() {
        let s = parse(
            r#"
schema_version = 1

[service.a]
cmd = "true"
depends_on = ["b"]

[service.b]
cmd = "true"
depends_on = ["a"]
"#,
        );
        let errs = validate(&s).unwrap_err();
        assert!(errs.iter().any(|e| matches!(e, ConfigError::Cycle { .. })));
    }

    #[test]
    fn detects_three_node_cycle() {
        let s = parse(
            r#"
schema_version = 1

[service.a]
cmd = "true"
depends_on = ["b"]

[service.b]
cmd = "true"
depends_on = ["c"]

[service.c]
cmd = "true"
depends_on = ["a"]
"#,
        );
        let errs = validate(&s).unwrap_err();
        let cycles: Vec<_> = errs
            .iter()
            .filter(|e| matches!(e, ConfigError::Cycle { .. }))
            .collect();
        assert_eq!(cycles.len(), 1, "expected exactly one cycle, got: {errs:?}");
    }

    #[test]
    fn detects_self_loop() {
        let s = parse(
            r#"
schema_version = 1

[service.a]
cmd = "true"
depends_on = ["a"]
"#,
        );
        let errs = validate(&s).unwrap_err();
        assert!(errs.iter().any(|e| matches!(e, ConfigError::Cycle { .. })));
    }

    #[test]
    fn linear_chain_is_not_a_cycle() {
        let s = parse(
            r#"
schema_version = 1

[service.a]
cmd = "true"

[service.b]
cmd = "true"
depends_on = ["a"]

[service.c]
cmd = "true"
depends_on = ["b"]
"#,
        );
        assert!(validate(&s).is_ok(), "expected linear chain to validate");
    }

    #[test]
    fn diamond_dependency_is_not_a_cycle() {
        // d depends on b and c; both depend on a. Common in real configs.
        let s = parse(
            r#"
schema_version = 1

[service.a]
cmd = "true"

[service.b]
cmd = "true"
depends_on = ["a"]

[service.c]
cmd = "true"
depends_on = ["a"]

[service.d]
cmd = "true"
depends_on = ["b", "c"]
"#,
        );
        assert!(validate(&s).is_ok());
    }

    #[test]
    fn external_service_without_health_is_an_error() {
        let s = parse(
            r#"
schema_version = 1

[service.postgres]
cmd = ""
external = true
"#,
        );
        let errs = validate(&s).unwrap_err();
        assert!(
            matches!(errs.as_slice(), [ConfigError::ExternalServiceMissingHealth { name }] if name == "postgres")
        );
    }

    #[test]
    fn external_service_with_health_is_valid() {
        let s = parse(
            r#"
schema_version = 1

[service.postgres]
cmd = ""
external = true
health = { tcp = "localhost:5432" }
"#,
        );
        assert!(validate(&s).is_ok());
    }

    #[test]
    fn unsupported_schema_version_is_an_error() {
        let s = parse("schema_version = 99");
        let errs = validate(&s).unwrap_err();
        assert!(matches!(
            errs.as_slice(),
            [ConfigError::UnsupportedSchemaVersion {
                found: 99,
                expected: 1
            }]
        ));
    }

    #[test]
    fn lint_flags_web_service_without_url_or_health() {
        // The motivating case: a Vite-style dev server, port but no url/health.
        let s = parse(
            r#"
schema_version = 1

[service.frontend]
cmd = "npm run dev"
port = { base = 5173, slot_offset = 10 }
"#,
        );
        let lints = lint(&s);
        assert_eq!(lints.len(), 1);
        assert_eq!(lints[0].target, "frontend");
        assert!(lints[0].message.contains("browser-openable"));
        assert!(lints[0].hint.contains("url ="));
    }

    #[test]
    fn lint_silent_when_url_declared() {
        let s = parse(
            r#"
schema_version = 1

[service.frontend]
cmd = "npm run dev"
port = { base = 5173, slot_offset = 10 }
url = "http://{host}:{port}"
"#,
        );
        assert!(lint(&s).is_empty());
    }

    #[test]
    fn lint_silent_when_health_declared() {
        // A tcp health check is a clear "this is a TCP/db service" signal — not
        // ambiguous, so no nudge.
        let s = parse(
            r#"
schema_version = 1

[service.db]
cmd = "docker run postgres"
port = { fixed = 5432 }
health = { tcp = "localhost:{port}" }
"#,
        );
        assert!(lint(&s).is_empty());
    }

    #[test]
    fn lint_flags_port_placeholder_without_port_spec() {
        let s = parse(
            r#"
schema_version = 1

[service.api]
cmd = "serve --port {port}"
url = "http://{host}:{port}"
"#,
        );
        let lints = lint(&s);
        assert_eq!(lints.len(), 1);
        assert_eq!(lints[0].target, "api");
        assert!(lints[0].message.contains("{port}"));
    }

    #[test]
    fn multiple_errors_returned_together() {
        // Two unknown deps + a name collision — we report all three.
        let s = parse(
            r#"
schema_version = 1

[step.db]
check = "true"

[service.db]
cmd = "true"
depends_on = ["ghost1", "ghost2"]
"#,
        );
        let errs = validate(&s).unwrap_err();
        assert!(
            errs.len() >= 3,
            "expected at least 3 errors, got {}: {errs:?}",
            errs.len()
        );
    }
}
