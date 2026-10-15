//! Pure portal presentation: scene nodes, overlay affordances and a PortalSurface.
//! The runtime owns placement, leases and drafts; rendering never builds wire messages.
use crate::tokens::PortalVisualTokens;
use crate::{
    ComposerStatus, InputDeliveryState, OutputKind, PortalError, PortalInputFeedback,
    PortalInputFeedbackState, PortalView, ProjectedPortalPresentation, ProjectionLifecycleState,
    TranscriptUnit,
};
use tze_hud_config::TimestampGranularity;
use tze_hud_scene::types::{
    HitRegionNode, LifecycleAccent, Node, NodeData, PortalDisplayState, PortalIdentity,
    PortalLifecycleState, PortalPart, PortalPartKind, PortalPeerClass, PortalSurface,
    SolidColorNode, TextColorRun, TextMarkdownNode, TextOverflow,
};
use tze_hud_scene::{Rect, Rgba, SceneId};

const PORTAL_DISCONNECT_MARKER_LINE: &str = "⊘ disconnected — stream stale";
const PORTAL_EMPTY_READY_LINE: &str = "Ready — waiting for the first message.";
const PORTAL_CONNECTING_LINE: &str = "◌ Connecting… — waiting for the session to come online.";
const PORTAL_UNREAD_DIVIDER_LINE: &str = "─── unread ───";
const PORTAL_ACTIVITY_MARKER_LINE: &str = "⋯ writing";
const PORTAL_STREAMING_CURSOR_GLYPH: &str = " ▍";
const PORTAL_TIMESTAMP_SEPARATOR: &str = " · ";
const WALL_CLOCK_MINUTE_US: u64 = 60_000_000;
pub const PORTAL_ACTIVITY_QUIESCE_WINDOW_US: u64 = 2_000_000;
const MAX_PORTAL_MARKDOWN_BYTES: usize = 16_384;
const EXPANDED_PORTAL_CORNER_RADIUS_PX: f32 = 14.0;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Size {
    pub width: f32,
    pub height: f32,
}
pub const DEFAULT_PORTAL_SIZE: Size = Size {
    width: 720.0,
    height: 360.0,
};
pub const DEFAULT_COMPACT_SIZE: Size = Size {
    width: 420.0,
    height: 96.0,
};

pub struct PortalRender {
    pub root: Node,
    pub children: Vec<Node>,
    pub accent: Option<LifecycleAccent>,
    pub unread: usize,
    pub surface: PortalSurface,
    pub composer: Option<HitRegionNode>,
}

pub fn render(
    view: &PortalView,
    tokens: &PortalVisualTokens,
    viewport: Size,
    now_us: u64,
) -> PortalRender {
    let renderer = Renderer { tokens, viewport };
    let (root, children) = renderer.portal_node(view, SceneId::new(), now_us);
    let (color, width_px) = if view.connection_degraded {
        (
            Some(tokens.disconnect_badge_color),
            tokens.disconnect_badge_width_px,
        )
    } else {
        (
            view.lifecycle_state
                .map(|s| lifecycle_accent_color(s, tokens)),
            tokens.lifecycle_accent_width_px,
        )
    };
    let accent = color
        .filter(|c| c.a > 0.0 && width_px > 0.0)
        .map(|color| LifecycleAccent { color, width_px });
    let composer = view.interaction_enabled.then(|| HitRegionNode {
        bounds: match view.presentation {
            ProjectedPortalPresentation::Expanded => renderer.geometry().input_pane,
            ProjectedPortalPresentation::Collapsed => renderer.bounds(),
        },
        interaction_id: format!("{}-composer", view.portal_id),
        accepts_focus: true,
        accepts_pointer: true,
        accepts_composer_input: true,
        ..Default::default()
    });
    PortalRender {
        root,
        children,
        accent,
        unread: view.unread_output_count.unwrap_or(0),
        surface: renderer.surface(view),
        composer,
    }
}

struct Renderer<'a> {
    tokens: &'a PortalVisualTokens,
    viewport: Size,
}
#[derive(Clone, Copy)]
struct ExpandedPortalGeometry {
    full: Rect,
    header: Rect,
    input_pane: Rect,
    input_content: Rect,
    output_pane: Rect,
    output_content: Rect,
    divider: Rect,
    divider_grip: Rect,
}

