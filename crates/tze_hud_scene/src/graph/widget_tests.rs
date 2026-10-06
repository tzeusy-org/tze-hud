use super::test_helpers::{make_gauge_definition, scene_with_gauge, scene_with_gauge_and_clock};
use super::*;
use crate::types::{ContentionPolicy, SceneId, WidgetInstance, WidgetParameterValue};

/// A Suspended lease (safe mode) blocks widget publishes; resuming restores them.
#[test]
fn widget_publish_with_suspended_lease_is_safe_mode_active() {
    let (mut scene, _tab) = scene_with_gauge(ContentionPolicy::LatestWins);
    let lease = scene.grant_lease("agent.test", 300_000);
    let params =
        || std::collections::HashMap::from([("level".to_string(), WidgetParameterValue::F32(0.5))]);
    scene.suspend_lease(&lease, 1).unwrap();
    let result = scene.publish_to_widget("gauge", params(), "agent.test", None, 0, None);
    assert!(
        matches!(
            result,
            Err(ValidationError::ZonePublishSafeModeActive { .. })
        ),
        "got: {result:?}"
    );
    scene.resume_lease(&lease, 2).unwrap();
    assert!(
        scene
            .publish_to_widget("gauge", params(), "agent.test", None, 0, None)
            .is_ok()
    );
}

// ── WidgetParameterValue validation ───────────────────────────────────────

/// WHEN an f32 NaN or infinity is submitted THEN publish_to_widget returns
/// WidgetParameterInvalidValue.
/// Source: widget-system/spec.md §Requirement: Widget Parameter Validation (F32 invariant).
#[test]
fn widget_publish_f32_nan_rejected() {
    for (label, value) in [
        ("NaN", f32::NAN),
        ("positive infinity", f32::INFINITY),
        ("negative infinity", f32::NEG_INFINITY),
    ] {
        let (mut scene, _tab) = scene_with_gauge(ContentionPolicy::LatestWins);
        let params = std::collections::HashMap::from([(
            "level".to_string(),
            WidgetParameterValue::F32(value),
        )]);
        let result = scene.publish_to_widget("gauge", params, "agent.test", None, 0, None);
        assert!(
            matches!(
                result,
                Err(ValidationError::WidgetParameterInvalidValue { .. })
            ),
            "{label} f32 should produce WidgetParameterInvalidValue, got: {result:?}"
        );
    }
}

/// WHEN a string value is submitted for an f32 parameter THEN type mismatch error.
/// Source: widget-system/spec.md §Requirement: Widget Parameter Validation (type safety).
#[test]
fn widget_publish_f32_type_mismatch_rejected() {
    let (mut scene, _tab) = scene_with_gauge(ContentionPolicy::LatestWins);
    let params = std::collections::HashMap::from([(
        "level".to_string(),
        WidgetParameterValue::String("not a float".to_string()),
    )]);
    let result = scene.publish_to_widget("gauge", params, "agent.test", None, 0, None);
    assert!(
        matches!(
            result,
            Err(ValidationError::WidgetParameterTypeMismatch { .. })
        ),
        "string for f32 param should produce WidgetParameterTypeMismatch, got: {result:?}"
    );
}

/// WHEN an enum value outside allowed_values is submitted THEN invalid value error.
/// Source: widget-system/spec.md §Requirement: Widget Parameter Validation (enum constraint).
#[test]
fn widget_publish_enum_out_of_allowed_values_rejected() {
    let (mut scene, _tab) = scene_with_gauge(ContentionPolicy::LatestWins);
    let params = std::collections::HashMap::from([(
        "severity".to_string(),
        WidgetParameterValue::Enum("critical".to_string()),
    )]);
    let result = scene.publish_to_widget("gauge", params, "agent.test", None, 0, None);
    assert!(
        matches!(
            result,
            Err(ValidationError::WidgetParameterInvalidValue { .. })
        ),
        "enum value outside allowed_values should produce WidgetParameterInvalidValue, got: {result:?}"
    );
}

/// WHEN an enum value within allowed_values is submitted THEN publish succeeds.
#[test]
fn widget_publish_enum_in_allowed_values_accepted() {
    let (mut scene, _tab) = scene_with_gauge(ContentionPolicy::LatestWins);
    let params = std::collections::HashMap::from([(
        "severity".to_string(),
        WidgetParameterValue::Enum("warning".to_string()),
    )]);
    let result = scene.publish_to_widget("gauge", params, "agent.test", None, 0, None);
    assert!(
        result.is_ok(),
        "valid enum value should be accepted, got: {result:?}"
    );
}

/// WHEN an f32 value is within [min, max] THEN it is accepted unchanged.
#[test]
fn widget_publish_f32_in_range_accepted_unchanged() {
    let (mut scene, _tab) = scene_with_gauge(ContentionPolicy::LatestWins);
    let params =
        std::collections::HashMap::from([("level".to_string(), WidgetParameterValue::F32(0.75))]);
    let result = scene.publish_to_widget("gauge", params, "agent.test", None, 0, None);
    assert!(result.is_ok(), "in-range f32 should be accepted");
}

/// WHEN an f32 value exceeds max THEN it is clamped, not rejected.
/// Source: widget-system/spec.md — f32 out of range is clamped.
#[test]
fn widget_publish_f32_above_max_clamped() {
    let (mut scene, _tab) = scene_with_gauge(ContentionPolicy::LatestWins);
    // level has max=1.0; submit 2.5 — should clamp to 1.0 without error
    let params =
        std::collections::HashMap::from([("level".to_string(), WidgetParameterValue::F32(2.5))]);
    let result = scene.publish_to_widget("gauge", params, "agent.test", None, 0, None);
    assert!(result.is_ok(), "out-of-range f32 should clamp, not reject");

    // The recorded publish should contain the clamped value.
    let pubs = scene.widget_registry.active_for_widget("gauge");
    assert_eq!(pubs.len(), 1);
    let recorded_level = pubs[0].params.get("level");
    assert!(
        matches!(recorded_level, Some(WidgetParameterValue::F32(v)) if (*v - 1.0).abs() < 1e-6),
        "clamped value should be 1.0, got: {recorded_level:?}"
    );
}

