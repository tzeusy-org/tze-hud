//! Scene startup: design tokens, config tabs, widget bundles, zone policies.
//!
//! Runs once before sessions are accepted:
//!
//! 1. Resolve canonical fallbacks → selected theme → `[design_tokens]`
//!    overrides → global token map.
//! 2. Materialize every `[[tabs]]` entry as a scene tab.
//! 3. Load widget bundles (SVG `{{token.key}}` placeholders resolved against
//!    the global token map), validate the dock against the winning bundles,
//!    and register widget definitions and instances.
//! 4. Fill each built-in zone's `RenderingPolicy` from the global tokens and
//!    install the resulting zone registry in the scene.
//!
//! Tokens must be resolved before any SVG is loaded (step 3).

use std::collections::HashMap;
use std::path::Path;

use tze_hud_config::dock::{
    CAPTION_WIDTH_TOKEN, DEFAULT_CAPTION_WIDTH_DP, DEFAULT_ROW_HEIGHT_DP, DockConfig,
    DockValidationErrors, ROW_HEIGHT_TOKEN,
};
use tze_hud_config::raw::RawConfig;
use tze_hud_config::themes::resolve_config_tokens;
use tze_hud_config::tokens::{DesignTokenMap, OsPreferences};
use tze_hud_scene::Rect;
use tze_hud_scene::dock::{DockLayout, DockMetrics, DockProjection};
use tze_hud_scene::graph::SceneGraph;
use tze_hud_scene::types::ZoneRegistry;

use crate::widget_startup::init_widget_registry;

/// Result of [`run_scene_startup`]. The zone and widget registries are
/// installed in the scene; what remains is for the compositor.
pub struct SceneStartupResult {
    /// Fully resolved global token map (canonical fallbacks, theme, config overrides).
    /// Pass to `compositor.set_token_map()`.
    pub global_tokens: DesignTokenMap,
    /// SVG assets from widget bundles for compositor registration.
    pub widget_svg_assets: Vec<crate::widget_startup::WidgetSvgAsset>,
    /// Nonfatal diagnostics from the same widget loading pass, without paths/content.
    pub startup_warnings: tze_hud_scene::startup_diagnostics::StartupWarnings,
    pub(crate) dock: Option<DockPolicy>,
}

