//! Surgical, in-place edits to the user's `global.toml`.
//!
//! `toml::to_string_pretty(&config)` round-trips the typed struct but throws
//! away every comment and any hand-formatting the user added. When devme
//! writes a single key back (from `devme config set` or the in-TUI settings
//! overlay) we instead patch just that line, preserving everything else.
//!
//! Ported from herdr's `config::io` (same approach, trimmed to the keys
//! devme has). All functions operate on raw file content and return the new
//! content; the caller owns reading/writing the file.

/// Insert or replace `key = value` inside `[section]`, creating the section
/// if it's missing. `value` is raw TOML (`"\"mocha\""`, `true`, `42`).
pub fn upsert_section_value(content: &str, section: &str, key: &str, value: &str) -> String {
    let header = format!("[{section}]");
    let assignment = format!("{key} = {value}");
    let lines: Vec<&str> = content.lines().collect();
    let mut result: Vec<String> = Vec::new();
    let mut i = 0;
    let mut found_section = false;
    let mut inserted = false;

    while i < lines.len() {
        let line = lines[i];
        if line.trim() == header {
            found_section = true;
            result.push(line.to_string());
            i += 1;
            // Walk the section body, replacing the key in place or appending
            // it just before the next section header.
            while i < lines.len() {
                let current = lines[i];
                let trimmed = current.trim();
                if trimmed.starts_with('[') && trimmed.ends_with(']') {
                    if !inserted {
                        result.push(assignment.clone());
                        inserted = true;
                    }
                    break;
                }
                if is_assignment_for(trimmed, key) {
                    result.push(assignment.clone());
                    inserted = true;
                } else {
                    result.push(current.to_string());
                }
                i += 1;
            }
            continue;
        }
        result.push(line.to_string());
        i += 1;
    }

    if !found_section {
        if !result.is_empty() && !result.last().is_some_and(|l| l.trim().is_empty()) {
            result.push(String::new());
        }
        result.push(header);
        result.push(assignment);
    } else if !inserted {
        result.push(assignment);
    }

    result.join("\n") + "\n"
}

/// Remove `key` from `[section]`, leaving the rest of the file intact. If
/// the section ends up empty its header is left behind (harmless, and keeps
/// the diff minimal).
pub fn remove_section_key(content: &str, section: &str, key: &str) -> String {
    let header = format!("[{section}]");
    let mut result: Vec<String> = Vec::new();
    let mut in_section = false;

    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            in_section = trimmed == header;
            result.push(line.to_string());
            continue;
        }
        if in_section && is_assignment_for(trimmed, key) {
            continue;
        }
        result.push(line.to_string());
    }

    result.join("\n") + "\n"
}

/// Does `trimmed` assign to `key` (i.e. `key =` or `key=`)?
fn is_assignment_for(trimmed: &str, key: &str) -> bool {
    trimmed.starts_with(&format!("{key} ")) || trimmed.starts_with(&format!("{key}="))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replaces_existing_key_in_place_keeping_comments() {
        let content = "# my settings\n[tui]\n# colour theme\ntheme = \"mocha\"\n";
        let updated = upsert_section_value(content, "tui", "theme", "\"latte\"");
        assert!(updated.contains("# my settings"));
        assert!(updated.contains("# colour theme"));
        assert!(updated.contains("theme = \"latte\""));
        assert!(!updated.contains("\"mocha\""));
    }

    #[test]
    fn adds_missing_section() {
        let updated = upsert_section_value("", "tui", "theme", "\"auto\"");
        assert!(updated.contains("[tui]"));
        assert!(updated.contains("theme = \"auto\""));
        assert!(toml::from_str::<toml::Value>(&updated).is_ok());
    }

    #[test]
    fn appends_key_to_existing_section() {
        let content = "[hints]\nskills = \"true\"\n";
        let updated = upsert_section_value(content, "hints", "extra", "false");
        assert!(updated.contains("skills = \"true\""));
        assert!(updated.contains("extra = false"));
    }

    #[test]
    fn remove_drops_only_the_named_key() {
        let content = "[tui]\ntheme = \"latte\"\nother = 1\n";
        let updated = remove_section_key(content, "tui", "theme");
        assert!(!updated.contains("theme = \"latte\""));
        assert!(updated.contains("other = 1"));
    }
}
