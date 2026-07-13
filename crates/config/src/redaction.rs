//! Compiled redaction patterns shared by task and service persistence.

use regex::Regex;

use crate::Stack;

#[derive(Debug, Default)]
pub struct Redactor {
    patterns: Vec<Regex>,
}

/// Configured regexes plus literal values carried by credential-shaped
/// environment keys. This keeps common tokens and signing secrets out of
/// history even when a project forgot to add an explicit pattern.
pub fn persistence_redaction_patterns(stack: &Stack) -> Vec<String> {
    let mut patterns = stack
        .logs
        .as_ref()
        .map(|policy| policy.redact.clone())
        .unwrap_or_default();
    let mut literals = std::collections::BTreeSet::new();
    let mut consider = |key: &str, value: &str| {
        if is_sensitive_key(key) && value.len() >= 4 && !value.contains('{') {
            literals.insert(value.to_string());
        }
    };
    for name in stack.env.keys() {
        if is_sensitive_key(name)
            && let Ok(value) = std::env::var(name)
        {
            consider(name, &value);
        }
    }
    for service in stack.service.values() {
        for (key, value) in &service.env {
            consider(key, value);
        }
    }
    for task in stack.task.values() {
        for (key, value) in &task.env {
            consider(key, value);
        }
    }
    patterns.extend(literals.into_iter().map(|value| regex::escape(&value)));
    patterns
}

/// Whether an environment key is credential-shaped and should have its value
/// treated as persistence-sensitive by every execution surface.
pub fn is_sensitive_key(key: &str) -> bool {
    let key = key.to_ascii_uppercase();
    [
        "SECRET",
        "TOKEN",
        "PASSWORD",
        "PASSPHRASE",
        "CREDENTIAL",
        "DEPLOY_KEY",
        "SIGNING_KEY",
        "PRIVATE_KEY",
        "CERTIFICATE",
    ]
    .iter()
    .any(|needle| key.contains(&needle.to_ascii_uppercase()))
}

impl Redactor {
    pub fn new(patterns: &[String]) -> Result<Self, regex::Error> {
        patterns
            .iter()
            .filter(|pattern| !pattern.is_empty())
            .map(|pattern| Regex::new(pattern))
            .collect::<Result<Vec<_>, _>>()
            .map(|patterns| Self { patterns })
    }

    pub fn apply(&self, text: &str) -> String {
        self.patterns
            .iter()
            .fold(text.to_string(), |value, pattern| {
                pattern.replace_all(&value, "[REDACTED]").into_owned()
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replaces_every_regex_match() {
        let redactor = Redactor::new(&[r"token-[0-9]+".into()]).unwrap();
        assert_eq!(redactor.apply("token-12 token-34"), "[REDACTED] [REDACTED]");
    }

    #[test]
    fn derives_literal_patterns_for_sensitive_service_and_task_env() {
        let stack = Stack::parse(
            r#"schema_version = 1
[service.api]
cmd = "true"
env = { CONVEX_DEPLOY_KEY = "service-secret", PUBLIC_URL = "not-secret" }
[task.sign]
cmd = "true"
env = { SIGNING_KEY_PASSWORD = "task-secret" }
"#,
        )
        .unwrap();
        let patterns = persistence_redaction_patterns(&stack);
        let redactor = Redactor::new(&patterns).unwrap();
        assert_eq!(
            redactor.apply("service-secret task-secret not-secret"),
            "[REDACTED] [REDACTED] not-secret"
        );
    }
}