/// Startup eligibility paired with the exact catalog used to validate it.
pub(crate) struct DockPolicy {
    pub(crate) config: DockConfig,
    pub(crate) extents: crate::widget_startup::DockExtents,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct DockDisplay {
    pub(crate) target: String,
    pub(crate) rect: Rect,
    pub(crate) scale: f64,
}

impl DockPolicy {
    pub(crate) fn project(
        &self,
        scene: &mut SceneGraph,
        tokens: &DesignTokenMap,
        display: Option<&DockDisplay>,
    ) -> bool {
        let Some(auto_clear) = std::num::NonZeroU64::new(self.config.auto_clear_ms) else {
            return false;
        };
        let projection = display.and_then(|display| {
            let px = display.scale as f32 * tze_hud_config::tokens::effective_scale(tokens);
            if !px.is_finite()
                || px <= 0.0
                || display.rect.width <= 0.0
                || display.rect.height <= 0.0
            {
                return None;
            }
            let number = |name| {
                tokens
                    .get(name)
                    .and_then(|value| tze_hud_config::tokens::parse_numeric(value))
            };
            let height = number(ROW_HEIGHT_TOKEN)
                .filter(|value| *value > 0.0)
                .unwrap_or(DEFAULT_ROW_HEIGHT_DP)
                * px;
            let spacing = |name, default| {
                number(name)
                    .filter(|value| *value >= 0.0)
                    .map(|value| value * px)
                    .filter(|value| value.is_finite())
                    .unwrap_or(default * px)
            };
            let metrics = DockMetrics {
                row_height: height,
                caption_width: spacing(CAPTION_WIDTH_TOKEN, DEFAULT_CAPTION_WIDTH_DP),
                gap: spacing("space.s", 8.0),
            };
            if !height.is_finite() || !metrics.caption_width.is_finite() || !metrics.gap.is_finite()
            {
                return None;
            }
            let aspect_ratios: Vec<_> = self
                .config
                .accepts
                .iter()
                .enumerate()
                .filter(|(i, name)| !self.config.accepts[..*i].contains(name))
                .filter_map(|(_, name)| {
                    self.extents[name].map(|(w, h)| (name.clone(), f64::from(w) / f64::from(h)))
                })
                .collect();
            let layout = DockLayout::new(
                display.rect,
                self.config.max_rows,
                metrics,
                aspect_ratios.iter().map(|(_, ratio)| *ratio),
            );
            Some(DockProjection {
                target: Some(display.target.clone()),
                layout,
                pixels_per_dp: px,
                aspect_ratios,
            })
        });
        scene.set_dock_projection(&self.config.accepts, projection, auto_clear)
    }
}

/// Run scene startup against `scene`.
///
/// `config_parent` is the config file's parent directory, used to resolve
/// relative `[widget_bundles].paths`; `None` resolves against the working
/// directory.
///
/// `os` holds the Windows preferences the tokens follow ("Show animations",
/// hud-ljhku.13; text size and caret blink): the windowed HUD
/// passes `os_prefs::read()`; headless runs, harnesses and tests pin
/// [`OsPreferences::default`] so the resolved tokens never depend on the
/// build host.
///
/// Returns the structured dock errors if a configured type is missing or
/// cannot use the resolved row height with its selected presentation.
pub fn run_scene_startup(
    raw: &RawConfig,
    config_parent: Option<&Path>,
    scene: &mut tze_hud_scene::graph::SceneGraph,
    os: OsPreferences,
) -> Result<SceneStartupResult, DockValidationErrors> {
    let config_tokens: DesignTokenMap = raw
        .design_tokens
        .as_ref()
        .map(|dt| dt.0.clone())
        .unwrap_or_default();
    let mut global_tokens = resolve_config_tokens(&config_tokens);
    // Theme -> Windows animation preference -> config overrides
    // (hud-ljhku.13), plus the Windows text size and caret blink, which
    // config cannot override.
    tze_hud_config::tokens::apply_os_preferences(&mut global_tokens, &config_tokens, &os);
    resolve_font_dir(&mut global_tokens, config_parent);
    // Monitor scale is not known yet: the effective zoom alone until the windows
    // report theirs through Appearance.
    tze_hud_config::tokens::set_px_scale(&mut global_tokens, 1.0);
    tracing::info!(
        token_count = global_tokens.len(),
        "scene_startup: design tokens loaded"
    );

    // Materialize tabs before widget loading so widget instances bind to them
    // and tile-creating mutations have an active tab from the start.
    let tab_map = bootstrap_config_tabs(scene, raw);

    let widgets = init_widget_registry(scene, raw, config_parent, &tab_map, &global_tokens)?;
    if let Some(dock) = &widgets.dock {
        // Listeners may start before the first usable window or display report.
        dock.project(scene, &global_tokens, None);
    }

    let mut zone_registry = ZoneRegistry::with_defaults();
    crate::appearance::restyle_zones(&mut zone_registry, &global_tokens);
    scene.zone_registry = zone_registry;
    scene.widget_registry.zoom = tze_hud_config::tokens::effective_scale(&global_tokens);
    scene.widget_registry.reduce_motion = tze_hud_config::tokens::os_reduce_motion(&global_tokens);
    tracing::info!(
        zone_count = scene.zone_registry.zones.len(),
        tab_count = tab_map.len(),
        "scene_startup: zone registry installed"
    );

    Ok(SceneStartupResult {
        global_tokens,
        widget_svg_assets: widgets.assets,
        startup_warnings: widgets.warnings,
        dock: widgets.dock,
    })
}

// ─── Fonts directory ──────────────────────────────────────────────────────────

/// Set the runtime-internal fonts-dir key to `<config_parent>/fonts` (made
/// absolute so the compositor needs no config path) when that directory
/// exists; otherwise remove it.
///
/// The key is never user config — config validation rejects it in
/// `[design_tokens]` — and any value that slipped in is overwritten or
/// dropped here.  With no `config_parent` (headless tests) fonts stay bundled.
pub(crate) fn resolve_font_dir(tokens: &mut DesignTokenMap, config_parent: Option<&Path>) {
    use tze_hud_compositor::fonts::RUNTIME_FONTS_DIR_KEY;
    tokens.remove(RUNTIME_FONTS_DIR_KEY);
    let Some(dir) = config_parent
        .map(|p| p.join("fonts"))
        .filter(|d| d.is_dir())
    else {
        return;
    };
    tracing::info!(dir = %dir.display(), "scene_startup: fonts dir");
    tokens.insert(
        RUNTIME_FONTS_DIR_KEY.to_owned(),
        dir.to_string_lossy().into_owned(),
    );
}

// ─── Config-declared tab bootstrap ────────────────────────────────────────────

/// Materialize every config-declared `[[tabs]]` entry as a scene tab.
///
/// The config loader requires at least one `[[tabs]]` entry and validates tab
/// name presence/uniqueness plus at-most-one `default_tab` (see
/// `tze_hud_config::loader::validate_tabs`). Prior to this step, scene tabs were
/// only pre-created for tabs that host widget instances (`widget_startup.rs`), so
/// a minimal valid config (one bare `[[tabs]]`, no widgets) booted with an empty
/// `scene.tabs` — every tile-creating mutation then failed PRECONDITION_FAILED
/// "No active tab" until a manual `create_tab`. This function honors the declared
/// contract: all tabs materialize, and `default_tab` (if any) becomes active.
///
/// Returns a map from tab name to its `SceneId` so widget instances can bind to
/// the already-created tabs in the subsequent widget-registry step.
///
fn bootstrap_config_tabs(
    scene: &mut tze_hud_scene::graph::SceneGraph,
    raw: &RawConfig,
) -> HashMap<String, tze_hud_scene::types::SceneId> {
    let mut tab_map: HashMap<String, tze_hud_scene::types::SceneId> = HashMap::new();
    let mut default_tab_id: Option<tze_hud_scene::types::SceneId> = None;

    for (tab_idx, tab) in raw.tabs.iter().enumerate() {
        // validate_tabs enforces a non-empty name; guard defensively so a
        // malformed-but-parsed config cannot panic startup.
        let tab_name = match tab.name.as_deref() {
            Some(n) if !n.is_empty() => n,
            _ => {
                tracing::warn!(
                    tab_idx,
                    "scene_startup: [[tabs]] entry missing name; skipping bootstrap"
                );
                continue;
            }
        };

        // Idempotent on duplicate names (validate_tabs rejects these, but be safe).
        let id = match tab_map.get(tab_name) {
            Some(&existing) => existing,
            None => match scene.create_tab(tab_name, tab_idx as u32) {
                Ok(id) => {
                    tab_map.insert(tab_name.to_string(), id);
                    tracing::debug!(tab_name, "scene_startup: materialized config-declared tab");
                    id
                }
                Err(e) => {
                    tracing::warn!(
                        tab_name,
                        error = %e,
                        "scene_startup: could not create config-declared tab; skipping"
                    );
                    continue;
                }
            },
        };

        if tab.default_tab {
            default_tab_id = Some(id);
        }
    }

    // Honor `default_tab` as the active tab. When no tab is marked default,
    // `create_tab` already left the first-created tab active (first-wins).
    if let Some(id) = default_tab_id {
        if let Err(e) = scene.switch_active_tab(id) {
            tracing::warn!(
                error = %e,
                "scene_startup: could not activate default tab"
            );
        }
    }

    tab_map
}

#[cfg(test)]
mod tests {
    use super::*;
    use tze_hud_config::raw::{RawConfig, RawDesignTokens};
    use tze_hud_scene::graph::SceneGraph;

