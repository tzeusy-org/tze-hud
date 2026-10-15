//! HUD control panel chrome (hud-vniri.2).
//!
//! The panel is runtime chrome like the system card: the runtime pushes a
//! platform-neutral [`ControlPanelModel`] through
//! [`Compositor::set_control_panel`]. It never enters the scene graph, so no
//! agent surface can observe it (docs/invariants.md §3), and `/admin/screenshot`
//! leaves it out. The final system-card pass draws it on the primary display,
//! in place of the card while it is open.
//!
//! Every size, color and font comes from the theme's role tokens (`color.*`,
//! `shape.*`, `space.*`, `type.*`, `state.*`, `focus.*`) plus the
//! `control_panel.*` tokens, so a new theme restyles the panel with no code
//! change. Missing tokens take their canonical defaults from `tze_hud_config`.
//! Resolved sizes are clamped to a legible range, and text colors that an
//! override made unreadable on the panel surfaces they are drawn on (contrast
//! below 4.5:1) fall back to the canonical colors. Colors drawn as fills or
//! accents (primary, success, outlines) are not contrast-checked.
//!
//! Layout is a pure function of (style, model, surface size, text measurer).
//! Besides draw commands it yields the [`ControlPanelGeometry`] the runtime
//! uses for pointer hits, wheel scrolling and scroll-into-view, so what is
//! clicked is always what was drawn.

use std::collections::HashMap;

use tze_hud_scene::types::{FontFamily, RenderingPolicy, TextAlign, TextOverflow};

use super::safe_mode_overlay::OverlayRect;
use super::token_colors::{parse_hex_color, resolve_token_color};
use super::*;
use crate::pipeline::{RoundedRectBorder, RoundedRectClip};

// ─── Model (pushed by the runtime) ───────────────────────────────────────────

/// Everything the panel shows. Built by the runtime's control-panel model;
/// the compositor only lays it out and draws it.
#[derive(Clone, Debug, PartialEq)]
pub struct ControlPanelModel {
    /// Search text typed so far (empty: the sections show).
    pub query: String,
    /// Shown in the search field while `query` is empty.
    pub placeholder: String,
    /// A short notice in the search field, e.g. that Windows refused focus.
    pub notice: Option<String>,
    /// Quick-action tiles, left to right.
    pub tiles: Vec<PanelItem>,
    /// Section jump chips, left to right.
    pub chips: Vec<PanelChip>,
    /// One-line summary beside the chips.
    pub state_line: String,
    pub body: PanelBody,
    /// Footer key hints, left to right.
    pub hints: Vec<String>,
    /// Footer hint at the right edge (how to close).
    pub hint_end: String,
    pub focus: PanelFocus,
    pub hover: Option<PanelHitTarget>,
    /// Body scroll offset in px; the layout clamps it to the content.
    pub scroll_px: f32,
    /// Panel zoom (1.0 = token sizes). Clamped to a legible range.
    pub scale: f32,
}

/// What the scrolling body shows.
#[derive(Clone, Debug, PartialEq)]
pub enum PanelBody {
    /// Section cards flowing into as many columns as fit.
    Sections(Vec<PanelSection>),
    /// One ranked list of search matches across all sections.
    Results {
        title: String,
        items: Vec<PanelItem>,
    },
    /// Nothing to list (e.g. no search match): one explanatory line.
    Empty(String),
}

#[derive(Clone, Debug, PartialEq)]
pub struct PanelSection {
    pub id: String,
    pub title: String,
    /// Jump accelerator shown in the card header (e.g. `Alt+1`).
    pub key: Option<String>,
    pub items: Vec<PanelItem>,
}

/// One setting or action, drawn as a row in a section or as a quick tile.
#[derive(Clone, Debug, PartialEq)]
pub struct PanelItem {
    /// Stable id, unique across the panel (`<section>.<row>`).
    pub id: String,
    pub label: String,
    pub detail: Option<String>,
    pub detail_tone: PanelTone,
    /// Section name, shown beside search results.
    pub context: Option<String>,
    pub control: PanelControl,
    pub badge: Option<PanelBadge>,
    /// Accelerator hint (e.g. `Alt+H`).
    pub shortcut: Option<String>,
    /// `Caution` marks a suppressing state (agents paused, content hidden);
    /// `Danger` a destructive action or a pending confirmation.
    pub emphasis: PanelTone,
}

