//! Structured CLI output at the JSON-to-TOON boundary.

use anyhow::Context;
use serde::Serialize;

/// Encode a serializable value using the official strict TOON encoder.
///
/// The encoder guarantees the current output invariants agents depend on:
/// UTF-8, LF indentation, correct collection counts, and no trailing newline.
pub fn toon<T: Serialize>(value: &T) -> anyhow::Result<String> {
    toon_format::encode_default(value).context("failed to encode structured TOON output")
}

/// Emit one complete TOON document on stdout without a trailing newline.
pub fn print_toon<T: Serialize>(value: &T) -> anyhow::Result<()> {
    print!("{}", toon(value)?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn official_strict_decoder_round_trips_encoded_output() {
        let value = json!({
            "status": "healthy",
            "services": [
                {"name": "api", "state": "running", "port": 3210},
                {"name": "web", "state": "waiting", "port": null}
            ],
            "help": ["Run `devme doctor api --full` for details"]
        });

        let encoded = toon(&value).unwrap();
        assert!(!encoded.ends_with('\n'));
        let decoded: serde_json::Value = toon_format::decode_strict(&encoded).unwrap();
        assert_eq!(decoded, value);
    }
}
