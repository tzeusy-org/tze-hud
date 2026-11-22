//! Action roles: the primary action is the first that is not destructive; a
//! destructive action draws in the error tone and, while held, a hold fill
//! sweeping in from the left that drains back on an early release.

use super::*;
use tze_hud_scene::clock::TestClock;

const ZONE: &str = "notification-area";

fn action(label: &str, destructive: bool) -> NotificationAction {
    NotificationAction {
        label: label.into(),
        callback_id: Some(label.into()),
        url: None,
        role: destructive.then_some(tze_hud_scene::types::ActionRole::Destructive),
    }
}

fn payload(urgency: u32, actions: &[(&str, bool)]) -> NotificationPayload {
    NotificationPayload {
        title: "Delete the branch?".into(),
        text: "It has unmerged work".into(),
        urgency,
        actions: actions.iter().map(|(l, d)| action(l, *d)).collect(),
        ..Default::default()
    }
}

/// A shipped notification-area card with `actions`, on a test clock.
fn role_scene(clock: &Arc<TestClock>, urgency: u32, actions: &[(&str, bool)]) -> SceneGraph {
    let mut scene = SceneGraph::new_with_clock(1280.0, 720.0, clock.clone());
    scene.create_tab("Main", 0).unwrap();
    let zone = ZoneRegistry::with_defaults().zones.remove(ZONE).unwrap();
    scene.register_zone(zone);
    scene
        .publish_to_zone(
            ZONE,
            ZoneContent::Notification(payload(urgency, actions)),
            "agent",
            None,
            None,
            None,
        )
        .unwrap();
    scene
}

fn action_region(scene: &SceneGraph, index: usize) -> ZoneHitRegion {
    scene
        .overlay
        .zone_hit_regions
        .iter()
        .find(|r| matches!(r.kind, ZoneInteractionKind::Action { index: i } if i == index))
        .unwrap()
        .clone()
}

fn style(compositor: &Compositor, scene: &SceneGraph) -> NotificationStyle {
    NotificationStyle::resolve(
        compositor.zone_tokens(ZONE),
        &scene.zone_registry.zones[ZONE].rendering_policy,
    )
}

fn theme_tokens(theme: &str) -> HashMap<String, String> {
    tze_hud_config::themes::resolve_config_tokens(&HashMap::from([(
        "theme".to_string(),
        theme.to_string(),
    )]))
}

/// Destructive labels hold WCAG AA on every card tone of every shipped theme:
/// `color.error` on the neutral tones, the tone's own text role on caution and
/// error (the outline carries the cue there).
#[test]
fn destructive_label_holds_contrast_on_every_tone_and_theme() {
    use crate::renderer::control_panel::{MIN_TEXT_CONTRAST, contrast};
    use crate::renderer::notification_style::MIN_SWEEP_CONTRAST;
    for theme in tze_hud_config::themes::builtin_theme_names() {
        let tokens = theme_tokens(theme);
        let error = super::token_colors::parse_hex_color(&tokens["color.error"]).unwrap();
        for zone in [ZONE, "alert-banner"] {
            let style = NotificationStyle::for_zone(zone, &tokens, &RenderingPolicy::default());
            for urgency in 0..=3 {
                let card = payload(urgency, &[("Keep", false), ("Delete", true)]);
                let tone = style.tone(urgency);
                let label = style.action_label_color(&card, 1, false);
                let fill = Rgba {
                    a: 1.0,
                    ..tone.fill
                };
                let ratio = contrast(label, fill);
                assert!(
                    ratio >= MIN_TEXT_CONTRAST,
                    "{theme} {zone} urgency {urgency}: destructive label {ratio:.2}:1"
                );
                if urgency >= 2 {
                    assert_eq!(
                        label, tone.title,
                        "{theme} {zone}: caution/error keep the tone"
                    );
                } else if zone == ZONE {
                    assert_eq!(label, error, "{theme}: neutral cards draw color.error");
                }
                assert_eq!(style.destructive_outline(), error);
                // The sweep stays visible on its card (error cards included)
                // and the held label stays readable on the sweep.
                let sweep = style.hold_fill(urgency);
                let visible = contrast(sweep, fill);
                assert!(
                    visible >= MIN_SWEEP_CONTRAST,
                    "{theme} {zone} urgency {urgency}: sweep {visible:.2}:1 against the card"
                );
                let held = style.action_label_color(&card, 1, true);
                let readable = contrast(held, sweep);
                assert!(
                    readable >= MIN_TEXT_CONTRAST,
                    "{theme} {zone} urgency {urgency}: held label {readable:.2}:1"
                );
            }
        }
    }
}