/// The control drawn at the right of an item.
#[derive(Clone, Debug, PartialEq)]
pub enum PanelControl {
    /// Label (and detail) only.
    None,
    /// An editable value shown as text.
    Value {
        text: String,
        mono: bool,
    },
    /// A value the panel cannot change here (set elsewhere, or a fact).
    ReadOnly {
        text: String,
        mono: bool,
    },
    Toggle(bool),
    /// One of several options, cycled with Left/Right: `‹ text ›`.
    Choice(String),
    /// A numeric value with − and + buttons.
    Stepper(String),
    /// The whole item is a button (label drawn in the action color).
    Action,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PanelTone {
    #[default]
    Normal,
    Muted,
    Success,
    Caution,
    Danger,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PanelBadge {
    pub text: String,
    pub tone: PanelTone,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PanelChip {
    pub title: String,
    pub key: String,
}

/// Where keyboard focus is.
#[derive(Clone, Debug, Default, PartialEq)]
pub enum PanelFocus {
    #[default]
    Search,
    Tile(usize),
    /// A row, by item id (in sections or results).
    Row(String),
}

/// Which part of an item a point is over.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PanelPart {
    Body,
    /// The − button of a stepper, or the ‹ of a choice.
    Decrement,
    /// The + button of a stepper, or the › of a choice.
    Increment,
}

/// What a pointer position on the panel addresses.
#[derive(Clone, Debug, PartialEq)]
pub enum PanelHitTarget {
    Search,
    Chip(usize),
    Tile(usize, PanelPart),
    Row(String, PanelPart),
    /// Inside the panel but over nothing interactive.
    Panel,
}

// ─── Geometry (handed back to the runtime) ───────────────────────────────────

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct PanelRect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl PanelRect {
    pub fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.x && y >= self.y && x < self.x + self.w && y < self.y + self.h
    }

    fn intersect(&self, other: &PanelRect) -> Option<PanelRect> {
        let x0 = self.x.max(other.x);
        let y0 = self.y.max(other.y);
        let x1 = (self.x + self.w).min(other.x + other.w);
        let y1 = (self.y + self.h).min(other.y + other.h);
        (x1 > x0 && y1 > y0).then_some(PanelRect {
            x: x0,
            y: y0,
            w: x1 - x0,
            h: y1 - y0,
        })
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct PanelHit {
    pub target: PanelHitTarget,
    pub rect: PanelRect,
}

/// A row or section's vertical extent in body content coordinates (before
/// scrolling).
#[derive(Clone, Debug, PartialEq)]
pub struct PanelSpan {
    pub id: String,
    pub top: f32,
    pub bottom: f32,
}

/// Where things were drawn, for pointer input and scrolling.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ControlPanelGeometry {
    /// The panel surface. Points inside it belong to the panel; points
    /// outside it pass through to the desktop.
    pub panel: PanelRect,
    /// Interactive regions, most specific first.
    pub hits: Vec<PanelHit>,
    pub viewport_height: f32,
    pub max_scroll: f32,
    pub rows: Vec<PanelSpan>,
    pub sections: Vec<PanelSpan>,
}

impl ControlPanelGeometry {
    /// The most specific target at `(x, y)`, or `None` outside the panel.
    pub fn hit(&self, x: f32, y: f32) -> Option<PanelHitTarget> {
        if !self.panel.contains(x, y) {
            return None;
        }
        Some(
            self.hits
                .iter()
                .find(|h| h.rect.contains(x, y))
                .map_or(PanelHitTarget::Panel, |h| h.target.clone()),
        )
    }

    /// The scroll offset that keeps `span` fully visible, starting from
    /// `scroll` and moving as little as possible.
    pub fn reveal(&self, span: &PanelSpan, scroll: f32) -> f32 {
        let top = span.top;
        let bottom = span.bottom;
        let scroll = if top < scroll {
            top
        } else if bottom > scroll + self.viewport_height {
            bottom - self.viewport_height
        } else {
            scroll
        };
        scroll.clamp(0.0, self.max_scroll)
    }
}

// ─── Style (resolved from tokens) ────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq)]
struct TypeStyle {
    size: f32,
    line: f32,
    weight: u16,
}

/// Every token-derived value the layout uses, already scaled and clamped.
#[derive(Clone, Debug, PartialEq)]
struct PanelStyle {
    scrim: Rgba,
    surface: Rgba,
    card: Rgba,
    field: Rgba,
    on_surface: Rgba,
    on_surface_variant: Rgba,
    outline: Rgba,
    outline_variant: Rgba,
    primary: Rgba,
    primary_container: Rgba,
    success: Rgba,
    caution: Rgba,
    caution_container: Rgba,
    on_caution_container: Rgba,
    caution_outline: Rgba,
    error: Rgba,
    error_container: Rgba,
    on_error_container: Rgba,
    hover_alpha: f32,
    ring: f32,
    shape_panel: f32,
    shape_card: f32,
    shape_row: f32,
    space_xs: f32,
    space_s: f32,
    space_m: f32,
    space_l: f32,
    space_xl: f32,
    inset: f32,
    column_min: f32,
    search: TypeStyle,
    title: TypeStyle,
    body: TypeStyle,
    label: TypeStyle,
    label_s: TypeStyle,
    readout: TypeStyle,
}

/// Legible zoom range for the panel.
const SCALE_RANGE: (f32, f32) = (0.75, 2.0);
/// WCAG AA for body text; a pair below it falls back to the defaults.
const MIN_TEXT_CONTRAST: f32 = 4.5;

fn luminance(c: Rgba) -> f32 {
    0.2126 * c.r + 0.7152 * c.g + 0.0722 * c.b
}

fn contrast(a: Rgba, b: Rgba) -> f32 {
    let (la, lb) = (luminance(a), luminance(b));
    (la.max(lb) + 0.05) / (la.min(lb) + 0.05)
}

fn with_alpha(c: Rgba, a: f32) -> Rgba {
    Rgba::new(c.r, c.g, c.b, a.clamp(0.0, 1.0))
}

impl PanelStyle {
    fn resolve(tokens: &HashMap<String, String>, scale: f32) -> Self {
        let scale = if scale.is_finite() {
            scale.clamp(SCALE_RANGE.0, SCALE_RANGE.1)
        } else {
            1.0
        };
        // Every default is the canonical token's (`tze_hud_config` owns them),
        // so the panel and the token table cannot drift apart.
        let default = |key: &str| {
            tze_hud_config::tokens::CANONICAL_TOKENS
                .iter()
                .find(|t| t.key == key)
                .map(|t| t.default_value)
        };
        let default_color = |key: &str| {
            default(key)
                .and_then(parse_hex_color)
                .unwrap_or(Rgba::WHITE)
        };
        let color =
            |key: &str| resolve_token_color(tokens, key).unwrap_or_else(|| default_color(key));
        let parse = |v: &str| {
            v.trim()
                .trim_end_matches("px")
                .parse::<f32>()
                .ok()
                .filter(|v| v.is_finite())
        };
        let num = |key: &str, min: f32, max: f32| {
            tokens
                .get(key)
                .and_then(|v| parse(v))
                .or_else(|| default(key).and_then(parse))
                .unwrap_or(min)
                .clamp(min, max)
        };
        let px = |key: &str, max: f32| num(key, 0.0, max) * scale;
        let ty = |name: &str, min_size: f32| {
            let size = num(&format!("type.{name}.size"), min_size, 48.0);
            let line = num(&format!("type.{name}.line_height"), size, size * 2.0);
            TypeStyle {
                size: size * scale,
                line: line * scale,
                weight: num(&format!("type.{name}.weight"), 100.0, 900.0) as u16,
            }
        };

        // Every text color must stay readable on every panel surface it is
        // drawn on, whatever a theme or override says; otherwise the whole
        // neutral set falls back to the canonical pairs. Status text falls
        // back per pair.
        let neutral = |pick: &dyn Fn(&str) -> Rgba| {
            (
                [
                    pick("color.surface.container.high"),
                    pick("color.surface.container"),
                    pick("color.surface.container.low"),
                ],
                [pick("color.on_surface"), pick("color.on_surface.variant")],
            )
        };
        let readable = |texts: &[Rgba], surfaces: &[Rgba]| {
            texts.iter().all(|t| {
                surfaces
                    .iter()
                    .all(|s| contrast(*t, *s) >= MIN_TEXT_CONTRAST)
            })
        };
        let (mut surfaces, mut texts) = neutral(&color);
        if !readable(&texts, &surfaces) {
            (surfaces, texts) = neutral(&default_color);
        }
        let pair = |text: &str, fill: &str| {
            let (t, f) = (color(text), color(fill));
            if readable(&[t], &[f]) {
                (t, f)
            } else {
                (default_color(text), default_color(fill))
            }
        };
        let (on_caution_container, caution_container) =
            pair("color.on_caution.container", "color.caution.container");
        let (on_error_container, error_container) =
            pair("color.on_error.container", "color.error.container");
        let scrim_alpha = num("opacity.scrim", 0.0, 0.9);

        Self {
            scrim: with_alpha(color("color.surface"), scrim_alpha),
            // The panel itself is opaque so content behind never bleeds through.
            surface: with_alpha(surfaces[0], 1.0),
            card: with_alpha(surfaces[1], 1.0),
            field: with_alpha(surfaces[2], 1.0),
            on_surface: texts[0],
            on_surface_variant: texts[1],
            outline: color("color.outline"),
            outline_variant: color("color.outline.variant"),
            primary: color("color.primary"),
            primary_container: color("color.primary.container"),
            success: color("color.success"),
            caution: color("color.caution"),
            caution_container,
            on_caution_container,
            caution_outline: color("color.caution.outline"),
            error: color("color.error"),
            error_container,
            on_error_container,
            hover_alpha: num("state.hover.opacity", 0.04, 0.3),
            ring: px("focus.ring.width", 4.0).max(1.0),
            shape_panel: px("shape.xl", 48.0),
            shape_card: px("shape.l", 32.0),
            shape_row: px("shape.s", 16.0),
            space_xs: px("space.xs", 12.0),
            space_s: px("space.s", 16.0),
            space_m: px("space.m", 24.0),
            space_l: px("space.l", 32.0),
            space_xl: px("space.xl", 48.0),
            inset: px("control_panel.inset", 160.0),
            column_min: num("control_panel.column.min_width", 240.0, 1200.0) * scale,
            search: ty("headline.s", 14.0),
            title: ty("title.m", 12.0),
            body: ty("body.m", 12.0),
            label: ty("label.m", 11.0),
            label_s: ty("label.s", 10.0),
            readout: ty("readout", 12.0),
        }
    }

    fn row_height(&self) -> f32 {
        (self.body.line + self.label.line + self.space_m).max(48.0 * self.body.size / 14.0)
    }
}

// ─── Layout ──────────────────────────────────────────────────────────────────

/// A rounded rectangle in straight-alpha linear color, before `gpu_color`.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct PanelShape {
    pub(super) rect: PanelRect,
    pub(super) radius: f32,
    pub(super) fill: Rgba,
    pub(super) border: Option<(f32, Rgba)>,
    pub(super) clip: Option<PanelRect>,
}

/// Draw commands (back to front) and geometry for one panel frame.
pub(super) struct ControlPanelLayout {
    pub(super) flat: Vec<OverlayRect>,
    pub(super) shapes: Vec<PanelShape>,
    pub(super) texts: Vec<TextItem>,
    pub(super) geometry: ControlPanelGeometry,
}

/// Width of `text` at a type style; `mono` selects the monospace role.
pub(super) type Measure<'a> = dyn FnMut(&str, f32, u16, bool) -> f32 + 'a;

struct Painter<'m, 'a> {
    style: PanelStyle,
    measure: &'m mut Measure<'a>,
    shapes: Vec<PanelShape>,
    texts: Vec<TextItem>,
    hits: Vec<PanelHit>,
}

#[derive(Clone, Copy)]
enum Align {
    Start,
    End,
    Center,
}

impl Painter<'_, '_> {
    fn width(&mut self, text: &str, ty: TypeStyle, mono: bool) -> f32 {
        (self.measure)(text, ty.size, ty.weight, mono).ceil()
    }

