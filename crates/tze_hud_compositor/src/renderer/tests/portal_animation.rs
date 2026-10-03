use super::*;

// ── Portal tile animation unit tests (CPU-only, no GPU required) ─────────
//
// These tests exercise the portal transition token wiring introduced in
// hud-58rg1 (spec §6.3).  They are CPU-only: no wgpu adapter is requested
// so they run cleanly even in minimal CI containers.

/// A scrollable tile that appears for the first time triggers a fade-in
/// animation with the token-configured `transition_in_ms` duration.
///
/// This exercises `update_portal_tile_animations` end-to-end.
///
/// GPU required (for `Compositor::new_headless`); skips gracefully when no
/// adapter is available.
#[tokio::test]
async fn portal_tile_fade_in_starts_on_first_content() {
    let (mut compositor, _surface) = require_gpu!(make_compositor_and_surface(256, 256).await);

    // Set a custom transition_in_ms token (200 ms) to distinguish from default.
    compositor
        .token_map
        .insert("portal.transition.in_ms".to_string(), "200".to_string());

    // Build a scene with one scrollable (portal) tile that has a root node.
    let mut scene = SceneGraph::new(256.0, 256.0);
    let tab_id = scene.create_tab("portal-test", 0).unwrap();
    let lease_id = scene.grant_lease("portal-test", 60_000);
    let tile_id = scene
        .create_tile(
            tab_id,
            "portal-test",
            lease_id,
            Rect::new(0.0, 0.0, 256.0, 256.0),
            1,
        )
        .unwrap();

    // Register a scroll config (identifies this tile as a portal tile).
    scene
        .register_tile_scroll_config(tile_id, TileScrollConfig::vertical())
        .unwrap();

    // Attach content — root node present.
    let node = Node {
        layout: Default::default(),
        id: SceneId::new(),
        children: vec![],
        data: NodeData::SolidColor(SolidColorNode {
            color: Rgba::WHITE,
            radius: None,
            bounds: Rect::new(0.0, 0.0, 256.0, 256.0),
        }),
    };
    scene.set_tile_root(tile_id, node).unwrap();

    // First call — tile goes from no-content to content.
    compositor.update_portal_tile_animations(&scene);

    // A fade-in animation must have been inserted for tile_id.
    let anim = compositor.portal_tile_anim_states.get(&tile_id);
    assert!(
        anim.is_some(),
        "update_portal_tile_animations must insert fade-in state for scrollable tile"
    );
    let anim = anim.unwrap();
    assert_eq!(
        anim.target_opacity, 1.0,
        "fade-in must have target_opacity 1.0"
    );
    assert_eq!(
        anim.duration_ms, 200,
        "fade-in duration must match token value 200 ms"
    );

    // Opacity must be near 0 immediately after the animation starts.
    let opacity = compositor.portal_tile_anim_opacity(tile_id);
    assert!(
        opacity < 0.2,
        "immediately after fade-in start, opacity must be near 0, got {opacity}"
    );
}

