//! Admission-time text fit: which published text the renderer would cut.
//!
//! The scene stays GPU- and font-free; the process that owns the renderer
//! supplies an implementation that measures with the same shaper and tokens
//! it draws with. MCP reports the result once, in the publish reply
//! (`docs/api.md` "clipped").

use crate::graph::SceneGraph;
use crate::types::{NotificationPayload, WidgetParameterValue};
use std::collections::HashMap;

/// Measures published text against the current tokens and geometry.
pub trait TextFit: Send + Sync + std::fmt::Debug {
    /// The notification fields (`"title"`, `"body"`, `"actions"`) whose
    /// rendered text would be cut on `zone`, in that order. Empty when
    /// everything fits or the zone does not draw notification cards.
    fn notification(
        &self,
        scene: &SceneGraph,
        zone: &str,
        payload: &NotificationPayload,
    ) -> Vec<&'static str>;

    /// The string parameters of widget instance `widget` whose rendered text
    /// would be cut with `params` applied, sorted by name.
    fn widget(
        &self,
        scene: &SceneGraph,
        widget: &str,
        params: &HashMap<String, WidgetParameterValue>,
    ) -> Vec<String>;
}