    const ANIMATIONS_OFF: OsPreferences = OsPreferences {
        animations_enabled: false,
        text_scale: 1.0,
        caret_blink_ms: tze_hud_config::tokens::DEFAULT_CARET_BLINK_MS,
    };

    #[test]
    fn fonts_dir_is_config_dir_fonts_when_present_and_never_user_set() {
        use tze_hud_compositor::fonts::RUNTIME_FONTS_DIR_KEY as KEY;
        let parent = std::env::temp_dir().join(format!("tze_hud_font_dir_{}", std::process::id()));
        std::fs::create_dir_all(&parent).unwrap();
        let smuggled = || -> DesignTokenMap { [(KEY.to_owned(), "/elsewhere".to_owned())].into() };

        // No fonts/ beside the config: key absent, a smuggled value dropped.
        let mut tokens = smuggled();
        resolve_font_dir(&mut tokens, Some(&parent));
        assert!(!tokens.contains_key(KEY));

        // fonts/ exists: key is its absolute path, overriding any smuggled value.
        std::fs::create_dir_all(parent.join("fonts")).unwrap();
        let mut tokens = smuggled();
        resolve_font_dir(&mut tokens, Some(&parent));
        assert_eq!(
            tokens.get(KEY).map(String::as_str),
            Some(parent.join("fonts").to_string_lossy().as_ref())
        );

        // No config dir (headless): nothing inferred.
        let mut tokens = smuggled();
        resolve_font_dir(&mut tokens, None);
        assert!(tokens.is_empty());
        std::fs::remove_dir_all(&parent).ok();
    }

