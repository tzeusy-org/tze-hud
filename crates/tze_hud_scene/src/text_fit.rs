//! Admission-time text fit: which published text the renderer would cut,
//! and how much of it is drawn.
//!
//! The scene stays GPU- and font-free; the process that owns the renderer
//! supplies an implementation that measures with the same shaper and tokens
//! it draws with. MCP reports the result once, in the publish reply
//! (`docs/api.md` "clipped").

use crate::graph::SceneGraph;
use crate::types::{NotificationPayload, WidgetParameterValue};
use std::collections::HashMap;

/// Measures published text against the current tokens and geometry.
///
/// Each cut field is reported with the number of characters (Unicode scalar
/// values) of the sent string that are drawn, not counting the renderer's
/// ellipsis: the first that many characters, republished, fit.
pub trait TextFit: Send + Sync + std::fmt::Debug {
    /// The notification fields (`"title"`, `"body"`, `"actions"`) whose
    /// rendered text would be cut on `zone`, in that order, each with its
    /// drawn character count; `"actions"` carries the smallest count among
    /// the cut labels. Empty when everything fits or the zone does not draw
    /// notification cards.
    fn notification(
        &self,
        scene: &SceneGraph,
        zone: &str,
        payload: &NotificationPayload,
    ) -> Vec<(&'static str, usize)>;

    /// The string parameters of widget instance `widget` whose rendered text
    /// would be cut with `params` applied, sorted by name, each with its
    /// drawn character count.
    fn widget(
        &self,
        scene: &SceneGraph,
        widget: &str,
        params: &HashMap<String, WidgetParameterValue>,
    ) -> Vec<(String, usize)>;
}