impl Renderer<'_> {
    fn bounds(&self) -> Rect {
        Rect::new(0.0, 0.0, self.viewport.width, self.viewport.height)
    }
    fn surface(&self, state: &PortalView) -> PortalSurface {
        let full = self.bounds();
        let part = |kind, bounds| PortalPart {
            kind,
            bounds,
            node: None,
        };
        let mut parts = vec![part(PortalPartKind::Frame, full)];
        match state.presentation {
            ProjectedPortalPresentation::Collapsed => {
                parts.push(part(PortalPartKind::CollapsedCard, full))
            }
            ProjectedPortalPresentation::Expanded => {
                let g = self.geometry();
                parts.extend([
                    part(PortalPartKind::Header, g.header),
                    part(PortalPartKind::Transcript, g.output_pane),
                    part(PortalPartKind::Divider, g.divider),
                ]);
                if state.interaction_enabled {
                    parts.push(part(PortalPartKind::Composer, g.input_pane));
                }
            }
        }
        PortalSurface {
            identity: PortalIdentity {
                session_id: state.portal_id.clone(),
                display_name: state.display_name.clone().unwrap_or_default(),
                peer_class: PortalPeerClass::ResidentLlm,
            },
            lifecycle: match state.lifecycle_state {
                None => PortalLifecycleState::Unspecified,
                Some(ProjectionLifecycleState::Active | ProjectionLifecycleState::Attached) => {
                    PortalLifecycleState::Active
                }
                Some(
                    ProjectionLifecycleState::Degraded | ProjectionLifecycleState::HudUnavailable,
                ) => PortalLifecycleState::Degraded,
                _ => PortalLifecycleState::Detached,
            },
            display_state: match state.presentation {
                ProjectedPortalPresentation::Expanded => PortalDisplayState::Expanded,
                ProjectedPortalPresentation::Collapsed => PortalDisplayState::Collapsed,
            },
            parts,
        }
    }
    fn solid_node(id: SceneId, color: Rgba, bounds: Rect, radius: f32) -> Node {
        Node {
            id,
            children: vec![],
            layout: Default::default(),
            data: NodeData::SolidColor(SolidColorNode {
                color,
                bounds,
                radius: Some(radius),
            }),
        }
    }
    fn chrome_text_node(content: String, bounds: Rect, color: Rgba, font_size_px: f32) -> Node {
        Node {
            id: SceneId::new(),
            children: vec![],
            layout: Default::default(),
            data: NodeData::TextMarkdown(TextMarkdownNode {
                content,
                bounds,
                font_size_px,
                font_family: Default::default(),
                alignment: Default::default(),
                color,
                background: Some(Rgba::TRANSPARENT),
                color_runs: Box::default(),
                overflow: TextOverflow::Clip,
            }),
        }
    }
    fn geometry(&self) -> ExpandedPortalGeometry {
        let full = self.bounds();
        let header_h = self.tokens.header_height_px.max(0.0).min(full.height);
        let divider_w = self.tokens.section_gap_px.max(1.0).min(full.width);
        let pane_width = ((full.width - divider_w).max(0.0) * 0.5).max(0.0);
        let body_y = header_h;
        let body_h = (full.height - body_y).max(0.0);
        let input_pane = Rect {
            x: 0.0,
            y: body_y,
            width: pane_width,
            height: body_h,
        };
        let divider = Rect {
            x: pane_width,
            y: body_y,
            width: divider_w,
            height: body_h,
        };
        let output_pane = Rect {
            x: divider.x + divider.width,
            y: body_y,
            width: (full.width - divider.x - divider.width).max(0.0),
            height: body_h,
        };
        let inset = self.tokens.content_inset_px.max(0.0);
        let label_band_h = (self.tokens.header_font_size_px + inset).min(body_h);
        let output_content_y = output_pane.y + label_band_h;
        let input_content_y = input_pane.y + label_band_h;
        let input_content = Rect {
            x: input_pane.x + inset,
            y: input_content_y,
            width: (input_pane.width - inset * 2.0).max(0.0),
            height: (input_pane.y + input_pane.height - input_content_y - inset).max(0.0),
        };
        let output_content = Rect {
            x: output_pane.x + inset,
            y: output_content_y,
            width: (output_pane.width - inset * 2.0).max(0.0),
            height: (output_pane.y + output_pane.height - output_content_y - inset).max(0.0),
        };
        let grip_w = (divider_w / 3.0).max(1.0).min(divider_w);
        let grip_h = (header_h * 0.85).max(grip_w).min(body_h);
        let divider_grip = Rect {
            x: divider.x + (divider.width - grip_w) / 2.0,
            y: divider.y + (divider.height - grip_h) / 2.0,
            width: grip_w,
            height: grip_h,
        };

        ExpandedPortalGeometry {
            full,
            header: Rect {
                x: 0.0,
                y: 0.0,
                width: full.width,
                height: header_h,
            },
            input_pane,
            input_content,
            output_pane,
            output_content,
            divider,
            divider_grip,
        }
    }
    fn portal_node(
        &self,
        state: &PortalView,
        root_id: SceneId,
        now_wall_us: u64,
    ) -> (Node, Vec<Node>) {
        if state.presentation == ProjectedPortalPresentation::Collapsed {
            return (
                self.portal_text_node(state, root_id, now_wall_us),
                Vec::new(),
            );
        }

        let geometry = self.geometry();
        let mut transcript = self.portal_text_node(state, SceneId::new(), now_wall_us);
        if let NodeData::TextMarkdown(text) = &mut transcript.data {
            text.bounds = geometry.output_content;
            text.background = Some(Rgba {
                r: 0.0,
                g: 0.0,
                b: 0.0,
                a: 0.0,
            });
        }

        let mut frame_color = self.tokens.frame_background;
        frame_color.a *= self.tokens.frame_opacity.clamp(0.0, 1.0);
        let header_background = if is_connection_degraded(state) {
            self.tokens.transcript_dim_background
        } else {
            frame_color
        };
        let output_background = if is_connection_degraded(state) {
            self.tokens.transcript_dim_background
        } else {
            self.tokens.transcript_background
        };
        let input_label_bounds = Rect {
            x: geometry.input_pane.x + self.tokens.content_inset_px,
            y: geometry.input_pane.y,
            width: (geometry.input_pane.width - self.tokens.content_inset_px * 2.0).max(0.0),
            height: self.tokens.header_font_size_px + self.tokens.content_inset_px,
        };
        let output_label_bounds = Rect {
            x: geometry.output_pane.x + self.tokens.content_inset_px,
            y: geometry.output_pane.y,
            width: (geometry.output_pane.width - self.tokens.content_inset_px * 2.0).max(0.0),
            height: self.tokens.header_font_size_px + self.tokens.content_inset_px,
        };
        let title = state.display_name.as_deref().unwrap_or("Projected session");
        let mut header_title = match state.lifecycle_state {
            Some(lifecycle) => format!("{title} · {lifecycle:?}"),
            None => title.to_string(),
        };
        if let Some(unread) = unread_indicator_line(state.unread_output_count) {
            header_title.push_str(" · ");
            header_title.push_str(&unread);
        }
        if awaiting_reply(state) {
            header_title.push_str(" · awaiting reply");
        } else if agent_activity_active(state, now_wall_us) {
            header_title.push_str(" · ");
            header_title.push_str(PORTAL_ACTIVITY_MARKER_LINE);
        }
        if is_connection_degraded(state) {
            header_title.push_str(" · ");
            header_title.push_str(PORTAL_DISCONNECT_MARKER_LINE);
        }

        let mut root = Self::solid_node(
            root_id,
            frame_color,
            geometry.full,
            EXPANDED_PORTAL_CORNER_RADIUS_PX,
        );
        let children = vec![
            Self::solid_node(
                SceneId::new(),
                header_background,
                geometry.header,
                EXPANDED_PORTAL_CORNER_RADIUS_PX,
            ),
            Self::chrome_text_node(
                header_title,
                geometry.header,
                self.tokens.header_text_color,
                self.tokens.header_font_size_px,
            ),
            Self::solid_node(
                SceneId::new(),
                self.tokens.composer_background,
                geometry.input_pane,
                EXPANDED_PORTAL_CORNER_RADIUS_PX,
            ),
            Self::chrome_text_node(
                "INPUT".to_string(),
                input_label_bounds,
                self.tokens.header_text_color,
                self.tokens.header_font_size_px,
            ),
            Self::chrome_text_node(
                portal_input_markdown(state, self.tokens.timestamp_granularity),
                geometry.input_content,
                self.tokens.composer_text_color,
                self.tokens.composer_font_size_px,
            ),
            Self::solid_node(
                SceneId::new(),
                output_background,
                geometry.output_pane,
                EXPANDED_PORTAL_CORNER_RADIUS_PX,
            ),
            Self::chrome_text_node(
                "OUTPUT".to_string(),
                output_label_bounds,
                self.tokens.header_text_color,
                self.tokens.header_font_size_px,
            ),
            transcript,
            Self::solid_node(
                SceneId::new(),
                self.tokens.divider_color,
                geometry.divider,
                geometry.divider.width * 0.5,
            ),
            Self::solid_node(
                SceneId::new(),
                self.tokens.divider_grip_color,
                geometry.divider_grip,
                geometry.divider_grip.width * 0.5,
            ),
        ];
        root.children = children.iter().map(|node| node.id).collect();
        (root, children)
    }
    fn portal_text_node(&self, state: &PortalView, id: SceneId, now_wall_us: u64) -> Node {
        let (mut color, mut background, font_size_px) = match state.presentation {
            ProjectedPortalPresentation::Expanded => (
                self.tokens.transcript_text_color,
                self.tokens.transcript_background,
                self.tokens.transcript_font_size_px,
            ),
            ProjectedPortalPresentation::Collapsed => (
                self.tokens.collapsed_text_color,
                self.tokens.collapsed_background,
                self.tokens.collapsed_font_size_px,
            ),
        };
        if state.connection_degraded && state.presentation == ProjectedPortalPresentation::Expanded
        {
            color = self.tokens.transcript_dim_text_color;
            background = self.tokens.transcript_dim_background;
        }
        let content = match state.presentation {
            ProjectedPortalPresentation::Expanded => {
                output_markdown(state, now_wall_us, self.tokens.timestamp_granularity)
            }
            ProjectedPortalPresentation::Collapsed => collapsed_markdown(state),
        };
        let content_len = content.len();
        let color_runs = {
            let mut runs = stale_marker_color_runs(state, self.tokens.stale_marker_color);
            runs.extend(lifecycle_marker_color_runs(state, self.tokens));
            runs.extend(unread_indicator_color_runs(
                state,
                self.tokens.unread_indicator_color,
            ));
            runs.extend(unread_divider_color_runs(
                state,
                self.tokens.unread_divider_color,
            ));
            runs.extend(awaiting_reply_color_runs(
                state,
                self.tokens.awaiting_reply_color,
            ));
            runs.extend(empty_state_color_runs(state, self.tokens.empty_state_color));
            runs.extend(connecting_color_runs(
                state,
                self.tokens.connecting_marker_color,
            ));
            runs.extend(activity_cue_color_runs(
                state,
                self.tokens.activity_cue_color,
                now_wall_us,
            ));
            runs.extend(streaming_cursor_color_runs(
                state,
                self.tokens.streaming_cursor_color,
                now_wall_us,
                content_len,
            ));
            runs.extend(delivery_cue_color_runs(state, self.tokens));
            runs.extend(timestamp_color_runs(
                state,
                self.tokens.timestamp_color,
                self.tokens.timestamp_granularity,
            ));
            runs
        }
        .into_boxed_slice();
        Node {
            id,
            children: vec![],
            layout: Default::default(),
            data: NodeData::TextMarkdown(TextMarkdownNode {
                content,
                bounds: self.bounds(),
                font_size_px,
                font_family: Default::default(),
                alignment: Default::default(),
                color,
                background: Some(background),
                color_runs,
                overflow: TextOverflow::Ellipsis,
            }),
        }
    }
}
fn format_wall_clock_arrival_hhmm(appended_at_wall_us: u64) -> String {
    let seconds_of_day = (appended_at_wall_us / 1_000_000) % 86_400;
    let hours = seconds_of_day / 3_600;
    let minutes = (seconds_of_day % 3_600) / 60;
    format!("{hours:02}:{minutes:02}")
}
fn agent_activity_active(state: &PortalView, now_wall_us: u64) -> bool {
    agent_activity_clear_deadline_us(state).is_some_and(|deadline| now_wall_us <= deadline)
}