    /// The config crate's reserved-key list must name the compositor's key.
    #[test]
    fn runtime_fonts_dir_key_is_reserved_in_config_validation() {
        assert!(
            tze_hud_config::tokens::RESERVED_RUNTIME_TOKEN_KEYS
                .contains(&tze_hud_compositor::fonts::RUNTIME_FONTS_DIR_KEY)
        );
    }

    // ── Helpers ───────────────────────────────────────────────────────────────

    fn make_scene() -> SceneGraph {
        SceneGraph::new(1920.0, 1080.0)
    }

    #[test]
    fn dock_startup_accepts_shipped_countdown_and_progress_viewports() {
        use tze_hud_scene::{dock::DockPublishError, validation::ValidationError};

        let mut raw: RawConfig = toml::from_str(
            r#"
            [dock]
            accepts = ["countdown", "progress-bar", "countdown"]
            [[tabs]]
            name = "Main"
            [[tabs.widgets]]
            instance_id = "standalone-countdown"
            widget_type = "countdown"
            "#,
        )
        .unwrap();
        let mut scene = make_scene();
        let startup = run_scene_startup(&raw, None, &mut scene, OsPreferences::default()).unwrap();
        let countdown = scene
            .widget_registry
            .dock_presentation("countdown")
            .unwrap();
        assert_eq!((countdown.width, countdown.height), (160.0, 52.0));
        assert!(countdown.uses_builtin_countdown_layout());
        assert_eq!(
            scene.widget_registry.instances["standalone-countdown"].widget_type_name,
            "countdown"
        );
        for (kind, file, view_box) in [
            ("countdown", "card.svg", "0 0 360 124"),
            ("progress-bar", "track.svg", "0 0 320 50"),
            ("progress-bar", "fill.svg", "0 0 320 50"),
        ] {
            let (_, _, svg) = startup
                .widget_svg_assets
                .iter()
                .find(|(asset_kind, asset_file, _)| asset_kind == kind && asset_file == file)
                .unwrap();
            assert!(std::str::from_utf8(svg).unwrap().contains(view_box));
        }
        assert_eq!(
            scene
                .dock_types()
                .map(|(kind, definition)| (kind, definition.id.as_str()))
                .collect::<Vec<_>>(),
            [("countdown", "countdown"), ("progress-bar", "progress-bar")]
        );
        let version = scene.version;
        assert!(scene.dock_projection().is_none());
        assert_eq!(
            scene.publish_to_dock("countdown", Default::default(), "before-layout", None, 0, 0),
            Err(DockPublishError::Full),
            "startup exposes configured types without enabling row admission"
        );
        assert!(scene.dock_definition("gauge").is_none());
        assert_eq!(
            scene.validate_dock_publish("gauge", &Default::default(), "before-layout"),
            Err(DockPublishError::Widget(ValidationError::WidgetNotFound {
                name: "gauge".into(),
            }))
        );
        assert!(
            !startup
                .dock
                .unwrap()
                .project(&mut scene, &startup.global_tokens, None)
        );
        assert_eq!(scene.version, version);
        assert!(scene.dock_rows().is_empty());
        raw.design_tokens = Some(RawDesignTokens(HashMap::from([
            ("progress.width".into(), "360".into()),
            ("progress.height".into(), "52".into()),
        ])));
        let signed_sizes =
            run_scene_startup(&raw, None, &mut make_scene(), OsPreferences::default()).unwrap();
        for file in ["track.svg", "fill.svg"] {
            let (_, _, svg) = signed_sizes
                .widget_svg_assets
                .iter()
                .find(|(kind, asset, _)| kind == "progress-bar" && asset == file)
                .unwrap();
            assert!(std::str::from_utf8(svg).unwrap().contains("0 0 360 52"));
        }

        raw.dock = None;
        let mut disabled = make_scene();
        let startup =
            run_scene_startup(&raw, None, &mut disabled, OsPreferences::default()).unwrap();
        assert!(startup.dock.is_none());
        assert!(
            disabled
                .widget_registry
                .get_definition("countdown")
                .is_some()
        );
        assert!(disabled.dock_types().next().is_none());
        assert_eq!(
            disabled.validate_dock_publish("countdown", &Default::default(), "before-layout"),
            Err(DockPublishError::Widget(ValidationError::WidgetNotFound {
                name: "countdown".into(),
            }))
        );
    }

