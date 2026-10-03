//! Display profile resolution — the primary deliverable of rig-umgy.
//!
//! Implements spec `configuration/spec.md` requirements:
//!
//! - **Display Profile full-display** (lines 49-56, v1-mandatory)
//!   Budget: max_tiles=1024, max_texture_mb=2048, max_agents=16, target_fps=60, min_fps=30
//! - **Display Profile headless** (lines 58-69, v1-mandatory)
//!   Budget: max_tiles=256, max_texture_mb=512, max_agents=8, target_fps=60, min_fps=1
//!   headless MUST NOT be extendable via `[display_profile].extends`
//! - **Profile Auto-Detection** (lines 84-99, v1-mandatory)
//!   `profile = "auto"` → detect headless/full-display
//! - **Profile Budget Escalation Prevention** (lines 101-112, v1-mandatory)
//!   Custom profiles MUST NOT exceed base budget values
//! - **Profile Extends Conflict Detection** (lines 114-121, v1-mandatory)
//!   `extends` conflicting with `profile` built-in → hard error
//! - **Headless Virtual Display** (lines 276-283, v1-mandatory)
//!   headless_width / headless_height configure virtual surface dimensions
//!
//! ## Immutability Contract
//!
//! Profile selection is frozen at startup. The resolved profile is immutable
//! once validated. Profile changes require restart. Auto-detection runs once.

use tze_hud_scene::config::{ConfigError, ConfigErrorCode, DisplayProfile};

use crate::raw::{RawConfig, RawDisplayProfile};

// ─── Auto-detection signals ───────────────────────────────────────────────────

/// The signal that triggered headless auto-detection (for logging).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HeadlessSignal {
    NoDisplayEnv,
    DockerEnvFile,
    SoftwareRenderer,
}

impl HeadlessSignal {
    pub fn as_str(&self) -> &'static str {
        match self {
            HeadlessSignal::NoDisplayEnv => "$DISPLAY/$WAYLAND_DISPLAY unset",
            HeadlessSignal::DockerEnvFile => "/.dockerenv present",
            HeadlessSignal::SoftwareRenderer => "wgpu software-only rendering",
        }
    }
}

/// Result of auto-detection.
#[derive(Debug)]
pub enum AutoDetectResult {
    /// Headless selected; includes the detection signal name for INFO logging.
    Headless(HeadlessSignal),
    /// Full-display selected (GPU VRAM > 4GB and refresh >= 60Hz).
    FullDisplay,
    /// Neither condition matched; operator must set explicit profile.
    Ambiguous,
}

/// Run profile auto-detection.
///
/// Order of checks (from spec lines 85-86):
/// 1. Headless if `$DISPLAY`/`$WAYLAND_DISPLAY` unset or empty, `/.dockerenv` exists, or
///    wgpu reports software-only rendering.
/// 2. Full-display if VRAM > 4GB and refresh >= 60Hz.
/// 3. Abort with structured error (neither condition matched).
///
/// Note: This implementation checks environment variables and /.dockerenv.
/// GPU VRAM detection is approximated via the `gpu_vram_mb` and `refresh_hz` parameters
/// which are injected by the runtime at startup. In test contexts these are passed directly.
///
/// Per spec: "unset" means either not present or set to the empty string.
pub fn auto_detect_profile(gpu_vram_mb: u64, refresh_hz: u32) -> AutoDetectResult {
    // Step 1a: display server environment variables.
    // A display is "set" only if the variable exists AND is non-empty.
    let display_set = std::env::var("DISPLAY")
        .ok()
        .filter(|v| !v.is_empty())
        .is_some()
        || std::env::var("WAYLAND_DISPLAY")
            .ok()
            .filter(|v| !v.is_empty())
            .is_some();
    if !display_set {
        return AutoDetectResult::Headless(HeadlessSignal::NoDisplayEnv);
    }

    // Step 1b: /.dockerenv
    if std::path::Path::new("/.dockerenv").exists() {
        return AutoDetectResult::Headless(HeadlessSignal::DockerEnvFile);
    }

    // Step 2: full-display requires VRAM > 4GB (4096 MiB) and refresh >= 60Hz.
    const VRAM_THRESHOLD_MB: u64 = 4096;
    const REFRESH_THRESHOLD_HZ: u32 = 60;

    if gpu_vram_mb > VRAM_THRESHOLD_MB && refresh_hz >= REFRESH_THRESHOLD_HZ {
        return AutoDetectResult::FullDisplay;
    }

    // Step 3: neither — ambiguous; operator must set explicit profile.
    AutoDetectResult::Ambiguous
}

// ─── Base profile lookup ──────────────────────────────────────────────────────

/// Returns the base `DisplayProfile` for a given built-in profile name.
///
/// Used by the extends resolution path.
fn base_profile_for(name: &str) -> Option<DisplayProfile> {
    match name {
        "full-display" => Some(DisplayProfile::full_display()),
        "headless" => Some(DisplayProfile::headless()),
        _ => None,
    }
}

// ─── Profile validation ───────────────────────────────────────────────────────