/// WHEN a parameter name is not in the widget schema THEN unknown-parameter error.
#[test]
fn widget_publish_unknown_parameter_rejected() {
    let (mut scene, _tab) = scene_with_gauge(ContentionPolicy::LatestWins);
    let params = std::collections::HashMap::from([(
        "bogus_param".to_string(),
        WidgetParameterValue::F32(0.5),
    )]);
    let result = scene.publish_to_widget("gauge", params, "agent.test", None, 0, None);
    assert!(
        matches!(result, Err(ValidationError::WidgetUnknownParameter { .. })),
        "unknown param name should produce WidgetUnknownParameter, got: {result:?}"
    );
}

/// WHEN a widget instance is not found THEN WidgetNotFound error.
#[test]
fn widget_publish_nonexistent_widget_rejected() {
    let mut scene = SceneGraph::new(1920.0, 1080.0);
    let params =
        std::collections::HashMap::from([("level".to_string(), WidgetParameterValue::F32(0.5))]);
    let result = scene.publish_to_widget("no-such-widget", params, "agent", None, 0, None);
    assert!(
        matches!(result, Err(ValidationError::WidgetNotFound { .. })),
        "nonexistent widget should produce WidgetNotFound, got: {result:?}"
    );
}

// ── Widget registry unit tests ─────────────────────────────────────────────

/// WHEN a widget definition is registered THEN it can be retrieved by id.
/// Source: widget-system/spec.md §Requirement: Widget Registry.
#[test]
fn widget_registry_register_and_retrieve_definition() {
    let mut scene = SceneGraph::new(1920.0, 1080.0);
    let def = make_gauge_definition();
    scene.widget_registry.register_definition(def.clone());

    let retrieved = scene.widget_registry.get_definition("gauge");
    assert!(
        retrieved.is_some(),
        "registered definition should be retrievable"
    );
    assert_eq!(retrieved.unwrap().id, "gauge");
    assert_eq!(retrieved.unwrap().parameter_schema.len(), 3);
}

/// WHEN a widget instance is registered THEN it can be retrieved by instance_name.
#[test]
fn widget_registry_register_and_retrieve_instance() {
    let mut scene = SceneGraph::new(1920.0, 1080.0);
    let tab_id = scene.create_tab("Main", 0).unwrap();

    scene
        .widget_registry
        .register_definition(make_gauge_definition());
    let instance = WidgetInstance {
        id: SceneId::new(),
        widget_type_name: "gauge".to_string(),
        tab_id,
        geometry_override: None,
        contention_override: None,
        instance_name: "cpu-gauge".to_string(),
        current_params: Default::default(),
    };
    scene.widget_registry.register_instance(instance);

    let retrieved = scene.widget_registry.get_instance("cpu-gauge");
    assert!(retrieved.is_some());
    assert_eq!(retrieved.unwrap().instance_name, "cpu-gauge");
    assert_eq!(retrieved.unwrap().widget_type_name, "gauge");
}

/// WHEN a definition is registered with the same id THEN it overwrites the old one.
#[test]
fn widget_registry_definition_overwrites_on_duplicate_id() {
    let mut scene = SceneGraph::new(1920.0, 1080.0);
    let mut def1 = make_gauge_definition();
    def1.description = "first".to_string();
    let mut def2 = make_gauge_definition();
    def2.description = "second".to_string();

    scene.widget_registry.register_definition(def1);
    scene.widget_registry.register_definition(def2);

    let retrieved = scene.widget_registry.get_definition("gauge").unwrap();
    assert_eq!(
        retrieved.description, "second",
        "second registration should win"
    );
}

#[test]
fn widget_registry_runtime_svg_handle_round_trip() {
    let mut scene = SceneGraph::new(1920.0, 1080.0);
    scene
        .widget_registry
        .register_runtime_svg_handle("gauge", "fill.svg", "asset:runtime-handle");
    assert_eq!(
        scene
            .widget_registry
            .runtime_svg_handle("gauge", "fill.svg"),
        Some("asset:runtime-handle")
    );
}

#[test]
fn pending_widget_svg_queue_drains_in_fifo_order() {
    let mut scene = SceneGraph::new(1920.0, 1080.0);
    scene.enqueue_widget_svg_asset("gauge", "a.svg", vec![1, 2, 3]);
    scene.enqueue_widget_svg_asset("gauge", "b.svg", vec![4, 5]);

    let drained = scene.drain_pending_widget_svg_assets();
    assert_eq!(drained.len(), 2);
    assert_eq!(drained[0].0, "gauge");
    assert_eq!(drained[0].1, "a.svg");
    assert_eq!(drained[0].2, vec![1, 2, 3]);
    assert_eq!(drained[1].1, "b.svg");
    assert!(scene.drain_pending_widget_svg_assets().is_empty());
}

/// WHEN querying occupancy with no active publications THEN effective_params
/// falls back to the definition's parameter defaults.
#[test]
fn widget_registry_occupancy_defaults_when_no_publications() {
    let (scene, tab_id) = scene_with_gauge(ContentionPolicy::LatestWins);

    let occ = scene
        .widget_registry
        .get_occupancy("gauge", tab_id)
        .unwrap();
    assert_eq!(occ.occupant_count, 0);
    assert_eq!(occ.active_publications.len(), 0);

    // Should fall back to definition defaults for all three declared parameters.
    let level = occ.effective_params.get("level");
    assert!(
        matches!(level, Some(WidgetParameterValue::F32(v)) if (*v - 0.0).abs() < 1e-6),
        "default level should be 0.0, got: {level:?}"
    );
    let label = occ.effective_params.get("label");
    assert!(
        matches!(label, Some(WidgetParameterValue::String(s)) if s.is_empty()),
        "default label should be empty string, got: {label:?}"
    );
    let severity = occ.effective_params.get("severity");
    assert!(
        matches!(severity, Some(WidgetParameterValue::Enum(s)) if s == "info"),
        "default severity should be 'info', got: {severity:?}"
    );
}

/// WHEN querying occupancy for an unknown instance THEN None is returned.
#[test]
fn widget_registry_occupancy_unknown_instance_returns_none() {
    let (scene, tab_id) = scene_with_gauge(ContentionPolicy::LatestWins);
    let occ = scene.widget_registry.get_occupancy("no-such-gauge", tab_id);
    assert!(occ.is_none(), "unknown instance should return None");
}

// ── get_occupancy per-policy effective_params tests ───────────────────────

