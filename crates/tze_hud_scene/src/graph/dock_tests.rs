use std::collections::HashMap;
use std::num::NonZeroU64;

use super::test_helpers::{make_gauge_definition, scene_with_gauge_and_clock};
use super::*;
use crate::clock::TestClock;
use crate::dock::{DockLayout, DockMetrics, DockPublishError};
use crate::scope::scoped_namespace;
use crate::widget_deadline::WidgetDeadline;

const PROGRESS: &str = "progress-bar";
const COUNTDOWN: &str = "countdown";

fn configure(scene: &mut SceneGraph, max_rows: usize, cap_ms: u64) {
    let layout = DockLayout::new(
        scene.display_area,
        max_rows,
        DockMetrics {
            row_height: 52.0,
            caption_width: 96.0,
            gap: 8.0,
        },
        [360.0 / 52.0, 160.0 / 52.0],
    );
    scene.configure_dock(
        vec![PROGRESS.into(), COUNTDOWN.into()],
        layout,
        NonZeroU64::new(cap_ms).unwrap(),
    );
}

fn setup(max_rows: usize) -> (SceneGraph, TestClock) {
    let (mut scene, _, clock) = scene_with_gauge_and_clock(ContentionPolicy::LatestWins);
    for widget_type in [PROGRESS, COUNTDOWN] {
        let mut definition = make_gauge_definition();
        definition.id = widget_type.into();
        definition.default_contention_policy = ContentionPolicy::Stack { max_depth: 2 };
        definition
            .parameter_schema
            .push(WidgetParameterDeclaration {
                name: "hover".into(),
                audience: WidgetParamAudience::Runtime,
                param_type: WidgetParamType::F32,
                default_value: WidgetParameterValue::F32(0.0),
                constraints: None,
            });
        if widget_type == COUNTDOWN {
            definition
                .parameter_schema
                .push(WidgetParameterDeclaration {
                    name: "ends_in_ms".into(),
                    audience: WidgetParamAudience::Model,
                    param_type: WidgetParamType::Deadline,
                    default_value: WidgetParameterValue::Deadline(WidgetDeadline::relative(0)),
                    constraints: None,
                });
        }
        scene.widget_registry.register_definition(definition);
    }
    configure(&mut scene, max_rows, 5_000);
    (scene, clock)
}

fn level(value: f32) -> HashMap<String, WidgetParameterValue> {
    HashMap::from([("level".into(), WidgetParameterValue::F32(value))])
}

fn record<'a>(scene: &'a SceneGraph, namespace: &str, ty: &str) -> &'a WidgetPublishRecord {
    let name = &scene.dock_instance(namespace, ty).unwrap().instance_name;
    let records = scene.widget_registry.active_for_widget(name);
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].publisher_namespace, namespace);
    &records[0]
}

