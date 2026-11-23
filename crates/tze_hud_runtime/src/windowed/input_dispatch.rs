use std::time::Instant;

use winit::event::MouseScrollDelta;

use crate::os_prefs::WheelLines;
use tze_hud_input::{WHEEL_LINE_PX, WheelStep};

/// A wheel event as `(delta_x px, delta_y, unit of delta_y)`, positive
/// scrolling down/right (hud-ljhku.13).
///
/// A notch (`LineDelta`) scrolls the Windows "lines per notch" setting
/// (`setting`, read per event) in text lines, or one page for
/// `WHEEL_PAGESCROLL`; the scroll target sizes the line or page. Off
/// Windows a notch stays [`WHEEL_LINE_PX`]. Horizontal notches and pixel
/// deltas (touchpads) are unchanged.
pub(super) fn normalize_mouse_wheel_delta(
    delta: &MouseScrollDelta,
    setting: WheelLines,
) -> (f32, f32, WheelStep) {
    match *delta {
        MouseScrollDelta::LineDelta(x, y) => {
            let dx = -x * WHEEL_LINE_PX;
            match setting {
                WheelLines::Fixed => (dx, -y * WHEEL_LINE_PX, WheelStep::Pixels),
                WheelLines::Lines(n) => (dx, -y * n as f32, WheelStep::Lines),
                WheelLines::Page => (dx, -y, WheelStep::Pages),
            }
        }
        MouseScrollDelta::PixelDelta(pos) => (-(pos.x as f32), -(pos.y as f32), WheelStep::Pixels),
    }
}

/// Monotonic nanoseconds since process start.
///
/// Uses process-relative time so values are comparable within a session.
pub(super) fn nanoseconds_since_start() -> u64 {
    // Use std::time::Instant for monotonic timing.
    // We store the process start time lazily and subtract.
    use std::sync::OnceLock;
    static START: OnceLock<Instant> = OnceLock::new();
    let start = START.get_or_init(Instant::now);
    start.elapsed().as_nanos() as u64
}

/// Map a winit `PhysicalKey` to the DOM `KeyboardEvent.code`-style string
/// used by `RawKeyDownEvent.key_code`.
///
/// Returns the `KeyCode` variant name (e.g. `"KeyA"`, `"ShiftLeft"`,
/// `"ArrowDown"`) for identified keys, and `"Unidentified"` for unknown ones.
pub(super) fn physical_key_to_key_code_str(key: &winit::keyboard::PhysicalKey) -> String {
    use winit::keyboard::PhysicalKey;
    match key {
        PhysicalKey::Code(code) => format!("{code:?}"),
        PhysicalKey::Unidentified(_) => "Unidentified".to_string(),
    }
}

/// Map a winit logical `Key` to the DOM `KeyboardEvent.key`-style string
/// used by `RawKeyDownEvent.key` and `RawKeyUpEvent.key`.
///
/// For character keys this is the character itself (e.g. `"a"`, `"A"`, `"1"`).
/// For named keys this is the `NamedKey` variant name (e.g. `"Enter"`,
/// `"Backspace"`, `"ArrowDown"`). Unknown keys map to `"Unidentified"`.
pub(super) fn logical_key_to_str(key: &winit::keyboard::Key) -> String {
    use winit::keyboard::Key;
    match key {
        Key::Character(s) => s.to_string(),
        Key::Named(named) => format!("{named:?}"),
        Key::Unidentified(_) => "Unidentified".to_string(),
        Key::Dead(Some(c)) => format!("Dead({c})"),
        Key::Dead(None) => "Dead".to_string(),
    }
}