    fn shape(
        &mut self,
        rect: PanelRect,
        radius: f32,
        fill: Rgba,
        border: Option<(f32, Rgba)>,
        clip: Option<PanelRect>,
    ) {
        let clip = match clip {
            Some(c) => match rect.intersect(&c) {
                Some(visible) if visible == rect => None,
                Some(visible) => Some(visible),
                None => return,
            },
            None => None,
        };
        self.shapes.push(PanelShape {
            rect,
            radius: radius.min(rect.w / 2.0).min(rect.h / 2.0),
            fill,
            border,
            clip,
        });
    }

    /// One line of text vertically centred on `cy`, in a box `w` wide starting
    /// at `x`. Text that does not fit ends in an ellipsis.
    #[allow(clippy::too_many_arguments)]
    fn text(
        &mut self,
        text: &str,
        x: f32,
        cy: f32,
        w: f32,
        ty: TypeStyle,
        color: Rgba,
        mono: bool,
        align: Align,
        clip: Option<PanelRect>,
    ) {
        if text.is_empty() || w < 1.0 {
            return;
        }
        let y = cy - ty.line / 2.0;
        let rect = PanelRect {
            x,
            y,
            w,
            h: ty.line,
        };
        let visible = match clip {
            Some(c) => match rect.intersect(&c) {
                Some(v) => v,
                None => return,
            },
            None => rect,
        };
        let policy = RenderingPolicy {
            font_size_px: Some(ty.size),
            font_weight: Some(ty.weight),
            font_family: Some(if mono {
                FontFamily::SystemMonospace
            } else {
                FontFamily::SystemSansSerif
            }),
            text_color: Some(color),
            text_align: Some(match align {
                Align::Start => TextAlign::Start,
                Align::End => TextAlign::End,
                Align::Center => TextAlign::Center,
            }),
            overflow: Some(TextOverflow::Ellipsis),
            margin_px: Some(0.0),
            ..Default::default()
        };
        let mut item = TextItem::from_zone_policy(text, x, y, w, ty.line, &policy, 1.0);
        item.line_height_multiplier = ty.line / ty.size;
        item.clip_pixel_x = visible.x;
        item.clip_pixel_y = visible.y;
        item.clip_bounds_width = visible.w;
        item.clip_bounds_height = visible.h;
        self.texts.push(item);
    }

    fn hit(&mut self, target: PanelHitTarget, rect: PanelRect, clip: Option<PanelRect>) {
        let rect = match clip {
            Some(c) => match rect.intersect(&c) {
                Some(v) => v,
                None => return,
            },
            None => rect,
        };
        self.hits.push(PanelHit { target, rect });
    }

    fn tone_color(&self, tone: PanelTone) -> Rgba {
        match tone {
            PanelTone::Normal => self.style.on_surface,
            PanelTone::Muted => self.style.on_surface_variant,
            PanelTone::Success => self.style.success,
            PanelTone::Caution => self.style.caution,
            PanelTone::Danger => self.style.error,
        }
    }

    /// A pill badge ending at `right`; returns its left edge.
    fn badge(&mut self, badge: &PanelBadge, right: f32, cy: f32, clip: Option<PanelRect>) -> f32 {
        let s = self.style.clone();
        let ty = s.label_s;
        let w = self.width(&badge.text, ty, false) + s.space_s * 2.0;
        let h = ty.line + s.space_xs;
        let rect = PanelRect {
            x: right - w,
            y: cy - h / 2.0,
            w,
            h,
        };
        let (fill, border, text) = match badge.tone {
            PanelTone::Caution => (
                s.caution_container,
                s.caution_outline,
                s.on_caution_container,
            ),
            PanelTone::Danger => (s.error_container, s.error_container, s.on_error_container),
            tone => (
                with_alpha(s.surface, 0.0),
                s.outline_variant,
                self.tone_color(tone),
            ),
        };
        self.shape(rect, h / 2.0, fill, Some((1.0, border)), clip);
        self.text(
            &badge.text,
            rect.x,
            cy,
            w,
            ty,
            text,
            false,
            Align::Center,
            clip,
        );
        rect.x
    }

    /// A round − or + button centred on `cx`.
    fn round_button(
        &mut self,
        glyph: &str,
        cx: f32,
        cy: f32,
        d: f32,
        target: PanelHitTarget,
        clip: Option<PanelRect>,
    ) {
        let s = self.style.clone();
        let rect = PanelRect {
            x: cx - d / 2.0,
            y: cy - d / 2.0,
            w: d,
            h: d,
        };
        self.shape(
            rect,
            d / 2.0,
            with_alpha(s.surface, 0.0),
            Some((1.0, s.outline)),
            clip,
        );
        self.text(
            glyph,
            rect.x,
            cy,
            d,
            s.title,
            s.on_surface,
            false,
            Align::Center,
            clip,
        );
        self.hit(target, rect, clip);
    }

    /// Draw `item`'s right-hand control ending at `right`; returns the left
    /// edge of what was drawn.
    #[allow(clippy::too_many_arguments)]
    fn control(
        &mut self,
        item: &PanelItem,
        target: &dyn Fn(PanelPart) -> PanelHitTarget,
        right: f32,
        cy: f32,
        text_color: Rgba,
        muted: Rgba,
        clip: Option<PanelRect>,
    ) -> f32 {
        let s = self.style.clone();
        match &item.control {
            PanelControl::None | PanelControl::Action => right,
            PanelControl::Value { text, mono } | PanelControl::ReadOnly { text, mono } => {
                let ty = if *mono { s.readout } else { s.body };
                let color = if matches!(item.control, PanelControl::ReadOnly { .. }) {
                    muted
                } else {
                    text_color
                };
                let w = self.width(text, ty, *mono) + 2.0;
                self.text(text, right - w, cy, w, ty, color, *mono, Align::End, clip);
                right - w
            }
            PanelControl::Toggle(on) => {
                let h = s.space_xl;
                let w = h * 5.0 / 3.0;
                let track = PanelRect {
                    x: right - w,
                    y: cy - h / 2.0,
                    w,
                    h,
                };
                let (fill, border, knob) = if *on {
                    (s.primary_container, s.primary, s.primary)
                } else {
                    (s.field, s.outline, s.on_surface_variant)
                };
                self.shape(track, h / 2.0, fill, Some((1.0, border)), clip);
                let pad = (s.space_xs - 1.0).max(1.0);
                let d = h - pad * 2.0;
                let knob_x = if *on {
                    track.x + w - pad - d
                } else {
                    track.x + pad
                };
                let knob_rect = PanelRect {
                    x: knob_x,
                    y: cy - d / 2.0,
                    w: d,
                    h: d,
                };
                self.shape(knob_rect, d / 2.0, knob, None, clip);
                track.x
            }
            PanelControl::Choice(text) => {
                let w = self.width(text, s.body, false) + 2.0;
                let arrow = s.space_xl;
                let left = right - arrow * 2.0 - w;
                self.text(
                    "‹",
                    left,
                    cy,
                    arrow,
                    s.title,
                    muted,
                    false,
                    Align::Center,
                    clip,
                );
                self.text(
                    text,
                    left + arrow,
                    cy,
                    w,
                    s.body,
                    text_color,
                    false,
                    Align::Center,
                    clip,
                );
                self.text(
                    "›",
                    right - arrow,
                    cy,
                    arrow,
                    s.title,
                    muted,
                    false,
                    Align::Center,
                    clip,
                );
                let half = |x: f32| PanelRect {
                    x,
                    y: cy - arrow / 2.0,
                    w: arrow,
                    h: arrow,
                };
                self.hit(target(PanelPart::Decrement), half(left), clip);
                self.hit(target(PanelPart::Increment), half(right - arrow), clip);
                left
            }
            PanelControl::Stepper(text) => {
                let d = s.space_xl + s.space_s;
                let w = (self.width(text, s.readout, true) + 2.0).max(d * 1.5);
                let plus_cx = right - d / 2.0;
                let minus_cx = right - d - s.space_xs - w - s.space_xs - d / 2.0;
                self.round_button("+", plus_cx, cy, d, target(PanelPart::Increment), clip);
                self.text(
                    text,
                    plus_cx - d / 2.0 - s.space_xs - w,
                    cy,
                    w,
                    s.readout,
                    text_color,
                    true,
                    Align::Center,
                    clip,
                );
                self.round_button("−", minus_cx, cy, d, target(PanelPart::Decrement), clip);
                minus_cx - d / 2.0
            }
        }
    }