/// Removing a scrollable tile's root node triggers a fade-out animation
/// with the token-configured `transition_out_ms` duration.
///
/// GPU required; skips gracefully when no adapter is available.
///
/// The test seeds `prev_portal_tile_has_content` directly (same-crate access)
/// to simulate the tile having had content in the previous frame, then calls
/// `update_portal_tile_animations` with no root node — matching the content-
/// gone transition without needing a SceneGraph API to clear the root.
#[tokio::test]
async fn portal_tile_fade_out_starts_on_content_removal() {
    let (mut compositor, _surface) = require_gpu!(make_compositor_and_surface(256, 256).await);

    compositor
        .token_map
        .insert("portal.transition.out_ms".to_string(), "150".to_string());

    // Scene has a scrollable tile with NO root node.
    let mut scene = SceneGraph::new(256.0, 256.0);
    let tab_id = scene.create_tab("portal-fade-out", 0).unwrap();
    let lease_id = scene.grant_lease("portal-fade-out", 60_000);
    let tile_id = scene
        .create_tile(
            tab_id,
            "portal-fade-out",
            lease_id,
            Rect::new(0.0, 0.0, 256.0, 256.0),
            1,
        )
        .unwrap();
    scene
        .register_tile_scroll_config(tile_id, TileScrollConfig::vertical())
        .unwrap();
    // root_node is None — content just disappeared.

    // Seed previous state: tile had content last frame.
    compositor
        .prev_portal_tile_has_content
        .insert(tile_id, true);

    compositor.update_portal_tile_animations(&scene);

    // Now the animation state must be a fade-out.
    let anim = compositor.portal_tile_anim_states.get(&tile_id);
    assert!(
        anim.is_some(),
        "update_portal_tile_animations must insert fade-out state after content removal"
    );
    let anim = anim.unwrap();
    assert_eq!(
        anim.target_opacity, 0.0,
        "fade-out must have target_opacity 0.0"
    );
    assert_eq!(
        anim.duration_ms, 150,
        "fade-out duration must match token value 150 ms"
    );
}

/// Interrupting a portal-tile fade-out with a restore must seed the new
/// fade-in from the **eased (on-screen) opacity**, not the linear
/// `current_opacity()` (hud-uir0w).
///
/// Portal tiles are *displayed* through the EaseInOut curve
/// (`portal_tile_anim_opacity`). If the interrupt path seeded the fade-in from
/// the linear value, the next fade-in would start at a different opacity than
/// the frame just rendered — a visible jump. This test drives a fade-out to a
/// progress point where the eased opacity and the linear opacity differ
/// meaningfully (t = 0.25: eased ≈ 0.844 vs linear = 0.750), interrupts it with
/// a restore, and asserts the new fade-in's start opacity matches the displayed
/// eased value, not the linear one. The continuity guarantee follows directly:
/// the new fade-in (also displayed eased) begins at the exact opacity on screen
/// at interruption.
///
/// GPU required (for `Compositor::new_headless`); skips gracefully when no
/// adapter is available.
#[tokio::test]
async fn portal_tile_interrupt_seeds_fade_in_from_eased_opacity() {
    let (mut compositor, _surface) = require_gpu!(make_compositor_and_surface(256, 256).await);

    compositor
        .token_map
        .insert("portal.transition.in_ms".to_string(), "200".to_string());

    // Scrollable tile WITH content present this frame (content was restored).
    let mut scene = SceneGraph::new(256.0, 256.0);
    let tab_id = scene.create_tab("portal-interrupt", 0).unwrap();
    let lease_id = scene.grant_lease("portal-interrupt", 60_000);
    let tile_id = scene
        .create_tile(
            tab_id,
            "portal-interrupt",
            lease_id,
            Rect::new(0.0, 0.0, 256.0, 256.0),
            1,
        )
        .unwrap();
    scene
        .register_tile_scroll_config(tile_id, TileScrollConfig::vertical())
        .unwrap();
    let node = Node {
        layout: Default::default(),
        id: SceneId::new(),
        children: vec![],
        data: NodeData::SolidColor(SolidColorNode {
            color: Rgba::WHITE,
            radius: None,
            bounds: Rect::new(0.0, 0.0, 256.0, 256.0),
        }),
    };
    scene.set_tile_root(tile_id, node).unwrap();

    // Previous frame: tile had NO content, so this frame is an appear (restore).
    compositor
        .prev_portal_tile_has_content
        .insert(tile_id, false);

    // Seed an in-flight fade-out, back-dated to 25% progress so eased and linear
    // opacity are clearly distinguishable. A deliberately LONG duration (15 s of
    // 60 s) keeps the eased curve nearly flat per millisecond: `displayed_eased`
    // is sampled here, but the code under test re-samples
    // `portal_tile_anim_opacity()` against the same live `Instant` a moment
    // later. At 25% of 60 s the eased fade-out moves < 2e-5/ms, so even tens of
    // milliseconds of scheduler delay on a loaded runner stay far under the 0.02
    // assertion tolerance (a short 200 ms duration here would flake — hud-uir0w).
    let mut fade_out = ZoneAnimationState::fade_out(60_000);
    fade_out.transition_start =
        std::time::Instant::now() - std::time::Duration::from_millis(15_000);
    compositor.portal_tile_anim_states.insert(tile_id, fade_out);

    // Opacity actually on screen at interruption (eased) vs the linear value.
    let displayed_eased = compositor.portal_tile_anim_opacity(tile_id);
    let linear = compositor.portal_tile_anim_states[&tile_id].current_opacity();

    // Sanity: the two must differ enough for the assertion below to be meaningful.
    assert!(
        (displayed_eased - linear).abs() > 0.05,
        "test setup: eased ({displayed_eased}) and linear ({linear}) opacity must differ"
    );

    // Interrupt: content restored mid fade-out.
    compositor.update_portal_tile_animations(&scene);

    let state = compositor
        .portal_tile_anim_states
        .get(&tile_id)
        .expect("portal animation state must exist after interrupt fade-in");
    assert_eq!(
        state.target_opacity, 1.0,
        "interrupt must produce a fade-in (target_opacity 1.0)"
    );

    // The new fade-in must start from the eased (displayed) opacity, NOT linear.
    assert!(
        (state.from_opacity - displayed_eased).abs() < 0.02,
        "fade-in must seed from eased/displayed opacity ~{displayed_eased}, got {}",
        state.from_opacity
    );
    assert!(
        (state.from_opacity - linear).abs() > 0.05,
        "fade-in must NOT seed from linear opacity {linear}, got {}",
        state.from_opacity
    );

    // Continuity: the first displayed frame of the new fade-in (eased at t≈0)
    // equals the opacity that was on screen at interruption — no jump.
    let post_interrupt_displayed = compositor.portal_tile_anim_opacity(tile_id);
    assert!(
        (post_interrupt_displayed - displayed_eased).abs() < 0.02,
        "displayed opacity must be continuous across interrupt: was {displayed_eased}, now {post_interrupt_displayed}"
    );
}