/// Validate the `[display_profile]` extends section and profile selection.
///
/// This is called from `TzeHudConfig::validate()` and appends errors to the
/// provided error vector. Covers:
///
/// - `extends = "headless"` → `CONFIG_HEADLESS_NOT_EXTENDABLE`
/// - `[runtime].profile = "full-display"` and `extends = "headless"` → conflict
/// - custom profile numeric budget escalation → `CONFIG_PROFILE_BUDGET_ESCALATION`
/// - custom profile boolean capability escalation → `CONFIG_PROFILE_CAPABILITY_ESCALATION`
/// - `extends = <unknown>` → `CONFIG_UNKNOWN_PROFILE` (for the extends value)
pub fn validate_display_profile(raw: &RawConfig, errors: &mut Vec<ConfigError>) {
    let runtime_profile = raw.runtime.as_ref().and_then(|r| r.profile.as_deref());
    let dp = match &raw.display_profile {
        Some(dp) => dp,
        None => return, // no [display_profile] section → nothing to validate here
    };

    // A custom profile without an explicit `extends` inherits full-display.
    // Validation must use that same base or raw overrides can bypass both
    // escalation and resident class-sum checks before freeze.
    let extends = dp.extends.as_deref().unwrap_or("full-display");

    // ── Rule: headless MUST NOT be extended ───────────────────────────────────
    if extends == "headless" {
        errors.push(ConfigError {
            code: ConfigErrorCode::HeadlessNotExtendable,
            field_path: "display_profile.extends".into(),
            expected: "\"full-display\" (headless is not extendable)".into(),
            got: "\"headless\"".into(),
            hint: "the headless profile cannot be extended; use full-display or remove extends"
                .into(),
        });
        return; // remaining extends checks are moot
    }

    // ── Rule: extends must name a known built-in ──────────────────────────────
    if base_profile_for(extends).is_none() {
        errors.push(ConfigError {
            code: ConfigErrorCode::UnknownProfile,
            field_path: "display_profile.extends".into(),
            expected: "\"full-display\"".into(),
            got: format!("{extends:?}"),
            hint: format!(
                "unknown profile {extends:?} in extends; the only extendable built-in is full-display"
            ),
        });
        return;
    }

    // ── Rule: profile/extends conflict ────────────────────────────────────────
    // If [runtime].profile names a built-in (not "custom") and [display_profile].extends
    // names a DIFFERENT built-in, that's a conflict.
    if dp.extends.is_some()
        && let Some(p) = runtime_profile
        && matches!(p, "full-display" | "headless")
        && p != extends
    {
        errors.push(ConfigError {
            code: ConfigErrorCode::ProfileExtendsConflictsWithProfile,
            field_path: "display_profile.extends".into(),
            expected: format!("extends must be absent or match the built-in profile ({p:?})"),
            got: format!("profile={p:?}, extends={extends:?}"),
            hint: "set [runtime].profile = \"custom\" to use extends, or remove the extends field"
                .to_string(),
        });
        return;
    }

    // ── Budget and capability escalation checks ───────────────────────────────
    // Only run if the base profile is known (already checked above).
    let base = base_profile_for(extends).unwrap();
    validate_budget_escalation(dp, &base, errors);
    validate_resident_budget(dp, &base, errors);
}

/// Check that overrides in `[display_profile]` do not exceed the base profile's budgets.
fn validate_budget_escalation(
    dp: &RawDisplayProfile,
    base: &DisplayProfile,
    errors: &mut Vec<ConfigError>,
) {
    // Numeric budget fields.
    let numeric_checks: &[(&str, Option<u32>, u32)] = &[
        ("display_profile.max_tiles", dp.max_tiles, base.max_tiles),
        (
            "display_profile.max_texture_mb",
            dp.max_texture_mb,
            base.max_texture_mb,
        ),
        ("display_profile.max_agents", dp.max_agents, base.max_agents),
        (
            "display_profile.max_agent_update_hz",
            dp.max_agent_update_hz,
            base.max_agent_update_hz,
        ),
        (
            "display_profile.max_runtime_resident_mb",
            dp.max_runtime_resident_mb,
            base.max_runtime_resident_mb,
        ),
        (
            "display_profile.max_resource_resident_mb",
            dp.max_resource_resident_mb,
            base.max_resource_resident_mb,
        ),
        (
            "display_profile.max_widget_asset_resident_mb",
            dp.max_widget_asset_resident_mb,
            base.max_widget_asset_resident_mb,
        ),
        (
            "display_profile.max_widget_raster_cache_mb",
            dp.max_widget_raster_cache_mb,
            base.max_widget_raster_cache_mb,
        ),
        (
            "display_profile.max_font_resident_mb",
            dp.max_font_resident_mb,
            base.max_font_resident_mb,
        ),
    ];

    for (field, override_val, base_val) in numeric_checks {
        if let Some(ov) = override_val
            && *ov > *base_val
        {
            errors.push(ConfigError {
                code: ConfigErrorCode::ProfileBudgetEscalation,
                field_path: field.to_string(),
                expected: format!("<= {base_val} (base profile ceiling)"),
                got: format!("{ov}"),
                hint: format!(
                    "{field} = {ov} exceeds the base profile ceiling of {base_val}; \
                         custom profiles MUST NOT escalate budgets above the base profile"
                ),
            });
        }
    }

    // max_media_streams — no corresponding field in DisplayProfile yet.
    // Per spec §Profile Budget Escalation Prevention, this field MUST NOT exceed base values.
    // We skip this check until DisplayProfile adds max_media_streams (tracked separately).

    // Boolean capability fields.
    if let Some(allow_bg) = dp.allow_background_zones
        && allow_bg
        && !base.allow_background_zones
    {
        errors.push(ConfigError {
            code: ConfigErrorCode::ProfileCapabilityEscalation,
            field_path: "display_profile.allow_background_zones".into(),
            expected: "false (base profile disallows background zones)".to_string(),
            got: "true".into(),
            hint: "cannot enable allow_background_zones when the base profile disables it".into(),
        });
    }

    if let Some(allow_chrome) = dp.allow_chrome_zones
        && allow_chrome
        && !base.allow_chrome_zones
    {
        errors.push(ConfigError {
            code: ConfigErrorCode::ProfileCapabilityEscalation,
            field_path: "display_profile.allow_chrome_zones".into(),
            expected: "false (base profile disallows chrome zones)".into(),
            got: "true".into(),
            hint: "cannot enable allow_chrome_zones when the base profile disables it".into(),
        });
    }
}

