//! Runtime widget SVG registration wiring.
//!
//! Connects runtime-registered SVG assets to the widget registry lifecycle:
//! validate structural compatibility, record asset handles, and enqueue SVG
//! bytes for compositor-side registration.

use tze_hud_compositor::widget::WidgetRenderer;

/// Register pending widget SVG assets in the compositor widget renderer.
pub fn process_pending_widget_svgs<I>(
    widget_renderer: Option<&mut WidgetRenderer>,
    pending_widget_svgs: I,
) where
    I: IntoIterator<Item = crate::widget_startup::WidgetSvgAsset>,
{
    if let Some(wr) = widget_renderer {
        for (type_id, filename, bytes) in pending_widget_svgs {
            wr.register_svg(&type_id, &filename, bytes);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use tze_hud_scene::graph::SceneGraph;
    use tze_hud_scene::types::{
        ContentionPolicy, GeometryPolicy, RenderingPolicy, WidgetBinding, WidgetBindingMapping,
        WidgetDefinition, WidgetParamType, WidgetParameterDeclaration, WidgetParameterValue,
        WidgetSvgLayer,
    };
    use tze_hud_widget::register_runtime_widget_svg_asset;

    fn make_scene() -> SceneGraph {
        let mut scene = SceneGraph::new(800.0, 600.0);
        scene.widget_registry.register_definition(WidgetDefinition {
            id: "gauge".to_string(),
            name: "Gauge".to_string(),
            description: "test".to_string(),
            parameter_schema: vec![WidgetParameterDeclaration {
                name: "level".to_string(),
                param_type: WidgetParamType::F32,
                default_value: WidgetParameterValue::F32(0.0),
                constraints: None,
            }],
            layers: vec![WidgetSvgLayer {
                svg_file: "fill.svg".to_string(),
                bindings: vec![WidgetBinding {
                    param: "level".to_string(),
                    target_element: "bar".to_string(),
                    target_attribute: "height".to_string(),
                    mapping: WidgetBindingMapping::Linear {
                        attr_min: 0.0,
                        attr_max: 100.0,
                    },
                }],
            }],
            default_geometry_policy: GeometryPolicy::Relative {
                x_pct: 0.0,
                y_pct: 0.0,
                width_pct: 1.0,
                height_pct: 1.0,
            },
            default_rendering_policy: RenderingPolicy::default(),
            default_contention_policy: ContentionPolicy::LatestWins,
            max_publishers: WidgetDefinition::default_max_publishers(),
            ephemeral: false,
            hover_behavior: None,
        });
        scene
    }

    #[test]
    fn runtime_registration_records_handle_and_enqueues_svg() {
        let mut scene = make_scene();
        let svg = br#"<svg viewBox="0 0 100 100"><rect id="bar" width="10" height="20"/></svg>"#;
        register_runtime_widget_svg_asset(
            &mut scene,
            "gauge",
            "fill.svg",
            svg,
            "asset:abc123",
            &HashMap::new(),
        )
        .expect("registration should succeed");

        assert_eq!(
            scene
                .widget_registry
                .runtime_svg_handle("gauge", "fill.svg"),
            Some("asset:abc123")
        );
        let drained = scene.drain_pending_widget_svg_assets();
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].0, "gauge");
        assert_eq!(drained[0].1, "fill.svg");
    }

    #[test]
    fn runtime_registration_rejects_incompatible_svg_targets() {
        let mut scene = make_scene();
        let bad_svg =
            br#"<svg viewBox="0 0 100 100"><rect id="not-bar" width="10" height="20"/></svg>"#;
        let err = register_runtime_widget_svg_asset(
            &mut scene,
            "gauge",
            "fill.svg",
            bad_svg,
            "asset:oops",
            &HashMap::new(),
        )
        .expect_err("registration must fail when bindings cannot resolve");
        assert_eq!(err.wire_code(), "WIDGET_ASSET_INVALID_SVG");
        assert!(
            scene
                .widget_registry
                .runtime_svg_handle("gauge", "fill.svg")
                .is_none()
        );
    }
}
