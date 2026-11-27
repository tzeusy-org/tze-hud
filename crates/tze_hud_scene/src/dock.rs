//! Scene-owned dock instances and geometry for admission, presentation and pointer routing.

use std::collections::HashMap;
use std::num::NonZeroU64;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::graph::SceneGraph;
use crate::types::{
    ContentionPolicy, Rect, SceneId, WidgetHoverBehavior, WidgetInstance, WidgetParamAudience,
    WidgetParameterValue, WidgetPublishRecord, WidgetSvgLayer, ZoneRegistry, resolve_zone_geometry,
};
use crate::validation::ValidationError;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DockMetrics {
    pub row_height: f32,
    pub caption_width: f32,
    pub gap: f32,
}

/// Owner-loaded alternate artwork and hover metadata for an existing widget type.
#[derive(Clone, Debug, PartialEq)]
pub struct DockPresentation {
    pub layers: Vec<WidgetSvgLayer>,
    pub width: f32,
    pub height: f32,
    pub hover_behavior: Option<WidgetHoverBehavior>,
    sources: HashMap<String, blake3::Hash>,
    builtin_countdown_layout: bool,
}

impl DockPresentation {
    pub fn new(
        layers: Vec<WidgetSvgLayer>,
        width: f32,
        height: f32,
        hover_behavior: Option<WidgetHoverBehavior>,
        svg_contents: &HashMap<String, Vec<u8>>,
    ) -> Self {
        let sources = layers
            .iter()
            .filter_map(|layer| {
                let file = &layer.svg_file;
                svg_contents
                    .get(file)
                    .map(|bytes| (file.clone(), blake3::hash(bytes)))
            })
            .collect();
        Self {
            layers,
            width,
            height,
            hover_behavior,
            sources,
            builtin_countdown_layout: false,
        }
    }

    /// Only runtime-selected embedded countdown artwork uses the compact text
    /// layout. Custom bundles keep their authored SVG layout unchanged.
    pub fn with_builtin_countdown_layout(mut self) -> Self {
        self.builtin_countdown_layout = true;
        self
    }

    pub fn uses_builtin_countdown_layout(&self) -> bool {
        self.builtin_countdown_layout
    }

    /// SVG registration may be denied by the renderer's resident budget.
    /// Never bind this descriptor against an older version left at the same filename.
    pub fn matches_source(&self, file: &str, bytes: &[u8]) -> bool {
        self.sources
            .get(file)
            .is_some_and(|expected| *expected == blake3::hash(bytes))
    }
}

/// Internal comparison identity of the selected startup/reload bundle.
#[derive(Clone, PartialEq, Eq)]
pub struct WidgetBundleOrigin(BundleSource);

#[derive(Clone, PartialEq, Eq)]
enum BundleSource {
    Directory(PathBuf),
    Embedded(String),
}

impl WidgetBundleOrigin {
    pub fn directory(path: &Path) -> Self {
        Self(BundleSource::Directory(path.into()))
    }

    pub fn embedded(label: &str) -> Self {
        Self(BundleSource::Embedded(label.into()))
    }
}

impl std::fmt::Debug for WidgetBundleOrigin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WidgetBundleOrigin").finish_non_exhaustive()
    }
}