/// The text a key press types, or `None` for non-text keys and chords.
///
/// Shared by the composer/agent character path and the control-panel search.
/// A key with a layout character types that character. Text with no layout
/// key, such as a Windows `VK_PACKET` from the touch keyboard or `SendInput`
/// Unicode events, reaches winit as `Key::Unidentified` with the character
/// only in `KeyEvent::text`, so that text is used instead. Control characters
/// (Enter, Tab, Backspace) are never text here, so a `"\r"` is not routed to
/// an agent as a character; their named keys handle them.
///
/// Characters outside the BMP sent as two `VK_PACKET` surrogates are not
/// covered: winit 0.30 finalizes each half as its own key event and drops the
/// unpaired surrogate before it reaches this function.
pub(super) fn key_press_text(
    key: &winit::keyboard::Key,
    text: Option<&str>,
    mods: winit::keyboard::ModifiersState,
) -> Option<String> {
    use winit::keyboard::Key;
    if mods.control_key() || mods.alt_key() || mods.super_key() {
        return None;
    }
    match key {
        Key::Character(ch) => Some(ch.to_string()),
        Key::Unidentified(_) => text
            .filter(|t| !t.is_empty() && !t.chars().any(char::is_control))
            .map(str::to_string),
        Key::Named(_) | Key::Dead(_) => None,
    }
}

