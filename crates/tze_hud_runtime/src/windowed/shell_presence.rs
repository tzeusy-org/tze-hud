//! Windows desktop presence. The existing display-watcher window owns native
//! resources; the network-runtime panel task owns every command's effects.

use serde::Serialize;
use tze_hud_config::tokens::DesignTokenMap;
use tze_hud_scene::graph::SurfaceClasses;

#[cfg(target_os = "windows")]
pub(super) mod native;

#[cfg(target_os = "windows")]
pub(crate) fn tray_status() -> serde_json::Value {
    native::status()
}

#[cfg(target_os = "windows")]
pub(crate) fn session_locked() -> bool {
    native::session_locked()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum GlyphState {
    Normal,
    Waiting,
    Hidden,
    Paused,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Inputs {
    pub paused: bool,
    pub hidden: SurfaceClasses,
    pub visible_questions: usize,
    /// Delivery (d) will supply lock-held questions through this same seam.
    pub held_questions: usize,
    /// The panel header's connected-or-showing set, already sorted by id.
    pub agents: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct Presentation {
    pub glyph: GlyphState,
    pub tooltip: String,
    pub all_hidden: bool,
}

impl From<&Inputs> for Presentation {
    fn from(input: &Inputs) -> Self {
        let waiting = input.visible_questions.saturating_add(input.held_questions);
        let any_hidden = input.hidden.portals || input.hidden.zones || input.hidden.widgets;
        let all_hidden = input.hidden.portals && input.hidden.zones && input.hidden.widgets;
        let glyph = if input.paused {
            GlyphState::Paused
        } else if any_hidden {
            GlyphState::Hidden
        } else if waiting > 0 {
            GlyphState::Waiting
        } else {
            GlyphState::Normal
        };
        let state = if input.paused {
            "Paused".to_owned()
        } else if all_hidden {
            "Everything hidden".to_owned()
        } else {
            match waiting {
                0 => "No questions".to_owned(),
                1 => "1 question waiting".to_owned(),
                n => format!("{n} questions waiting"),
            }
        };
        let agents = if input.agents.is_empty() {
            "No agents".to_owned()
        } else {
            // A namespace cannot introduce a third tooltip line or a NUL.
            input.agents.join(", ").replace(['\r', '\n', '\0'], " ")
        };
        let remaining = 127 - state.encode_utf16().count() - 1;
        Self {
            glyph,
            tooltip: format!("{state}\n{}", ellipsize(&agents, remaining)),
            all_hidden,
        }
    }
}

/// Preserve complete Unicode scalars at the shell's UTF-16 limit, reserving
/// space for the ellipsis itself. szTip's final NUL is the 128th unit.
fn ellipsize(text: &str, units: usize) -> String {
    if text.encode_utf16().count() <= units {
        return text.to_owned();
    }
    let mut remaining = units.saturating_sub(1);
    let mut result = String::new();
    for c in text.chars() {
        if c.len_utf16() > remaining {
            break;
        }
        result.push(c);
        remaining -= c.len_utf16();
    }
    if units > 0 {
        result.push('…');
    }
    result
}

#[cfg(test)]
use crate::shell::control_panel::TrayCommand;

/// Track lock and connection independently: a connect event alone must not
/// unlock a locked session, nor may unlock expose a disconnected session.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct SessionState {
    pub locked: bool,
    pub disconnected: bool,
}

impl SessionState {
    pub(crate) fn held(self) -> bool {
        self.locked || self.disconnected
    }

    pub(crate) fn event(&mut self, code: u32) -> bool {
        let before = self.held();
        match code {
            1 | 3 => self.disconnected = false, // console / remote connect
            2 | 4 => self.disconnected = true,  // console / remote disconnect
            7 => self.locked = true,
            8 => self.locked = false,
            _ => {}
        }
        before != self.held()
    }
}

/// Only these resolved canonical tokens affect the tray. HUD scale, opacity,
/// font or unrelated token changes do not trigger another rasterization.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct GlyphColors {
    on_light: String,
    on_dark: String,
    caution: String,
    outline: String,
    primary: String,
}

impl GlyphColors {
    pub(crate) fn from_tokens(tokens: &DesignTokenMap) -> Self {
        let color = |key: &str| tokens.get(key).expect("resolved canonical token").clone();
        Self {
            on_light: color("tray.glyph.on_light"),
            on_dark: color("tray.glyph.on_dark"),
            caution: color("color.caution"),
            outline: color("color.outline"),
            primary: color("color.primary"),
        }
    }

    pub(crate) fn svg(&self, state: GlyphState, light_taskbar: bool) -> String {
        let base = if light_taskbar {
            &self.on_light
        } else {
            &self.on_dark
        };
        let (glyph, mark) = match state {
            GlyphState::Normal => (base, String::new()),
            GlyphState::Hidden => (&self.outline, String::new()),
            GlyphState::Waiting => (
                base,
                format!(
                    "<circle cx=\"19\" cy=\"18\" r=\"4\" fill=\"{}\"/>",
                    self.primary
                ),
            ),
            GlyphState::Paused => (
                base,
                format!(
                    "<path d=\"M14 13h3v9h-3zm5 0h3v9h-3z\" fill=\"{}\"/>",
                    self.caution
                ),
            ),
        };
        include_str!("../../../../assets/tray.svg")
            .replace("{{glyph}}", glyph)
            .replace("{{mark}}", &mark)
    }
}

fn rasterize(svg: &str, size: u32) -> Result<resvg::tiny_skia::Pixmap, String> {
    let tree = resvg::usvg::Tree::from_str(svg, &resvg::usvg::Options::default())
        .map_err(|e| e.to_string())?;
    let mut pixmap = resvg::tiny_skia::Pixmap::new(size, size).ok_or("tray pixmap allocation")?;
    resvg::render(
        &tree,
        resvg::tiny_skia::Transform::from_scale(
            size as f32 / tree.size().width(),
            size as f32 / tree.size().height(),
        ),
        &mut pixmap.as_mut(),
    );
    Ok(pixmap)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tray_presentation_and_session_transitions() {
        let mut input = Inputs::default();
        assert_eq!(Presentation::from(&input).glyph, GlyphState::Normal);
        assert_eq!(
            Presentation::from(&input).tooltip,
            "No questions\nNo agents"
        );
        input.visible_questions = 1;
        input.held_questions = 1;
        input.agents = vec!["connected".into(), "showing".into()];
        let view = Presentation::from(&input);
        assert_eq!(view.glyph, GlyphState::Waiting);
        assert_eq!(view.tooltip, "2 questions waiting\nconnected, showing");
        input.hidden = SurfaceClasses::ALL;
        assert_eq!(Presentation::from(&input).glyph, GlyphState::Hidden);
        assert!(Presentation::from(&input).all_hidden);
        input.paused = true;
        input.agents = vec!["🦀".repeat(100)];
        let view = Presentation::from(&input);
        assert_eq!(view.glyph, GlyphState::Paused);
        assert!(view.tooltip.starts_with("Paused\n"));
        assert!(view.tooltip.ends_with('…'));
        let units: Vec<_> = view.tooltip.encode_utf16().collect();
        assert!(units.len() <= 127);
        assert!(String::from_utf16(&units).is_ok());

        let mut session = SessionState::default();
        for (code, held) in [
            (7, true),
            (2, true),
            (8, true),
            (1, false),
            (4, true),
            (3, false),
        ] {
            session.event(code);
            assert_eq!(session.held(), held, "WTS code {code}");
        }
        assert!(!session.event(99));
        assert_eq!(
            TrayCommand::from_menu_id(1, &view),
            Some(TrayCommand::OpenPanel)
        );
        assert_eq!(
            TrayCommand::from_menu_id(2, &view),
            Some(TrayCommand::Resume)
        );
        assert_eq!(
            TrayCommand::from_menu_id(3, &view),
            Some(TrayCommand::ShowAll)
        );
        assert_eq!(TrayCommand::from_menu_id(4, &view), Some(TrayCommand::Quit));
        assert_eq!(TrayCommand::from_menu_id(0, &view), None);
        assert_ne!(TrayCommand::TogglePanel, TrayCommand::OpenPanel);
    }

    #[test]
    fn tray_svg_uses_taskbar_contrast_and_semantic_state_tokens() {
        let tokens =
            tze_hud_config::tokens::resolve_tokens(&DesignTokenMap::new(), &DesignTokenMap::new());
        let colors = GlyphColors::from_tokens(&tokens);
        let light = colors.svg(GlyphState::Normal, true);
        let dark = colors.svg(GlyphState::Normal, false);
        assert!(light.contains(&tokens["tray.glyph.on_light"]));
        assert!(dark.contains(&tokens["tray.glyph.on_dark"]));
        let light_pixels = rasterize(&light, 32).unwrap();
        let dark_pixels = rasterize(&dark, 32).unwrap();
        assert_ne!(light_pixels.data(), dark_pixels.data());
        assert!(light_pixels.pixels().iter().any(|pixel| pixel.alpha() > 0));
        for (state, key) in [
            (GlyphState::Paused, "color.caution"),
            (GlyphState::Hidden, "color.outline"),
            (GlyphState::Waiting, "color.primary"),
        ] {
            let svg = colors.svg(state, false);
            assert!(svg.contains(&tokens[key]));
            assert!(!svg.contains("{{"));
            assert_ne!(rasterize(&svg, 32).unwrap().data(), dark_pixels.data());
        }
    }
}