pub fn agent_activity_clear_deadline_us(state: &PortalView) -> Option<u64> {
    if state.presentation != ProjectedPortalPresentation::Expanded
        || is_connection_degraded(state)
        || !state.has_ever_connected
    {
        return None;
    }
    state.visible_transcript.last().and_then(|unit| {
        (unit.output_kind != OutputKind::Viewer && !unit.expects_reply).then(|| {
            unit.appended_at_wall_us
                .saturating_add(PORTAL_ACTIVITY_QUIESCE_WINDOW_US)
        })
    })
}

fn is_connection_degraded(state: &PortalView) -> bool {
    state.connection_degraded
}
fn output_markdown(state: &PortalView, now_us: u64, timestamps: TimestampGranularity) -> String {
    if state.visible_transcript.is_empty() {
        return empty_portal_markdown(state).into();
    }
    let mut content = visible_transcript_markdown(
        &state.visible_transcript,
        unread_divider_boundary(state),
        timestamps,
    );
    if agent_activity_active(state, now_us) {
        content.push_str(PORTAL_STREAMING_CURSOR_GLYPH);
    }
    if content.len() > MAX_PORTAL_MARKDOWN_BYTES {
        let mut cut = content.len() - MAX_PORTAL_MARKDOWN_BYTES;
        while !content.is_char_boundary(cut) {
            cut += 1;
        }
        content.drain(..cut);
    }
    content
}
fn collapsed_markdown(state: &PortalView) -> String {
    let title = state.display_name.as_deref().unwrap_or("Projected session");
    let mut result = format!("**{title}**\n`{}` · Collapsed · Ambient", state.portal_id);
    if let Some(line) = unread_indicator_line(state.unread_output_count) {
        push_line(&mut result, &line);
    }
    if awaiting_reply(state) {
        push_line(&mut result, "? awaiting reply");
    }
    if state.latest_viewer_delivery_state == Some(InputDeliveryState::Delivered) {
        push_line(&mut result, "✓✓ delivered");
    }
    if state.connection_degraded {
        push_line(&mut result, PORTAL_DISCONNECT_MARKER_LINE);
    }
    if let Some(lifecycle) = state.lifecycle_state {
        push_line(
            &mut result,
            &format!("status: {} {lifecycle:?}", lifecycle_glyph(lifecycle)),
        );
    }
    if let Some(note) = &state.status_text {
        push_line(&mut result, &format!("note: {note}"));
    }
    let preview = state
        .visible_transcript
        .last()
        .map(|u| u.output_text.as_str())
        .unwrap_or("compact projection affordance");
    push_line(&mut result, &clamp_one_line(preview, 160));
    let tail_start = result.len();
    push_input_status_lines(&mut result, state);
    if result.len() > MAX_PORTAL_MARKDOWN_BYTES {
        let tail = result.split_off(tail_start);
        let head_budget = MAX_PORTAL_MARKDOWN_BYTES.saturating_sub(tail.len());
        result.truncate(clamp_utf8(&result, head_budget).len());
        result.push_str(&tail);
    }
    result
}
fn composer_line(state: &PortalView) -> String {
    if !state.interaction_enabled {
        return "composer: unavailable".into();
    }
    match state.composer {
        ComposerStatus::Ready => "composer: ready",
        ComposerStatus::Composing => "composer: composing",
        ComposerStatus::AtCapacity => "composer: [!] at capacity",
    }
    .into()
}
fn push_input_status_lines(result: &mut String, state: &PortalView) {
    if let Some(pending) = state.pending_input_count {
        push_line(result, &format!("pending HUD input: {pending}"));
    }
    if let Some(feedback) = &state.last_input_feedback {
        push_line(result, &composer_feedback_line(feedback));
    }
}