    /// One row of a section card or the results list.
    #[allow(clippy::too_many_arguments)]
    fn row(
        &mut self,
        item: &PanelItem,
        rect: PanelRect,
        focused: bool,
        hovered: bool,
        context_w: f32,
        clip: Option<PanelRect>,
    ) {
        let s = self.style.clone();
        if focused {
            self.shape(
                rect,
                s.shape_row,
                with_alpha(s.on_surface, s.hover_alpha),
                Some((s.ring, s.primary)),
                clip,
            );
        } else if hovered {
            self.shape(
                rect,
                s.shape_row,
                with_alpha(s.on_surface, s.hover_alpha),
                None,
                clip,
            );
        }
        let cy = rect.y + rect.h / 2.0;
        let pad = s.space_m;
        let id = item.id.clone();
        let target = move |part| PanelHitTarget::Row(id.clone(), part);
        let mut right = self.control(
            item,
            &target,
            rect.x + rect.w - pad,
            cy,
            s.on_surface,
            s.on_surface_variant,
            clip,
        );
        if let Some(badge) = &item.badge {
            right = self.badge(badge, right - s.space_s, cy, clip);
        }
        if let Some(shortcut) = &item.shortcut {
            let w = self.width(shortcut, s.label_s, true) + 2.0;
            let x = right - s.space_s - w;
            self.text(
                shortcut,
                x,
                cy,
                w,
                s.label_s,
                s.on_surface_variant,
                true,
                Align::End,
                clip,
            );
            right = x;
        }
        if let Some(context) = &item.context
            && context_w > 0.0
        {
            let x = right - s.space_l - context_w;
            self.text(
                context,
                x,
                cy,
                context_w,
                s.label,
                s.on_surface_variant,
                false,
                Align::Start,
                clip,
            );
            right = x;
        }
        let label_x = rect.x + pad;
        let label_w = (right - s.space_m - label_x).max(1.0);
        let label_color = match (item.emphasis, &item.control) {
            (PanelTone::Danger, _) => s.error,
            (_, PanelControl::Action) => s.primary,
            _ => s.on_surface,
        };
        match &item.detail {
            Some(detail) => {
                let block = s.body.line + s.label.line;
                let top = cy - block / 2.0;
                self.text(
                    &item.label,
                    label_x,
                    top + s.body.line / 2.0,
                    label_w,
                    s.body,
                    label_color,
                    false,
                    Align::Start,
                    clip,
                );
                let detail_color = match item.detail_tone {
                    PanelTone::Normal => s.on_surface_variant,
                    tone => self.tone_color(tone),
                };
                self.text(
                    detail,
                    label_x,
                    top + s.body.line + s.label.line / 2.0,
                    label_w,
                    s.label,
                    detail_color,
                    false,
                    Align::Start,
                    clip,
                );
            }
            None => self.text(
                &item.label,
                label_x,
                cy,
                label_w,
                s.body,
                label_color,
                false,
                Align::Start,
                clip,
            ),
        }
        // Parts were pushed first, so they win over the whole-row body.
        self.hit(
            PanelHitTarget::Row(item.id.clone(), PanelPart::Body),
            rect,
            clip,
        );
    }