#[test]
fn dock_identity_and_order_survive_updates_and_rejected_admission() {
    let (mut scene, clock) = setup(3);
    let a = scoped_namespace("agent", "a");
    let b = scoped_namespace("agent", "b");
    let a_id = scene
        .publish_to_dock(PROGRESS, level(0.25), &a, None, 120, 0)
        .unwrap();
    let b_id = scene
        .publish_to_dock(PROGRESS, level(0.5), &b, None, 0, 0)
        .unwrap();
    let bare = scene
        .publish_to_dock(PROGRESS, level(1.0), "agent", None, 0, 0)
        .unwrap();
    assert_ne!(a_id, b_id);
    assert_ne!(a_id, bare);
    assert_ne!(b_id, bare);
    assert_eq!(
        scene
            .dock_rows()
            .iter()
            .map(|row| row.namespace.as_str())
            .collect::<Vec<_>>(),
        [a.as_str(), b.as_str(), "agent"]
    );
    let a_progress = record(&scene, &a, PROGRESS);
    assert_eq!(a_progress.transition_ms, 120);
    let a_name = a_progress.widget_name.clone();
    let initial = scene.take_snapshot(clock.now_us(), clock.monotonic_us());
    let version = scene.version;
    assert!(matches!(
        scene.publish_to_dock(PROGRESS, level(2.0), &a, None, 0, 0),
        Err(DockPublishError::Widget(
            ValidationError::WidgetParameterInvalidValue { .. }
        ))
    ));
    assert!(matches!(
        scene.publish_to_dock("gauge", level(0.0), &a, None, 0, 0),
        Err(DockPublishError::Widget(
            ValidationError::WidgetNotFound { .. }
        ))
    ));
    assert_eq!(
        scene.publish_to_dock(PROGRESS, level(0.0), "new-agent", None, 0, 0),
        Err(DockPublishError::Full)
    );
    assert_eq!(scene.version, version);
    assert_eq!(
        scene.take_snapshot(clock.now_us(), clock.monotonic_us()),
        initial
    );
    assert_eq!(scene.dock_rows()[0].widgets[0].instance_name, a_name);
    assert_eq!(scene.dock_rows()[0].widgets.len(), 1);
    let deadline = HashMap::from([(
        "ends_in_ms".into(),
        WidgetParameterValue::Deadline(WidgetDeadline::relative(300_000)),
    )]);
    let countdown_id = scene
        .publish_to_dock(COUNTDOWN, deadline, &a, None, 0, 0)
        .unwrap();
    let original_deadline = record(&scene, &a, COUNTDOWN).params["ends_in_ms"].clone();
    let WidgetParameterValue::Deadline(deadline) = &original_deadline else {
        panic!("snapshot countdown deadline")
    };
    assert_eq!(deadline.at_wall_us, Some(clock.now_us() + 300_000_000));
    clock.advance(250);
    let label = HashMap::from([(
        "label".into(),
        WidgetParameterValue::String("updated".into()),
    )]);
    assert_eq!(
        scene.publish_to_dock(COUNTDOWN, label.clone(), &a, None, 0, 0),
        Ok(countdown_id)
    );
    let countdown = record(&scene, &a, COUNTDOWN);
    assert_eq!(countdown.params["ends_in_ms"], original_deadline);
    assert_eq!(countdown.params["label"], label["label"]);
    assert_eq!(
        scene.publish_to_dock(PROGRESS, level(0.9), &a, None, 0, 0),
        Ok(a_id)
    );
    assert_eq!(record(&scene, &a, PROGRESS).params, level(0.9));
    assert_eq!(record(&scene, &b, PROGRESS).params, level(0.5));
    assert_eq!(record(&scene, "agent", PROGRESS).params, level(1.0));
    assert_eq!(
        scene.dock_rows()[0]
            .widgets
            .iter()
            .map(|widget| widget.widget_type.as_str())
            .collect::<Vec<_>>(),
        [PROGRESS, COUNTDOWN]
    );
    assert_eq!(scene.dock_rows()[0].widgets[0].instance_name, a_name);

    // A smaller display hides the excess rows later in presentation, without
    // evicting the scene publications or interfering with their updates.
    configure(&mut scene, 1, 5_000);
    assert_eq!(scene.dock_rows().len(), 3);
    assert_eq!(
        scene.publish_to_dock(PROGRESS, level(0.6), &b, None, 0, 0),
        Ok(b_id)
    );
    assert_eq!(
        scene.publish_to_dock(PROGRESS, level(0.0), "new-agent", None, 0, 0),
        Err(DockPublishError::Full)
    );
}