/// LatestWins: WHEN one publication is active THEN effective_params = that
/// publication's params merged over schema defaults.
///
/// Source: widget-system/spec.md §Requirement: Widget Contention.
#[test]
fn widget_occupancy_latest_wins_merges_over_defaults() {
    let (mut scene, tab_id) = scene_with_gauge(ContentionPolicy::LatestWins);

    // Publish only "level"; "label" and "severity" should fall back to defaults.
    scene
        .publish_to_widget(
            "gauge",
            std::collections::HashMap::from([(
                "level".to_string(),
                WidgetParameterValue::F32(0.75),
            )]),
            "agent.a",
            None,
            0,
            None,
        )
        .unwrap();

    let occ = scene
        .widget_registry
        .get_occupancy("gauge", tab_id)
        .unwrap();
    assert_eq!(occ.occupant_count, 1);

    // Published param should reflect the publication value.
    let level = occ.effective_params.get("level");
    assert!(
        matches!(level, Some(WidgetParameterValue::F32(v)) if (*v - 0.75).abs() < 1e-6),
        "LatestWins level should be 0.75, got: {level:?}"
    );

    // Unpublished params should retain schema defaults.
    let label = occ.effective_params.get("label");
    assert!(
        matches!(label, Some(WidgetParameterValue::String(s)) if s.is_empty()),
        "LatestWins: missing label should fall back to default empty string, got: {label:?}"
    );
    let severity = occ.effective_params.get("severity");
    assert!(
        matches!(severity, Some(WidgetParameterValue::Enum(s)) if s == "info"),
        "LatestWins: missing severity should fall back to default 'info', got: {severity:?}"
    );
}

/// LatestWins: WHEN two sequential publishes arrive THEN effective_params
/// reflects only the most recent one (merged over defaults).
#[test]
fn widget_occupancy_latest_wins_uses_most_recent() {
    let (mut scene, tab_id) = scene_with_gauge(ContentionPolicy::LatestWins);

    scene
        .publish_to_widget(
            "gauge",
            std::collections::HashMap::from([(
                "level".to_string(),
                WidgetParameterValue::F32(0.2),
            )]),
            "agent.a",
            None,
            0,
            None,
        )
        .unwrap();
    scene
        .publish_to_widget(
            "gauge",
            std::collections::HashMap::from([(
                "level".to_string(),
                WidgetParameterValue::F32(0.9),
            )]),
            "agent.b",
            None,
            0,
            None,
        )
        .unwrap();

    let occ = scene
        .widget_registry
        .get_occupancy("gauge", tab_id)
        .unwrap();
    assert_eq!(
        occ.occupant_count, 1,
        "LatestWins retains only 1 publication"
    );
    let level = occ.effective_params.get("level");
    assert!(
        matches!(level, Some(WidgetParameterValue::F32(v)) if (*v - 0.9).abs() < 1e-6),
        "LatestWins: most recent level (0.9) should win, got: {level:?}"
    );
}

/// Stack: WHEN three publishes arrive THEN effective_params reflects the
/// top-of-stack (most recent) publication merged over defaults.
///
/// Source: widget-system/spec.md §Requirement: Widget Contention (Stack).
#[test]
fn widget_occupancy_stack_uses_top_of_stack() {
    let (mut scene, tab_id) = scene_with_gauge(ContentionPolicy::Stack { max_depth: 5 });

    for (i, level) in [0.1f32, 0.5f32, 0.8f32].iter().enumerate() {
        scene
            .publish_to_widget(
                "gauge",
                std::collections::HashMap::from([(
                    "level".to_string(),
                    WidgetParameterValue::F32(*level),
                )]),
                &format!("agent.{i}"),
                None,
                0,
                None,
            )
            .unwrap();
    }

    let occ = scene
        .widget_registry
        .get_occupancy("gauge", tab_id)
        .unwrap();
    assert_eq!(
        occ.occupant_count, 3,
        "Stack should have 3 active publications"
    );

    // Top-of-stack = most recent = last pushed = 0.8.
    let level = occ.effective_params.get("level");
    assert!(
        matches!(level, Some(WidgetParameterValue::F32(v)) if (*v - 0.8).abs() < 1e-6),
        "Stack: top-of-stack level should be 0.8, got: {level:?}"
    );

    // Unpublished params should fall back to schema defaults.
    let label = occ.effective_params.get("label");
    assert!(
        matches!(label, Some(WidgetParameterValue::String(s)) if s.is_empty()),
        "Stack: missing label should fall back to default empty string, got: {label:?}"
    );
}

/// Stack: WHEN stack exceeds max_depth THEN effective_params still reflects
/// the most recent (top-of-stack) publication.
#[test]
fn widget_occupancy_stack_top_after_depth_cap() {
    let (mut scene, tab_id) = scene_with_gauge(ContentionPolicy::Stack { max_depth: 3 });

    // Push 5 publications; oldest 2 will be evicted, leaving levels [0.2, 0.3, 0.4].
    for (i, level) in [0.0f32, 0.1f32, 0.2f32, 0.3f32, 0.4f32].iter().enumerate() {
        scene
            .publish_to_widget(
                "gauge",
                std::collections::HashMap::from([(
                    "level".to_string(),
                    WidgetParameterValue::F32(*level),
                )]),
                &format!("agent.{i}"),
                None,
                0,
                None,
            )
            .unwrap();
    }

    let occ = scene
        .widget_registry
        .get_occupancy("gauge", tab_id)
        .unwrap();
    assert_eq!(
        occ.occupant_count, 3,
        "Stack(3) should cap at 3 publications"
    );

    // Top-of-stack is the most recent surviving publication (0.4).
    let level = occ.effective_params.get("level");
    assert!(
        matches!(level, Some(WidgetParameterValue::F32(v)) if (*v - 0.4).abs() < 1e-6),
        "Stack: top-of-stack after depth cap should be 0.4, got: {level:?}"
    );
}

