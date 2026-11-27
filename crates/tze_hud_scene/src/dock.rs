//! Scene-owned dock instances and geometry for admission, presentation and pointer routing.

use std::collections::HashMap;
use std::num::NonZeroU64;

use serde::{Deserialize, Serialize};

use crate::graph::SceneGraph;
use crate::types::{
    ContentionPolicy, Rect, SceneId, WidgetInstance, WidgetParamAudience, WidgetParameterValue,
    WidgetPublishRecord,
};
use crate::validation::ValidationError;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DockMetrics {
    pub row_height: f32,
    pub caption_width: f32,
    pub gap: f32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DockLayout {
    pub column: Rect,
    pub capacity: usize,
    pub scale: f32,
    metrics: DockMetrics,
}

impl DockLayout {
    /// Metrics are already converted to physical pixels for this display.
    /// Aspect ratios are from eligible bundles in owner-configured order.
    pub fn new(
        display: Rect,
        max_rows: usize,
        metrics: DockMetrics,
        aspect_ratios: impl IntoIterator<Item = f32>,
    ) -> Self {
        let widest_row = aspect_ratios
            .into_iter()
            .fold(metrics.caption_width, |width, aspect| {
                width + metrics.gap + metrics.row_height * aspect
            });
        let width_limit = display.width * 0.35;
        let scale = if widest_row > width_limit {
            width_limit / widest_row
        } else {
            1.0
        };
        let metrics = DockMetrics {
            row_height: metrics.row_height * scale,
            caption_width: metrics.caption_width * scale,
            gap: metrics.gap * scale,
        };
        let column = Rect::new(
            display.x + display.width * 0.05,
            display.y + display.height * 0.12,
            widest_row * scale,
            display.height * 0.73,
        );
        let pitch = metrics.row_height + metrics.gap;
        let capacity = if pitch > 0.0 {
            max_rows.min(((column.height + metrics.gap) / pitch).floor() as usize)
        } else {
            0
        };
        Self {
            column,
            capacity,
            scale,
            metrics,
        }
    }

    pub fn caption(&self, row: usize) -> Rect {
        Rect::new(
            self.column.x,
            self.column.y + row as f32 * (self.metrics.row_height + self.metrics.gap),
            self.metrics.caption_width,
            self.metrics.row_height,
        )
    }

    /// Place a widget after its live predecessors without moving earlier items.
    pub fn widget(
        &self,
        row: usize,
        preceding: impl IntoIterator<Item = f32>,
        aspect_ratio: f32,
    ) -> Rect {
        let caption = self.caption(row);
        let x = preceding
            .into_iter()
            .fold(caption.x + caption.width + self.metrics.gap, |x, aspect| {
                x + self.metrics.row_height * aspect + self.metrics.gap
            });
        Rect::new(
            x,
            caption.y,
            self.metrics.row_height * aspect_ratio,
            self.metrics.row_height,
        )
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct DockWidget {
    pub widget_type: String,
    pub instance_name: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct DockRow {
    /// Exact publication owner, including the session scope when present.
    pub namespace: String,
    /// Live instances, oldest first.
    pub widgets: Vec<DockWidget>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct DockState {
    accepts: Vec<String>,
    capacity: usize,
    auto_clear_ms: NonZeroU64,
    rows: Vec<DockRow>,
}

impl DockState {
    pub(crate) fn remove_instance(&mut self, instance_name: &str) -> bool {
        let Some((row_index, widget_index)) = self.rows.iter().enumerate().find_map(|(i, row)| {
            row.widgets
                .iter()
                .position(|widget| widget.instance_name == instance_name)
                .map(|j| (i, j))
        }) else {
            return false;
        };
        let row = &mut self.rows[row_index];
        row.widgets.remove(widget_index);
        if row.widgets.is_empty() {
            self.rows.remove(row_index);
        }
        true
    }
}

#[derive(Debug, PartialEq, thiserror::Error)]
pub enum DockPublishError {
    #[error("dock full; show this in a card")]
    Full,
    #[error(transparent)]
    Widget(#[from] ValidationError),
}

impl SceneGraph {
    /// Install owner policy after bundle validation. A smaller display only
    /// limits new rows. The new lifetime applies to the next accepted publish.
    pub fn configure_dock(
        &mut self,
        accepts: Vec<String>,
        layout: DockLayout,
        auto_clear_ms: NonZeroU64,
    ) {
        if let Some(dock) = &mut self.dock {
            if dock.accepts == accepts
                && dock.capacity == layout.capacity
                && dock.auto_clear_ms == auto_clear_ms
            {
                return;
            }
            dock.accepts = accepts;
            dock.capacity = layout.capacity;
            dock.auto_clear_ms = auto_clear_ms;
        } else {
            self.dock = Some(DockState {
                accepts,
                capacity: layout.capacity,
                auto_clear_ms,
                rows: Vec::new(),
            });
        }
        self.version += 1;
    }

    pub fn dock_rows(&self) -> &[DockRow] {
        self.dock.as_ref().map_or(&[], |dock| dock.rows.as_slice())
    }

    pub fn dock_instance(&self, namespace: &str, widget_type: &str) -> Option<&WidgetInstance> {
        let name = self.dock_instance_name(namespace, widget_type)?;
        self.widget_registry.instances.get(name)
    }

    fn dock_instance_name(&self, namespace: &str, widget_type: &str) -> Option<&str> {
        self.dock_rows()
            .iter()
            .find(|row| row.namespace == namespace)?
            .widgets
            .iter()
            .find(|widget| widget.widget_type == widget_type)
            .map(|widget| widget.instance_name.as_str())
    }

    pub(crate) fn is_dock_widget(&self, instance_name: &str) -> bool {
        self.dock_rows()
            .iter()
            .flat_map(|row| &row.widgets)
            .any(|widget| widget.instance_name == instance_name)
    }

    /// Publish immediately for a live, authenticated scope supplied by the caller.
    /// A zero requested lifetime uses the owner's finite cap.
    pub fn publish_to_dock(
        &mut self,
        widget_type: &str,
        params: HashMap<String, WidgetParameterValue>,
        namespace: &str,
        lease_id: Option<SceneId>,
        transition_ms: u32,
        ttl_ms: u64,
    ) -> Result<SceneId, DockPublishError> {
        self.check_widget_publish_safe_mode(namespace)?;
        if let Some(lease_id) = lease_id {
            self.check_zone_publish_lease(namespace, lease_id)?;
        }
        let not_found = || ValidationError::WidgetNotFound {
            name: widget_type.into(),
        };
        let dock = self
            .dock
            .as_ref()
            .filter(|dock| dock.accepts.iter().any(|accepted| accepted == widget_type))
            .ok_or_else(not_found)?;
        let definition = self
            .widget_registry
            .get_definition(widget_type)
            .ok_or_else(not_found)?;
        let params = Self::validate_widget_params_for_definition(widget_type, definition, &params)?;
        let row_index = dock.rows.iter().position(|row| row.namespace == namespace);
        if row_index.is_none() && dock.rows.len() >= dock.capacity {
            return Err(DockPublishError::Full);
        }
        let auto_clear_ms = dock.auto_clear_ms.get();
        let definition = definition.clone();
        let existing = self.dock_instance(namespace, widget_type).cloned();
        let is_new = existing.is_none();
        let mut instance = existing.unwrap_or_else(|| {
            let (id, instance_name) = loop {
                let id = SceneId::new();
                let name = format!("dock:{id}");
                if !self.widget_registry.instances.contains_key(&name) {
                    break (id, name);
                }
            };
            WidgetInstance {
                id,
                widget_type_name: widget_type.into(),
                tab_id: SceneId::null(),
                geometry_override: None,
                contention_override: Some(ContentionPolicy::LatestWins),
                instance_name,
                current_params: definition
                    .parameter_schema
                    .iter()
                    .map(|p| (p.name.clone(), p.default_value.clone()))
                    .collect(),
                auto_clear_ms: None,
            }
        });
        // A lease handoff starts a new publication. Preserve widget identity
        // and local state but do not carry ordinary values from the old lease.
        if self
            .widget_registry
            .active_publishes
            .get(&instance.instance_name)
            .and_then(|records| records.first())
            .is_some_and(|record| record.lease_id != lease_id)
        {
            for declaration in &definition.parameter_schema {
                if declaration.audience == WidgetParamAudience::Model {
                    instance
                        .current_params
                        .insert(declaration.name.clone(), declaration.default_value.clone());
                }
            }
        }
        instance.auto_clear_ms = Some(auto_clear_ms);
        let id = instance.id;
        let now = self.now_wall_us();
        let record = WidgetPublishRecord {
            ttl_paused_us: 0,
            widget_name: instance.instance_name.clone(),
            publisher_namespace: namespace.into(),
            params,
            published_at_wall_us: now,
            merge_key: None,
            expires_at_wall_us: (ttl_ms != 0)
                .then(|| now.saturating_add(ttl_ms.saturating_mul(1_000))),
            transition_ms,
            lease_id,
            publication_origin: None,
        };
        let new_widget = is_new.then(|| DockWidget {
            widget_type: widget_type.into(),
            instance_name: instance.instance_name.clone(),
        });
        self.commit_widget_publication(instance, &definition, record)?;
        if let Some(widget) = new_widget {
            let rows = &mut self.dock.as_mut().expect("configured dock").rows;
            if let Some(index) = row_index {
                rows[index].widgets.push(widget);
            } else {
                rows.push(DockRow {
                    namespace: namespace.into(),
                    widgets: vec![widget],
                });
            }
        }
        Ok(id)
    }

    /// Retime only this scope's publication, within the cap captured on publish.
    pub fn hold_dock_widget(&mut self, namespace: &str, widget_type: &str, ttl_ms: u64) -> bool {
        let Some(name) = self
            .dock_instance_name(namespace, widget_type)
            .map(str::to_owned)
        else {
            return false;
        };
        let expiry = (ttl_ms != 0).then(|| {
            self.now_wall_us()
                .saturating_add(ttl_ms.saturating_mul(1_000))
        });
        self.hold_widget_publications_inner(&name, namespace, expiry)
    }

    /// Clear only this scope's instance. Releasing its last instance frees the row.
    pub fn clear_dock_widget(&mut self, namespace: &str, widget_type: &str) {
        if let Some(name) = self
            .dock_instance_name(namespace, widget_type)
            .map(str::to_owned)
        {
            self.clear_widget_for_publisher_inner(&name, namespace);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn metrics(display_scale: f32) -> DockMetrics {
        DockMetrics {
            row_height: 52.0 * display_scale,
            caption_width: 96.0 * display_scale,
            gap: 8.0 * display_scale,
        }
    }

    #[test]
    fn dock_layout_capacity_follows_usable_height_and_configured_limit() {
        for (width, height, scale, max_rows, rows) in [
            (3840.0, 2160.0, 1.5, 32, 17),
            (3840.0, 2160.0, 1.5, 16, 16),
            (1920.0, 1080.0, 1.0, 32, 13),
        ] {
            let layout = DockLayout::new(
                Rect::new(0.0, 0.0, width, height),
                max_rows,
                metrics(scale),
                [360.0 / 52.0, 160.0 / 52.0],
            );
            assert_eq!(layout.capacity, rows);
            let last = layout.caption(rows - 1);
            assert!(last.is_within(&layout.column));
            if rows < max_rows {
                assert!(!layout.caption(rows).is_within(&layout.column));
            }
        }
    }

    #[test]
    fn dock_layout_uses_monitor_origin_and_scales_wide_rows_uniformly() {
        let monitor = Rect::new(-960.0, 100.0, 960.0, 600.0);
        let layout = DockLayout::new(monitor, 32, metrics(1.0), [7.0, 4.0]);
        assert_eq!((layout.column.x, layout.column.y), (-912.0, 172.0));
        assert!((layout.column.width - 336.0).abs() < 0.01);
        assert!(layout.scale < 1.0);
        let progress = layout.widget(0, [], 7.0);
        let countdown = layout.widget(0, [7.0], 4.0);
        let next = layout.widget(1, [], 7.0);
        assert_eq!(progress.height, countdown.height);
        assert_eq!(progress.height, 52.0 * layout.scale);
        assert_eq!(progress.width / progress.height, 7.0);
        assert!(countdown.x > progress.x + progress.width);
        assert!(next.y > progress.y + progress.height);
        assert_eq!(next.x, progress.x);
    }
}