    #[test]
    fn dock_startup_rejects_known_default_layers_without_a_common_viewport() {
        use std::fs;

        let root = tempfile::tempdir().unwrap();
        let bundle = root.path().join("layered");
        fs::create_dir(&bundle).unwrap();
        fs::write(
            bundle.join("widget.toml"),
            "name = 'layered'\nversion = '1.0.0'\n[[layers]]\nsvg_file = 'base.svg'\n[[layers]]\nsvg_file = 'top.svg'\n",
        )
        .unwrap();
        fs::write(bundle.join("base.svg"), "<svg viewBox='0 0 160 52'/>").unwrap();
        let mut raw = toml::from_str::<RawConfig>("[widget_bundles]\npaths = ['.']").unwrap();
        for size in ["160 51", "0 52"] {
            fs::write(
                bundle.join("top.svg"),
                format!("<svg viewBox='0 0 {size}'/>"),
            )
            .unwrap();
            raw.dock = None;
            let mut scene = make_scene();
            let startup = run_scene_startup(
                &raw,
                Some(root.path()),
                &mut scene,
                OsPreferences::default(),
            )
            .unwrap();
            assert!(startup.dock.is_none());
            assert!(scene.dock_projection().is_none());
            assert!(scene.widget_registry.get_definition("layered").is_some());

            raw.dock = Some(tze_hud_config::dock::DockConfig {
                accepts: vec!["layered".into()],
                ..Default::default()
            });
            let Err(DockValidationErrors(errors)) = run_scene_startup(
                &raw,
                Some(root.path()),
                &mut make_scene(),
                OsPreferences::default(),
            ) else {
                panic!("bad selected extent {size} passed startup");
            };
            assert_eq!(errors.len(), 1);
            let error = &errors[0];
            assert_eq!(error.field_path, "dock.accepts");
            assert_eq!(error.got, "layered");
            assert!(error.expected.contains("authored width and height"));
        }
    }

    // ── Step 2: Design token loading ──────────────────────────────────────────

    /// WHEN [design_tokens] is absent THEN global tokens are the canonical
    /// fallbacks under the default theme.
    #[test]
    fn absent_design_tokens_uses_default_theme() {
        let raw = RawConfig::default();
        let mut scene = make_scene();
        let result = run_scene_startup(&raw, None, &mut scene, OsPreferences::default()).unwrap();

        let mut expected = tze_hud_config::themes::resolve_config_tokens(&DesignTokenMap::new());
        tze_hud_config::tokens::set_px_scale(&mut expected, 1.0);
        assert_eq!(result.global_tokens, expected);
        assert_eq!(
            result.global_tokens.get("color.text.primary"),
            tze_hud_config::themes::builtin_theme(tze_hud_config::themes::DEFAULT_THEME)
                .unwrap()
                .get("color.text.primary"),
            "absent [design_tokens] should produce the default theme's color.text.primary"
        );
        assert!(
            result.global_tokens.len() >= tze_hud_config::tokens::CANONICAL_TOKENS.len(),
            "every canonical token must be present"
        );
    }

    /// WHEN [design_tokens] has overrides THEN they take precedence over canonical fallbacks.
    #[test]
    fn design_tokens_override_canonical_fallbacks() {
        let mut raw = RawConfig::default();
        let mut dt_map = HashMap::new();
        dt_map.insert("color.text.primary".to_string(), "#FF0000".to_string());
        raw.design_tokens = Some(RawDesignTokens(dt_map));

        let mut scene = make_scene();
        let result = run_scene_startup(&raw, None, &mut scene, OsPreferences::default()).unwrap();

        assert_eq!(
            result
                .global_tokens
                .get("color.text.primary")
                .map(|s| s.as_str()),
            Some("#FF0000"),
            "config token override should take precedence over canonical fallback"
        );
    }

    // ── Step 6: Effective rendering policies ──────────────────────────────────

    /// WHEN global tokens are set THEN subtitle zone gets token-derived text_color.
    #[test]
    fn subtitle_zone_gets_token_derived_text_color() {
        let mut raw = RawConfig::default();
        let mut dt_map = HashMap::new();
        dt_map.insert("color.on_surface".to_string(), "#AABBCC".to_string());
        raw.design_tokens = Some(RawDesignTokens(dt_map));

        let mut scene = make_scene();
        run_scene_startup(&raw, None, &mut scene, OsPreferences::default()).unwrap();

        let subtitle_zone = scene
            .zone_registry
            .zones
            .get("subtitle")
            .expect("subtitle zone should be registered");

        let text_color = subtitle_zone
            .rendering_policy
            .text_color
            .expect("subtitle zone should have token-derived text_color");
        // #AABBCC is sRGB; scene colours are linear.
        let linear = |v: u8| ((v as f32 / 255.0 + 0.055) / 1.055).powf(2.4);
        assert!(
            (text_color.r - linear(0xAA)).abs() < 1e-3,
            "text_color.r should match #AABBCC red component"
        );
        assert!(
            (text_color.g - linear(0xBB)).abs() < 1e-3,
            "text_color.g should match #AABBCC green component"
        );
        assert!(
            (text_color.b - linear(0xCC)).abs() < 1e-3,
            "text_color.b should match #AABBCC blue component"
        );
    }

