//! Shared fixtures for the scene graph behavior tests.

use super::*;
use crate::clock::TestClock;
use crate::types::{
    ContentionPolicy, GeometryPolicy, RenderingPolicy, SceneId, WidgetDefinition, WidgetInstance,
    WidgetParamConstraints, WidgetParamType, WidgetParameterDeclaration, WidgetParameterValue,
    WidgetSvgLayer,
};
use std::sync::Arc;

pub(super) fn make_scene() -> SceneGraph {
    SceneGraph::new(1920.0, 1080.0)
}

pub(super) fn make_scene_with_clock() -> (SceneGraph, Arc<TestClock>) {
    let clock = Arc::new(TestClock::new(1_000_000));
    let scene = SceneGraph::new_with_clock(1920.0, 1080.0, clock.clone());
    (scene, clock)
}

/// Build a minimal gauge WidgetDefinition for testing.
///
/// Parameters: level (f32, 0–1), label (string), severity (enum info/warning/error).
pub(super) fn make_gauge_definition() -> WidgetDefinition {
    WidgetDefinition {
        id: "gauge".to_string(),
        name: "gauge".to_string(),
        description: "test gauge".to_string(),
        parameter_schema: vec![
            WidgetParameterDeclaration {
                name: "level".to_string(),
                param_type: WidgetParamType::F32,
                default_value: WidgetParameterValue::F32(0.0),
                constraints: Some(WidgetParamConstraints {
                    f32_min: Some(0.0),
                    f32_max: Some(1.0),
                    ..Default::default()
                }),
            },
            WidgetParameterDeclaration {
                name: "label".to_string(),
                param_type: WidgetParamType::String,
                default_value: WidgetParameterValue::String(String::new()),
                constraints: None,
            },
            WidgetParameterDeclaration {
                name: "severity".to_string(),
                param_type: WidgetParamType::Enum,
                default_value: WidgetParameterValue::Enum("info".to_string()),
                constraints: Some(WidgetParamConstraints {
                    enum_allowed_values: vec![
                        "info".to_string(),
                        "warning".to_string(),
                        "error".to_string(),
                    ],
                    ..Default::default()
                }),
            },
        ],
        layers: vec![WidgetSvgLayer {
            svg_file: "fill.svg".to_string(),
            bindings: vec![],
        }],
        default_geometry_policy: GeometryPolicy::Relative {
            x_pct: 0.0,
            y_pct: 0.0,
            width_pct: 0.25,
            height_pct: 0.25,
        },
        default_rendering_policy: RenderingPolicy::default(),
        default_contention_policy: ContentionPolicy::LatestWins,
        max_publishers: u32::MAX,
        ephemeral: false,
        hover_behavior: None,
    }
}

/// Register gauge definition + instance in a scene with one tab.
pub(super) fn scene_with_gauge(contention: ContentionPolicy) -> (SceneGraph, SceneId /* tab_id */) {
    let mut scene = SceneGraph::new(1920.0, 1080.0);
    let tab_id = scene.create_tab("Main", 0).unwrap();

    let mut def = make_gauge_definition();
    def.default_contention_policy = contention;

    scene.widget_registry.register_definition(def);
    scene.widget_registry.register_instance(WidgetInstance {
        id: SceneId::new(),
        widget_type_name: "gauge".to_string(),
        tab_id,
        geometry_override: None,
        contention_override: None,
        instance_name: "gauge".to_string(),
        current_params: std::collections::HashMap::from([
            ("level".to_string(), WidgetParameterValue::F32(0.0)),
            (
                "label".to_string(),
                WidgetParameterValue::String(String::new()),
            ),
            (
                "severity".to_string(),
                WidgetParameterValue::Enum("info".to_string()),
            ),
        ]),
    });

    (scene, tab_id)
}

// ── Widget publication TTL / expiry tests ─────────────────────────────────

/// Helper: scene with a gauge backed by a controllable TestClock.
pub(super) fn scene_with_gauge_and_clock(
    contention: ContentionPolicy,
) -> (SceneGraph, SceneId, TestClock) {
    let clock = TestClock::new(1_000); // t=1 000 ms = 1 000 000 µs
    let mut scene = SceneGraph::new_with_clock(1920.0, 1080.0, Arc::new(clock.clone()));
    let tab_id = scene.create_tab("Main", 0).unwrap();

    let mut def = make_gauge_definition();
    def.default_contention_policy = contention;
    scene.widget_registry.register_definition(def);
    scene.widget_registry.register_instance(WidgetInstance {
        id: SceneId::new(),
        widget_type_name: "gauge".to_string(),
        tab_id,
        geometry_override: None,
        contention_override: None,
        instance_name: "gauge".to_string(),
        current_params: std::collections::HashMap::from([
            ("level".to_string(), WidgetParameterValue::F32(0.0)),
            (
                "label".to_string(),
                WidgetParameterValue::String(String::new()),
            ),
            (
                "severity".to_string(),
                WidgetParameterValue::Enum("info".to_string()),
            ),
        ]),
    });

    (scene, tab_id, clock)
}