#[test]
fn dock_hold_and_expiry_use_the_cap_captured_by_each_publish() {
    let (mut scene, clock) = setup(3);
    let a_id = scene
        .publish_to_dock(PROGRESS, level(0.0), "a", None, 80, 0)
        .unwrap();
    let short_id = scene
        .publish_to_dock(PROGRESS, level(0.0), "b", None, 0, 1_000)
        .unwrap();
    scene
        .publish_to_dock(PROGRESS, level(0.0), "c", None, 0, 50_000)
        .unwrap();
    let initial = clock.now_us();
    let expiry = |scene: &SceneGraph, owner| scene.widget_expiry(record(scene, owner, PROGRESS));
    assert_eq!(expiry(&scene, "a"), Some(initial + 5_000_000));
    assert_eq!(expiry(&scene, "b"), Some(initial + 1_000_000));
    assert_eq!(expiry(&scene, "c"), Some(initial + 5_000_000));

    clock.advance(250);
    configure(&mut scene, 3, 2_000);
    assert!(scene.hold_dock_widget("c", PROGRESS, 0));
    assert_eq!(expiry(&scene, "c"), Some(initial + 5_000_000));
    assert!(scene.hold_dock_widget("a", PROGRESS, 500));
    assert_eq!(expiry(&scene, "a"), Some(initial + 750_000));
    assert!(scene.hold_dock_widget("a", PROGRESS, 60_000));
    assert_eq!(expiry(&scene, "a"), Some(initial + 5_000_000));
    assert!(
        scene
            .publish_to_dock(PROGRESS, level(2.0), "a", None, 0, 0)
            .is_err()
    );
    assert_eq!(expiry(&scene, "a"), Some(initial + 5_000_000));
    assert_eq!(
        scene.publish_to_dock(PROGRESS, level(0.25), "a", None, 0, 0),
        Ok(a_id)
    );
    assert_eq!(expiry(&scene, "a"), Some(initial + 2_250_000));

    let expired_widget = record(&scene, "b", PROGRESS).widget_name.clone();
    scene.pause_widget_ttls();
    clock.advance(10_000);
    assert_eq!(scene.next_publication_expiry_wall_us(), None);
    assert_eq!(scene.drain_expired_widget_publications(), 0);
    scene.resume_widget_ttls();
    assert_eq!(expiry(&scene, "a"), Some(initial + 12_250_000));
    assert_eq!(expiry(&scene, "b"), Some(initial + 11_000_000));
    clock.advance(749);
    assert_eq!(scene.drain_expired_widget_publications(), 0);
    clock.advance(1);
    assert_eq!(scene.drain_expired_widget_publications(), 1);
    assert!(
        !scene
            .widget_registry
            .instances
            .contains_key(&expired_widget)
    );
    assert!(
        !scene
            .widget_registry
            .active_publishes
            .contains_key(&expired_widget)
    );
    assert_eq!(scene.dock_rows().len(), 2);
    let readmitted = scene
        .publish_to_dock(PROGRESS, level(0.0), "b", None, 0, 0)
        .unwrap();
    assert_ne!(readmitted, short_id);
    assert_eq!(scene.dock_rows()[2].namespace, "b");
}