    // ── Step 8: Zone registry applied to scene ────────────────────────────────

    /// WHEN run_scene_startup completes THEN scene.zone_registry contains all
    /// default zones with effective rendering policies.
    #[test]
    fn zone_registry_applied_to_scene() {
        let raw = RawConfig::default();
        let mut scene = make_scene();
        run_scene_startup(&raw, None, &mut scene, OsPreferences::default()).unwrap();

        // All 6 default zones must be present
        for zone_name in &[
            "subtitle",
            "notification-area",
            "status-bar",
            "pip",
            "ambient-background",
            "alert-banner",
        ] {
            assert!(
                scene.zone_registry.zones.contains_key(*zone_name),
                "zone '{zone_name}' should be in the zone registry"
            );
        }
    }

    // ── Step 2→8 dependency: tokens before effective policies ─────────────────

    /// WHEN tokens are configured THEN ALL built-in zones receive token-derived
    /// policies (not just subtitle); validates step 2→6→8 pipeline.
    #[test]
    fn all_builtin_zones_receive_token_derived_policies_after_startup() {
        let mut raw = RawConfig::default();
        let mut dt_map = HashMap::new();
        dt_map.insert("color.text.primary".to_string(), "#123456".to_string());
        raw.design_tokens = Some(RawDesignTokens(dt_map));

        let mut scene = make_scene();
        run_scene_startup(&raw, None, &mut scene, OsPreferences::default()).unwrap();

        // subtitle should get text_color from tokens
        let subtitle = scene.zone_registry.zones.get("subtitle").unwrap();
        assert!(
            subtitle.rendering_policy.text_color.is_some(),
            "subtitle should get token-derived text_color"
        );
        // notification-area should also get text_color from tokens
        let notif = scene.zone_registry.zones.get("notification-area").unwrap();
        assert!(
            notif.rendering_policy.text_color.is_some(),
            "notification-area should get token-derived text_color"
        );
    }

    // ── End-to-end startup with a [design_tokens] section ────────────────────