/// MergeByKey: WHEN two different-keyed publications are active THEN
/// effective_params merges both over defaults.
///
/// Source: widget-system/spec.md §Requirement: Widget Contention (MergeByKey).
#[test]
fn widget_occupancy_merge_by_key_merges_all_keys_over_defaults() {
    let (mut scene, tab_id) = scene_with_gauge(ContentionPolicy::MergeByKey { max_keys: 8 });

    // "cpu" key sets level=0.4; "mem" key sets level=0.6.
    // Since both touch the same param ("level"), the last-inserted key wins.
    scene
        .publish_to_widget(
            "gauge",
            std::collections::HashMap::from([(
                "level".to_string(),
                WidgetParameterValue::F32(0.4),
            )]),
            "agent.a",
            Some("cpu".to_string()),
            0,
            None,
        )
        .unwrap();
    scene
        .publish_to_widget(
            "gauge",
            std::collections::HashMap::from([
                ("level".to_string(), WidgetParameterValue::F32(0.6)),
                (
                    "label".to_string(),
                    WidgetParameterValue::String("mem".to_string()),
                ),
            ]),
            "agent.b",
            Some("mem".to_string()),
            0,
            None,
        )
        .unwrap();

    let occ = scene
        .widget_registry
        .get_occupancy("gauge", tab_id)
        .unwrap();
    assert_eq!(
        occ.occupant_count, 2,
        "MergeByKey should have 2 active publications"
    );

    // "mem" was pushed after "cpu", so its level (0.6) wins for "level".
    let level = occ.effective_params.get("level");
    assert!(
        matches!(level, Some(WidgetParameterValue::F32(v)) if (*v - 0.6).abs() < 1e-6),
        "MergeByKey: last-inserted key's level (0.6) should win, got: {level:?}"
    );

    // "label" was only set by "mem" — should appear in effective_params.
    let label = occ.effective_params.get("label");
    assert!(
        matches!(label, Some(WidgetParameterValue::String(s)) if s == "mem"),
        "MergeByKey: label from 'mem' key should be 'mem', got: {label:?}"
    );

    // "severity" was not set by either key — should fall back to schema default.
    let severity = occ.effective_params.get("severity");
    assert!(
        matches!(severity, Some(WidgetParameterValue::Enum(s)) if s == "info"),
        "MergeByKey: missing severity should fall back to default 'info', got: {severity:?}"
    );
}

/// MergeByKey: WHEN the same key is updated THEN effective_params reflects
/// the updated value.
#[test]
fn widget_occupancy_merge_by_key_updated_key_reflects_latest_value() {
    let (mut scene, tab_id) = scene_with_gauge(ContentionPolicy::MergeByKey { max_keys: 8 });

    scene
        .publish_to_widget(
            "gauge",
            std::collections::HashMap::from([(
                "level".to_string(),
                WidgetParameterValue::F32(0.3),
            )]),
            "agent.a",
            Some("cpu".to_string()),
            0,
            None,
        )
        .unwrap();
    // Same key — should replace the previous value in-place.
    scene
        .publish_to_widget(
            "gauge",
            std::collections::HashMap::from([(
                "level".to_string(),
                WidgetParameterValue::F32(0.7),
            )]),
            "agent.a",
            Some("cpu".to_string()),
            0,
            None,
        )
        .unwrap();

    let occ = scene
        .widget_registry
        .get_occupancy("gauge", tab_id)
        .unwrap();
    assert_eq!(
        occ.occupant_count, 1,
        "Same-key update should not add a second record"
    );

    let level = occ.effective_params.get("level");
    assert!(
        matches!(level, Some(WidgetParameterValue::F32(v)) if (*v - 0.7).abs() < 1e-6),
        "MergeByKey: updated key level should be 0.7, got: {level:?}"
    );
}

/// Replace: WHEN a publication is active THEN effective_params = that
/// publication's params only (no defaults for missing keys).
///
/// Source: widget-system/spec.md §Requirement: Widget Contention (Replace).
#[test]
fn widget_occupancy_replace_no_default_fallback_for_missing_keys() {
    let (mut scene, tab_id) = scene_with_gauge(ContentionPolicy::Replace);

    // Publish only "level" — "label" and "severity" are omitted intentionally.
    scene
        .publish_to_widget(
            "gauge",
            std::collections::HashMap::from([(
                "level".to_string(),
                WidgetParameterValue::F32(0.5),
            )]),
            "agent.a",
            None,
            0,
            None,
        )
        .unwrap();

    let occ = scene
        .widget_registry
        .get_occupancy("gauge", tab_id)
        .unwrap();
    assert_eq!(occ.occupant_count, 1);

    let level = occ.effective_params.get("level");
    assert!(
        matches!(level, Some(WidgetParameterValue::F32(v)) if (*v - 0.5).abs() < 1e-6),
        "Replace level should be 0.5, got: {level:?}"
    );

    // Replace must NOT include defaults for missing keys.
    assert!(
        !occ.effective_params.contains_key("label"),
        "Replace: absent keys must NOT be filled from defaults (label), got: {:?}",
        occ.effective_params.get("label")
    );
    assert!(
        !occ.effective_params.contains_key("severity"),
        "Replace: absent keys must NOT be filled from defaults (severity), got: {:?}",
        occ.effective_params.get("severity")
    );
}

/// Replace: WHEN two sequential publishes arrive THEN effective_params
/// reflects only the most recent one (no merge, no defaults).
#[test]
fn widget_occupancy_replace_uses_most_recent_params_only() {
    let (mut scene, tab_id) = scene_with_gauge(ContentionPolicy::Replace);

    scene
        .publish_to_widget(
            "gauge",
            std::collections::HashMap::from([(
                "level".to_string(),
                WidgetParameterValue::F32(0.1),
            )]),
            "agent.a",
            None,
            0,
            None,
        )
        .unwrap();
    scene
        .publish_to_widget(
            "gauge",
            std::collections::HashMap::from([(
                "label".to_string(),
                WidgetParameterValue::String("replaced".to_string()),
            )]),
            "agent.b",
            None,
            0,
            None,
        )
        .unwrap();

    let occ = scene
        .widget_registry
        .get_occupancy("gauge", tab_id)
        .unwrap();
    assert_eq!(occ.occupant_count, 1, "Replace retains only 1 publication");

    // Second publish only set "label"; "level" must NOT appear (not in params,
    // and Replace does not fall back to defaults).
    assert!(
        !occ.effective_params.contains_key("level"),
        "Replace: prior 'level' must be gone after Replace by second publish, got: {:?}",
        occ.effective_params.get("level")
    );
    let label = occ.effective_params.get("label");
    assert!(
        matches!(label, Some(WidgetParameterValue::String(s)) if s == "replaced"),
        "Replace: label from second publish should be 'replaced', got: {label:?}"
    );
}