#[test]
fn dock_named_ingress_cannot_bypass_scope_and_cleanup_reclaims_exact_owners() {
    let (mut scene, _) = setup(3);
    let a = scoped_namespace("agent", "a");
    let b = scoped_namespace("agent", "b");
    let lease_a = scene.grant_lease(&a, 60_000);
    let lease_b = scene.grant_lease(&b, 60_000);
    scene
        .publish_to_widget("gauge", level(0.9), "unrelated", None, 0, None)
        .unwrap();
    let a_id = scene
        .publish_to_dock(PROGRESS, level(0.1), &a, Some(lease_a), 0, 0)
        .unwrap();
    scene
        .publish_to_dock(COUNTDOWN, HashMap::new(), &a, Some(lease_a), 0, 0)
        .unwrap();
    scene
        .publish_to_dock(PROGRESS, level(0.3), &b, Some(lease_b), 0, 0)
        .unwrap();
    scene
        .publish_to_dock(PROGRESS, level(0.4), "unrelated", None, 0, 0)
        .unwrap();
    let name_a = record(&scene, &a, PROGRESS).widget_name.clone();
    let name_a_countdown = record(&scene, &a, COUNTDOWN).widget_name.clone();
    let name_b = record(&scene, &b, PROGRESS).widget_name.clone();
    let version = scene.version;
    assert!(matches!(
        scene.validate_widget_params(&name_a, &level(1.0)),
        Err(ValidationError::WidgetNotFound { .. })
    ));
    for caller in [a.as_str(), b.as_str(), "unrelated"] {
        assert!(matches!(
            scene.publish_to_widget_for_origin(
                &name_a,
                level(1.0),
                caller,
                None,
                0,
                None,
                None,
                Some(SceneId::new())
            ),
            Err(ValidationError::WidgetNotFound { .. })
        ));
        assert!(!scene.hold_widget_publications(&name_a, caller, Some(1)));
        assert!(matches!(
            scene.clear_widget_for_publisher(&name_a, caller),
            Err(ValidationError::WidgetNotFound { .. })
        ));
    }
    assert!(!scene.set_widget_auto_clear(&name_a, None));
    assert!(matches!(
        scene.publish_to_dock(PROGRESS, level(1.0), &a, Some(lease_b), 0, 0),
        Err(DockPublishError::Widget(
            ValidationError::ZonePublishLeaseNotFound { .. }
        ))
    ));
    assert_eq!(scene.version, version);
    assert_eq!(record(&scene, &a, PROGRESS).params, level(0.1));
    scene
        .set_widget_param_local(&name_a, "hover", WidgetParameterValue::F32(1.0))
        .unwrap();
    assert_eq!(record(&scene, &a, PROGRESS).params, level(0.1));
    assert_eq!(
        scene.dock_instance(&a, PROGRESS).unwrap().current_params["hover"],
        WidgetParameterValue::F32(1.0)
    );

    scene.clear_dock_widget(&a, COUNTDOWN);
    assert!(
        !scene
            .widget_registry
            .instances
            .contains_key(&name_a_countdown)
    );
    assert_eq!(scene.dock_rows()[0].widgets.len(), 1);

    // A new exact-owner lease reuses the instance but receives fresh defaults.
    let later_lease = scene.grant_lease(&a, 60_000);
    assert_eq!(
        scene.publish_to_dock(PROGRESS, HashMap::new(), &a, Some(later_lease), 0, 0),
        Ok(a_id)
    );
    assert_eq!(record(&scene, &a, PROGRESS).lease_id, Some(later_lease));
    assert_eq!(
        scene.dock_instance(&a, PROGRESS).unwrap().current_params["level"],
        WidgetParameterValue::F32(0.0)
    );
    assert_eq!(
        scene.dock_instance(&a, PROGRESS).unwrap().current_params["hover"],
        WidgetParameterValue::F32(1.0)
    );
    scene.revoke_lease(lease_a).unwrap();
    assert_eq!(scene.dock_instance(&a, PROGRESS).unwrap().id, a_id);
    assert_eq!(scene.release_scope(&a).cleared.widgets, 1);
    assert!(!scene.widget_registry.instances.contains_key(&name_a));
    assert!(scene.dock_instance(&b, PROGRESS).is_some());
    assert!(
        scene
            .publish_to_dock(PROGRESS, level(1.0), &a, Some(later_lease), 0, 0)
            .is_err()
    );
    assert_eq!(scene.reclaim_agent("agent").cleared.widgets, 1);
    assert!(!scene.widget_registry.instances.contains_key(&name_b));
    assert_eq!(scene.dock_rows().len(), 1);
    assert_eq!(scene.dock_rows()[0].namespace, "unrelated");
    assert_eq!(record(&scene, "unrelated", PROGRESS).params, level(0.4));
    assert_eq!(
        scene.widget_registry.active_for_widget("gauge")[0].params,
        level(0.9)
    );
}
