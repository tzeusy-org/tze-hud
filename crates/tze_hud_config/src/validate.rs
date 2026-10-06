//! Startup config validation. Every config section is read once at startup;
//! changing one requires a restart.

use tze_hud_scene::config::{ConfigError, ConfigErrorCode, ConfigLoader};

use crate::loader::TzeHudConfig;

/// Parse and validate a configuration TOML string.
///
/// Returns the parse error or every validation error found; never touches
/// runtime state. Unsupported document/runtime/tab keys fail with guidance to
/// remove or correct the key; malformed TOML retains syntax diagnostics.
pub fn validate_config(toml_src: &str) -> Result<(), Vec<ConfigError>> {
    let loader = TzeHudConfig::parse(toml_src).map_err(|parse_err| {
        // Read serde's diagnostic, rather than duplicating its supported-key
        // schema. Rendered TOML source lines have a gutter, so a syntax error
        // whose source mentions "unknown field" is not mistaken for this case.
        let unknown_key = parse_err.message.lines().find_map(|line| {
            line.strip_prefix("unknown field `")?
                .split_once('`')
                .map(|(key, _)| key)
        });
        let (expected, hint) = if let Some(key) = unknown_key {
            (
                "supported configuration key",
                format!(
                    "remove or correct unsupported key `{key}` at line {}, column {}; \
                     the parser diagnostic lists the supported fields",
                    parse_err.line, parse_err.column
                ),
            )
        } else {
            (
                "valid TOML",
                format!(
                    "fix the TOML syntax error at line {}, column {}",
                    parse_err.line, parse_err.column
                ),
            )
        };
        vec![ConfigError {
            code: ConfigErrorCode::ParseError,
            field_path: unknown_key.unwrap_or_default().into(),
            expected: expected.into(),
            got: parse_err.message.clone(),
            hint,
        }]
    })?;

    let errors = loader.validate();
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_toml_reports_parse_error() {
        let errors = validate_config("this is not valid toml [\n").unwrap_err();
        assert!(matches!(errors[0].code, ConfigErrorCode::ParseError));
    }

    #[test]
    fn unknown_profile_reports_validation_error() {
        let errors =
            validate_config("[runtime]\nprofile = \"mobile\"\n\n[[tabs]]\nname = \"Main\"\n")
                .unwrap_err();
        assert!(
            errors
                .iter()
                .any(|e| matches!(e.code, ConfigErrorCode::UnknownProfile))
        );
    }
}