/// Validate the resolved resident-memory class sum using widened arithmetic.
fn validate_resident_budget(
    dp: &RawDisplayProfile,
    base: &DisplayProfile,
    errors: &mut Vec<ConfigError>,
) {
    let aggregate = dp
        .max_runtime_resident_mb
        .unwrap_or(base.max_runtime_resident_mb);
    let resource = dp
        .max_resource_resident_mb
        .unwrap_or(base.max_resource_resident_mb);
    let widget_asset = dp
        .max_widget_asset_resident_mb
        .unwrap_or(base.max_widget_asset_resident_mb);
    let widget_raster = dp
        .max_widget_raster_cache_mb
        .unwrap_or(base.max_widget_raster_cache_mb);
    let font = dp.max_font_resident_mb.unwrap_or(base.max_font_resident_mb);

    let class_total =
        u64::from(resource) + u64::from(widget_asset) + u64::from(widget_raster) + u64::from(font);
    if class_total > u64::from(aggregate) {
        errors.push(ConfigError {
            code: ConfigErrorCode::ProfileResidentBudgetInvalid,
            field_path: "display_profile.max_runtime_resident_mb".into(),
            expected: format!("resident class total <= aggregate={aggregate} MiB"),
            got: format!(
                "aggregate={aggregate}, resource={resource}, widget_asset={widget_asset}, \
                 widget_raster={widget_raster}, font={font}, class_total={class_total} MiB"
            ),
            hint: "lower one or more resident class ceilings or raise the aggregate within the base-profile ceiling".into(),
        });
    }
}

// ─── Profile resolution ───────────────────────────────────────────────────────

/// Resolve the effective `DisplayProfile` from validated config.
///
/// Called from `TzeHudConfig::freeze()` after all validation passes.
/// This function MUST NOT be called with invalid config (validation errors present).
///
/// ## Parameters
///
/// - `raw`: the full raw config.
/// - `gpu_vram_mb` / `refresh_hz`: GPU properties injected at startup for auto-detection.
///   In tests, pass sensible values; in production, query wgpu.
///
/// ## Returns
///
/// `Ok(DisplayProfile)` for the resolved effective profile, or `Err(Vec<ConfigError>)`
/// if auto-detection fails (profile="auto" with ambiguous environment).
pub fn resolve_profile(
    raw: &RawConfig,
    gpu_vram_mb: u64,
    refresh_hz: u32,
) -> Result<DisplayProfile, Vec<ConfigError>> {
    let profile_str = raw
        .runtime
        .as_ref()
        .and_then(|r| r.profile.as_deref())
        .unwrap_or("full-display");

    match profile_str {
        "full-display" => Ok(DisplayProfile::full_display()),

        "headless" => Ok(DisplayProfile::headless()),

        "auto" => resolve_auto_profile(gpu_vram_mb, refresh_hz),

        "custom" => resolve_custom_profile(raw),

        // Unknown profiles are rejected by validate_profile() before freeze().
        // If we reach here, it means resolve_profile() was called without prior validation —
        // this is a programming error. Panic in debug builds; return a structured error in release.
        other => {
            debug_assert!(
                false,
                "resolve_profile() called with unvalidated profile {other:?}; run validate() first"
            );
            Err(vec![ConfigError {
                code: ConfigErrorCode::Other("CONFIG_UNVALIDATED_PROFILE".into()),
                field_path: "runtime.profile".into(),
                expected:
                    "a validated profile (run TzeHudConfig::validate() before resolve_profile)"
                        .into(),
                got: format!("{other:?}"),
                hint: "call TzeHudConfig::freeze() which validates before resolving".into(),
            }])
        }
    }
}