/// The filled button is the first non-destructive action; a card of only
/// destructive actions has none. A destructive action is never filled at
/// rest: a `color.error` hairline and an error-tone label.
#[tokio::test]
async fn primary_is_first_safe_action_and_destructive_is_outlined() {
    let (mut compositor, _surface) = require_gpu!(make_compositor_and_surface(1280, 720).await);
    compositor.init_text_renderer(wgpu::TextureFormat::Rgba8UnormSrgb);
    let clock = Arc::new(TestClock::new(1_000));
    for (actions, primary) in [
        (&[("Delete", true), ("Keep", false)][..], Some(1)),
        (&[("Delete", true), ("Purge", true)][..], None),
        (&[("Yes", false), ("No", false)][..], Some(0)),
    ] {
        let mut scene = role_scene(&clock, 1, actions);
        compositor.populate_zone_hit_regions(&mut scene, 1280.0, 720.0);
        let style = style(&compositor, &scene);
        let commands = compositor
            .collect_all_rounded_rect_cmds(&scene, 1280.0, 720.0)
            .chrome;
        let items = compositor.collect_text_items(&scene, 1280.0, 720.0);
        let primary_fill = primary.map(|index| {
            compositor.gpu_color(style.action_fill(&payload(1, actions), index).unwrap())
        });
        for (index, (label, destructive)) in actions.iter().enumerate() {
            let b = action_region(&scene, index).bounds;
            let drawn: Vec<_> = commands
                .iter()
                .filter(|c| (c.x, c.y, c.width, c.height) == (b.x, b.y, b.width, b.height))
                .collect();
            let filled = drawn.iter().any(|c| Some(c.color) == primary_fill);
            assert_eq!(filled, primary == Some(index), "{actions:?} action {index}");
            if *destructive {
                assert_eq!(drawn.len(), 1, "a resting destructive action: outline only");
                assert_eq!(drawn[0].color, [0.0; 4]);
                let border = drawn[0].border.unwrap();
                assert_eq!(border.width, style.hairline);
                assert_eq!(
                    border.color,
                    compositor.gpu_color(style.destructive_outline())
                );
            }
            let item = items.iter().find(|i| &*i.text == *label).unwrap();
            let expected = style.action_label_color(&payload(1, actions), index, false);
            assert_eq!(item.color, crate::text::rgba_to_srgb_u8(expected));
        }
    }
}

/// Pixel at (`x`, `y`) of a 1280-wide readback.
fn px(pixels: &[u8], x: f32, y: f32) -> [u8; 4] {
    let i = (y as usize * 1280 + x as usize) * 4;
    [pixels[i], pixels[i + 1], pixels[i + 2], pixels[i + 3]]
}

fn close(a: [u8; 4], b: [u8; 4]) -> bool {
    a.iter().zip(b).all(|(x, y)| x.abs_diff(y) <= 3)
}