    /// One quick-action tile.
    fn tile(
        &mut self,
        index: usize,
        item: &PanelItem,
        rect: PanelRect,
        focused: bool,
        hovered: bool,
    ) {
        let s = self.style.clone();
        let active = item.emphasis == PanelTone::Caution;
        let (fill, border, text, muted) = if active {
            (
                s.caution_container,
                s.caution_outline,
                s.on_caution_container,
                s.on_caution_container,
            )
        } else {
            (
                s.card,
                s.outline_variant,
                s.on_surface,
                s.on_surface_variant,
            )
        };
        let border = if focused {
            (s.ring, s.primary)
        } else {
            (1.0, border)
        };
        self.shape(rect, s.shape_card, fill, Some(border), None);
        if hovered {
            self.shape(
                rect,
                s.shape_card,
                with_alpha(s.on_surface, s.hover_alpha),
                None,
                None,
            );
        }
        let cy = rect.y + rect.h / 2.0;
        let mut x = rect.x + s.space_l;
        if let PanelControl::Toggle(on) = item.control {
            // A state dot: filled while the suppressing state is on.
            let d = s.space_s + s.space_xs / 2.0;
            let dot = PanelRect {
                x,
                y: cy - d / 2.0,
                w: d,
                h: d,
            };
            if on {
                self.shape(dot, d / 2.0, s.caution, None, None);
            } else {
                self.shape(
                    dot,
                    d / 2.0,
                    with_alpha(fill, 0.0),
                    Some((1.0, s.on_surface_variant)),
                    None,
                );
            }
            x += d + s.space_m;
        }
        let target = move |part| PanelHitTarget::Tile(index, part);
        let mut right = rect.x + rect.w - s.space_l;
        if matches!(item.control, PanelControl::Stepper(_)) {
            right = self.control(item, &target, right, cy, text, muted, None);
        } else if let Some(shortcut) = &item.shortcut {
            let w = self.width(shortcut, s.label_s, true) + 2.0;
            self.text(
                shortcut,
                right - w,
                cy,
                w,
                s.label_s,
                muted,
                true,
                Align::End,
                None,
            );
            right -= w;
        }
        let w = (right - s.space_s - x).max(1.0);
        let block = s.title.line + s.label.line;
        let top = cy - block / 2.0;
        self.text(
            &item.label,
            x,
            top + s.title.line / 2.0,
            w,
            s.title,
            text,
            false,
            Align::Start,
            None,
        );
        if let Some(detail) = &item.detail {
            self.text(
                detail,
                x,
                top + s.title.line + s.label.line / 2.0,
                w,
                s.label,
                muted,
                false,
                Align::Start,
                None,
            );
        }
        self.hit(PanelHitTarget::Tile(index, PanelPart::Body), rect, None);
    }
}

/// Split `heights` (in order) into at most `columns` contiguous runs so the
/// tallest column is as short as possible; returns each item's column. Keeping
/// runs contiguous keeps reading order top-to-bottom, then left-to-right, which
/// is also the Up/Down keyboard order.
fn balance_columns(heights: &[f32], columns: usize, gap: f32) -> Vec<usize> {
    let n = heights.len();
    let k = columns.clamp(1, n.max(1));
    if n == 0 {
        return Vec::new();
    }
    // run(i, j): height of items i..j stacked with gaps.
    let run = |i: usize, j: usize| -> f32 {
        heights[i..j].iter().sum::<f32>() + gap * (j - i).saturating_sub(1) as f32
    };
    // best[c][j]: minimal tallest column placing the first j items in c columns.
    let mut best = vec![vec![f32::INFINITY; n + 1]; k + 1];
    let mut cut = vec![vec![0usize; n + 1]; k + 1];
    best[0][0] = 0.0;
    for c in 1..=k {
        for j in 1..=n {
            for i in (c - 1)..j {
                let cost = best[c - 1][i].max(run(i, j));
                if cost < best[c][j] {
                    best[c][j] = cost;
                    cut[c][j] = i;
                }
            }
        }
    }
    // Use the fewest columns that reach the optimum (no empty columns).
    let c_best = (1..=k)
        .min_by(|a, b| best[*a][n].total_cmp(&best[*b][n]))
        .unwrap_or(1);
    let mut out = vec![0; n];
    let (mut c, mut j) = (c_best, n);
    while c > 0 {
        let i = cut[c][j];
        for slot in out.iter_mut().take(j).skip(i) {
            *slot = c - 1;
        }
        j = i;
        c -= 1;
    }
    out
}

pub(super) fn control_panel_layout(
    tokens: &HashMap<String, String>,
    model: &ControlPanelModel,
    sw: f32,
    sh: f32,
    measure: &mut Measure<'_>,
) -> ControlPanelLayout {
    let style = PanelStyle::resolve(tokens, model.scale);
    let s = style.clone();
    let mut p = Painter {
        style,
        measure,
        shapes: Vec::new(),
        texts: Vec::new(),
        hits: Vec::new(),
    };
    let flat = vec![OverlayRect {
        x: 0.0,
        y: 0.0,
        w: sw,
        h: sh,
        color: s.scrim.to_array(),
    }];

    let inset = s.inset.min(sw / 4.0).min(sh / 4.0);
    let panel = PanelRect {
        x: inset,
        y: inset,
        w: (sw - inset * 2.0).max(1.0),
        h: (sh - inset * 2.0).max(1.0),
    };
    p.shape(
        panel,
        s.shape_panel,
        s.surface,
        Some((1.0, s.outline_variant)),
        None,
    );
    let pad_x = s.space_xl;
    let inner_x = panel.x + pad_x;
    let inner_w = (panel.w - pad_x * 2.0).max(1.0);
    let mut y = panel.y + s.space_l + s.space_xs;

    // ── Search field ──────────────────────────────────────────────────────
    let search_h = s.search.line + s.space_m * 2.0;
    let field = PanelRect {
        x: inner_x,
        y,
        w: inner_w,
        h: search_h,
    };
    let search_focused = model.focus == PanelFocus::Search;
    let field_border = if search_focused {
        (s.ring, s.primary)
    } else {
        (1.0, s.outline_variant)
    };
    p.shape(field, s.shape_card, s.field, Some(field_border), None);
    let cy = y + search_h / 2.0;
    let text_x = field.x + s.space_l;
    let mut text_right = field.x + field.w - s.space_l;
    if let Some(notice) = &model.notice {
        let badge = PanelBadge {
            text: notice.clone(),
            tone: PanelTone::Caution,
        };
        text_right = p.badge(&badge, text_right, cy, None) - s.space_m;
    }
    let text_w = (text_right - text_x).max(1.0);
    if model.query.is_empty() {
        p.text(
            &model.placeholder,
            text_x,
            cy,
            text_w,
            s.search,
            s.on_surface_variant,
            false,
            Align::Start,
            None,
        );
    } else {
        p.text(
            &model.query,
            text_x,
            cy,
            text_w,
            s.search,
            s.on_surface,
            false,
            Align::Start,
            None,
        );
    }
    if search_focused {
        // A steady caret: no blink, so an idle panel draws nothing.
        let at = if model.query.is_empty() {
            0.0
        } else {
            p.width(&model.query, s.search, false)
        };
        let caret = PanelRect {
            x: (text_x + at + 1.0).min(text_right),
            y: cy - s.search.size * 0.6,
            w: (s.ring).max(1.0),
            h: s.search.size * 1.2,
        };
        p.shape(caret, 0.0, s.primary, None, None);
    }
    p.hit(PanelHitTarget::Search, field, None);
    y += search_h + s.space_m;

    // ── Quick-action tiles ────────────────────────────────────────────────
    if !model.tiles.is_empty() {
        // Tiles take a column-sized slot each (as many slots as columns
        // fit), so a few tiles stay compact; more tiles than slots share
        // the row.
        let gap = s.space_m;
        let fit = ((inner_w + gap) / (s.column_min + gap)).floor().max(1.0);
        let n = (model.tiles.len() as f32).max(fit);
        let tile_w = ((inner_w - gap * (n - 1.0)) / n).max(1.0);
        let tile_h = s.space_l * 2.0 + s.title.line + s.label.line;
        for (i, item) in model.tiles.iter().enumerate() {
            let rect = PanelRect {
                x: inner_x + i as f32 * (tile_w + gap),
                y,
                w: tile_w,
                h: tile_h,
            };
            let focused = model.focus == PanelFocus::Tile(i);
            let hovered = matches!(model.hover, Some(PanelHitTarget::Tile(h, _)) if h == i);
            p.tile(i, item, rect, focused, hovered);
        }
        y += tile_h + s.space_m;
    }

    // ── Section chips and the state line ──────────────────────────────────
    let chip_h = s.label.line + s.space_m;
    let chip_cy = y + chip_h / 2.0;
    let mut x = inner_x;
    for (i, chip) in model.chips.iter().enumerate() {
        let kw = p.width(&chip.key, s.label_s, true);
        let tw = p.width(&chip.title, s.label, false);
        let w = s.space_m + kw + s.space_s + tw + s.space_m;
        let rect = PanelRect { x, y, w, h: chip_h };
        let hovered = model.hover == Some(PanelHitTarget::Chip(i));
        let fill = if hovered {
            with_alpha(s.on_surface, s.hover_alpha)
        } else {
            with_alpha(s.surface, 0.0)
        };
        p.shape(
            rect,
            chip_h / 2.0,
            fill,
            Some((1.0, s.outline_variant)),
            None,
        );
        p.text(
            &chip.key,
            x + s.space_m,
            chip_cy,
            kw + 2.0,
            s.label_s,
            s.on_surface_variant,
            true,
            Align::Start,
            None,
        );
        p.text(
            &chip.title,
            x + s.space_m + kw + s.space_s,
            chip_cy,
            tw + 2.0,
            s.label,
            s.on_surface,
            false,
            Align::Start,
            None,
        );
        p.hit(PanelHitTarget::Chip(i), rect, None);
        x += w + s.space_s;
    }
    let state_x = x + s.space_l;
    p.text(
        &model.state_line,
        state_x,
        chip_cy,
        (inner_x + inner_w - state_x).max(0.0),
        s.label,
        s.on_surface_variant,
        false,
        Align::End,
        None,
    );
    y += chip_h + s.space_l;
    let divider = |y: f32| PanelRect {
        x: panel.x,
        y,
        w: panel.w,
        h: 1.0,
    };
    p.shape(divider(y), 0.0, s.outline_variant, None, None);
    let body_top = y + 1.0;

    // ── Footer ────────────────────────────────────────────────────────────
    let footer_h = s.label.line + s.space_l + s.space_xs;
    let footer_y = panel.y + panel.h - footer_h;
    p.shape(divider(footer_y), 0.0, s.outline_variant, None, None);
    let footer_cy = footer_y + footer_h / 2.0;
    let end_w = p.width(&model.hint_end, s.label, false) + 2.0;
    let end_x = inner_x + inner_w - end_w;
    p.text(
        &model.hint_end,
        end_x,
        footer_cy,
        end_w,
        s.label,
        s.on_surface_variant,
        false,
        Align::End,
        None,
    );
    let mut hx = inner_x;
    for hint in &model.hints {
        let w = p.width(hint, s.label, false) + 2.0;
        if hx + w > end_x - s.space_xl {
            break;
        }
        p.text(
            hint,
            hx,
            footer_cy,
            w,
            s.label,
            s.on_surface_variant,
            false,
            Align::Start,
            None,
        );
        hx += w + s.space_xl;
    }

    // ── Scrolling body ────────────────────────────────────────────────────
    let viewport = PanelRect {
        x: panel.x,
        y: body_top,
        w: panel.w,
        h: (footer_y - body_top).max(0.0),
    };
    let row_h = s.row_height();
    let head_h = s.label.line + s.space_m + s.space_xs;
    let card_pad = s.space_s;
    let col_gap = s.space_l;
    let content_w = inner_w;
    let mut rows: Vec<PanelSpan> = Vec::new();
    let mut sections: Vec<PanelSpan> = Vec::new();
    // Items to draw once the scroll offset is known: (item, rect in content
    // coordinates, context width).
    let mut row_items: Vec<(&PanelItem, PanelRect, f32)> = Vec::new();
    let mut cards: Vec<PanelRect> = Vec::new();
    let mut heads: Vec<(String, Option<String>, PanelRect)> = Vec::new();
    let mut empty_line: Option<&str> = None;
    let content_h = match &model.body {
        PanelBody::Sections(list) => {
            let heights: Vec<f32> = list
                .iter()
                .map(|sec| card_pad * 2.0 + head_h + row_h * sec.items.len() as f32)
                .collect();
            let fit = ((content_w + col_gap) / (s.column_min + col_gap))
                .floor()
                .max(1.0) as usize;
            let columns = fit.min(list.len().max(1));
            let col_w = (content_w - col_gap * (columns as f32 - 1.0)) / columns as f32;
            let assignment = balance_columns(&heights, columns, col_gap);
            let mut col_y = vec![s.space_l; columns];
            for (i, sec) in list.iter().enumerate() {
                let c = assignment[i];
                let card = PanelRect {
                    x: c as f32 * (col_w + col_gap),
                    y: col_y[c],
                    w: col_w,
                    h: heights[i],
                };
                cards.push(card);
                heads.push((
                    sec.title.clone(),
                    sec.key.clone(),
                    PanelRect {
                        x: card.x + card_pad,
                        y: card.y + card_pad,
                        w: card.w - card_pad * 2.0,
                        h: head_h,
                    },
                ));
                sections.push(PanelSpan {
                    id: sec.id.clone(),
                    top: card.y,
                    bottom: card.y + card.h,
                });
                for (r, item) in sec.items.iter().enumerate() {
                    let rect = PanelRect {
                        x: card.x + card_pad,
                        y: card.y + card_pad + head_h + r as f32 * row_h,
                        w: card.w - card_pad * 2.0,
                        h: row_h,
                    };
                    row_items.push((item, rect, 0.0));
                }
                col_y[c] += card.h + col_gap;
            }
            col_y.iter().copied().fold(0.0, f32::max) - col_gap + s.space_l
        }
        PanelBody::Results { title, items } => {
            let card = PanelRect {
                x: 0.0,
                y: s.space_l,
                w: content_w,
                h: card_pad * 2.0 + head_h + row_h * items.len() as f32,
            };
            cards.push(card);
            heads.push((
                title.clone(),
                None,
                PanelRect {
                    x: card.x + card_pad,
                    y: card.y + card_pad,
                    w: card.w - card_pad * 2.0,
                    h: head_h,
                },
            ));
            let context_w = (content_w * 0.18).clamp(0.0, 200.0 * s.body.size / 14.0);
            for (r, item) in items.iter().enumerate() {
                let rect = PanelRect {
                    x: card.x + card_pad,
                    y: card.y + card_pad + head_h + r as f32 * row_h,
                    w: card.w - card_pad * 2.0,
                    h: row_h,
                };
                row_items.push((item, rect, context_w));
            }
            card.y + card.h + s.space_l
        }
        PanelBody::Empty(line) => {
            empty_line = Some(line);
            s.space_l * 2.0 + s.body.line
        }
    };
    let max_scroll = (content_h - viewport.h).max(0.0);
    let scroll = if model.scroll_px.is_finite() {
        model.scroll_px.clamp(0.0, max_scroll)
    } else {
        0.0
    };
    // Content coordinates → screen.
    let ox = inner_x;
    let oy = viewport.y - scroll;
    let place = |r: PanelRect| PanelRect {
        x: r.x + ox,
        y: r.y + oy,
        ..r
    };
    let clip = Some(viewport);
    for card in &cards {
        p.shape(place(*card), s.shape_card, s.card, None, clip);
    }
    for (title, key, rect) in &heads {
        let r = place(*rect);
        let cy = r.y + r.h / 2.0;
        let mut right = r.x + r.w - s.space_s;
        if let Some(key) = key {
            let w = p.width(key, s.label_s, true) + 2.0;
            p.text(
                key,
                right - w,
                cy,
                w,
                s.label_s,
                s.on_surface_variant,
                true,
                Align::End,
                clip,
            );
            right -= w + s.space_s;
        }
        p.text(
            &title.to_uppercase(),
            r.x + s.space_s,
            cy,
            (right - r.x - s.space_s).max(1.0),
            s.label,
            s.on_surface_variant,
            false,
            Align::Start,
            clip,
        );
    }
    for (item, rect, context_w) in &row_items {
        rows.push(PanelSpan {
            id: item.id.clone(),
            top: rect.y,
            bottom: rect.y + rect.h,
        });
        let focused = model.focus == PanelFocus::Row(item.id.clone());
        let hovered = matches!(&model.hover, Some(PanelHitTarget::Row(id, _)) if *id == item.id);
        p.row(item, place(*rect), focused, hovered, *context_w, clip);
    }
    if let Some(line) = empty_line {
        p.text(
            line,
            inner_x,
            viewport.y + s.space_l + s.body.line / 2.0,
            inner_w,
            s.body,
            s.on_surface_variant,
            false,
            Align::Center,
            clip,
        );
    }
    if max_scroll > 0.0 {
        let track_h = (viewport.h - s.space_s * 2.0).max(1.0);
        let thumb_h = (track_h * viewport.h / content_h)
            .max(s.space_xl)
            .min(track_h);
        let thumb = PanelRect {
            x: panel.x + panel.w - s.space_s - s.space_xs,
            y: viewport.y + s.space_s + (track_h - thumb_h) * scroll / max_scroll,
            w: s.space_xs,
            h: thumb_h,
        };
        p.shape(thumb, s.space_xs / 2.0, s.outline, None, None);
    }

    let geometry = ControlPanelGeometry {
        panel,
        hits: p.hits,
        viewport_height: viewport.h,
        max_scroll,
        rows,
        sections,
    };
    ControlPanelLayout {
        flat,
        shapes: p.shapes,
        texts: p.texts,
        geometry,
    }
}

/// The panel's notice while it is closed (e.g. "Hidden: portals · open with
/// Ctrl+Shift+F11"): one caution pill centred at the top of a `sw`-wide
/// surface, styled like a caution badge at the base scale. Text too wide for
/// the surface ends in an ellipsis.
pub(super) fn closed_notice_layout(
    tokens: &HashMap<String, String>,
    text: &str,
    sw: f32,
    measure: &mut Measure<'_>,
) -> (Vec<PanelShape>, Vec<TextItem>) {
    let style = PanelStyle::resolve(tokens, 1.0);
    let s = style.clone();
    let mut p = Painter {
        style,
        measure,
        shapes: Vec::new(),
        texts: Vec::new(),
        hits: Vec::new(),
    };
    let ty = s.label;
    let max_w = (sw - s.space_m * 2.0).max(1.0);
    let w = (p.width(text, ty, false) + s.space_m * 2.0).min(max_w);
    let h = ty.line + s.space_s;
    let rect = PanelRect {
        x: (sw - w) / 2.0,
        y: s.space_m,
        w,
        h,
    };
    p.shape(
        rect,
        h / 2.0,
        s.caution_container,
        Some((1.0, s.caution_outline)),
        None,
    );
    p.text(
        text,
        rect.x + s.space_s,
        rect.y + h / 2.0,
        (w - s.space_s * 2.0).max(1.0),
        ty,
        s.on_caution_container,
        false,
        Align::Center,
        None,
    );
    (p.shapes, p.texts)
}

/// Text width estimate for a compositor without a text rasterizer (tests,
/// text-less headless runs): about half an em per glyph.
fn estimate_width(text: &str, size: f32, _weight: u16, mono: bool) -> f32 {
    let em = if mono { 0.6 } else { 0.55 };
    text.chars().count() as f32 * size * em
}

/// The geometry `model` would have on a `sw` x `sh` surface, with estimated
/// text widths. For GPU-free hosts (the event-loop harness) that need the
/// panel's hit regions without a compositor; drawing uses exact widths.
pub fn estimate_control_panel_geometry(
    tokens: &HashMap<String, String>,
    model: &ControlPanelModel,
    sw: f32,
    sh: f32,
) -> ControlPanelGeometry {
    control_panel_layout(tokens, model, sw, sh, &mut estimate_width).geometry
}

impl Compositor {
    /// Mirror the runtime's control panel (`None`: closed); returns true when
    /// it changed (the caller must then repaint once, since no scene change
    /// accompanies it). While open, the panel replaces the system card.
    pub fn set_control_panel(&mut self, panel: Option<ControlPanelModel>) -> bool {
        if self.control_panel == panel {
            return false;
        }
        self.control_panel = panel;
        self.control_panel_layout = None;
        true
    }