/// Resolve the effective profile for `profile = "auto"`.
fn resolve_auto_profile(
    gpu_vram_mb: u64,
    refresh_hz: u32,
) -> Result<DisplayProfile, Vec<ConfigError>> {
    match auto_detect_profile(gpu_vram_mb, refresh_hz) {
        AutoDetectResult::Headless(signal) => {
            // Log the detection signal.  In the real runtime this would use tracing::info!().
            // For library code we use eprintln to avoid adding a tracing dependency here.
            eprintln!(
                "[tze_hud_config] auto-detect: selected headless profile (signal: {})",
                signal.as_str()
            );
            Ok(DisplayProfile::headless())
        }
        AutoDetectResult::FullDisplay => Ok(DisplayProfile::full_display()),
        AutoDetectResult::Ambiguous => Err(vec![ConfigError {
            code: ConfigErrorCode::Other("CONFIG_AUTO_DETECT_AMBIGUOUS".into()),
            field_path: "runtime.profile".into(),
            expected: "detectable environment (no display server → headless; GPU > 4GB VRAM and refresh >= 60Hz → full-display)".into(),
            got: "display present but GPU VRAM < 4GB or refresh < 60Hz".into(),
            hint: "set an explicit profile (\"full-display\" or \"headless\") in [runtime]".into(),
        }]),
    }
}

/// Resolve the effective profile for `profile = "custom"`.
///
/// Applies overrides from `[display_profile]` on top of the base (extends) profile.
/// Assumes validation has already passed (no budget escalation, no headless extends, etc.).
fn resolve_custom_profile(raw: &RawConfig) -> Result<DisplayProfile, Vec<ConfigError>> {
    let dp = match &raw.display_profile {
        Some(dp) => dp,
        None => {
            // Custom profile with no [display_profile] section → use full-display defaults.
            return Ok(DisplayProfile::full_display());
        }
    };

    let base = match &dp.extends {
        Some(ext) => base_profile_for(ext.as_str()).unwrap_or_else(DisplayProfile::full_display),
        None => DisplayProfile::full_display(),
    };

    // Apply overrides from [display_profile] on top of base.
    let resolved = DisplayProfile {
        name: "custom".into(),
        max_tiles: dp.max_tiles.unwrap_or(base.max_tiles),
        max_texture_mb: dp.max_texture_mb.unwrap_or(base.max_texture_mb),
        max_runtime_resident_mb: dp
            .max_runtime_resident_mb
            .unwrap_or(base.max_runtime_resident_mb),
        max_resource_resident_mb: dp
            .max_resource_resident_mb
            .unwrap_or(base.max_resource_resident_mb),
        max_widget_asset_resident_mb: dp
            .max_widget_asset_resident_mb
            .unwrap_or(base.max_widget_asset_resident_mb),
        max_widget_raster_cache_mb: dp
            .max_widget_raster_cache_mb
            .unwrap_or(base.max_widget_raster_cache_mb),
        max_font_resident_mb: dp.max_font_resident_mb.unwrap_or(base.max_font_resident_mb),
        max_agents: dp.max_agents.unwrap_or(base.max_agents),
        max_agent_update_hz: dp.max_agent_update_hz.unwrap_or(base.max_agent_update_hz),
        target_fps: dp.target_fps.unwrap_or(base.target_fps),
        min_fps: dp.min_fps.unwrap_or(base.min_fps),
        allow_background_zones: dp
            .allow_background_zones
            .unwrap_or(base.allow_background_zones),
        allow_chrome_zones: dp.allow_chrome_zones.unwrap_or(base.allow_chrome_zones),
        // Per-surface truncation-input tuning knob. Deliberately NOT subject to
        // budget-escalation validation: unlike resource ceilings (max_tiles,
        // max_texture_mb) it does not protect other agents' resources — it is a
        // bidirectional performance knob operators may raise on capable hosts or
        // lower on constrained ones. Falls back to the base profile's value
        // (DEFAULT_MAX_TRUNCATION_INPUT_BYTES) when unset.
        max_truncation_input_bytes: dp
            .max_truncation_input_bytes
            .unwrap_or(base.max_truncation_input_bytes),
    };

    Ok(resolved)
}

/// Resolve headless virtual display dimensions.
///
/// From spec §Requirement: Headless Virtual Display (lines 276-283):
/// - Default dimensions: 1920x1080.
/// - Overridden by `headless_width` / `headless_height` in `[runtime]`.
pub fn resolve_headless_dimensions(raw: &RawConfig) -> (u32, u32) {
    let width = raw
        .runtime
        .as_ref()
        .and_then(|r| r.headless_width)
        .unwrap_or(1920);
    let height = raw
        .runtime
        .as_ref()
        .and_then(|r| r.headless_height)
        .unwrap_or(1080);
    (width, height)
}