/// Non-scrollable tiles must NOT get portal animation states.
///
/// Ensures `update_portal_tile_animations` only affects tiles with a
/// registered `TileScrollConfig` (i.e. portal tiles).
///
/// GPU required; skips gracefully when no adapter is available.
#[tokio::test]
async fn non_scrollable_tile_has_no_portal_animation_state() {
    let (mut compositor, _surface) = require_gpu!(make_compositor_and_surface(256, 256).await);

    let mut scene = SceneGraph::new(256.0, 256.0);
    let tab_id = scene.create_tab("non-scroll-test", 0).unwrap();
    let lease_id = scene.grant_lease("non-scroll-test", 60_000);
    let tile_id = scene
        .create_tile(
            tab_id,
            "non-scroll-test",
            lease_id,
            Rect::new(0.0, 0.0, 256.0, 256.0),
            1,
        )
        .unwrap();

    // NO register_tile_scroll_config — this is NOT a portal tile.
    let node = Node {
        layout: Default::default(),
        id: SceneId::new(),
        children: vec![],
        data: NodeData::SolidColor(SolidColorNode {
            color: Rgba::WHITE,
            radius: None,
            bounds: Rect::new(0.0, 0.0, 256.0, 256.0),
        }),
    };
    scene.set_tile_root(tile_id, node).unwrap();

    compositor.update_portal_tile_animations(&scene);

    // No animation state must have been created for this non-portal tile.
    assert!(
        !compositor.portal_tile_anim_states.contains_key(&tile_id),
        "non-scrollable tile must not receive a portal animation state"
    );

    // portal_tile_anim_opacity must return 1.0 (fully visible, no dimming).
    let opacity = compositor.portal_tile_anim_opacity(tile_id);
    assert!(
        (opacity - 1.0).abs() < f32::EPSILON,
        "non-scrollable tile opacity must be 1.0, got {opacity}"
    );
}
