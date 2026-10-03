use super::*;

/// Idle render gate (hud-ilivg): the compositor frame loop skips the
/// build/encode/present pass when the scene graph is unchanged since the last
/// presented frame AND nothing is animating, but renders on any scene-version
/// bump OR while an animation is in flight.
///
/// This pins the exact `dirty` predicate used by the windowed frame loop
/// (`scene.version != last_rendered_scene_version || has_inflight_animation`)
/// against all three acceptance cases:
///   1. idle (no change, no animation)            -> skip
///   2. scene-version bump                        -> render
///   3. in-flight animation, version pinned       -> render (no freeze)
#[tokio::test]
async fn idle_render_gate_skips_static_scene_renders_on_change_or_animation() {
    let (mut compositor, _surface) = require_gpu!(make_compositor_and_surface(720, 480).await);
    // Windowed profile: scroll smoothing active (headless snaps and never
    // registers a smoother, so enable it explicitly for this gate test).
    compositor.scroll_smoothing_enabled = true;

    let mut scene = SceneGraph::new(720.0, 480.0);
    let tab_id = scene.create_tab("gate", 0).unwrap();
    let lease_id = scene.grant_lease("gate", 120_000);
    let tile_id = scene
        .create_tile(
            tab_id,
            "gate",
            lease_id,
            Rect::new(0.0, 0.0, 400.0, 300.0),
            1,
        )
        .unwrap();
    scene
        .register_tile_scroll_config(
            tile_id,
            tze_hud_scene::types::TileScrollConfig {
                scrollable_x: false,
                scrollable_y: true,
                content_width: None,
                content_height: Some(2000.0),
            },
        )
        .unwrap();

    // Observe the tile once: its smoother starts settled on the current offset
    // (no initial jump), so nothing is animating.
    compositor.update_scroll_smoothing(&scene);

    // -- Case 1: idle -- scene unchanged, nothing animating -> SKIP. --
    assert!(
        !compositor.has_inflight_animation(&scene),
        "a freshly-settled scene must report no in-flight animation"
    );
    let last_rendered = scene.version;
    let dirty_idle = scene.version != last_rendered || compositor.has_inflight_animation(&scene);
    assert!(
        !dirty_idle,
        "idle frame (no scene change, no animation) MUST skip render/present"
    );

    // -- Case 2: scene-version bump -> RENDER. --
    // A scene diff / mutation bumps scene.version; the gate must not skip it.
    scene.version += 1;
    let dirty_versioned =
        scene.version != last_rendered || compositor.has_inflight_animation(&scene);
    assert!(
        dirty_versioned,
        "a scene-version bump MUST force a render even with no animation"
    );

    // -- Case 3: in-flight animation, version pinned -> RENDER (no freeze). --
    // Move the authoritative scroll target far away and advance the smoother one
    // frame: it is now mid-flight (displayed offset still near 0, target 1500).
    scene
        .set_tile_scroll_offset_local(tile_id, 0.0, 1500.0)
        .unwrap();
    compositor.update_scroll_smoothing(&scene);
    assert!(
        compositor.has_inflight_animation(&scene),
        "a mid-flight smooth-scroll catch-up must report an in-flight animation"
    );
    // Pin the gate's last-rendered version to the CURRENT version so the only
    // possible source of dirtiness is the animation itself.
    let pinned = scene.version;
    let dirty_animating = scene.version != pinned || compositor.has_inflight_animation(&scene);
    assert!(
        dirty_animating,
        "an in-flight animation MUST force a render even when scene.version is unchanged"
    );
}

fn caret_test_draft() -> LocalComposerState {
    LocalComposerState {
        text: "hi".to_owned(),
        cursor_byte: 2,
        selection_anchor: 2,
        at_capacity: false,
        node_id: tze_hud_scene::types::SceneId::new(),
        placeholder: None,
    }
}

/// An idle focused composer schedules its next wake at the caret toggle
/// boundary, not at the next frame.
#[tokio::test]
async fn caret_blink_wakes_at_toggle_not_every_frame() {
    let (mut compositor, _surface) = require_gpu!(make_compositor_and_surface(320, 200).await);
    assert_eq!(
        compositor.next_animation_deadline(),
        None,
        "no composer, no deadline"
    );
    *compositor.local_composer_state.lock().unwrap() = Some(Some(caret_test_draft()));
    assert!(compositor.drain_local_composer_and_needs_render());

    let start = compositor.composer_caret_blink_start;
    assert_eq!(
        compositor.next_animation_deadline(),
        Some(start + CARET_BLINK_HALF_PERIOD),
        "first wake is the solid -> hidden toggle"
    );
    // Many frames inside the solid phase: none dirty, deadline unmoved.
    for _ in 0..100 {
        assert!(!compositor.drain_local_composer_and_needs_render());
    }
    assert_eq!(
        compositor.next_animation_deadline(),
        Some(start + CARET_BLINK_HALF_PERIOD)
    );
    // Past the toggle the following boundary is one half-period later.
    compositor.composer_caret_blink_start = std::time::Instant::now()
        .checked_sub(CARET_BLINK_HALF_PERIOD)
        .expect("test clock must have enough uptime to rewind one half-period");
    assert_eq!(
        compositor.next_animation_deadline(),
        Some(compositor.composer_caret_blink_start + CARET_BLINK_HALF_PERIOD * 2)
    );
}

