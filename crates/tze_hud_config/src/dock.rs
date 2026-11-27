//! Owner configuration for the optional per-session widget dock.

use std::{collections::HashMap, fmt};

use serde::{Deserialize, Serialize};
use tze_hud_scene::config::{ConfigError, ConfigErrorCode};

pub const ROW_HEIGHT_TOKEN: &str = "dock.row.height";
pub const CAPTION_WIDTH_TOKEN: &str = "dock.caption.width";
pub const DEFAULT_ROW_HEIGHT_DP: f32 = 52.0;
pub const DEFAULT_CAPTION_WIDTH_DP: f32 = 96.0;
pub const DEFAULT_AUTO_CLEAR_MS: u64 = 900_000;
const MAX_ACCEPTS: usize = 6;

/// Errors validating dock configuration against the selected widget bundles.
#[derive(Debug)]
pub struct DockValidationErrors(pub Vec<ConfigError>);

impl fmt::Display for DockValidationErrors {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "dock configuration is invalid")?;
        for error in &self.0 {
            write!(
                f,
                "\n[{:?}] {} (expected {}, got {}). {}",
                error.code, error.field_path, error.expected, error.got, error.hint,
            )?;
        }
        Ok(())
    }
}

impl std::error::Error for DockValidationErrors {}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct DockConfig {
    pub accepts: Vec<String>,
    pub max_rows: usize,
    pub auto_clear_ms: u64,
}

impl Default for DockConfig {
    fn default() -> Self {
        Self {
            accepts: Vec::new(),
            max_rows: 16,
            auto_clear_ms: DEFAULT_AUTO_CLEAR_MS,
        }
    }
}

impl DockConfig {
    pub(crate) fn validate(&self, errors: &mut Vec<ConfigError>) {
        if let Some(first_extra) = self.accepts.get(MAX_ACCEPTS) {
            errors.push(error(
                "accepts",
                "at most 6 widget types",
                first_extra.clone(),
                "remove this type or another entry from dock.accepts",
            ));
        }
        if !(1..=32).contains(&self.max_rows) {
            errors.push(error(
                "max_rows",
                "1–32 rows",
                self.max_rows.to_string(),
                "set dock.max_rows to a value from 1 to 32",
            ));
        }
        if self.auto_clear_ms == 0 {
            errors.push(error(
                "auto_clear_ms",
                "a positive duration in milliseconds",
                "0".into(),
                "set the time to clear a widget after its last publish",
            ));
        }
    }