/// WHEN a publish is recorded THEN active_for_widget returns it.
#[test]
fn widget_registry_publish_recorded_in_active_for_widget() {
    let (mut scene, _tab) = scene_with_gauge(ContentionPolicy::LatestWins);
    let params =
        std::collections::HashMap::from([("level".to_string(), WidgetParameterValue::F32(0.8))]);
    scene
        .publish_to_widget("gauge", params, "agent.a", None, 0, None)
        .unwrap();

    let active = scene.widget_registry.active_for_widget("gauge");
    assert_eq!(active.len(), 1);
    let level = active[0].params.get("level");
    assert!(
        matches!(level, Some(WidgetParameterValue::F32(v)) if (*v - 0.8).abs() < 1e-6),
        "recorded level should be 0.8, got: {level:?}"
    );
}

/// WHEN snapshot() is called THEN it includes all registered types and instances.
#[test]
fn widget_registry_snapshot_includes_all_types_and_instances() {
    let (mut scene, tab_id) = scene_with_gauge(ContentionPolicy::LatestWins);

    // Add a second instance
    scene.widget_registry.register_instance(WidgetInstance {
        id: SceneId::new(),
        widget_type_name: "gauge".to_string(),
        tab_id,
        geometry_override: None,
        contention_override: None,
        instance_name: "mem-gauge".to_string(),
        current_params: Default::default(),
    });

    let snapshot = scene.widget_registry.snapshot();
    assert_eq!(snapshot.widget_types.len(), 1, "one type registered");
    assert_eq!(snapshot.widget_instances.len(), 2, "two instances");
}

// ── Widget contention policy tests ─────────────────────────────────────────

/// LatestWins: WHEN two publishes arrive THEN only the latest is retained.
/// Source: widget-system/spec.md §Requirement: Widget Contention.
#[test]
fn widget_contention_latest_wins_replaces_previous() {
    let (mut scene, _tab) = scene_with_gauge(ContentionPolicy::LatestWins);

    scene
        .publish_to_widget(
            "gauge",
            std::collections::HashMap::from([(
                "level".to_string(),
                WidgetParameterValue::F32(0.3),
            )]),
            "agent.a",
            None,
            0,
            None,
        )
        .unwrap();
    scene
        .publish_to_widget(
            "gauge",
            std::collections::HashMap::from([(
                "level".to_string(),
                WidgetParameterValue::F32(0.7),
            )]),
            "agent.b",
            None,
            0,
            None,
        )
        .unwrap();

    let active = scene.widget_registry.active_for_widget("gauge");
    assert_eq!(active.len(), 1, "LatestWins keeps only one publication");
    assert!(
        matches!(active[0].params.get("level"), Some(WidgetParameterValue::F32(v)) if (*v - 0.7).abs() < 1e-6),
        "latest publish (0.7) should win"
    );
}

/// Replace: identical to LatestWins in effect — only one record retained.
#[test]
fn widget_contention_replace_retains_only_latest() {
    let (mut scene, _tab) = scene_with_gauge(ContentionPolicy::Replace);

    scene
        .publish_to_widget(
            "gauge",
            std::collections::HashMap::from([(
                "level".to_string(),
                WidgetParameterValue::F32(0.1),
            )]),
            "agent.a",
            None,
            0,
            None,
        )
        .unwrap();
    scene
        .publish_to_widget(
            "gauge",
            std::collections::HashMap::from([(
                "level".to_string(),
                WidgetParameterValue::F32(0.9),
            )]),
            "agent.b",
            None,
            0,
            None,
        )
        .unwrap();

    let active = scene.widget_registry.active_for_widget("gauge");
    assert_eq!(active.len(), 1, "Replace keeps only one publication");
    assert!(
        matches!(active[0].params.get("level"), Some(WidgetParameterValue::F32(v)) if (*v - 0.9).abs() < 1e-6),
    );
}

/// Stack: WHEN max_depth=3 and 4 publishes arrive THEN oldest is evicted.
/// Source: widget-system/spec.md §Requirement: Widget Contention (Stack depth cap).
#[test]
fn widget_contention_stack_evicts_oldest_at_max_depth() {
    let (mut scene, _tab) = scene_with_gauge(ContentionPolicy::Stack { max_depth: 3 });

    for i in 0u32..4 {
        scene
            .publish_to_widget(
                "gauge",
                std::collections::HashMap::from([(
                    "level".to_string(),
                    WidgetParameterValue::F32(i as f32 * 0.25),
                )]),
                &format!("agent.{i}"),
                None,
                0,
                None,
            )
            .unwrap();
    }

    let active = scene.widget_registry.active_for_widget("gauge");
    assert_eq!(active.len(), 3, "Stack(3) should keep at most 3 records");

    // The oldest (i=0, level=0.0) should have been evicted.
    let has_zero = active.iter().any(|r| {
        matches!(r.params.get("level"), Some(WidgetParameterValue::F32(v)) if (*v).abs() < 1e-6)
    });
    assert!(!has_zero, "oldest publish (level=0.0) should be evicted");

    // The correct items (i=1,2,3) should all be present.
    let levels: std::collections::BTreeSet<u32> = active
        .iter()
        .filter_map(|r| {
            if let Some(WidgetParameterValue::F32(v)) = r.params.get("level") {
                Some((v * 4.0).round() as u32)
            } else {
                None
            }
        })
        .collect();
    let expected_levels: std::collections::BTreeSet<u32> = [1, 2, 3].into();
    assert_eq!(
        levels, expected_levels,
        "Stack(3) should contain levels for i=1, 2, 3"
    );
}

/// Stack: WHEN max_depth=0 THEN every publish is immediately trimmed out,
/// leaving the stack empty.
///
/// Canonical semantics (matches zone publish_to_zone behavior): the push is
/// followed by a trim that drains all entries when max_depth == 0, so the
/// record is silently discarded.  The old widget implementation had a
/// diverged `if max > 0 &&` guard that made max_depth=0 unbounded instead —
/// that was a bug corrected by extracting apply_contention.
#[test]
fn widget_contention_stack_max_depth_zero_discards_all() {
    let (mut scene, _tab) = scene_with_gauge(ContentionPolicy::Stack { max_depth: 0 });

    for i in 0u32..3 {
        scene
            .publish_to_widget(
                "gauge",
                std::collections::HashMap::from([(
                    "level".to_string(),
                    WidgetParameterValue::F32(i as f32 * 0.1),
                )]),
                &format!("agent.{i}"),
                None,
                0,
                None,
            )
            .unwrap();
    }

    let active = scene.widget_registry.active_for_widget("gauge");
    assert_eq!(
        active.len(),
        0,
        "Stack(0) trims to 0: all publishes must be discarded (canonical semantics)"
    );
}

