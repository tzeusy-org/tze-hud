//! Scene startup: design tokens, config tabs, widget bundles, zone policies.
//!
//! Runs once before sessions are accepted:
//!
//! 1. Resolve canonical fallbacks → selected theme → `[design_tokens]`
//!    overrides → global token map.
//! 2. Materialize every `[[tabs]]` entry as a scene tab.
//! 3. Load widget bundles (SVG `{{token.key}}` placeholders resolved against
//!    the global token map) and register widget definitions and instances.
//! 4. Fill each built-in zone's `RenderingPolicy` from the global tokens and
//!    install the resulting zone registry in the scene.
//!
//! Tokens must be resolved before any SVG is loaded (step 3).

use std::collections::HashMap;
use std::path::Path;

use tze_hud_config::policy_builder::build_all_effective_policies;
use tze_hud_config::raw::RawConfig;
use tze_hud_config::themes::resolve_config_tokens;
use tze_hud_config::tokens::DesignTokenMap;
use tze_hud_scene::types::{RenderingPolicy, ZoneRegistry};

use crate::widget_startup::init_widget_registry;

/// Result of [`run_scene_startup`]. The zone and widget registries are
/// installed in the scene; what remains is for the compositor.
pub struct SceneStartupResult {
    /// Fully resolved global token map (canonical fallbacks, theme, config overrides).
    /// Pass to `compositor.set_token_map()`.
    pub global_tokens: DesignTokenMap,
    /// SVG assets from widget bundles for compositor registration.
    pub widget_svg_assets: Vec<crate::widget_startup::WidgetSvgAsset>,
}

/// Run scene startup against `scene`.
///
/// `config_parent` is the config file's parent directory, used to resolve
/// relative `[widget_bundles].paths`; `None` resolves against the working
/// directory.
pub fn run_scene_startup(
    raw: &RawConfig,
    config_parent: Option<&Path>,
    scene: &mut tze_hud_scene::graph::SceneGraph,
) -> SceneStartupResult {
    let config_tokens: DesignTokenMap = raw
        .design_tokens
        .as_ref()
        .map(|dt| dt.0.clone())
        .unwrap_or_default();
    let global_tokens = resolve_config_tokens(&config_tokens);
    tracing::info!(
        token_count = global_tokens.len(),
        "scene_startup: design tokens loaded"
    );

    // Materialize tabs before widget loading so widget instances bind to them
    // and tile-creating mutations have an active tab from the start.
    let tab_map = bootstrap_config_tabs(scene, raw);

    let widget_svg_assets =
        init_widget_registry(scene, raw, config_parent, &tab_map, &global_tokens);

    let mut zone_registry = ZoneRegistry::with_defaults();
    let zone_defaults: HashMap<String, RenderingPolicy> = zone_registry
        .zones
        .iter()
        .map(|(name, def)| (name.clone(), def.rendering_policy.clone()))
        .collect();
    for (zone_name, policy) in build_all_effective_policies(&zone_defaults, &global_tokens) {
        if let Some(zone_def) = zone_registry.zones.get_mut(&zone_name) {
            zone_def.rendering_policy = policy;
        }
    }
    scene.zone_registry = zone_registry;
    tracing::info!(
        zone_count = scene.zone_registry.zones.len(),
        tab_count = tab_map.len(),
        "scene_startup: zone registry installed"
    );

    SceneStartupResult {
        global_tokens,
        widget_svg_assets,
    }
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
/// Spec reference: configuration/spec.md §Tab Configuration Validation.
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

    // ── Helpers ───────────────────────────────────────────────────────────────

    fn make_scene() -> SceneGraph {
        SceneGraph::new(1920.0, 1080.0)
    }

    // ── Step 2: Design token loading ──────────────────────────────────────────

    /// WHEN [design_tokens] is absent THEN global tokens are the canonical
    /// fallbacks under the default theme.
    #[test]
    fn absent_design_tokens_uses_default_theme() {
        let raw = RawConfig::default();
        let mut scene = make_scene();
        let result = run_scene_startup(&raw, None, &mut scene);

        assert_eq!(
            result.global_tokens,
            tze_hud_config::themes::resolve_config_tokens(&DesignTokenMap::new()),
        );
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
        let result = run_scene_startup(&raw, None, &mut scene);

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
        dt_map.insert("color.text.primary".to_string(), "#AABBCC".to_string());
        raw.design_tokens = Some(RawDesignTokens(dt_map));

        let mut scene = make_scene();
        run_scene_startup(&raw, None, &mut scene);

        let subtitle_zone = scene
            .zone_registry
            .zones
            .get("subtitle")
            .expect("subtitle zone should be registered");

        let text_color = subtitle_zone
            .rendering_policy
            .text_color
            .expect("subtitle zone should have token-derived text_color");
        // #AABBCC → R=0xAA/255≈0.667, G=0xBB/255≈0.733, B=0xCC/255≈0.8
        assert!(
            (text_color.r - 0xAA as f32 / 255.0).abs() < 1e-3,
            "text_color.r should match #AABBCC red component"
        );
        assert!(
            (text_color.g - 0xBB as f32 / 255.0).abs() < 1e-3,
            "text_color.g should match #AABBCC green component"
        );
        assert!(
            (text_color.b - 0xCC as f32 / 255.0).abs() < 1e-3,
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
        run_scene_startup(&raw, None, &mut scene);

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
        run_scene_startup(&raw, None, &mut scene);

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
"color.text.primary" = "#FFFFFF"
"color.backdrop.default" = "#000000"
"opacity.backdrop.default" = "0.7"
"stroke.outline.width" = "2.0"
"color.outline.default" = "#FFFF00"
"typography.subtitle.size" = "18"
"typography.subtitle.weight" = "600"
"typography.subtitle.family" = "system-ui"
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
        let result = run_scene_startup(&raw, None, &mut scene);

        // Global token map should include the overrides
        assert_eq!(
            result
                .global_tokens
                .get("color.text.primary")
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
    }

    // ── Step 2.5: Config-declared tab bootstrap (hud-d5rcd) ───────────────────

    /// WHEN a minimal valid config declares a single widget-less `[[tabs]]` with
    /// `default_tab = true` THEN that tab materializes in `scene.tabs` at startup
    /// and is set active — so tile-creating mutations succeed without a manual
    /// `create_tab` (regression test for hud-d5rcd).
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
        run_scene_startup(&raw, None, &mut scene);

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
        run_scene_startup(&raw, None, &mut scene);

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
        run_scene_startup(&raw, None, &mut scene);

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