fn composer_feedback_line(feedback: &PortalInputFeedback) -> String {
    match feedback.feedback_state {
        PortalInputFeedbackState::Accepted => "✓ sent".to_string(),
        PortalInputFeedbackState::Rejected => {
            let reason = match feedback.error_code {
                Some(PortalError::NotHeld) => "the agent is disconnected".to_string(),
                Some(PortalError::QueueFull) => {
                    "too many replies waiting — try again shortly".to_string()
                }
                Some(PortalError::TooLarge { .. }) => "message too long".to_string(),
                _ => clamp_one_line(&feedback.status_summary, 160),
            };
            format!("⚠ not sent — {reason}")
        }
    }
}
fn stale_marker_color_runs(state: &PortalView, stale_marker_color: Rgba) -> Vec<TextColorRun> {
    if !is_connection_degraded(state) {
        return Vec::new();
    }
    vec![TextColorRun {
        start_byte: 0,
        end_byte: 0,
        color: stale_marker_color,
    }]
}

fn unread_indicator_line(unread_output_count: Option<usize>) -> Option<String> {
    match unread_output_count {
        Some(count) if count > 0 => Some(format!("{count} unread")),
        _ => None,
    }
}

fn unread_indicator_color_runs(
    state: &PortalView,
    unread_indicator_color: Rgba,
) -> Vec<TextColorRun> {
    match state.unread_output_count {
        Some(count) if count > 0 => vec![TextColorRun {
            start_byte: 0,
            end_byte: 0,
            color: unread_indicator_color,
        }],
        _ => Vec::new(),
    }
}