/// MergeByKey: WHEN same key is published twice THEN the record is replaced.
/// WHEN a different key is published THEN both records coexist.
/// Source: widget-system/spec.md §Requirement: Widget Contention (MergeByKey).
#[test]
fn widget_contention_merge_by_key_replaces_same_key() {
    let (mut scene, _tab) = scene_with_gauge(ContentionPolicy::MergeByKey { max_keys: 8 });

    scene
        .publish_to_widget(
            "gauge",
            std::collections::HashMap::from([(
                "level".to_string(),
                WidgetParameterValue::F32(0.4),
            )]),
            "agent.a",
            Some("cpu".to_string()),
            0,
            None,
        )
        .unwrap();
    scene
        .publish_to_widget(
            "gauge",
            std::collections::HashMap::from([(
                "level".to_string(),
                WidgetParameterValue::F32(0.6),
            )]),
            "agent.b",
            Some("mem".to_string()),
            0,
            None,
        )
        .unwrap();
    // Overwrite "cpu" key
    scene
        .publish_to_widget(
            "gauge",
            std::collections::HashMap::from([(
                "level".to_string(),
                WidgetParameterValue::F32(0.2),
            )]),
            "agent.a",
            Some("cpu".to_string()),
            0,
            None,
        )
        .unwrap();

    let active = scene.widget_registry.active_for_widget("gauge");
    assert_eq!(active.len(), 2, "MergeByKey should keep one record per key");

    let cpu_pub = active
        .iter()
        .find(|r| r.merge_key.as_deref() == Some("cpu"))
        .unwrap();
    assert!(
        matches!(cpu_pub.params.get("level"), Some(WidgetParameterValue::F32(v)) if (*v - 0.2).abs() < 1e-6),
        "cpu key should have updated to 0.2"
    );

    // The mem key must remain unaffected at its original value (0.6).
    let mem_pub = active
        .iter()
        .find(|r| r.merge_key.as_deref() == Some("mem"))
        .unwrap();
    assert!(
        matches!(mem_pub.params.get("level"), Some(WidgetParameterValue::F32(v)) if (*v - 0.6).abs() < 1e-6),
        "mem key should be unaffected and still be 0.6"
    );
}

/// WHEN drain_expired_widget_publications is called before any expiry time
/// has elapsed THEN no publications are removed.
///
/// Source: widget-system/spec.md §Requirement: Expiration Policy.
#[test]
fn widget_ttl_publication_not_expired_before_deadline() {
    let (mut scene, _tab, _clock) = scene_with_gauge_and_clock(ContentionPolicy::LatestWins);

    // Publish with an expiry 10 s in the future (clock is at 1 000 ms = 1 000 000 µs).
    let expires_at = 1_000_000u64 + 10_000_000u64; // +10 s
    scene
        .publish_to_widget(
            "gauge",
            std::collections::HashMap::from([(
                "level".to_string(),
                WidgetParameterValue::F32(0.5),
            )]),
            "agent.test",
            None,
            0,
            Some(expires_at),
        )
        .unwrap();

    // Drain without advancing the clock — publication must survive.
    let removed = scene.drain_expired_widget_publications();
    assert_eq!(removed, 0, "no publications should expire before deadline");
    assert_eq!(
        scene.widget_registry.active_for_widget("gauge").len(),
        1,
        "publication must still be present"
    );
}

/// WHEN drain_expired_widget_publications is called after the expiry time
/// has elapsed THEN the publication is removed.
///
/// Source: widget-system/spec.md §Requirement: Expiration Policy.
#[test]
fn widget_ttl_publication_expires_after_deadline() {
    let (mut scene, _tab, clock) = scene_with_gauge_and_clock(ContentionPolicy::LatestWins);

    // Publish with a 1 s TTL (expires 1 s after t=1 000 ms).
    let expires_at = 1_000_000u64 + 1_000_000u64; // expires at t=2 000 ms
    scene
        .publish_to_widget(
            "gauge",
            std::collections::HashMap::from([(
                "level".to_string(),
                WidgetParameterValue::F32(0.5),
            )]),
            "agent.test",
            None,
            0,
            Some(expires_at),
        )
        .unwrap();

    // Advance clock past the expiry point.
    clock.advance(1_001); // now at t=2 001 ms = 2 001 000 µs

    let removed = scene.drain_expired_widget_publications();
    assert_eq!(removed, 1, "one publication should have expired");
    assert_eq!(
        scene.widget_registry.active_for_widget("gauge").len(),
        0,
        "expired publication must be removed"
    );
}

/// WHEN drain_expired_widget_publications removes all publications from a
/// widget THEN the active_publishes entry is cleaned up (no empty Vec left).
///
/// Source: widget-system/spec.md §Requirement: Expiration Policy.
#[test]
fn widget_ttl_empty_entry_cleaned_up_after_expiry() {
    let (mut scene, _tab, clock) = scene_with_gauge_and_clock(ContentionPolicy::LatestWins);

    let expires_at = 1_000_000u64 + 500_000u64; // +500 ms
    scene
        .publish_to_widget(
            "gauge",
            std::collections::HashMap::from([(
                "level".to_string(),
                WidgetParameterValue::F32(0.75),
            )]),
            "agent.test",
            None,
            0,
            Some(expires_at),
        )
        .unwrap();

    clock.advance(600); // advance 600 ms past expiry
    scene.drain_expired_widget_publications();

    // The HashMap entry itself must be gone (no empty Vec).
    assert!(
        !scene.widget_registry.active_publishes.contains_key("gauge"),
        "empty widget publication entry must be removed after expiry"
    );
}

