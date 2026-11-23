//! Owner configuration for the optional per-session widget dock.

use serde::{Deserialize, Serialize};
use tze_hud_scene::config::{ConfigError, ConfigErrorCode};

pub const ROW_HEIGHT_TOKEN: &str = "dock.row.height";
pub const CAPTION_WIDTH_TOKEN: &str = "dock.caption.width";
pub const DEFAULT_ROW_HEIGHT_DP: f32 = 52.0;
pub const DEFAULT_CAPTION_WIDTH_DP: f32 = 96.0;
pub const DEFAULT_AUTO_CLEAR_MS: u64 = 900_000;
const MAX_ACCEPTS: usize = 6;

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

    /// Validate against the natural height of the bundles selected at startup.
    /// The height is measured in SVG units before applying display scale.
    pub fn validate_bundles(
        &self,
        row_height_dp: f32,
        mut natural_height: impl FnMut(&str) -> Option<f32>,
    ) -> Vec<ConfigError> {
        let height_limit = row_height_dp * (4.0 / 3.0);
        let mut errors = Vec::new();
        for name in &self.accepts {
            match natural_height(name) {
                None => errors.push(ConfigError {
                    code: ConfigErrorCode::UnknownWidgetType,
                    ..error(
                        "accepts",
                        "a loaded widget bundle type",
                        name.clone(),
                        "use a widget type loaded from a bundle or a built-in",
                    )
                }),
                Some(height) if height > height_limit => errors.push(error(
                    "accepts",
                    "natural height at most 4/3 of dock.row.height",
                    format!("{name} ({height:.1} dp, limit {height_limit:.1} dp)"),
                    "remove the widget type or increase dock.row.height",
                )),
                Some(_) => {}
            }
        }
        errors
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
            accepts: ["progress-bar", "unknown", "gauge", "status-indicator"]
                .map(str::to_owned)
                .into(),
            ..Default::default()
        };
        let errors = dock.validate_bundles(DEFAULT_ROW_HEIGHT_DP, |name| match name {
            "progress-bar" => Some(52.0),
            "gauge" => Some(224.0),
            "status-indicator" => Some(124.0),
            _ => None,
        });
        assert_eq!(errors.len(), 3);
        assert_eq!(errors[0].got, "unknown");
        assert_eq!(errors[0].code, ConfigErrorCode::UnknownWidgetType);
        for (error, name) in errors[1..].iter().zip(["gauge", "status-indicator"]) {
            assert!(error.got.starts_with(name) && error.expected.contains("dock.row.height"));
        }
    }
}