fn unread_divider_boundary(state: &PortalView) -> Option<usize> {
    let count = match state.visible_unread_output_count {
        Some(count) if count > 0 => count,
        _ => return None,
    };
    let mut agent_seen = 0usize;
    let mut boundary = None;
    for (index, unit) in state.visible_transcript.iter().enumerate().rev() {
        if unit.output_kind != OutputKind::Viewer {
            agent_seen += 1;
            boundary = Some(index);
            if agent_seen == count {
                break;
            }
        }
    }
    boundary
}

fn unread_divider_color_runs(state: &PortalView, unread_divider_color: Rgba) -> Vec<TextColorRun> {
    if state.presentation == ProjectedPortalPresentation::Expanded
        && !state.visible_transcript.is_empty()
        && unread_divider_boundary(state).is_some()
    {
        vec![TextColorRun {
            start_byte: 0,
            end_byte: 0,
            color: unread_divider_color,
        }]
    } else {
        Vec::new()
    }
}

fn awaiting_reply(state: &PortalView) -> bool {
    state
        .visible_transcript
        .last()
        .is_some_and(|unit| unit.expects_reply)
}

fn awaiting_reply_color_runs(state: &PortalView, awaiting_reply_color: Rgba) -> Vec<TextColorRun> {
    if awaiting_reply(state) {
        vec![TextColorRun {
            start_byte: 0,
            end_byte: 0,
            color: awaiting_reply_color,
        }]
    } else {
        Vec::new()
    }
}