fn countdown_notification_scene() -> SceneGraph {
    countdown_notification_scene_on(SceneGraph::new(1280.0, 720.0))
}

/// The countdown scene on a caller-supplied graph (e.g. one with a `TestClock`).
fn countdown_notification_scene_on(mut scene: SceneGraph) -> SceneGraph {
    scene.register_zone(ZoneDefinition {
        id: SceneId::new(),
        name: "notification-area".to_owned(),
        description: "countdown".to_owned(),
        geometry_policy: GeometryPolicy::Relative {
            x_pct: 0.0,
            y_pct: 0.0,
            width_pct: 1.0,
            height_pct: 1.0,
        },
        accepted_media_types: vec![ZoneMediaType::ShortTextWithIcon],
        rendering_policy: RenderingPolicy::default(),
        contention_policy: ContentionPolicy::Stack { max_depth: 5 },
        max_publishers: 8,
        auto_clear_ms: Some(8_000),
        ephemeral: false,
        layer_attachment: LayerAttachment::Chrome,
    });
    scene
        .publish_to_zone(
            "notification-area",
            ZoneContent::Notification(NotificationPayload {
                text: "hello".to_owned(),
                icon: String::new(),
                urgency: 1,
                ttl_ms: None,
                title: String::new(),
                actions: Vec::new(),
            }),
            "agent-a",
            None,
            None,
            None,
        )
        .unwrap();
    scene
}

/// A visible notification counting down to its fade is not in flight: the
/// gate stays idle until the fade starts, and the loop wakes exactly then.
#[tokio::test]
async fn notification_countdown_is_not_inflight_animation() {
    let (mut compositor, _surface) = require_gpu!(make_compositor_and_surface(320, 200).await);
    let scene = countdown_notification_scene();
    compositor.update_publication_animations(&scene);

    assert!(
        !compositor.has_inflight_animation(&scene),
        "a notification still counting down must not hold the loop at 60 fps"
    );
    let state = compositor.pub_animation_states["notification-area"]
        .values()
        .next()
        .unwrap();
    assert_eq!(
        compositor.next_animation_deadline(),
        Some(state.first_seen + std::time::Duration::from_millis(state.ttl_ms.unwrap())),
        "the wake is the fade start"
    );

    // Past the TTL the next tick starts the fade, which is in flight.
    for zone in compositor.pub_animation_states.values_mut() {
        for s in zone.values_mut() {
            s.first_seen = std::time::Instant::now() - std::time::Duration::from_secs(9);
        }
    }
    compositor.update_publication_animations(&scene);
    assert!(compositor.has_inflight_animation(&scene));
    assert_eq!(
        compositor.next_animation_deadline(),
        None,
        "once fading, frames (not a deadline) drive the animation"
    );
}

/// Count of frames the idle gate would present across a notification
/// lifetime, driven by the deadline the runtime would sleep on: one render
/// when the notification appears, then only the fade (no 60 fps countdown).
#[tokio::test]
async fn idle_with_notification_renders_only_at_fade() {
    let (mut compositor, _surface) = require_gpu!(make_compositor_and_surface(320, 200).await);
    let scene = countdown_notification_scene();
    compositor.update_publication_animations(&scene);
    let (origin, ttl) = {
        let s = compositor.pub_animation_states["notification-area"]
            .values()
            .next()
            .unwrap();
        (
            s.first_seen,
            std::time::Duration::from_millis(s.ttl_ms.unwrap()),
        )
    };

    // Virtual clock: step to each wake the loop would take. Rewinding
    // `first_seen` by the virtual elapsed time stands in for waiting.
    let mut renders_before_fade = 0;
    let mut wakes = 0;
    while let Some(deadline) = compositor.next_animation_deadline() {
        wakes += 1;
        let virtual_now = deadline - origin;
        assert_eq!(virtual_now, ttl, "the only wake before the fade is the TTL");
        for zone in compositor.pub_animation_states.values_mut() {
            for s in zone.values_mut() {
                s.first_seen = origin - virtual_now;
            }
        }
        compositor.update_publication_animations(&scene);
        if compositor.has_inflight_animation(&scene) {
            break;
        }
        renders_before_fade += 1;
        assert!(wakes < 3, "must not keep waking without a fade");
    }
    assert_eq!(wakes, 1, "one wake for the whole countdown");
    assert_eq!(
        renders_before_fade, 0,
        "no frames rendered during countdown"
    );
    assert!(compositor.has_inflight_animation(&scene), "fade renders");
}