/// WHEN a publication with no expiry and one with an expiry coexist (Stack
/// policy) THEN only the expired publication is removed.
///
/// Source: widget-system/spec.md §Requirement: Expiration Policy.
#[test]
fn widget_ttl_only_expired_publication_removed_when_mixed() {
    let (mut scene, _tab, clock) =
        scene_with_gauge_and_clock(ContentionPolicy::Stack { max_depth: 10 });

    let now_us = 1_000_000u64; // clock starts at t=1 000 ms
    let expires_soon = now_us + 500_000u64; // expires in 500 ms

    // Publish the soon-to-expire record first.
    scene
        .publish_to_widget(
            "gauge",
            std::collections::HashMap::from([(
                "level".to_string(),
                WidgetParameterValue::F32(0.1),
            )]),
            "agent.short",
            None,
            0,
            Some(expires_soon),
        )
        .unwrap();

    // Publish a permanent record (no expiry).
    scene
        .publish_to_widget(
            "gauge",
            std::collections::HashMap::from([(
                "level".to_string(),
                WidgetParameterValue::F32(0.9),
            )]),
            "agent.permanent",
            None,
            0,
            None,
        )
        .unwrap();

    assert_eq!(
        scene.widget_registry.active_for_widget("gauge").len(),
        2,
        "both publications should be present before expiry"
    );

    // Advance clock past the short expiry.
    clock.advance(600);

    let removed = scene.drain_expired_widget_publications();
    assert_eq!(removed, 1, "only the TTL publication should expire");

    let remaining = scene.widget_registry.active_for_widget("gauge");
    assert_eq!(remaining.len(), 1, "one publication should remain");
    assert_eq!(
        remaining[0].publisher_namespace, "agent.permanent",
        "the permanent publication should survive"
    );
}

/// WHEN drain_expired_widget_publications removes a publication THEN the
/// scene version is incremented.
///
/// Source: widget-system/spec.md §Requirement: Expiration Policy.
#[test]
fn widget_ttl_expiry_bumps_scene_version() {
    let (mut scene, _tab, clock) = scene_with_gauge_and_clock(ContentionPolicy::LatestWins);

    let expires_at = 1_000_000u64 + 200_000u64;
    scene
        .publish_to_widget(
            "gauge",
            std::collections::HashMap::from([(
                "level".to_string(),
                WidgetParameterValue::F32(0.3),
            )]),
            "agent.test",
            None,
            0,
            Some(expires_at),
        )
        .unwrap();

    let version_before = scene.version;
    clock.advance(300);
    scene.drain_expired_widget_publications();

    assert!(
        scene.version > version_before,
        "scene version must be incremented when a widget publication expires"
    );
}

/// WHEN drain_expired_widget_publications is called with no publications
/// THEN it returns 0 and does not panic.
///
/// Source: widget-system/spec.md §Requirement: Expiration Policy.
#[test]
fn widget_ttl_drain_with_no_publications_is_noop() {
    let (mut scene, _tab, _clock) = scene_with_gauge_and_clock(ContentionPolicy::LatestWins);

    let removed = scene.drain_expired_widget_publications();
    assert_eq!(removed, 0, "draining an empty registry must return 0");
}

// ── clear_widget_for_publisher tests ──────────────────────────────────────

/// WHEN clear_widget_for_publisher is called with the publishing namespace
/// THEN that agent's publications are removed and the widget reverts to defaults.
#[test]
fn clear_widget_for_publisher_removes_own_publications() {
    let (mut scene, _tab) = scene_with_gauge(ContentionPolicy::LatestWins);

    // Publish as "agent.a"
    scene
        .publish_to_widget(
            "gauge",
            std::collections::HashMap::from([(
                "level".to_string(),
                WidgetParameterValue::F32(0.9),
            )]),
            "agent.a",
            None,
            0,
            None,
        )
        .unwrap();
    assert_eq!(scene.widget_registry.active_for_widget("gauge").len(), 1);

    // Clear as "agent.a" — should remove the publication
    scene
        .clear_widget_for_publisher("gauge", "agent.a")
        .unwrap();
    assert_eq!(
        scene.widget_registry.active_for_widget("gauge").len(),
        0,
        "agent.a's publication should be cleared"
    );
    match scene.widget_registry.instances["gauge"]
        .current_params
        .get("level")
    {
        Some(WidgetParameterValue::F32(v)) => {
            assert!(
                (*v - 0.0).abs() < f32::EPSILON,
                "level should reset to default after clear, got {v}"
            )
        }
        other => panic!("expected default F32 level after clear, got {other:?}"),
    }
}

/// WHEN the top stacked widget publication is cleared THEN current_params
/// refresh to the remaining publication instead of retaining stale pixels.
#[test]
fn clear_widget_for_publisher_refreshes_current_params_from_remaining_publish() {
    let (mut scene, _tab) = scene_with_gauge(ContentionPolicy::Stack { max_depth: 4 });

    scene
        .publish_to_widget(
            "gauge",
            std::collections::HashMap::from([(
                "level".to_string(),
                WidgetParameterValue::F32(0.3),
            )]),
            "agent.a",
            None,
            0,
            None,
        )
        .unwrap();
    scene
        .publish_to_widget(
            "gauge",
            std::collections::HashMap::from([(
                "level".to_string(),
                WidgetParameterValue::F32(0.7),
            )]),
            "agent.b",
            None,
            0,
            None,
        )
        .unwrap();

    scene
        .clear_widget_for_publisher("gauge", "agent.b")
        .unwrap();

    match scene.widget_registry.instances["gauge"]
        .current_params
        .get("level")
    {
        Some(WidgetParameterValue::F32(v)) => {
            assert!(
                (*v - 0.3).abs() < f32::EPSILON,
                "level should refresh to remaining publication, got {v}"
            )
        }
        other => panic!("expected remaining F32 level after clear, got {other:?}"),
    }
}

/// WHEN clear_widget_for_publisher is called with a different namespace
/// THEN only the matching publisher's records are removed.
#[test]
fn clear_widget_for_publisher_only_affects_own_publications() {
    let (mut scene, _tab) = scene_with_gauge(ContentionPolicy::Stack { max_depth: 4 });

    // Publish as "agent.a" and "agent.b"
    scene
        .publish_to_widget(
            "gauge",
            std::collections::HashMap::from([(
                "level".to_string(),
                WidgetParameterValue::F32(0.3),
            )]),
            "agent.a",
            None,
            0,
            None,
        )
        .unwrap();
    scene
        .publish_to_widget(
            "gauge",
            std::collections::HashMap::from([(
                "level".to_string(),
                WidgetParameterValue::F32(0.7),
            )]),
            "agent.b",
            None,
            0,
            None,
        )
        .unwrap();
    assert_eq!(scene.widget_registry.active_for_widget("gauge").len(), 2);

    // Clear as "agent.a" — only "agent.a"'s publication should be removed
    scene
        .clear_widget_for_publisher("gauge", "agent.a")
        .unwrap();
    let remaining = scene.widget_registry.active_for_widget("gauge");
    assert_eq!(
        remaining.len(),
        1,
        "only agent.a's publication should be cleared"
    );
    assert_eq!(
        remaining[0].publisher_namespace, "agent.b",
        "agent.b's publication should remain"
    );
}

