//! Compiled redaction patterns shared by task and service persistence.

use regex::Regex;

#[derive(Debug, Default)]
pub struct Redactor {
    patterns: Vec<Regex>,
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
}