/// Idle render gate — composer carve-out (hud-ilivg / hud-r3ax6).
///
/// The local draft echo and the caret blink are driven off out-of-band state
/// that never bumps `scene.version`. Before the idle gate the compositor thread
/// ran `render_frame` unconditionally, so both worked; with the gate they would
/// freeze unless `drain_local_composer_and_needs_render` (called before the
/// gate) (a) applies a pending keystroke and (b) keeps a focused composer dirty.
///
/// This pins the gate's composer input across the full lifecycle:
///   1. no composer                       → needs_render = false  (idle skips)
///   2. pending echo (slot Some(Some))    → needs_render = true   (renders)
///   3. focused, no new keystroke         → needs_render only at caret toggles
///   4. deactivation (slot Some(None))    → needs_render = true   (clears overlay)
///   5. gone                              → needs_render = false  (idle skips)
#[tokio::test]
async fn idle_render_gate_renders_for_composer_echo_and_caret_blink() {
    use tze_hud_scene::types::SceneId;

    let (mut compositor, _surface) = require_gpu!(make_compositor_and_surface(320, 200).await);

    // ── 1. No composer focused, nothing pending → gate must skip. ──
    assert!(
        !compositor.drain_local_composer_and_needs_render(),
        "with no composer focused and no pending echo the gate MUST be able to skip"
    );

    // ── 2. A keystroke writes the slot → pending echo MUST render. ──
    let node_id = SceneId::new();
    {
        let mut guard = compositor.local_composer_state.lock().unwrap();
        *guard = Some(Some(LocalComposerState {
            text: "hi".to_owned(),
            cursor_byte: 2,
            selection_anchor: 2,
            at_capacity: false,
            node_id,
            placeholder: None,
        }));
    }
    assert!(
        compositor.drain_local_composer_and_needs_render(),
        "a pending local-composer echo MUST mark the frame dirty so it renders promptly"
    );
    assert!(
        compositor.local_composer.is_some(),
        "draining before the gate must have applied the pending draft"
    );

    // ── 3. Focused, no keystroke: the gate wakes only at caret toggles. ──
    assert!(
        compositor.local_composer_state.lock().unwrap().is_none(),
        "slot must have been drained to None by the previous call"
    );
    assert!(
        !compositor.drain_local_composer_and_needs_render(),
        "between blink toggles an idle focused composer MUST NOT render"
    );
    // Rewind one half-period: the phase flips solid -> hidden exactly once.
    compositor.composer_caret_blink_start = std::time::Instant::now()
        .checked_sub(CARET_BLINK_HALF_PERIOD)
        .expect("test clock must have enough uptime to rewind one half-period");
    assert!(
        compositor.drain_local_composer_and_needs_render(),
        "a caret phase toggle MUST render one frame"
    );
    assert!(
        !compositor.drain_local_composer_and_needs_render(),
        "the toggled phase is rendered once, then idle again"
    );

    // ── 4. Deactivation: slot delivers Some(None) → render once to clear. ──
    {
        let mut guard = compositor.local_composer_state.lock().unwrap();
        *guard = Some(None);
    }
    assert!(
        compositor.drain_local_composer_and_needs_render(),
        "the deactivation transition MUST render one frame to clear the composer overlay"
    );
    assert!(
        compositor.local_composer.is_none(),
        "deactivation must have cleared the drained composer state"
    );

    // ── 5. Composer gone → gate must skip again (no 60Hz idle burn). ──
    assert!(
        !compositor.drain_local_composer_and_needs_render(),
        "once the composer is gone the gate MUST be able to skip the static idle frame"
    );

    // ── 6. Real out-of-band focus/grip handles dirty transition frames. ──
    // These are the same shared slots written by the windowed runtime. They do
    // not bump scene.version, so the idle gate itself must observe each change.
    let tab_id = SceneId::new();
    let tile_id = SceneId::new();
    let focused = FocusRingOwner {
        tab_id,
        tile_id,
        node_id: None,
    };
    *compositor.focus_ring_owner_state.lock().unwrap() = Some(focused);
    assert!(compositor.drain_local_composer_and_needs_render());
    assert!(
        !compositor.drain_local_composer_and_needs_render(),
        "an unchanged focus-ring owner must not keep the idle gate dirty"
    );
    *compositor.focus_ring_owner_state.lock().unwrap() = None;
    assert!(compositor.drain_local_composer_and_needs_render());
    assert!(!compositor.drain_local_composer_and_needs_render());

    *compositor.resize_grip_hover_state.lock().unwrap() = Some(tile_id);
    assert!(compositor.drain_local_composer_and_needs_render());
    assert!(
        !compositor.drain_local_composer_and_needs_render(),
        "an unchanged resize-grip target must not keep the idle gate dirty"
    );
    *compositor.resize_grip_hover_state.lock().unwrap() = None;
    assert!(compositor.drain_local_composer_and_needs_render());
    assert!(!compositor.drain_local_composer_and_needs_render());

    // The viewer close-button hover target dirties the gate once per change and
    // not while it stays put (hud-jm8nq.11: no per-frame redraw while idle).
    *compositor.tile_close_hover_state.lock().unwrap() = Some(tile_id);
    assert!(compositor.drain_local_composer_and_needs_render());
    assert!(
        !compositor.drain_local_composer_and_needs_render(),
        "an unchanged close-hover target must not keep the idle gate dirty"
    );
    *compositor.tile_close_hover_state.lock().unwrap() = None;
    assert!(compositor.drain_local_composer_and_needs_render());
    assert!(!compositor.drain_local_composer_and_needs_render());
}