    /// Validate the winning bundles' authored dimensions before display scaling.
    /// A missing type and a loaded type with no common extent are distinct errors.
    pub fn validate_bundles(
        &self,
        row_height_token: Option<&str>,
        authored_extents: &HashMap<String, Option<(f32, f32)>>,
    ) -> Result<(), DockValidationErrors> {
        if self.accepts.is_empty() {
            return Ok(());
        }
        let row_height_dp = match row_height_token {
            Some(value) => crate::tokens::parse_numeric(value)
                .filter(|height| *height > 0.0)
                .ok_or_else(|| {
                    DockValidationErrors(vec![ConfigError {
                        code: ConfigErrorCode::TokenValueParseError,
                        field_path: format!("design_tokens.{ROW_HEIGHT_TOKEN}"),
                        expected: "a finite positive height in logical pixels".into(),
                        got: value.into(),
                        hint: format!("set {ROW_HEIGHT_TOKEN} to a positive number"),
                    }])
                })?,
            None => DEFAULT_ROW_HEIGHT_DP,
        };
        let height_limit = f64::from(row_height_dp) * (4.0 / 3.0);
        let mut errors = Vec::new();
        for name in &self.accepts {
            match authored_extents.get(name) {
                None => errors.push(ConfigError {
                    code: ConfigErrorCode::UnknownWidgetType,
                    ..error(
                        "accepts",
                        "a loaded widget bundle type",
                        name.clone(),
                        "use a widget type loaded from a bundle or a built-in",
                    )
                }),
                Some(Some((width, height)))
                    if width.is_finite() && *width > 0.0 && height.is_finite() && *height > 0.0 =>
                {
                    if f64::from(*height) * 0.75 > f64::from(row_height_dp) {
                        errors.push(error(
                            "accepts",
                            "natural height at most 4/3 of dock.row.height",
                            format!("{name} ({height} dp, limit {height_limit:.3} dp)"),
                            "remove the widget type or increase dock.row.height",
                        ));
                    }
                }
                Some(_) => errors.push(error(
                    "accepts",
                    "a common finite positive authored width and height across selected SVG layers",
                    name.clone(),
                    "give the selected SVG layers a matching viewBox or explicit width and height",
                )),
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(DockValidationErrors(errors))
        }
    }
}

fn error(field: &str, expected: &str, got: String, hint: &str) -> ConfigError {
    ConfigError {
        code: ConfigErrorCode::Other("CONFIG_INVALID_DOCK".into()),
        field_path: format!("dock.{field}"),
        expected: expected.into(),
        got,
        hint: hint.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{TzeHudConfig, raw::RawConfig};

    #[test]
    fn dock_is_optional_and_uses_finite_lifetime_defaults() {
        let base = "[runtime]\nprofile = 'full-display'\n";
        let raw: RawConfig = toml::from_str(base).unwrap();
        assert!(raw.dock.is_none());
        let raw: RawConfig = toml::from_str(&format!(
            "{base}[dock]\naccepts = ['progress-bar', 'countdown']\n"
        ))
        .unwrap();
        let dock = raw.dock.unwrap();
        assert_eq!(dock.accepts, ["progress-bar", "countdown"]);
        assert_eq!(dock.max_rows, 16);
        assert_eq!(dock.auto_clear_ms, DEFAULT_AUTO_CLEAR_MS);
        assert_eq!(
            crate::tokens::canonical_token(ROW_HEIGHT_TOKEN)
                .unwrap()
                .default_value,
            "52",
        );
        assert_eq!(
            crate::tokens::canonical_token(CAPTION_WIDTH_TOKEN)
                .unwrap()
                .default_value,
            "96",
        );
    }

    #[test]
    fn dock_validation_names_extra_bundle_and_rejects_invalid_limits() {
        let base = "[runtime]\nprofile = 'full-display'\n";
        let config = TzeHudConfig::parse(&format!(
            "{base}[dock]\naccepts = ['a', 'b', 'c', 'd', 'e', 'f', 'seventh']\nmax_rows = 0\nauto_clear_ms = 0"
        ))
        .unwrap();
        let errors = config.validate();
        assert!(
            errors
                .iter()
                .any(|e| e.field_path == "dock.accepts" && e.got == "seventh")
        );
        assert!(errors.iter().any(|e| e.field_path == "dock.max_rows"));
        assert!(errors.iter().any(|e| e.field_path == "dock.auto_clear_ms"));
        for (rows, accepted) in [(1, true), (32, true), (33, false)] {
            let config = TzeHudConfig::parse(&format!("{base}[dock]\nmax_rows = {rows}")).unwrap();
            assert_eq!(
                !config
                    .validate()
                    .iter()
                    .any(|e| e.field_path == "dock.max_rows"),
                accepted,
            );
        }
    }

    #[test]
    fn dock_validates_bundle_names_and_viewbox_height() {
        let dock = DockConfig {
            accepts: [
                "progress-bar",
                "unknown",
                "gauge",
                "status-indicator",
                "unmeasurable",
            ]
            .map(str::to_owned)
            .into(),
            ..Default::default()
        };
        let extents = HashMap::from([
            ("progress-bar".into(), Some((360.0, 52.0))),
            ("gauge".into(), Some((72.0, 224.0))),
            ("status-indicator".into(), Some((320.0, 124.0))),
            ("unmeasurable".into(), None),
        ]);
        let DockValidationErrors(errors) = dock.validate_bundles(None, &extents).unwrap_err();
        assert_eq!(errors.len(), 4);
        assert_eq!(errors[0].got, "unknown");
        assert_eq!(errors[0].code, ConfigErrorCode::UnknownWidgetType);
        for (error, name) in errors[1..3].iter().zip(["gauge", "status-indicator"]) {
            assert!(error.got.starts_with(name) && error.expected.contains("dock.row.height"));
        }
        assert_eq!(errors[3].got, "unmeasurable");
        assert_eq!(
            errors[3].code,
            ConfigErrorCode::Other("CONFIG_INVALID_DOCK".into())
        );
        assert!(errors[3].expected.contains("authored width and height"));
        for invalid in ["0", "-1", "NaN"] {
            let DockValidationErrors(errors) =
                dock.validate_bundles(Some(invalid), &extents).unwrap_err();
            assert_eq!(errors.len(), 1);
            assert_eq!(errors[0].code, ConfigErrorCode::TokenValueParseError);
            assert_eq!(errors[0].field_path, "design_tokens.dock.row.height");
        }
    }
}