/// Convert winit's `ModifiersState` to `KeyboardModifiers`.
///
/// CapsLock and NumLock toggle states are not exposed by winit's
/// `ModifiersState`; they default to `false` here. Full toggle-key tracking
/// can be added via `WindowEvent::KeyboardInput` state tracking if needed.
pub(super) fn winit_mods_to_keyboard_modifiers(
    mods: winit::keyboard::ModifiersState,
) -> tze_hud_input::KeyboardModifiers {
    tze_hud_input::KeyboardModifiers {
        shift: mods.shift_key(),
        ctrl: mods.control_key(),
        alt: mods.alt_key(),
        meta: mods.super_key(),
        caps_lock: false, // winit ModifiersState does not expose CapsLock state
        num_lock: false,  // winit ModifiersState does not expose NumLock state
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tze_hud_scene::SceneGraph;

    #[test]
    fn key_press_text_types_layout_and_packet_text_but_not_chords() {
        use winit::keyboard::{Key, ModifiersState as M, NamedKey, NativeKey};
        let none = M::empty();
        let packet = Key::Unidentified(NativeKey::Windows(0xE7));
        // Windows VK_PACKET: no layout character, the event text carries it.
        assert_eq!(
            key_press_text(&packet, Some("é"), none).as_deref(),
            Some("é")
        );
        assert_eq!(
            key_press_text(&packet, Some(" "), none).as_deref(),
            Some(" ")
        );
        assert_eq!(key_press_text(&packet, Some("\r"), none), None);
        assert_eq!(key_press_text(&packet, None, none), None);
        assert_eq!(key_press_text(&packet, Some("é"), M::CONTROL), None);
        // Layout characters type with Shift, not with Ctrl/Alt/Super chords.
        let h = Key::Character("h".into());
        assert_eq!(key_press_text(&h, Some("h"), none).as_deref(), Some("h"));
        assert_eq!(
            key_press_text(&h, Some("H"), M::SHIFT).as_deref(),
            Some("h")
        );
        for chord in [M::CONTROL, M::ALT, M::SUPER] {
            assert_eq!(key_press_text(&h, None, chord), None);
        }
        // Named keys never type text; their key-down handling owns them.
        assert_eq!(
            key_press_text(&Key::Named(NamedKey::Enter), Some("\r"), none),
            None
        );
        assert_eq!(
            key_press_text(&Key::Named(NamedKey::Space), Some(" "), none),
            None
        );
    }

    #[test]
    fn nanoseconds_since_start_is_monotonic() {
        let t1 = nanoseconds_since_start();
        std::thread::sleep(std::time::Duration::from_millis(1));
        let t2 = nanoseconds_since_start();
        assert!(t2 > t1, "timestamps must be monotonically increasing");
    }

    // Non-Windows (`WheelLines::Fixed`) keeps today's 40 px per notch.
    #[test]
    fn mouse_wheel_delta_positive_line_scrolls_toward_top() {
        let (x, y, step) =
            normalize_mouse_wheel_delta(&MouseScrollDelta::LineDelta(0.0, 1.0), WheelLines::Fixed);

        assert_eq!(x, 0.0);
        assert_eq!(y, -40.0);
        assert_eq!(step, WheelStep::Pixels);
    }

    #[test]
    fn mouse_wheel_delta_negative_line_scrolls_down_transcript() {
        let (_, y, _) =
            normalize_mouse_wheel_delta(&MouseScrollDelta::LineDelta(0.0, -1.0), WheelLines::Fixed);

        assert_eq!(y, 40.0);
    }

    // hud-ljhku.13 R13.3 S1: a notch is the Windows lines-per-notch setting,
    // in text lines the scroll target sizes; WHEEL_PAGESCROLL is one page.
    #[test]
    fn mouse_wheel_notch_uses_windows_lines_per_notch() {
        let notch_down = MouseScrollDelta::LineDelta(0.0, -1.0);
        assert_eq!(
            normalize_mouse_wheel_delta(&notch_down, WheelLines::Lines(5)),
            (0.0, 5.0, WheelStep::Lines)
        );
        // The setting is read per event: the next notch uses the new value.
        assert_eq!(
            normalize_mouse_wheel_delta(&notch_down, WheelLines::Lines(2)),
            (0.0, 2.0, WheelStep::Lines)
        );
        assert_eq!(
            normalize_mouse_wheel_delta(&MouseScrollDelta::LineDelta(0.0, 2.0), WheelLines::Page),
            (0.0, -2.0, WheelStep::Pages)
        );
        // Horizontal notches and touchpad pixels are unchanged.
        assert_eq!(
            normalize_mouse_wheel_delta(
                &MouseScrollDelta::LineDelta(1.0, 0.0),
                WheelLines::Lines(5)
            ),
            (-40.0, -0.0, WheelStep::Lines)
        );
        let pixels = MouseScrollDelta::PixelDelta(winit::dpi::PhysicalPosition::new(0.0, -12.0));
        assert_eq!(
            normalize_mouse_wheel_delta(&pixels, WheelLines::Lines(5)),
            (-0.0, 12.0, WheelStep::Pixels)
        );
    }

    /// hud-ljhku.13 R13.3 S1: five lines per notch scroll five OUTPUT text
    /// rows on a portal (its shaped line height), five 40 px lines on a
    /// surface without text, and WHEEL_PAGESCROLL one visible page.
    #[test]
    fn wheel_lines_follow_the_target_text_line_height() {
        use std::sync::Arc;
        use tze_hud_portal::hub::{PortalHub, PortalKey, Publish};
        use tze_hud_scene::{NodeData, PortalDocumentLayout, PortalPartKind, Rect};

        let clock = tze_hud_scene::TestClock::from_us(200);
        let mut driver = crate::portal::PortalDriver::new(tze_hud_mcp::PortalHandle::new(
            PortalHub::default(),
            Arc::new(clock.clone()),
        ));
        driver
            .hub_mut()
            .publish(
                &PortalKey::new("agent", "wheel"),
                Publish {
                    text: Some("hello".into()),
                    ..Default::default()
                },
                100,
            )
            .expect("publish accepted");
        let mut scene = SceneGraph::new_with_clock(1920.0, 1080.0, Arc::new(clock.clone()));
        let tab = scene.create_tab("Main", 0).unwrap();
        let mut processor = tze_hud_input::InputProcessor::new();
        driver.drain(&mut scene, &mut processor, Some(tab));
        let tile_id = *scene.tiles.keys().next().expect("portal tile");
        let (node_id, source) = scene
            .nodes
            .iter()
            .find_map(
                |(id, node)| match (scene.portal_document_kind(tile_id, *id), &node.data) {
                    (Some(PortalPartKind::Transcript), NodeData::TextMarkdown(t)) => {
                        Some((*id, t.clone()))
                    }
                    _ => None,
                },
            )
            .expect("transcript document");
        let pane = scene
            .portal_surface(tile_id)
            .unwrap()
            .parts
            .iter()
            .find(|p| p.kind == PortalPartKind::Transcript)
            .unwrap()
            .bounds;
        let content: String = (0..100).map(|i| format!("l{i}\n")).collect();
        let rows: Vec<(usize, f32)> = (0..100).map(|i| (i * 4, i as f32 * 22.0)).collect();
        let tile = scene.tiles[&tile_id].bounds;
        scene.overlay.portal_document_layouts.insert(
            (tile_id, PortalPartKind::Transcript),
            PortalDocumentLayout {
                node_id,
                signature: [3; 32],
                source,
                tile_size: (tile.width, tile.height),
                font_scale: scene.tile_font_scale(tile_id),
                content: content.into(),
                height: 2200.0,
                viewport: Rect::new(pane.x, pane.y, pane.width, 110.0),
                line_height: 22.0,
                rows: rows.into(),
                reading: Default::default(),
            },
        );
        processor.reconcile_portal_document_layouts(&mut scene);
        let (x, y) = (tile.x + pane.x + pane.width / 2.0, tile.y + pane.y + 5.0);
        assert_eq!(
            scene.portal_document_at(tile_id, x, y),
            Some(PortalPartKind::Transcript)
        );
        // Scrolled back to the top first so downward notches have room.
        let top = scene.tile_scroll_offset_local(tile_id).1;
        processor.process_scroll_event(
            &tze_hud_input::ScrollEvent {
                x,
                y,
                delta_x: 0.0,
                delta_y: -top,
            },
            &mut scene,
        );
        let offset = |scene: &SceneGraph| scene.tile_scroll_offset_local(tile_id).1;
        let start = offset(&scene);
        let (_, dy, step) = normalize_mouse_wheel_delta(
            &MouseScrollDelta::LineDelta(0.0, -1.0),
            WheelLines::Lines(5),
        );
        let notch =
            |scene: &mut SceneGraph, processor: &mut tze_hud_input::InputProcessor, dy, step| {
                processor.process_wheel_event(
                    &tze_hud_input::ScrollEvent {
                        x,
                        y,
                        delta_x: 0.0,
                        delta_y: dy,
                    },
                    step,
                    scene,
                );
            };
        notch(&mut scene, &mut processor, dy, step);
        assert_eq!(offset(&scene) - start, 5.0 * 22.0, "five OUTPUT rows");
        let before = offset(&scene);
        let (_, dy, step) =
            normalize_mouse_wheel_delta(&MouseScrollDelta::LineDelta(0.0, -1.0), WheelLines::Page);
        notch(&mut scene, &mut processor, dy, step);
        assert_eq!(offset(&scene) - before, 110.0, "one visible page");

        // A surface without text: five 40 px lines.
        let lease = scene.grant_lease("plain", 60_000);
        let plain = scene
            .create_tile(
                tab,
                "plain",
                lease,
                Rect::new(1500.0, 600.0, 300.0, 300.0),
                50,
            )
            .unwrap();
        scene
            .register_tile_scroll_config(plain, tze_hud_scene::TileScrollConfig::vertical())
            .unwrap();
        let (_, dy, step) = normalize_mouse_wheel_delta(
            &MouseScrollDelta::LineDelta(0.0, -1.0),
            WheelLines::Lines(5),
        );
        processor.process_wheel_event(
            &tze_hud_input::ScrollEvent {
                x: 1600.0,
                y: 700.0,
                delta_x: 0.0,
                delta_y: dy,
            },
            step,
            &mut scene,
        );
        assert_eq!(scene.tile_scroll_offset_local(plain).1, 5.0 * 40.0);
    }
}