/// WHEN clear_widget_for_publisher is called for a namespace with no publications
/// THEN it succeeds as a no-op.
#[test]
fn clear_widget_for_publisher_noop_when_no_publications() {
    let (mut scene, _tab) = scene_with_gauge(ContentionPolicy::LatestWins);

    // No publications yet — clear should succeed silently
    let result = scene.clear_widget_for_publisher("gauge", "agent.nobody");
    assert!(
        result.is_ok(),
        "should succeed even when no publications exist"
    );
    assert_eq!(scene.widget_registry.active_for_widget("gauge").len(), 0);
}

/// WHEN clear_widget_for_publisher is called with an unknown widget name
/// THEN it returns WidgetNotFound.
#[test]
fn clear_widget_for_publisher_widget_not_found() {
    let (mut scene, _tab) = scene_with_gauge(ContentionPolicy::LatestWins);

    let result = scene.clear_widget_for_publisher("nonexistent", "agent.a");
    assert!(
        matches!(result, Err(ValidationError::WidgetNotFound { .. })),
        "unknown widget should produce WidgetNotFound, got: {result:?}"
    );
}

/// WHEN clear_publications_for_lease is called
/// THEN ALL widget publications under that lease are removed across all widgets,
/// and publications under other leases remain.
#[test]
fn clear_publications_for_lease_removes_only_that_leases_widget_pubs() {
    let (lease_a, lease_b) = (SceneId::new(), SceneId::new());
    let (mut scene, tab_id) = scene_with_gauge(ContentionPolicy::LatestWins);

    // Register a second widget instance using the same definition
    scene.widget_registry.register_instance(WidgetInstance {
        id: SceneId::new(),
        widget_type_name: "gauge".to_string(),
        tab_id,
        geometry_override: None,
        contention_override: None,
        instance_name: "mem-gauge".to_string(),
        current_params: Default::default(),
    });

    // Publish as "agent.a" to both widgets
    scene
        .publish_to_widget_for_lease(
            "gauge",
            std::collections::HashMap::from([(
                "level".to_string(),
                WidgetParameterValue::F32(0.5),
            )]),
            "agent.a",
            None,
            0,
            None,
            Some(lease_a),
        )
        .unwrap();
    scene
        .publish_to_widget_for_lease(
            "mem-gauge",
            std::collections::HashMap::from([(
                "level".to_string(),
                WidgetParameterValue::F32(0.8),
            )]),
            "agent.a",
            None,
            0,
            None,
            Some(lease_a),
        )
        .unwrap();

    // Publish as "agent.b" to "gauge" only
    scene
        .publish_to_widget_for_lease(
            "gauge",
            std::collections::HashMap::from([(
                "level".to_string(),
                WidgetParameterValue::F32(0.9),
            )]),
            "agent.b",
            None,
            0,
            None,
            Some(lease_b),
        )
        .unwrap();

    // Clear ALL of lease_a's publications
    scene.clear_publications_for_lease(lease_a);

    // "agent.a"'s publication on "gauge" is gone; "agent.b"'s remains
    let gauge_pubs = scene.widget_registry.active_for_widget("gauge");
    assert_eq!(
        gauge_pubs.len(),
        1,
        "only agent.b's gauge pub should remain"
    );
    assert_eq!(gauge_pubs[0].publisher_namespace, "agent.b");

    // "agent.a"'s publication on "mem-gauge" is gone
    let mem_pubs = scene.widget_registry.active_for_widget("mem-gauge");
    assert_eq!(
        mem_pubs.len(),
        0,
        "agent.a's mem-gauge pub should be cleared"
    );
    match scene.widget_registry.instances["mem-gauge"]
        .current_params
        .get("level")
    {
        Some(WidgetParameterValue::F32(v)) => {
            assert!(
                (*v - 0.0).abs() < f32::EPSILON,
                "mem-gauge should reset to default, got {v}"
            )
        }
        other => {
            panic!("expected default level for mem-gauge after namespace clear, got {other:?}")
        }
    }
}

/// WHEN ClearWidget is sent as a scene mutation batch
/// THEN it removes the agent's publications via the standard pipeline.
#[test]
fn clear_widget_via_mutation_batch() {
    use crate::mutation::{MutationBatch, SceneMutation};

    let (mut scene, _tab) = scene_with_gauge(ContentionPolicy::Stack { max_depth: 4 });

    // Publish as two agents
    scene
        .publish_to_widget(
            "gauge",
            std::collections::HashMap::from([(
                "level".to_string(),
                WidgetParameterValue::F32(0.5),
            )]),
            "agent.a",
            None,
            0,
            None,
        )
        .unwrap();
    scene
        .publish_to_widget(
            "gauge",
            std::collections::HashMap::from([(
                "level".to_string(),
                WidgetParameterValue::F32(0.3),
            )]),
            "agent.b",
            None,
            0,
            None,
        )
        .unwrap();
    assert_eq!(scene.widget_registry.active_for_widget("gauge").len(), 2);

    // Send ClearWidget from "agent.a"
    let batch = MutationBatch {
        batch_id: SceneId::new(),
        agent_namespace: "agent.a".to_string(),
        mutations: vec![SceneMutation::ClearWidget {
            widget_name: "gauge".to_string(),
            instance_id: None,
        }],
        timing_hints: None,
        lease_id: None,
    };
    let result = scene.apply_batch(&batch);
    assert!(result.applied, "ClearWidget batch should be accepted");

    // Only "agent.b"'s publication should remain
    let remaining = scene.widget_registry.active_for_widget("gauge");
    assert_eq!(
        remaining.len(),
        1,
        "agent.a's publication should be cleared"
    );
    assert_eq!(remaining[0].publisher_namespace, "agent.b");
}