    /// WHEN config has [design_tokens] THEN startup populates the zone registry
    /// with token-derived policies.
    #[test]
    fn end_to_end_startup_with_design_tokens_section() {
        // Use r##"..."## to avoid premature termination on "#RRGGBB" hex values.
        let toml_str = r##"
[runtime]
profile = "headless"

[[tabs]]
name = "Main"
default_tab = true

[design_tokens]
"color.on_surface" = "#FFFFFF"
"opacity.backdrop.default" = "0.7"
"stroke.subtitle.text" = "2.0"
"color.subtitle.outline" = "#FFFF00"
"type.caption.display.size" = "18"
"type.caption.display.weight" = "600"
"type.caption.display.family" = "system-ui"
"typography.body.size" = "14"
"typography.body.weight" = "400"
"typography.body.family" = "system-ui"
"spacing.padding.medium" = "8"
"spacing.padding.large" = "16"
"typography.status.size" = "12"
"typography.status.weight" = "400"
"typography.status.family" = "system-ui"
"typography.alert.size" = "16"
"typography.alert.weight" = "700"
"typography.alert.family" = "system-ui"
"typography.notification.size" = "14"
"typography.notification.weight" = "500"
"typography.notification.family" = "system-ui"
"color.text.muted" = "#AAAAAA"
"opacity.text.muted" = "0.7"
"color.accent.primary" = "#3399FF"
"color.accent.secondary" = "#33FF99"
"color.surface.primary" = "#1A1A2E"
"color.surface.secondary" = "#16213E"
"color.border.default" = "#444466"
"##;

        let raw: RawConfig = toml::from_str(toml_str).expect("TOML parse should succeed");
        let mut scene = SceneGraph::new(1920.0, 1080.0);
        let result = run_scene_startup(&raw, None, &mut scene, OsPreferences::default()).unwrap();

        // Global token map should include the overrides
        assert_eq!(
            result
                .global_tokens
                .get("color.on_surface")
                .map(|s| s.as_str()),
            Some("#FFFFFF")
        );

        // Zone registry should be in the scene with all 6 zones
        assert_eq!(
            scene.zone_registry.zones.len(),
            6,
            "expected 6 built-in zones after startup"
        );

        // Subtitle should have token-derived text_color (white)
        let subtitle = scene.zone_registry.zones.get("subtitle").unwrap();
        let text_color = subtitle.rendering_policy.text_color.unwrap();
        assert!(
            (text_color.r - 1.0).abs() < 1e-3,
            "subtitle text_color.r should be 1.0 for #FFFFFF"
        );
        assert!(
            (text_color.g - 1.0).abs() < 1e-3,
            "subtitle text_color.g should be 1.0 for #FFFFFF"
        );
        assert!(
            (text_color.b - 1.0).abs() < 1e-3,
            "subtitle text_color.b should be 1.0 for #FFFFFF"
        );
        assert_eq!(subtitle.rendering_policy.transition_in_ms, Some(180));
        assert_eq!(subtitle.rendering_policy.transition_out_ms, Some(120));
        assert_eq!(
            tze_hud_config::tokens::resolve_motion_duration_ms(
                &result.global_tokens,
                "motion.state.ms",
            ),
            100
        );

        // A selected theme and explicit override reach the actual startup producer.
        let mut overridden = raw.clone();
        let tokens = &mut overridden.design_tokens.as_mut().unwrap().0;
        tokens.insert("theme".into(), "classic".into());
        tokens.insert("motion.enter.ms".into(), "275".into());
        tokens.insert("motion.state.ms".into(), "240".into());
        let mut changed_scene = SceneGraph::new(1920.0, 1080.0);
        let changed = run_scene_startup(
            &overridden,
            None,
            &mut changed_scene,
            OsPreferences::default(),
        )
        .unwrap();
        let policy = &changed_scene.zone_registry.zones["subtitle"].rendering_policy;
        assert_eq!(policy.transition_in_ms, Some(275));
        assert_eq!(policy.transition_out_ms, Some(100));
        assert_eq!(
            tze_hud_config::tokens::resolve_motion_duration_ms(
                &changed.global_tokens,
                "motion.state.ms",
            ),
            240
        );
    }

    /// hud-ljhku.13 R13.4 S1, pinned rather than read from the host: with
    /// Windows "Show animations" off, startup zeroes the motion durations
    /// (zone transitions included) and marks reduced motion for gRPC
    /// publishes, while an explicit config duration still wins.
    #[test]
    fn startup_with_animations_off_zeroes_motion_below_config() {
        let mut scene = make_scene();
        let off =
            run_scene_startup(&RawConfig::default(), None, &mut scene, ANIMATIONS_OFF).unwrap();
        let subtitle = &scene.zone_registry.zones["subtitle"].rendering_policy;
        assert_eq!(subtitle.transition_in_ms, Some(0));
        assert_eq!(subtitle.transition_out_ms, Some(0));
        assert_eq!(
            tze_hud_config::tokens::resolve_motion_duration_ms(
                &off.global_tokens,
                "motion.state.ms"
            ),
            0
        );
        assert!(scene.widget_registry.reduce_motion);

        let mut raw = RawConfig::default();
        let mut dt_map = DesignTokenMap::new();
        dt_map.insert("motion.enter.ms".into(), "275".into());
        raw.design_tokens = Some(RawDesignTokens(dt_map));
        let mut scene = make_scene();
        run_scene_startup(&raw, None, &mut scene, ANIMATIONS_OFF).unwrap();
        let subtitle = &scene.zone_registry.zones["subtitle"].rendering_policy;
        assert_eq!(subtitle.transition_in_ms, Some(275), "config wins");
        assert_eq!(subtitle.transition_out_ms, Some(0));

        let mut scene = make_scene();
        run_scene_startup(
            &RawConfig::default(),
            None,
            &mut scene,
            OsPreferences::default(),
        )
        .unwrap();
        assert!(!scene.widget_registry.reduce_motion);
    }