    pub fn control_panel_open(&self) -> bool {
        self.control_panel.is_some()
    }

    /// Mirror the panel's notice for while it is closed (`None`: no notice);
    /// returns true when it changed (the caller must then repaint once).
    pub fn set_closed_notice(&mut self, notice: Option<String>) -> bool {
        if self.closed_notice == notice {
            return false;
        }
        self.closed_notice = notice;
        true
    }

    /// The notice pill on a `sw`-wide surface; empty while the panel is open
    /// (it shows the same state itself) or without a notice.
    fn closed_notice_draw(&mut self, sw: f32) -> (Vec<PanelShape>, Vec<TextItem>) {
        let Some(text) = self
            .closed_notice
            .as_deref()
            .filter(|_| self.control_panel.is_none())
        else {
            return Default::default();
        };
        match self.text_rasterizer.as_mut() {
            Some(tr) => {
                closed_notice_layout(&self.token_map, text, sw, &mut |t, size, weight, mono| {
                    let family = if mono {
                        FontFamily::SystemMonospace
                    } else {
                        FontFamily::SystemSansSerif
                    };
                    tr.measure_single_line(t, size, family, weight)
                })
            }
            None => closed_notice_layout(&self.token_map, text, sw, &mut estimate_width),
        }
    }

    /// Lay the panel out for a `sw` x `sh` surface, reusing the last layout
    /// when neither the model, the tokens nor the size changed.
    pub(super) fn ensure_control_panel_layout(
        &mut self,
        sw: f32,
        sh: f32,
    ) -> Option<&ControlPanelLayout> {
        let model = self.control_panel.as_ref()?;
        let key = (sw.to_bits(), sh.to_bits());
        if self
            .control_panel_layout
            .as_ref()
            .is_none_or(|(k, _)| *k != key)
        {
            let layout = match self.text_rasterizer.as_mut() {
                Some(tr) => control_panel_layout(
                    &self.token_map,
                    model,
                    sw,
                    sh,
                    &mut |text, size, weight, mono| {
                        let family = if mono {
                            FontFamily::SystemMonospace
                        } else {
                            FontFamily::SystemSansSerif
                        };
                        tr.measure_single_line(text, size, family, weight)
                    },
                ),
                None => control_panel_layout(&self.token_map, model, sw, sh, &mut estimate_width),
            };
            self.control_panel_layout = Some((key, layout));
            self.control_panel_layout_generation += 1;
        }
        self.control_panel_layout.as_ref().map(|(_, l)| l)
    }