fn delivery_cue_color_runs(state: &PortalView, tokens: &PortalVisualTokens) -> Vec<TextColorRun> {
    if state.latest_viewer_delivery_state == Some(InputDeliveryState::Delivered) {
        vec![TextColorRun {
            start_byte: 0,
            end_byte: 0,
            color: tokens.delivery_delivered_color,
        }]
    } else {
        Vec::new()
    }
}

fn lifecycle_glyph(lifecycle: ProjectionLifecycleState) -> char {
    match lifecycle {
        ProjectionLifecycleState::Active => '◆',
        ProjectionLifecycleState::Attached => '◇',
        ProjectionLifecycleState::Degraded | ProjectionLifecycleState::HudUnavailable => '◈',
        ProjectionLifecycleState::Detached
        | ProjectionLifecycleState::CleanupPending
        | ProjectionLifecycleState::Expired => '○',
    }
}

fn lifecycle_accent_color(
    lifecycle: ProjectionLifecycleState,
    tokens: &PortalVisualTokens,
) -> Rgba {
    match lifecycle {
        ProjectionLifecycleState::Active => tokens.lifecycle_active_color,
        ProjectionLifecycleState::Attached => tokens.lifecycle_attached_color,
        ProjectionLifecycleState::Degraded | ProjectionLifecycleState::HudUnavailable => {
            tokens.lifecycle_attention_color
        }
        ProjectionLifecycleState::Detached
        | ProjectionLifecycleState::CleanupPending
        | ProjectionLifecycleState::Expired => tokens.lifecycle_inactive_color,
    }
}

fn lifecycle_marker_color_runs(
    state: &PortalView,
    tokens: &PortalVisualTokens,
) -> Vec<TextColorRun> {
    let Some(lifecycle) = state.lifecycle_state else {
        return Vec::new();
    };
    vec![TextColorRun {
        start_byte: 0,
        end_byte: 0,
        color: lifecycle_accent_color(lifecycle, tokens),
    }]
}

fn empty_portal_markdown(state: &PortalView) -> &'static str {
    if !state.has_ever_connected {
        return PORTAL_CONNECTING_LINE;
    }
    PORTAL_EMPTY_READY_LINE
}

fn empty_state_color_runs(state: &PortalView, empty_state_color: Rgba) -> Vec<TextColorRun> {
    if state.presentation == ProjectedPortalPresentation::Expanded
        && state.visible_transcript.is_empty()
        && state.has_ever_connected
    {
        vec![TextColorRun {
            start_byte: 0,
            end_byte: 0,
            color: empty_state_color,
        }]
    } else {
        Vec::new()
    }
}

fn connecting_color_runs(state: &PortalView, connecting_marker_color: Rgba) -> Vec<TextColorRun> {
    if state.presentation == ProjectedPortalPresentation::Expanded
        && state.visible_transcript.is_empty()
        && !state.has_ever_connected
    {
        vec![TextColorRun {
            start_byte: 0,
            end_byte: 0,
            color: connecting_marker_color,
        }]
    } else {
        Vec::new()
    }
}

fn activity_cue_color_runs(
    state: &PortalView,
    activity_cue_color: Rgba,
    now_wall_us: u64,
) -> Vec<TextColorRun> {
    if agent_activity_active(state, now_wall_us) {
        vec![TextColorRun {
            start_byte: 0,
            end_byte: 0,
            color: activity_cue_color,
        }]
    } else {
        Vec::new()
    }
}

fn streaming_cursor_color_runs(
    state: &PortalView,
    streaming_cursor_color: Rgba,
    now_wall_us: u64,
    content_len: usize,
) -> Vec<TextColorRun> {
    if agent_activity_active(state, now_wall_us) {
        let Ok(end) = u32::try_from(content_len) else {
            return Vec::new();
        };
        vec![TextColorRun {
            start_byte: end,
            end_byte: end,
            color: streaming_cursor_color,
        }]
    } else {
        Vec::new()
    }
}

fn timestamp_color_runs(
    state: &PortalView,
    timestamp_color: Rgba,
    granularity: TimestampGranularity,
) -> Vec<TextColorRun> {
    if granularity != TimestampGranularity::Off
        && state.presentation == ProjectedPortalPresentation::Expanded
        && !state.visible_transcript.is_empty()
    {
        vec![TextColorRun {
            start_byte: 0,
            end_byte: 0,
            color: timestamp_color,
        }]
    } else {
        Vec::new()
    }
}

fn visible_transcript_markdown_with(
    units: &[TranscriptUnit],
    unread_boundary: Option<usize>,
    timestamps: TimestampGranularity,
) -> String {
    visible_transcript_markdown(units, unread_boundary, timestamps)
}