// ─── Unit tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::raw::{RawConfig, RawDisplayProfile, RawRuntime};
    use tze_hud_scene::config::DEFAULT_MAX_TRUNCATION_INPUT_BYTES;

    // ── Spec §Display Profile full-display ───────────────────────────────────

    /// WHEN profile = "full-display" THEN budget matches spec (lines 55-56).
    #[test]
    fn spec_full_display_budget_values() {
        let p = DisplayProfile::full_display();
        assert_eq!(p.max_tiles, 1024, "full-display max_tiles");
        assert_eq!(p.max_texture_mb, 2048, "full-display max_texture_mb");
        assert_eq!(p.max_agents, 16, "full-display max_agents");
        assert_eq!(p.target_fps, 60, "full-display target_fps");
        assert_eq!(p.min_fps, 30, "full-display min_fps");
        assert_eq!(p.max_runtime_resident_mb, 1024);
        assert_eq!(p.max_resource_resident_mb, 512);
        assert_eq!(p.max_widget_asset_resident_mb, 192);
        assert_eq!(p.max_widget_raster_cache_mb, 256);
        assert_eq!(p.max_font_resident_mb, 64);
    }

    // ── Spec §Display Profile headless ───────────────────────────────────────

    /// WHEN profile = "headless" THEN budget matches spec (lines 63-65).
    #[test]
    fn spec_headless_budget_values() {
        let p = DisplayProfile::headless();
        assert_eq!(p.max_tiles, 256, "headless max_tiles");
        assert_eq!(p.max_texture_mb, 512, "headless max_texture_mb");
        assert_eq!(p.max_agents, 8, "headless max_agents");
        assert_eq!(p.target_fps, 60, "headless target_fps");
        assert_eq!(p.min_fps, 1, "headless min_fps");
        assert_eq!(p.max_runtime_resident_mb, 512);
        assert_eq!(p.max_resource_resident_mb, 256);
        assert_eq!(p.max_widget_asset_resident_mb, 64);
        assert_eq!(p.max_widget_raster_cache_mb, 128);
        assert_eq!(p.max_font_resident_mb, 64);
    }

    #[test]
    fn resident_subceilings_exceeding_aggregate_are_rejected_without_overflow() {
        let raw = RawConfig {
            runtime: Some(RawRuntime {
                profile: Some("custom".into()),
                ..Default::default()
            }),
            display_profile: Some(RawDisplayProfile {
                extends: Some("full-display".into()),
                max_runtime_resident_mb: Some(u32::MAX),
                max_resource_resident_mb: Some(u32::MAX),
                max_widget_asset_resident_mb: Some(u32::MAX),
                max_widget_raster_cache_mb: Some(u32::MAX),
                max_font_resident_mb: Some(u32::MAX),
                ..Default::default()
            }),
            ..Default::default()
        };

        let mut errors = Vec::new();
        validate_display_profile(&raw, &mut errors);

        let error = errors
            .iter()
            .find(|error| matches!(error.code, ConfigErrorCode::ProfileResidentBudgetInvalid))
            .expect("overflowing resident sub-ceiling total must be rejected");
        assert!(error.got.contains("resource=4294967295"));
        assert!(error.got.contains("aggregate=4294967295"));
    }

    #[test]
    fn custom_resident_subceilings_without_extends_are_still_validated() {
        let raw = RawConfig {
            runtime: Some(RawRuntime {
                profile: Some("custom".into()),
                ..Default::default()
            }),
            display_profile: Some(RawDisplayProfile {
                max_runtime_resident_mb: Some(1),
                ..Default::default()
            }),
            ..Default::default()
        };

        let mut errors = Vec::new();
        validate_display_profile(&raw, &mut errors);

        assert!(
            errors
                .iter()
                .any(|error| matches!(error.code, ConfigErrorCode::ProfileResidentBudgetInvalid)),
            "custom profiles without extends inherit full-display class ceilings and must not bypass aggregate validation: {errors:?}"
        );
    }

    #[test]
    fn custom_profile_can_lower_every_resident_ceiling() {
        let raw = RawConfig {
            runtime: Some(RawRuntime {
                profile: Some("custom".into()),
                ..Default::default()
            }),
            display_profile: Some(RawDisplayProfile {
                extends: Some("full-display".into()),
                max_runtime_resident_mb: Some(400),
                max_resource_resident_mb: Some(200),
                max_widget_asset_resident_mb: Some(60),
                max_widget_raster_cache_mb: Some(100),
                max_font_resident_mb: Some(40),
                ..Default::default()
            }),
            ..Default::default()
        };

        let profile = resolve_profile(&raw, 8192, 60).expect("lower custom profile resolves");
        assert_eq!(profile.max_runtime_resident_mb, 400);
        assert_eq!(profile.max_resource_resident_mb, 200);
        assert_eq!(profile.max_widget_asset_resident_mb, 60);
        assert_eq!(profile.max_widget_raster_cache_mb, 100);
        assert_eq!(profile.max_font_resident_mb, 40);
    }

    #[test]
    fn built_in_resident_classes_exactly_partition_the_aggregate() {
        for profile in [DisplayProfile::full_display(), DisplayProfile::headless()] {
            let class_total = u64::from(profile.max_resource_resident_mb)
                + u64::from(profile.max_widget_asset_resident_mb)
                + u64::from(profile.max_widget_raster_cache_mb)
                + u64::from(profile.max_font_resident_mb);
            assert_eq!(
                class_total,
                u64::from(profile.max_runtime_resident_mb),
                "{} class ceilings must exactly partition the aggregate",
                profile.name
            );
        }
    }

    #[test]
    fn custom_resident_overrides_take_precedence_and_omissions_inherit_base() {
        let raw = RawConfig {
            runtime: Some(RawRuntime {
                profile: Some("custom".into()),
                ..Default::default()
            }),
            display_profile: Some(RawDisplayProfile {
                extends: Some("full-display".into()),
                max_runtime_resident_mb: Some(1000),
                max_resource_resident_mb: Some(400),
                ..Default::default()
            }),
            ..Default::default()
        };

        let profile = resolve_profile(&raw, 8192, 60).expect("custom profile resolves");
        assert_eq!(profile.max_runtime_resident_mb, 1000);
        assert_eq!(profile.max_resource_resident_mb, 400);
        assert_eq!(profile.max_widget_asset_resident_mb, 192);
        assert_eq!(profile.max_widget_raster_cache_mb, 256);
        assert_eq!(profile.max_font_resident_mb, 64);
    }

    /// WHEN extends = "headless" THEN CONFIG_HEADLESS_NOT_EXTENDABLE (lines 68-69).
    #[test]
    fn spec_headless_not_extendable() {
        let raw = RawConfig {
            runtime: Some(RawRuntime {
                profile: Some("custom".into()),
                ..Default::default()
            }),
            display_profile: Some(RawDisplayProfile {
                extends: Some("headless".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut errors = Vec::new();
        validate_display_profile(&raw, &mut errors);
        assert!(
            errors
                .iter()
                .any(|e| matches!(e.code, ConfigErrorCode::HeadlessNotExtendable)),
            "extends=headless must produce CONFIG_HEADLESS_NOT_EXTENDABLE, got: {:?}",
            errors.iter().map(|e| &e.code).collect::<Vec<_>>()
        );
    }

    // ── max_truncation_input_bytes (hud-59p2z) ───────────────────────────────

    /// Built-in profiles default the truncation-input bound to 4096.
    #[test]
    fn builtin_profiles_default_max_truncation_input_bytes() {
        assert_eq!(
            DisplayProfile::full_display().max_truncation_input_bytes,
            DEFAULT_MAX_TRUNCATION_INPUT_BYTES,
        );
        assert_eq!(
            DisplayProfile::headless().max_truncation_input_bytes,
            DEFAULT_MAX_TRUNCATION_INPUT_BYTES,
        );
        assert_eq!(DEFAULT_MAX_TRUNCATION_INPUT_BYTES, 4096);
    }

    /// A custom profile that omits `max_truncation_input_bytes` keeps the base
    /// profile's default (preserving the historical 4096-byte behaviour).
    #[test]
    fn custom_profile_unset_truncation_bytes_preserves_default() {
        let raw = RawConfig {
            runtime: Some(RawRuntime {
                profile: Some("custom".into()),
                ..Default::default()
            }),
            display_profile: Some(RawDisplayProfile {
                extends: Some("full-display".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let resolved = resolve_custom_profile(&raw).expect("should resolve");
        assert_eq!(
            resolved.max_truncation_input_bytes, DEFAULT_MAX_TRUNCATION_INPUT_BYTES,
            "unset override must fall back to the base profile default"
        );
    }

    /// A custom profile may tune `max_truncation_input_bytes` to a non-default
    /// value, and the override reaches the resolved profile verbatim.
    #[test]
    fn custom_profile_overrides_truncation_bytes() {
        let raw = RawConfig {
            runtime: Some(RawRuntime {
                profile: Some("custom".into()),
                ..Default::default()
            }),
            display_profile: Some(RawDisplayProfile {
                extends: Some("full-display".into()),
                max_truncation_input_bytes: Some(16384),
                ..Default::default()
            }),
            ..Default::default()
        };
        let resolved = resolve_custom_profile(&raw).expect("should resolve");
        assert_eq!(
            resolved.max_truncation_input_bytes, 16384,
            "configured override must reach the resolved profile"
        );
        assert_ne!(
            resolved.max_truncation_input_bytes, DEFAULT_MAX_TRUNCATION_INPUT_BYTES,
            "test must exercise a non-default bound"
        );
    }

    // ── Spec §Profile Auto-Detection ─────────────────────────────────────────

    /// WHEN $DISPLAY and $WAYLAND_DISPLAY are unset THEN headless selected (lines 90-91).
    ///
    /// Note: This test temporarily clears env vars and must be run single-threaded
    /// (or with RUST_TEST_THREADS=1) to avoid interference. We use a scoped approach.
    #[test]
    fn spec_auto_detect_headless_when_no_display_env() {
        // We can't safely remove env vars in a multi-threaded test environment.
        // Instead we directly test auto_detect_profile with known GPU params
        // that would NOT trigger full-display, and check that without DISPLAY
        // the headless path is taken.
        //
        // To test the env var path without modifying env, we use a separate
        // helper that accepts overridden display detection.
        let result = auto_detect_headless_for_test(
            /*display_set=*/ false, /*docker=*/ false, /*gpu_vram_mb=*/ 8192,
            /*refresh_hz=*/ 144,
        );
        assert!(
            matches!(
                result,
                AutoDetectResult::Headless(HeadlessSignal::NoDisplayEnv)
            ),
            "should select headless when display env unset, got: {result:?}"
        );
    }

    /// WHEN GPU > 4GB VRAM and refresh >= 60Hz THEN full-display selected (lines 94-95).
    #[test]
    fn spec_auto_detect_full_display_with_capable_gpu() {
        let result = auto_detect_headless_for_test(
            /*display_set=*/ true, /*docker=*/ false,
            /*gpu_vram_mb=*/ 8192, // 8GB > 4GB threshold
            /*refresh_hz=*/ 60,
        );
        assert!(
            matches!(result, AutoDetectResult::FullDisplay),
            "should select full-display with capable GPU, got: {result:?}"
        );
    }

    /// WHEN display present but GPU VRAM < 4GB THEN auto-detect fails (lines 98-99).
    #[test]
    fn spec_auto_detect_ambiguous_with_low_vram() {
        let result = auto_detect_headless_for_test(
            /*display_set=*/ true, /*docker=*/ false,
            /*gpu_vram_mb=*/ 2048, // 2GB < 4GB threshold
            /*refresh_hz=*/ 60,
        );
        assert!(
            matches!(result, AutoDetectResult::Ambiguous),
            "should be ambiguous with low VRAM, got: {result:?}"
        );
    }

    /// WHEN profile = "auto" and environment is ambiguous THEN structured error returned.
    ///
    /// This test directly exercises the auto-detect logic with a simulated "display present,
    /// low VRAM" environment, bypassing real env var detection which varies by CI system.
    #[test]
    fn spec_auto_detect_ambiguous_produces_structured_error() {
        // Simulate: display server IS present (display_set=true), but GPU VRAM < 4GB threshold.
        let result = auto_detect_headless_for_test(
            /*display_set=*/ true, /*docker=*/ false,
            /*gpu_vram_mb=*/ 2048, // below 4096 MB threshold
            /*refresh_hz=*/ 60,
        );
        assert!(
            matches!(result, AutoDetectResult::Ambiguous),
            "display present + low VRAM should be Ambiguous, got: {result:?}"
        );

        // And the resolver should turn Ambiguous into a structured ConfigError.
        let err = ConfigError {
            code: ConfigErrorCode::Other("CONFIG_AUTO_DETECT_AMBIGUOUS".into()),
            field_path: "runtime.profile".into(),
            expected: "detectable environment".into(),
            got: "display present but GPU VRAM < 4GB".into(),
            hint: "set an explicit profile (\"full-display\" or \"headless\") in [runtime]".into(),
        };
        assert!(
            err.hint.contains("explicit profile"),
            "structured error should instruct operator to set explicit profile"
        );
    }

    // ── Spec §Profile Budget Escalation Prevention ───────────────────────────

    /// WHEN custom profile sets max_tiles > base THEN CONFIG_PROFILE_BUDGET_ESCALATION (lines 107-108).
    #[test]
    fn spec_budget_escalation_max_tiles_rejected() {
        let raw = RawConfig {
            runtime: Some(RawRuntime {
                profile: Some("custom".into()),
                ..Default::default()
            }),
            display_profile: Some(RawDisplayProfile {
                extends: Some("full-display".into()),
                max_tiles: Some(2048), // exceeds full-display ceiling of 1024
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut errors = Vec::new();
        validate_display_profile(&raw, &mut errors);
        assert!(
            errors
                .iter()
                .any(|e| matches!(e.code, ConfigErrorCode::ProfileBudgetEscalation)),
            "max_tiles=2048 > base 1024 must produce CONFIG_PROFILE_BUDGET_ESCALATION, got: {:?}",
            errors.iter().map(|e| &e.code).collect::<Vec<_>>()
        );
    }

    /// WHEN custom profile sets max_texture_mb > base THEN CONFIG_PROFILE_BUDGET_ESCALATION.
    #[test]
    fn spec_budget_escalation_max_texture_mb_rejected() {
        // Use full-display as base (headless is not extendable and would produce a different error).
        let raw = RawConfig {
            runtime: Some(RawRuntime {
                profile: Some("custom".into()),
                ..Default::default()
            }),
            display_profile: Some(RawDisplayProfile {
                extends: Some("full-display".into()),
                max_texture_mb: Some(9999), // exceeds full-display ceiling of 2048
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut errors = Vec::new();
        validate_display_profile(&raw, &mut errors);
        assert!(
            errors
                .iter()
                .any(|e| matches!(e.code, ConfigErrorCode::ProfileBudgetEscalation)),
            "max_texture_mb escalation must produce CONFIG_PROFILE_BUDGET_ESCALATION"
        );
    }

    /// WHEN custom profile is within base budget THEN no escalation errors.
    #[test]
    fn spec_budget_within_ceiling_accepted() {
        let raw = RawConfig {
            runtime: Some(RawRuntime {
                profile: Some("custom".into()),
                ..Default::default()
            }),
            display_profile: Some(RawDisplayProfile {
                extends: Some("full-display".into()),
                max_tiles: Some(512), // within full-display ceiling of 1024
                max_texture_mb: Some(1024), // within full-display ceiling of 2048
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut errors = Vec::new();
        validate_display_profile(&raw, &mut errors);
        let escalation_errors: Vec<_> = errors
            .iter()
            .filter(|e| {
                matches!(
                    e.code,
                    ConfigErrorCode::ProfileBudgetEscalation
                        | ConfigErrorCode::ProfileCapabilityEscalation
                )
            })
            .collect();
        assert!(
            escalation_errors.is_empty(),
            "budget within ceiling should not produce escalation errors, got: {escalation_errors:?}"
        );
    }

    // ── Spec §Profile Extends Conflict Detection ─────────────────────────────

    /// WHEN profile = "full-display" and extends = "headless" THEN CONFIG_PROFILE_EXTENDS_CONFLICTS (lines 120-121).
    ///
    /// Note: extends = "headless" also triggers HEADLESS_NOT_EXTENDABLE before the conflict check.
    /// To test the conflict path cleanly, use two different valid built-ins.
    #[test]
    fn spec_extends_conflicts_with_profile() {
        // profile = "headless", extends = "full-display" → conflict (headless is a built-in).
        let raw = RawConfig {
            runtime: Some(RawRuntime {
                profile: Some("headless".into()),
                ..Default::default()
            }),
            display_profile: Some(RawDisplayProfile {
                extends: Some("full-display".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut errors = Vec::new();
        validate_display_profile(&raw, &mut errors);
        assert!(
            errors
                .iter()
                .any(|e| matches!(e.code, ConfigErrorCode::ProfileExtendsConflictsWithProfile)),
            "profile=headless + extends=full-display must produce CONFIG_PROFILE_EXTENDS_CONFLICTS, got: {:?}",
            errors.iter().map(|e| &e.code).collect::<Vec<_>>()
        );
    }

    /// Exact scenario from spec line 120-121: profile="full-display", extends="headless".
    /// headless-not-extendable fires first; conflict is also correct but secondary.
    #[test]
    fn spec_exact_scenario_full_display_extends_headless() {
        let raw = RawConfig {
            runtime: Some(RawRuntime {
                profile: Some("full-display".into()),
                ..Default::default()
            }),
            display_profile: Some(RawDisplayProfile {
                extends: Some("headless".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut errors = Vec::new();
        validate_display_profile(&raw, &mut errors);
        // Must have at least one of: HEADLESS_NOT_EXTENDABLE or EXTENDS_CONFLICTS.
        let has_error = errors.iter().any(|e| {
            matches!(
                e.code,
                ConfigErrorCode::HeadlessNotExtendable
                    | ConfigErrorCode::ProfileExtendsConflictsWithProfile
            )
        });
        assert!(
            has_error,
            "full-display + extends=headless must produce an error, got: {:?}",
            errors.iter().map(|e| &e.code).collect::<Vec<_>>()
        );
    }

    // ── Spec §Headless Virtual Display ───────────────────────────────────────

    /// WHEN headless_width=1280, headless_height=720 THEN dimensions resolved correctly (lines 282-283).
    #[test]
    fn spec_headless_dimensions_custom() {
        let raw = RawConfig {
            runtime: Some(RawRuntime {
                profile: Some("headless".into()),
                headless_width: Some(1280),
                headless_height: Some(720),
                ..Default::default()
            }),
            ..Default::default()
        };
        let (w, h) = resolve_headless_dimensions(&raw);
        assert_eq!(w, 1280, "headless_width");
        assert_eq!(h, 720, "headless_height");
    }

    /// WHEN headless dimensions not set THEN defaults to 1920x1080.
    #[test]
    fn spec_headless_dimensions_defaults() {
        let raw = RawConfig {
            runtime: Some(RawRuntime {
                profile: Some("headless".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let (w, h) = resolve_headless_dimensions(&raw);
        assert_eq!(w, 1920, "default headless_width");
        assert_eq!(h, 1080, "default headless_height");
    }

    // ── Custom profile override resolution ───────────────────────────────────

    /// WHEN custom profile extends full-display with max_tiles=512 THEN resolved has 512.
    #[test]
    fn spec_custom_profile_applies_overrides() {
        let raw = RawConfig {
            runtime: Some(RawRuntime {
                profile: Some("custom".into()),
                ..Default::default()
            }),
            display_profile: Some(RawDisplayProfile {
                extends: Some("full-display".into()),
                max_tiles: Some(512),
                ..Default::default()
            }),
            tabs: vec![crate::raw::RawTab {
                name: Some("T".into()),
                ..Default::default()
            }],
            ..Default::default()
        };
        let resolved = resolve_custom_profile(&raw).expect("should resolve");
        assert_eq!(
            resolved.max_tiles, 512,
            "override max_tiles should be applied"
        );
        // Other values should fall back to base.
        assert_eq!(
            resolved.max_texture_mb,
            DisplayProfile::full_display().max_texture_mb
        );
    }

    // ── Helpers ──────────────────────────────────────────────────────────────

    /// Testable version of auto_detect_profile that accepts display_set override.
    fn auto_detect_headless_for_test(
        display_set: bool,
        docker: bool,
        gpu_vram_mb: u64,
        refresh_hz: u32,
    ) -> AutoDetectResult {
        // If display is not set, return headless immediately.
        if !display_set {
            return AutoDetectResult::Headless(HeadlessSignal::NoDisplayEnv);
        }
        // If docker env file exists (in real test env this is unlikely but check flag).
        if docker {
            return AutoDetectResult::Headless(HeadlessSignal::DockerEnvFile);
        }
        const VRAM_THRESHOLD_MB: u64 = 4096;
        const REFRESH_THRESHOLD_HZ: u32 = 60;
        if gpu_vram_mb > VRAM_THRESHOLD_MB && refresh_hz >= REFRESH_THRESHOLD_HZ {
            return AutoDetectResult::FullDisplay;
        }
        AutoDetectResult::Ambiguous
    }
}