#[test]
fn static_focus_and_resize_grip_transitions_each_dirty_exactly_once() {
    let tab_id = SceneId::new();
    let tile_id = SceneId::new();
    let focused = Some(FocusRingOwner {
        tab_id,
        tile_id,
        node_id: None,
    });

    assert!(focus_or_grip_changed(None, focused, None, None));
    assert!(!focus_or_grip_changed(focused, focused, None, None));
    assert!(focus_or_grip_changed(focused, None, None, None));
    assert!(!focus_or_grip_changed(None, None, None, None));

    assert!(focus_or_grip_changed(None, None, None, Some(tile_id)));
    assert!(!focus_or_grip_changed(
        None,
        None,
        Some(tile_id),
        Some(tile_id),
    ));
}

/// `hud_hold` on a visible notification moves exactly its one fade deadline
/// (counted from the hold, not the original publish), and `ttl_ms:0` removes
/// it: no deadline, no in-flight animation, and the scene sweep keeps it past
/// the zone's `auto_clear_ms`. `hud_clear` still removes it. All on the
/// injected scene clock; the only wake is the fade start.
#[tokio::test]
async fn hold_moves_the_fade_deadline_and_ttl_zero_never_fades() {
    use std::sync::Arc;
    use std::time::Duration;
    use tze_hud_scene::clock::TestClock;

    let (mut compositor, _surface) = require_gpu!(make_compositor_and_surface(320, 200).await);
    let clock = Arc::new(TestClock::new(1_000));
    let mut scene =
        countdown_notification_scene_on(SceneGraph::new_with_clock(1280.0, 720.0, clock.clone()));
    let delay = |c: &Compositor| {
        let states: Vec<_> = c.pub_animation_states["notification-area"]
            .values()
            .collect();
        assert_eq!(states.len(), 1);
        states[0].ttl_ms
    };
    compositor.update_publication_animations(&scene);
    assert_eq!(
        delay(&compositor),
        Some(7_850),
        "urgency default, 8 s - fade"
    );

    // 5 s in, hold 20 s: the fade is due 19.85 s from now, one deadline.
    clock.advance(5_000);
    let first_deadline = compositor.next_animation_deadline().unwrap();
    assert!(scene.hold_zone_publications("notification-area", "agent-a", Some(20_000_000)));
    compositor.update_publication_animations(&scene);
    assert_eq!(delay(&compositor), Some(19_850));
    let moved = compositor.next_animation_deadline().unwrap();
    assert!(
        moved > first_deadline + Duration::from_secs(10),
        "deadline moved out"
    );
    assert!(
        !compositor.has_inflight_animation(&scene),
        "still just counting down"
    );

    // ttl_ms:0 holds: no deadline, no animation, past auto_clear_ms and the sweep.
    assert!(scene.hold_zone_publications("notification-area", "agent-a", None));
    compositor.update_publication_animations(&scene);
    assert_eq!(delay(&compositor), None);
    assert_eq!(
        compositor.next_animation_deadline(),
        None,
        "held schedules no wake"
    );
    clock.advance(60_000);
    assert_eq!(
        scene.drain_expired_zone_publications(),
        0,
        "scene sweep keeps it"
    );
    compositor.update_publication_animations(&scene);
    assert!(!compositor.has_inflight_animation(&scene));
    assert_eq!(
        compositor.pub_opacity(
            "notification-area",
            &scene.zone_registry.active_publishes["notification-area"][0]
        ),
        1.0
    );

    // hud_clear removes it.
    scene
        .clear_zone_for_publisher("notification-area", "agent-a")
        .unwrap();
    compositor.update_publication_animations(&scene);
    assert!(
        compositor
            .pub_animation_states
            .values()
            .all(|zone| zone.is_empty())
    );
    assert_eq!(compositor.next_animation_deadline(), None);
}