    /// Pinned rather than read from the host:
    /// Windows text size 125% with Zoom 100% zooms widget boxes and every dp
    /// by 1.25, and the caret key records blinking off; at the defaults
    /// neither key is present (R12.3 S1).
    #[test]
    fn startup_follows_windows_text_size_and_caret_blink() {
        let mut scene = make_scene();
        let os = OsPreferences {
            text_scale: 1.25,
            caret_blink_ms: 0,
            ..OsPreferences::default()
        };
        let at_125 = run_scene_startup(&RawConfig::default(), None, &mut scene, os).unwrap();
        assert_eq!(scene.widget_registry.zoom, 1.25);
        assert_eq!(
            tze_hud_config::tokens::px_scale(&at_125.global_tokens),
            1.25
        );
        assert_eq!(
            tze_hud_config::tokens::caret_blink_ms(&at_125.global_tokens),
            0
        );
        let notes_125 = scene.zone_registry.zones["notification-area"]
            .rendering_policy
            .font_size_px
            .unwrap();

        let mut scene = make_scene();
        let defaults = run_scene_startup(
            &RawConfig::default(),
            None,
            &mut scene,
            OsPreferences::default(),
        )
        .unwrap();
        assert_eq!(scene.widget_registry.zoom, 1.0);
        for key in [
            tze_hud_config::tokens::TEXT_SCALE_KEY,
            tze_hud_config::tokens::CARET_BLINK_MS_KEY,
        ] {
            assert!(!defaults.global_tokens.contains_key(key), "{key}");
        }
        let notes = scene.zone_registry.zones["notification-area"]
            .rendering_policy
            .font_size_px
            .unwrap();
        assert_eq!(notes_125, notes * 1.25);
    }

    // ── Step 2.5: Config-declared tab bootstrap ───────────────────

    /// WHEN a minimal valid config declares a single widget-less `[[tabs]]` with
    /// `default_tab = true` THEN that tab materializes in `scene.tabs` at startup
    /// and is set active — so tile-creating mutations succeed without a manual
    /// `create_tab`.
    #[test]
    fn widgetless_config_tab_materializes_and_is_active() {
        let toml_str = r##"
[runtime]
profile = "headless"

[[tabs]]
name = "Main"
default_tab = true
"##;
        let raw: RawConfig = toml::from_str(toml_str).expect("TOML parse should succeed");
        let mut scene = make_scene();
        run_scene_startup(&raw, None, &mut scene, OsPreferences::default()).unwrap();

        // The declared tab must exist even though it hosts no widgets.
        assert_eq!(
            scene.tabs.len(),
            1,
            "config-declared tab must materialize even without widget instances"
        );
        let (tab_id, tab) = scene.tabs.iter().next().expect("one tab present");
        assert_eq!(
            tab.name, "Main",
            "materialized tab should carry the config name"
        );

        // default_tab = true must be honored as the active tab.
        assert_eq!(
            scene.active_tab,
            Some(*tab_id),
            "default_tab must be the active tab after startup"
        );
    }

    /// WHEN multiple tabs are declared and a non-first one is `default_tab` THEN
    /// all tabs materialize and the marked default (not the first-declared) is
    /// active.
    #[test]
    fn multiple_tabs_honor_default_tab_selection() {
        let toml_str = r##"
[runtime]
profile = "headless"

[[tabs]]
name = "First"

[[tabs]]
name = "Second"
default_tab = true

[[tabs]]
name = "Third"
"##;
        let raw: RawConfig = toml::from_str(toml_str).expect("TOML parse should succeed");
        let mut scene = make_scene();
        run_scene_startup(&raw, None, &mut scene, OsPreferences::default()).unwrap();

        assert_eq!(scene.tabs.len(), 3, "all declared tabs must materialize");
        let second_id = scene
            .tabs
            .iter()
            .find(|(_, t)| t.name == "Second")
            .map(|(id, _)| *id)
            .expect("Second tab present");
        assert_eq!(
            scene.active_tab,
            Some(second_id),
            "the tab marked default_tab must be active, not the first declared"
        );
    }

    /// WHEN tabs are declared with no `default_tab` THEN all materialize and the
    /// first-declared tab is active (first-wins fallback from `create_tab`).
    #[test]
    fn tabs_without_default_activate_first_declared() {
        let toml_str = r##"
[runtime]
profile = "headless"

[[tabs]]
name = "Alpha"

[[tabs]]
name = "Beta"
"##;
        let raw: RawConfig = toml::from_str(toml_str).expect("TOML parse should succeed");
        let mut scene = make_scene();
        run_scene_startup(&raw, None, &mut scene, OsPreferences::default()).unwrap();

        assert_eq!(scene.tabs.len(), 2, "all declared tabs must materialize");
        let alpha_id = scene
            .tabs
            .iter()
            .find(|(_, t)| t.name == "Alpha")
            .map(|(id, _)| *id)
            .expect("Alpha tab present");
        assert_eq!(
            scene.active_tab,
            Some(alpha_id),
            "with no default_tab, the first-declared tab should be active"
        );
    }
}