fn visible_transcript_markdown(
    units: &[TranscriptUnit],
    unread_boundary: Option<usize>,
    timestamps: TimestampGranularity,
) -> String {
    let mut result = String::new();
    let mut last_stamped_minute: Option<u64> = None;
    for (index, unit) in units.iter().enumerate() {
        if index > 0 {
            result.push_str("\n---\n");
        }
        if unread_boundary == Some(index) {
            result.push_str(PORTAL_UNREAD_DIVIDER_LINE);
            result.push('\n');
        }
        let minute_bucket = unit.appended_at_wall_us / WALL_CLOCK_MINUTE_US;
        let show_stamp = match timestamps {
            TimestampGranularity::Off => false,
            TimestampGranularity::PerTurn => true,
            TimestampGranularity::Grouped => last_stamped_minute != Some(minute_bucket),
        };
        if show_stamp {
            result.push_str(&format_wall_clock_arrival_hhmm(unit.appended_at_wall_us));
            result.push_str(PORTAL_TIMESTAMP_SEPARATOR);
            last_stamped_minute = Some(minute_bucket);
        }
        result.push_str(clamp_utf8(&unit.output_text, 4_096));
    }
    result
}
fn input_history_markdown(units: &[TranscriptUnit], timestamps: TimestampGranularity) -> String {
    visible_transcript_markdown_with(units, None, timestamps)
}

fn portal_input_markdown(state: &PortalView, timestamps: TimestampGranularity) -> String {
    let status = composer_line(state);
    let mut tail = String::new();
    push_input_status_lines(&mut tail, state);
    push_line(&mut tail, &status);
    let max_history_bytes = MAX_PORTAL_MARKDOWN_BYTES.saturating_sub(tail.len() + 1);
    let history = input_history_markdown(&state.input_history, timestamps);
    let mut result = clamp_utf8(&history, max_history_bytes).to_string();
    push_line(&mut result, &tail);
    result
}

fn clamp_one_line(text: &str, max_bytes: usize) -> String {
    clamp_utf8(text.lines().next().unwrap_or_default(), max_bytes).to_string()
}

fn clamp_utf8(text: &str, max_bytes: usize) -> &str {
    if text.len() <= max_bytes {
        return text;
    }
    let mut cut = max_bytes;
    while cut > 0 && !text.is_char_boundary(cut) {
        cut -= 1;
    }
    &text[..cut]
}