    /// Geometry of the panel as drawn on a `sw` x `sh` surface, with a
    /// generation that changes whenever it is recomputed; `None` while closed.
    pub fn control_panel_geometry(
        &mut self,
        sw: f32,
        sh: f32,
    ) -> Option<(u64, &ControlPanelGeometry)> {
        self.ensure_control_panel_layout(sw, sh)?;
        let generation = self.control_panel_layout_generation;
        self.control_panel_layout
            .as_ref()
            .map(|(_, layout)| (generation, &layout.geometry))
    }

    /// Flat quads of the final chrome pass: the panel scrim while the panel is
    /// open, else the system card backdrop.
    pub(super) fn final_chrome_vertices(&mut self, sw: f32, sh: f32) -> Vec<RectVertex> {
        if self.control_panel.is_none() {
            return self.system_card_vertices(sw, sh);
        }
        let rects = match self.ensure_control_panel_layout(sw, sh) {
            Some(layout) => layout.flat.clone(),
            None => return Vec::new(),
        };
        rects
            .iter()
            .flat_map(|r| rect_vertices(r.x, r.y, r.w, r.h, sw, sh, self.gpu_color_raw(r.color)))
            .collect()
    }

    /// Rounded shapes of the final chrome pass, in `gpu_color` form: the
    /// panel's while it is open, else its notice pill (if any).
    pub(super) fn final_chrome_rounded_rects(
        &mut self,
        sw: f32,
        sh: f32,
    ) -> Vec<RoundedRectDrawCmd> {
        let shapes = match self.ensure_control_panel_layout(sw, sh) {
            Some(layout) => layout.shapes.clone(),
            None => self.closed_notice_draw(sw).0,
        };
        shapes
            .iter()
            .map(|shape| RoundedRectDrawCmd {
                x: shape.rect.x,
                y: shape.rect.y,
                width: shape.rect.w,
                height: shape.rect.h,
                radius: shape.radius,
                color: self.gpu_color(shape.fill),
                border: shape.border.map(|(width, color)| RoundedRectBorder {
                    width,
                    color: self.gpu_color(color),
                }),
                clip: shape.clip.map(|c| RoundedRectClip {
                    x: c.x,
                    y: c.y,
                    width: c.w,
                    height: c.h,
                }),
            })
            .collect()
    }