#[derive(Clone, Debug)]
pub(crate) struct DockPresentationEntry {
    pub origin: WidgetBundleOrigin,
    pub presentation: Arc<DockPresentation>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DockLayout {
    pub display: Rect,
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
        aspect_ratios: impl IntoIterator<Item = f64>,
    ) -> Self {
        let widest_row = aspect_ratios
            .into_iter()
            .fold(f64::from(metrics.caption_width), |width, aspect| {
                width + f64::from(metrics.gap) + f64::from(metrics.row_height) * aspect
            });
        let (x, y, width_limit, height) = resolve_zone_geometry(
            &ZoneRegistry::dock_aperture(),
            display.width,
            display.height,
        );
        let scale = if widest_row > f64::from(width_limit) {
            f64::from(width_limit) / widest_row
        } else {
            1.0
        };
        let metrics = DockMetrics {
            row_height: (f64::from(metrics.row_height) * scale) as f32,
            caption_width: (f64::from(metrics.caption_width) * scale) as f32,
            gap: (f64::from(metrics.gap) * scale) as f32,
        };
        let column = Rect::new(
            display.x + x,
            display.y + y,
            (widest_row * scale) as f32,
            height,
        );
        let pitch = metrics.row_height + metrics.gap;
        let capacity = if metrics.row_height > 0.0 {
            max_rows.min(((column.height + metrics.gap) / pitch).floor() as usize)
        } else {
            0
        };
        Self {
            display,
            column,
            capacity,
            scale: scale as f32,
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
        preceding: impl IntoIterator<Item = f64>,
        aspect_ratio: f64,
    ) -> Rect {
        let caption = self.caption(row);
        let row_height = f64::from(self.metrics.row_height);
        let x = preceding.into_iter().fold(
            f64::from(caption.x + caption.width + self.metrics.gap),
            |x, aspect| x + row_height * aspect + f64::from(self.metrics.gap),
        );
        Rect::new(
            x as f32,
            caption.y,
            (row_height * aspect_ratio) as f32,
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

#[derive(Clone, Debug, PartialEq)]
pub struct DockProjection {
    pub target: Option<String>,
    pub layout: DockLayout,
    /// Physical pixels per logical pixel on the selected display, before fitting the row.
    pub pixels_per_dp: f32,
    /// Committed fit inputs in owner roster order, shared by draw and hit consumers.
    pub aspect_ratios: Vec<(String, f64)>,
}

/// The placement of an instance relative to the accepted dock projection.
pub enum DockWidgetSlot {
    /// Standalone instances and layout-only callers use registry geometry.
    Registry,
    Hidden,
    Placed(Rect),
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct DockState {
    accepts: Vec<String>,
    capacity: usize,
    auto_clear_ms: NonZeroU64,
    rows: Vec<DockRow>,
    /// A restored scene must wait for the current runtime's actual display.
    #[serde(skip)]
    projection: Option<DockProjection>,
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
        self.set_dock_projection(
            &accepts,
            Some(DockProjection {
                target: None,
                layout,
                pixels_per_dp: 1.0,
                aspect_ratios: Vec::new(),
            }),
            auto_clear_ms,
        );
    }

    /// Update the runtime projection without dropping rows or retiming publications.
    pub fn set_dock_projection(
        &mut self,
        accepts: &[String],
        projection: Option<DockProjection>,
        auto_clear_ms: NonZeroU64,
    ) -> bool {
        let capacity = projection.as_ref().map_or(0, |p| p.layout.capacity);
        if let Some(dock) = &mut self.dock {
            if dock.accepts == accepts
                && dock.projection == projection
                && dock.auto_clear_ms == auto_clear_ms
            {
                return false;
            }
            dock.accepts = accepts.to_vec();
            dock.capacity = capacity;
            dock.auto_clear_ms = auto_clear_ms;
            dock.projection = projection;
        } else if projection.is_some() {
            self.dock = Some(DockState {
                accepts: accepts.to_vec(),
                capacity,
                auto_clear_ms,
                rows: Vec::new(),
                projection,
            });
        } else {
            return false;
        }
        self.version += 1;
        true
    }

    pub fn dock_projection(&self) -> Option<&DockProjection> {
        self.dock.as_ref()?.projection.as_ref()
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

    pub fn is_dock_widget(&self, instance_name: &str) -> bool {
        self.dock_rows()
            .iter()
            .flat_map(|row| &row.widgets)
            .any(|widget| widget.instance_name == instance_name)
    }

    pub fn dock_widget_slot(&self, instance_name: &str) -> DockWidgetSlot {
        use DockWidgetSlot::{Hidden, Placed, Registry};

        let Some((index, row, widget_index)) =
            self.dock_rows()
                .iter()
                .enumerate()
                .find_map(|(index, row)| {
                    row.widgets
                        .iter()
                        .position(|widget| widget.instance_name == instance_name)
                        .map(|widget_index| (index, row, widget_index))
                })
        else {
            return Registry;
        };
        let Some(projection) = self.dock_projection() else {
            return Hidden;
        };
        if index >= projection.layout.capacity {
            return Hidden;
        }
        if projection.target.is_none() {
            return Registry;
        }

        let aspect = |widget: &DockWidget| {
            projection
                .aspect_ratios
                .iter()
                .find(|(name, _)| *name == widget.widget_type)
                .map(|(_, ratio)| *ratio)
        };
        let Some(ratio) = aspect(&row.widgets[widget_index]) else {
            return Hidden;
        };
        // An absent committed predecessor would put this widget in the wrong
        // slot. Check before advancing across any of the row's widths.
        let preceding = &row.widgets[..widget_index];
        if preceding.iter().any(|widget| aspect(widget).is_none()) {
            return Hidden;
        }
        let slot = projection
            .layout
            .widget(index, preceding.iter().filter_map(aspect), ratio);
        if slot.width > 0.0
            && slot.height > 0.0
            && [slot.x, slot.y, slot.width, slot.height]
                .into_iter()
                .all(f32::is_finite)
        {
            Placed(slot)
        } else {
            Hidden
        }
    }

    /// A configured widget always uses its default artwork, even when its
    /// type has a loaded dock alternate.
    pub fn widget_dock_presentation(
        &self,
        instance: &WidgetInstance,
    ) -> Option<&Arc<DockPresentation>> {
        if !self.is_dock_widget(&instance.instance_name) {
            return None;
        }
        self.widget_registry
            .dock_presentation(&instance.widget_type_name)
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
        if row_index.is_none() && (dock.projection.is_none() || dock.rows.len() >= dock.capacity) {
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

        let extreme_aspect = f64::from(f32::MAX) / f64::from(f32::MIN_POSITIVE);
        let too_thin = DockLayout::new(monitor, 32, metrics(1.0), [extreme_aspect]);
        assert!(too_thin.column.width.is_finite());
        assert_eq!(too_thin.capacity, 0, "no representable row height");
    }
}