fn push_line(result: &mut String, line: &str) {
    if !result.is_empty() {
        result.push('\n');
    }
    result.push_str(line);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render_goldens::{unit, view};

    #[test]
    fn agent_activity_clear_deadline_matches_active_predicate() {
        let mut state = view();
        state.visible_transcript = vec![unit("hello", 1_000_000)];
        let due = 1_000_000 + PORTAL_ACTIVITY_QUIESCE_WINDOW_US;
        assert_eq!(agent_activity_clear_deadline_us(&state), Some(due));
        assert!(agent_activity_active(&state, due));
        assert!(!agent_activity_active(&state, due + 1));
        for mode in 0..5 {
            let mut nonwriting = state.clone();
            match mode {
                0 => nonwriting.connection_degraded = true,
                1 => nonwriting.has_ever_connected = false,
                2 => nonwriting.presentation = ProjectedPortalPresentation::Collapsed,
                3 => nonwriting.visible_transcript[0].expects_reply = true,
                _ => nonwriting.visible_transcript[0].output_kind = OutputKind::Viewer,
            }
            assert_eq!(agent_activity_clear_deadline_us(&nonwriting), None);
        }
    }

    #[test]
    fn lifecycle_state_drives_distinct_token_accent_runs() {
        let tokens = PortalVisualTokens::default();
        for (lifecycle, glyph, color) in [
            (
                ProjectionLifecycleState::Active,
                '◆',
                tokens.lifecycle_active_color,
            ),
            (
                ProjectionLifecycleState::Attached,
                '◇',
                tokens.lifecycle_attached_color,
            ),
            (
                ProjectionLifecycleState::Degraded,
                '◈',
                tokens.lifecycle_attention_color,
            ),
            (
                ProjectionLifecycleState::HudUnavailable,
                '◈',
                tokens.lifecycle_attention_color,
            ),
            (
                ProjectionLifecycleState::Detached,
                '○',
                tokens.lifecycle_inactive_color,
            ),
            (
                ProjectionLifecycleState::CleanupPending,
                '○',
                tokens.lifecycle_inactive_color,
            ),
            (
                ProjectionLifecycleState::Expired,
                '○',
                tokens.lifecycle_inactive_color,
            ),
        ] {
            let mut state = view();
            state.lifecycle_state = Some(lifecycle);
            let rendered = render(&state, &tokens, DEFAULT_PORTAL_SIZE, 0);
            assert_eq!(rendered.accent.unwrap().color, color);
            assert_eq!(
                lifecycle_marker_color_runs(&state, &tokens),
                [TextColorRun {
                    start_byte: 0,
                    end_byte: 0,
                    color
                }]
            );
            state.presentation = ProjectedPortalPresentation::Collapsed;
            assert!(collapsed_markdown(&state).contains(&format!("status: {glyph} {lifecycle:?}")));
        }
    }

    #[test]
    fn unread_divider_boundary_uses_visible_count_not_aggregate() {
        let mut state = view();
        state.visible_transcript = vec![unit("old", 0), unit("new", 0), unit("reply", 0)];
        state.visible_transcript[2].output_kind = OutputKind::Viewer;
        state.unread_output_count = Some(20);
        for (count, expected) in [(0, None), (1, Some(1)), (2, Some(0)), (50, Some(0))] {
            state.visible_unread_output_count = Some(count);
            assert_eq!(unread_divider_boundary(&state), expected);
            let output = output_markdown(&state, 10_000_000, TimestampGranularity::Off);
            assert_eq!(
                output.matches(PORTAL_UNREAD_DIVIDER_LINE).count(),
                usize::from(expected.is_some())
            );
        }
        state.visible_transcript = vec![unit("reply", 0)];
        state.visible_transcript[0].output_kind = OutputKind::Viewer;
        assert_eq!(unread_divider_boundary(&state), None);
    }

    #[test]
    fn grouped_granularity_coalesces_consecutive_same_minute_turns() {
        let units = [unit("a", 0), unit("b", 30_000_000), unit("c", 90_000_000)];
        assert_eq!(
            visible_transcript_markdown(&units, None, TimestampGranularity::Grouped),
            "00:00 · a\n---\nb\n---\n00:01 · c"
        );
        assert_eq!(
            visible_transcript_markdown(&units, None, TimestampGranularity::PerTurn),
            "00:00 · a\n---\n00:00 · b\n---\n00:01 · c"
        );
        assert_eq!(
            visible_transcript_markdown(&units, None, TimestampGranularity::Off),
            "a\n---\nb\n---\nc"
        );
        assert_eq!(
            format_wall_clock_arrival_hhmm((13 * 3600 + 45 * 60 + 59) * 1_000_000 + 999_999),
            "13:45"
        );
        assert_eq!(
            visible_transcript_markdown(
                &[unit("it is really 23:59", 0)],
                None,
                TimestampGranularity::PerTurn
            ),
            "00:00 · it is really 23:59"
        );
    }

    #[test]
    fn portal_output_markdown_keeps_newest_turn_when_formatting_exceeds_wire_cap() {
        let mut state = view();
        state.visible_transcript = (0..8).map(|_| unit(&"é".repeat(2048), 0)).collect();
        state.visible_transcript.push(unit("newest tail 日本語", 1));
        let content = output_markdown(&state, 1, TimestampGranularity::PerTurn);
        assert!(content.len() <= MAX_PORTAL_MARKDOWN_BYTES);
        assert!(content.ends_with("newest tail 日本語 ▍"));
        let runs = streaming_cursor_color_runs(&state, Rgba::WHITE, 1, content.len());
        assert_eq!(runs[0].start_byte, content.len() as u32);
        assert_eq!(runs[0].end_byte, runs[0].start_byte);
        assert!(content.is_char_boundary(runs[0].start_byte as usize));
    }

    #[test]
    fn composer_status_line_stays_last_under_large_input_history() {
        let mut state = view();
        state.input_history = (0..8).map(|_| unit(&"reply ".repeat(1000), 0)).collect();
        state.composer = ComposerStatus::AtCapacity;
        let content = portal_input_markdown(&state, TimestampGranularity::Off);
        assert!(content.len() <= MAX_PORTAL_MARKDOWN_BYTES);
        assert!(content.ends_with("pending HUD input: 0\ncomposer: [!] at capacity"));
        state.interaction_enabled = false;
        assert!(
            portal_input_markdown(&state, TimestampGranularity::Off)
                .ends_with("composer: unavailable")
        );
    }

    #[test]
    fn delivered_cue_is_absent_until_input_is_delivered() {
        let mut state = view();
        let tokens = PortalVisualTokens::default();
        assert!(delivery_cue_color_runs(&state, &tokens).is_empty());
        state.latest_viewer_delivery_state = Some(InputDeliveryState::Delivered);
        assert_eq!(
            delivery_cue_color_runs(&state, &tokens)[0].color,
            tokens.delivery_delivered_color
        );
        state.presentation = ProjectedPortalPresentation::Collapsed;
        assert!(collapsed_markdown(&state).contains("✓✓ delivered"));
    }
}