    /// Text of the final chrome pass: the panel's while it is open, else the
    /// panel notice's and the system card's.
    pub(super) fn final_chrome_text_items(&mut self, sw: f32, sh: f32) -> Vec<TextItem> {
        if self.control_panel.is_none() {
            let mut items = self.closed_notice_draw(sw).1;
            items.extend(self.system_card_text_items(sw, sh));
            return items;
        }
        self.ensure_control_panel_layout(sw, sh)
            .map(|layout| layout.texts.clone())
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(id: &str, label: &str, control: PanelControl) -> PanelItem {
        PanelItem {
            id: id.into(),
            label: label.into(),
            detail: None,
            detail_tone: PanelTone::Normal,
            context: None,
            control,
            badge: None,
            shortcut: None,
            emphasis: PanelTone::Normal,
        }
    }

    fn section(id: &str, rows: usize) -> PanelSection {
        PanelSection {
            id: id.into(),
            title: id.into(),
            key: Some("Alt+1".into()),
            items: (0..rows)
                .map(|r| {
                    item(
                        &format!("{id}.{r}"),
                        &format!("Row {r}"),
                        PanelControl::Toggle(r % 2 == 0),
                    )
                })
                .collect(),
        }
    }

    fn model(body: PanelBody) -> ControlPanelModel {
        ControlPanelModel {
            query: String::new(),
            placeholder: "Search every HUD setting".into(),
            notice: None,
            tiles: vec![
                PanelItem {
                    emphasis: PanelTone::Caution,
                    ..item("agents.pause", "Agents paused", PanelControl::Toggle(true))
                },
                item(
                    "appearance.zoom",
                    "Zoom",
                    PanelControl::Stepper("100%".into()),
                ),
            ],
            chips: vec![PanelChip {
                title: "Agents".into(),
                key: "1".into(),
            }],
            state_line: String::new(),
            body,
            hints: vec!["↑↓ move".into()],
            hint_end: "Esc close".into(),
            focus: PanelFocus::Search,
            hover: None,
            scroll_px: 0.0,
            scale: 1.0,
        }
    }

    fn layout(m: &ControlPanelModel, w: f32, h: f32) -> ControlPanelLayout {
        control_panel_layout(&HashMap::new(), m, w, h, &mut estimate_width)
    }

    /// The panel spans the display minus the inset token on every side and
    /// the scrim covers the whole surface.
    #[test]
    fn panel_spans_the_display_minus_the_inset() {
        let l = layout(
            &model(PanelBody::Sections(vec![section("a", 2)])),
            1600.0,
            900.0,
        );
        assert_eq!(
            l.geometry.panel,
            PanelRect {
                x: 32.0,
                y: 32.0,
                w: 1536.0,
                h: 836.0
            }
        );
        assert_eq!((l.flat[0].w, l.flat[0].h), (1600.0, 900.0));
        let tokens = HashMap::from([("control_panel.inset".to_string(), "64".to_string())]);
        let m = model(PanelBody::Sections(vec![]));
        let l = control_panel_layout(&tokens, &m, 1600.0, 900.0, &mut estimate_width);
        assert_eq!(l.geometry.panel.x, 64.0);
    }

    /// Quick tiles take column-sized slots: a lone tile stays compact, and
    /// more tiles than slots share the row evenly.
    #[test]
    fn quick_tiles_take_column_sized_slots() {
        let tile_rects = |n: usize| {
            let mut m = model(PanelBody::Sections(vec![section("a", 1)]));
            m.tiles = (0..n)
                .map(|i| item(&format!("t.{i}"), "Tile", PanelControl::Toggle(false)))
                .collect();
            let l = layout(&m, 1600.0, 900.0);
            (0..n)
                .map(|i| {
                    l.geometry
                        .hits
                        .iter()
                        .find(|h| h.target == PanelHitTarget::Tile(i, PanelPart::Body))
                        .unwrap()
                        .rect
                })
                .collect::<Vec<_>>()
        };
        let one = tile_rects(1);
        assert!(
            one[0].w >= 360.0 && one[0].w < 400.0,
            "one of four slots: {}",
            one[0].w
        );
        let six = tile_rects(6);
        assert!(
            six.windows(2)
                .all(|p| (p[0].w - p[1].w).abs() < 0.01 && p[1].x > p[0].x)
        );
        let right = six[5].x + six[5].w;
        assert!(
            right <= 1600.0 - 32.0,
            "the row stays inside the panel: {right}"
        );
    }

    /// Sections flow into as many `column.min_width` columns as fit, in
    /// contiguous reading order.
    #[test]
    fn sections_flow_into_as_many_columns_as_fit() {
        let secs: Vec<_> = (0..6)
            .map(|i| section(&format!("s{i}"), 2 + i % 3))
            .collect();
        let wide = layout(&model(PanelBody::Sections(secs.clone())), 1600.0, 2000.0);
        let columns = |l: &ControlPanelLayout| {
            let mut tops: Vec<f32> = l
                .geometry
                .hits
                .iter()
                .filter_map(|h| match &h.target {
                    PanelHitTarget::Row(id, PanelPart::Body) if id.ends_with(".0") => {
                        Some(h.rect.x)
                    }
                    _ => None,
                })
                .collect();
            tops.sort_by(f32::total_cmp);
            tops.dedup();
            tops.len()
        };
        // 1600 - 2*32 inset - 2*24 padding = 1488 = 4 columns of 360 + 3 gaps.
        assert_eq!(columns(&wide), 4);
        let narrow = layout(&model(PanelBody::Sections(secs)), 700.0, 2000.0);
        assert_eq!(columns(&narrow), 1);
    }

    #[test]
    fn balance_columns_is_contiguous_and_minimises_the_tallest_column() {
        assert_eq!(
            balance_columns(&[1.0, 1.0, 1.0, 1.0], 2, 0.0),
            vec![0, 0, 1, 1]
        );
        assert_eq!(
            balance_columns(&[3.0, 1.0, 1.0, 1.0], 2, 0.0),
            vec![0, 1, 1, 1]
        );
        assert_eq!(balance_columns(&[1.0], 3, 0.0), vec![0]);
        assert!(balance_columns(&[], 3, 0.0).is_empty());
        let a = balance_columns(&[2.0, 5.0, 1.0, 1.0, 4.0, 2.0], 3, 1.0);
        assert!(a.windows(2).all(|w| w[0] <= w[1]), "contiguous: {a:?}");
    }

    /// Content taller than the viewport scrolls; the offset is clamped, rows
    /// outside the viewport get no hit region, and `reveal` brings a row fully
    /// into view.
    #[test]
    fn tall_content_scrolls_and_reveal_keeps_the_row_visible() {
        let mut m = model(PanelBody::Sections(vec![section("a", 30)]));
        let l = layout(&m, 1600.0, 900.0);
        let g = &l.geometry;
        assert!(g.max_scroll > 0.0);
        let last = g.rows.last().unwrap().clone();
        let hit_ids = |g: &ControlPanelGeometry| {
            g.hits
                .iter()
                .filter_map(|h| match &h.target {
                    PanelHitTarget::Row(id, PanelPart::Body) => Some(id.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>()
        };
        assert!(!hit_ids(g).contains(&last.id), "off-screen row has no hit");
        let scroll = g.reveal(&last, 0.0);
        assert!(scroll > 0.0 && scroll <= g.max_scroll);
        assert!(last.bottom - scroll <= g.viewport_height + 0.01);
        m.scroll_px = scroll;
        assert!(hit_ids(&layout(&m, 1600.0, 900.0).geometry).contains(&last.id));
        m.scroll_px = 1e9;
        let clamped = layout(&m, 1600.0, 900.0);
        assert!(
            hit_ids(&clamped.geometry).contains(&last.id),
            "over-scroll clamps"
        );
        // Revealing a row already visible does not move.
        let first = g.rows[0].clone();
        assert_eq!(g.reveal(&first, 0.0), 0.0);
    }

    /// Points outside the panel pass through; inside, parts win over the row
    /// body and empty space is still the panel's.
    #[test]
    fn hits_resolve_parts_first_and_pass_through_outside() {
        let m = model(PanelBody::Sections(vec![PanelSection {
            id: "appearance".into(),
            title: "Appearance".into(),
            key: None,
            items: vec![item(
                "appearance.zoom",
                "Zoom",
                PanelControl::Stepper("100%".into()),
            )],
        }]));
        let g = layout(&m, 1600.0, 900.0).geometry;
        assert_eq!(g.hit(5.0, 5.0), None);
        assert_eq!(
            g.hit(g.panel.x + 2.0, g.panel.y + g.panel.h - 2.0),
            Some(PanelHitTarget::Panel)
        );
        let plus = g
            .hits
            .iter()
            .find(|h| {
                h.target == PanelHitTarget::Row("appearance.zoom".into(), PanelPart::Increment)
            })
            .expect("stepper + hit")
            .rect;
        assert_eq!(
            g.hit(plus.x + plus.w / 2.0, plus.y + plus.h / 2.0),
            Some(PanelHitTarget::Row(
                "appearance.zoom".into(),
                PanelPart::Increment
            ))
        );
        let tile = g
            .hits
            .iter()
            .find(|h| h.target == PanelHitTarget::Tile(0, PanelPart::Body))
            .unwrap()
            .rect;
        assert_eq!(
            g.hit(tile.x + 4.0, tile.y + 4.0),
            Some(PanelHitTarget::Tile(0, PanelPart::Body))
        );
    }

    /// An override that makes text unreadable on the panel falls back to the
    /// default pair; legible overrides are honoured.
    #[test]
    fn illegible_color_overrides_fall_back_to_defaults() {
        let bad = HashMap::from([
            ("color.on_surface".to_string(), "#222833".to_string()),
            (
                "color.surface.container.high".to_string(),
                "#232934".to_string(),
            ),
        ]);
        let s = PanelStyle::resolve(&bad, 1.0);
        for fill in [s.surface, s.card, s.field] {
            assert!(contrast(s.on_surface, fill) >= MIN_TEXT_CONTRAST);
            assert!(contrast(s.on_surface_variant, fill) >= MIN_TEXT_CONTRAST);
        }
        // Rows sit on cards: a card override that hides text falls back too.
        let card = HashMap::from([("color.surface.container".to_string(), "#B0B4BA".to_string())]);
        let s = PanelStyle::resolve(&card, 1.0);
        assert!(contrast(s.on_surface_variant, s.card) >= MIN_TEXT_CONTRAST);
        // A status pair falls back on its own, leaving the rest alone.
        let caution = HashMap::from([
            (
                "color.on_caution.container".to_string(),
                "#4B3B11".to_string(),
            ),
            ("color.on_surface".to_string(), "#FFFFFF".to_string()),
        ]);
        let s = PanelStyle::resolve(&caution, 1.0);
        assert!(contrast(s.on_caution_container, s.caution_container) >= MIN_TEXT_CONTRAST);
        assert_eq!(s.on_surface, parse_hex_color("#FFFFFF").unwrap());
        let good = HashMap::from([("color.on_surface".to_string(), "#FFFFFF".to_string())]);
        let s = PanelStyle::resolve(&good, 1.0);
        assert_eq!(s.on_surface, parse_hex_color("#FFFFFF").unwrap());
    }

    /// Sizes follow tokens and scale, within a legible clamp.
    #[test]
    fn sizes_follow_tokens_and_scale_within_a_legible_clamp() {
        let base = PanelStyle::resolve(&HashMap::new(), 1.0);
        assert_eq!(base.body.size, 14.0);
        assert_eq!(PanelStyle::resolve(&HashMap::new(), 1.5).body.size, 21.0);
        assert_eq!(
            PanelStyle::resolve(&HashMap::new(), 50.0).body.size,
            28.0,
            "scale clamps to 2x"
        );
        let tiny = HashMap::from([("type.body.m.size".to_string(), "3".to_string())]);
        assert_eq!(PanelStyle::resolve(&tiny, 1.0).body.size, 12.0);
        let big = HashMap::from([("type.body.m.size".to_string(), "18".to_string())]);
        assert_eq!(PanelStyle::resolve(&big, 1.0).body.size, 18.0);
    }

    /// Results list rows carry their section as context and keep ids unique.
    #[test]
    fn results_and_empty_bodies_lay_out() {
        let mut found = item(
            "input.pause",
            "Pause chord",
            PanelControl::ReadOnly {
                text: "Ctrl+Shift+F12".into(),
                mono: true,
            },
        );
        found.context = Some("Input".into());
        let g = layout(
            &model(PanelBody::Results {
                title: "1 result".into(),
                items: vec![found],
            }),
            1600.0,
            900.0,
        )
        .geometry;
        assert_eq!(g.rows.len(), 1);
        let empty = layout(
            &model(PanelBody::Empty("No settings match “x”".into())),
            1600.0,
            900.0,
        );
        assert!(empty.geometry.rows.is_empty());
        assert!(
            empty
                .texts
                .iter()
                .any(|t| t.text.contains("No settings match"))
        );
    }
}
