//! Startup config validation. Every config section is read once at startup;
//! changing one requires a restart.

use tze_hud_scene::config::{ConfigError, ConfigErrorCode, ConfigLoader};

use crate::loader::TzeHudConfig;

/// Parse and validate a configuration TOML string.
///
/// Returns the parse error or every validation error found; never touches
/// runtime state.
pub fn validate_config(toml_src: &str) -> Result<(), Vec<ConfigError>> {
    let loader = TzeHudConfig::parse(toml_src).map_err(|parse_err| {
        vec![ConfigError {
            code: ConfigErrorCode::ParseError,
            field_path: String::new(),
            expected: "valid TOML".into(),
            got: parse_err.message.clone(),
            hint: format!(
                "fix the TOML syntax error at line {}, column {}",
                parse_err.line, parse_err.column
            ),
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