/// Halfway through a hold the hold fill covers the left half of the button
/// and not the right; under reduced motion the full fill shows at once.
#[tokio::test]
async fn hold_sweeps_left_to_right_and_reduced_motion_fills_at_once() {
    let (mut compositor, surface) = require_gpu!(make_compositor_and_surface(1280, 720).await);
    compositor.init_text_renderer(wgpu::TextureFormat::Rgba8UnormSrgb);
    let clock = Arc::new(TestClock::new(1_000));
    let mut scene = role_scene(&clock, 1, &[("Keep", false), ("Delete", true)]);
    let frame = |compositor: &mut Compositor, scene: &mut SceneGraph| {
        compositor.prime_markdown_cache(scene);
        compositor.prime_truncation_cache(scene);
        compositor.render_frame_headless(scene, &surface);
        surface.read_pixels(&compositor.device)
    };
    frame(&mut compositor, &mut scene);
    let region = action_region(&scene, 1);
    let b = region.bounds;
    // Above the label line, inside the hairline and clear of the corners.
    let y = b.y + 4.0;
    let (left, right) = (b.x + b.width * 0.2, b.x + b.width * 0.8);
    let rest = frame(&mut compositor, &mut scene);

    // Full fill: a hold that has run its whole length.
    scene.begin_notification_hold(region.clone(), None);
    clock.advance(600);
    let full = frame(&mut compositor, &mut scene);
    assert!(!close(px(&full, left, y), px(&rest, left, y)), "fill shows");
    assert!(close(px(&full, left, y), px(&full, right, y)));
    assert!(
        !compositor.has_inflight_animation(&scene),
        "a full sweep idles"
    );

    // Halfway: left half filled, right half at rest.
    scene.begin_notification_hold(region.clone(), None);
    clock.advance(300);
    let half = frame(&mut compositor, &mut scene);
    assert!(close(px(&half, left, y), px(&full, left, y)));
    assert!(!close(px(&half, right, y), px(&full, right, y)));
    let sweep = compositor
        .collect_all_rounded_rect_cmds(&scene, 1280.0, 720.0)
        .chrome
        .into_iter()
        .find_map(|c| c.clip.filter(|_| c.x == b.x && c.y == b.y))
        .unwrap();
    assert!((sweep.width - b.width * 0.5).abs() < 0.5);
    assert!(
        compositor.has_inflight_animation(&scene),
        "the sweep animates"
    );

    // Reduced motion: the same instant shows the full fill, with no frames.
    let mut tokens = compositor.token_map.clone();
    tokens.insert(
        tze_hud_config::tokens::OS_REDUCE_MOTION_KEY.into(),
        "true".into(),
    );
    compositor.set_token_map(tokens);
    let reduced = frame(&mut compositor, &mut scene);
    assert!(close(px(&reduced, left, y), px(&full, left, y)));
    assert!(close(px(&reduced, right, y), px(&full, right, y)));
    assert!(!compositor.has_inflight_animation(&scene));
    // Release: the fill vanishes, no drain.
    scene.cancel_notification_hold(600, 100);
    let released = frame(&mut compositor, &mut scene);
    assert!(close(px(&released, left, y), px(&rest, left, y)));
    assert!(!compositor.has_inflight_animation(&scene));
}

/// An early release drains the fill from where it reached over
/// `motion.state.ms`, then draws and schedules nothing.
#[tokio::test]
async fn early_release_drains_over_state_motion() {
    let (compositor, _surface) = require_gpu!(make_compositor_and_surface(1280, 720).await);
    let clock = Arc::new(TestClock::new(1_000));
    let mut scene = role_scene(&clock, 3, &[("Delete", true)]);
    compositor.populate_zone_hit_regions(&mut scene, 1280.0, 720.0);
    let region = action_region(&scene, 0);
    let b = region.bounds;
    let sweep = |compositor: &Compositor, scene: &SceneGraph| {
        compositor
            .collect_all_rounded_rect_cmds(scene, 1280.0, 720.0)
            .chrome
            .into_iter()
            .find_map(|c| c.clip.filter(|_| c.x == b.x && c.y == b.y))
            .map(|clip| clip.width / b.width)
    };
    scene.begin_notification_hold(region, None);
    clock.advance(300);
    scene.cancel_notification_hold(600, 100);
    assert!((sweep(&compositor, &scene).unwrap() - 0.5).abs() < 0.01);
    clock.advance(50);
    assert!((sweep(&compositor, &scene).unwrap() - 0.25).abs() < 0.01);
    assert!(compositor.has_inflight_animation(&scene));
    clock.advance(50);
    assert_eq!(sweep(&compositor, &scene), None);
    assert!(!compositor.has_inflight_animation(&scene));
}
