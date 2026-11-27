//! Scene-owned dock instances and geometry for admission, presentation and pointer routing.

use std::collections::HashMap;
use std::num::NonZeroU64;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

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
    max_rows: usize,
    physical_capacity: usize,
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
        let physical_capacity = if metrics.row_height > 0.0 {
            ((column.height + metrics.gap) / pitch).floor() as usize
        } else {
            0
        };
        Self {
            display,
            column,
            capacity: max_rows.min(physical_capacity),
            scale: scale as f32,
            max_rows,
            physical_capacity,
            metrics,
        }
    }

    pub fn caption(&self, row: usize) -> Rect {
        self.caption_at(row as f32)
    }

    fn caption_at(&self, row: f32) -> Rect {
        Rect::new(
            self.column.x,
            self.column.y + row * (self.metrics.row_height + self.metrics.gap),
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
    #[serde(skip)]
    x: Option<f64>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct DockRow {
    /// Exact publication owner, including the session scope when present.
    pub namespace: String,
    /// Live instances, oldest first.
    pub widgets: Vec<DockWidget>,
    #[serde(skip)]
    slot: usize,
    #[serde(skip)]
    position: f32,
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DockPointer {
    Unknown = 0,
    Inside = 1,
    Outside = 2,
}

/// A certified passive column shared with one window-loop producer. The
/// contact and latest hold edge occupy one atomic word. Revoking a column
/// drops Scene's reference, so a late write can only update the old snapshot.
#[derive(Clone, Debug)]
pub struct DockPassiveColumn {
    pub target: String,
    pub display: Rect,
    pub column: Rect,
    contact: Arc<AtomicU64>,
}

impl DockPassiveColumn {
    pub fn observe(&self, pointer: DockPointer) -> bool {
        let old = self.contact.load(Ordering::Relaxed);
        let contact = pointer as u64;
        if old & 3 == contact {
            return false;
        }
        let hold = (old & !3)
            + if pointer == DockPointer::Outside {
                0
            } else {
                4
            };
        self.contact.store(hold | contact, Ordering::Relaxed);
        true
    }

    fn sample(&self) -> (bool, u64) {
        let state = self.contact.load(Ordering::Relaxed);
        (state & 3 == DockPointer::Outside as u64, state >> 2)
    }
}

#[derive(Clone, Copy, Debug)]
struct DockMotion {
    started_us: u64,
    duration_us: u64,
    ease: fn(f32) -> f32,
    progress: f32,
    hold_edge: u64,
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
    #[serde(skip)]
    pointer: Option<DockPassiveColumn>,
    #[serde(skip)]
    needs_compaction: bool,
    #[serde(skip)]
    motion: Option<DockMotion>,
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
        let gap = if row.widgets.is_empty() {
            self.rows.remove(row_index);
            row_index < self.rows.len()
        } else {
            widget_index < row.widgets.len()
        };
        if self.rows.is_empty() {
            self.reset_motion();
        } else if gap && self.projection.as_ref().is_some_and(|p| p.target.is_some()) {
            self.needs_compaction = true;
            self.motion = None;
        }
        true
    }

    fn reset_motion(&mut self) {
        self.needs_compaction = false;
        self.motion = None;
        self.pointer = None;
    }

    fn normalize(&mut self) {
        if self.projection.is_some() {
            self.compact(1.0, true);
        }
        self.reset_motion();
    }

    fn next_row_slot(&self) -> Option<usize> {
        let projection = self.projection.as_ref()?;
        if projection.target.is_none() {
            return (self.rows.len() < self.capacity).then_some(self.rows.len());
        }
        let layout = &projection.layout;
        let slot = self.rows.last().map_or(0, |row| row.slot + 1);
        (self.rows.len() < layout.max_rows && slot < layout.physical_capacity).then_some(slot)
    }

    fn append(
        &mut self,
        index: Option<usize>,
        slot: Option<usize>,
        namespace: &str,
        mut widget: DockWidget,
    ) {
        if let Some(index) = index {
            let row = &mut self.rows[index];
            let previous = row.widgets.last().expect("live row");
            widget.x = self.projection.as_ref().and_then(|projection| {
                previous
                    .x
                    .and_then(|x| projection.after(x, &previous.widget_type))
                    .filter(|x| projection.fits(*x, &widget.widget_type))
            });
            row.widgets.push(widget);
        } else {
            let slot = slot.expect("admitted dock row");
            let projection = self.projection.as_ref().expect("admitted dock projection");
            let first = projection.first_widget_x();
            widget.x = projection.fits(first, &widget.widget_type).then_some(first);
            self.rows.push(DockRow {
                namespace: namespace.into(),
                widgets: vec![widget],
                slot,
                position: slot as f32,
            });
        }
    }

    fn has_gap(&self) -> bool {
        let Some(projection) = &self.projection else {
            return false;
        };
        self.rows.iter().enumerate().any(|(index, row)| {
            if row.slot != index || row.position != index as f32 {
                return true;
            }
            let mut x = Some(projection.first_widget_x());
            row.widgets.iter().any(|widget| {
                let goal = x.filter(|x| projection.fits(*x, &widget.widget_type));
                x = goal.and_then(|x| projection.after(x, &widget.widget_type));
                widget.x != goal
            })
        })
    }

    fn compact(&mut self, fraction: f32, terminal: bool) -> bool {
        let projection = self.projection.as_ref().expect("projected dock");
        let mut changed = false;
        for (index, row) in self.rows.iter_mut().enumerate() {
            let position = if terminal {
                row.slot = index;
                index as f32
            } else {
                row.position + (index as f32 - row.position) * fraction
            };
            changed |= row.position != position;
            row.position = position;
            let mut x = Some(projection.first_widget_x());
            for widget in &mut row.widgets {
                let goal = x.filter(|x| projection.fits(*x, &widget.widget_type));
                let position = if terminal {
                    goal
                } else {
                    widget
                        .x
                        .zip(goal)
                        .map(|(from, to)| from + (to - from) * f64::from(fraction))
                };
                changed |= widget.x != position;
                widget.x = position;
                x = goal.and_then(|x| projection.after(x, &widget.widget_type));
            }
        }
        changed
    }
}

impl DockProjection {
    fn first_widget_x(&self) -> f64 {
        f64::from(
            self.layout.column.x + self.layout.metrics.caption_width + self.layout.metrics.gap,
        )
    }

    fn widget_width(&self, widget_type: &str) -> Option<f64> {
        self.aspect_ratios
            .iter()
            .find(|(name, _)| name == widget_type)
            .map(|(_, ratio)| f64::from(self.layout.metrics.row_height) * *ratio)
            .filter(|width| *width > 0.0 && width.is_finite())
    }

    fn after(&self, x: f64, widget_type: &str) -> Option<f64> {
        Some(x + self.widget_width(widget_type)? + f64::from(self.layout.metrics.gap))
    }

    fn fits(&self, x: f64, widget_type: &str) -> bool {
        self.widget_width(widget_type).is_some_and(|width| {
            let aperture = f64::from(self.layout.column.width);
            let used = x - self.first_widget_x()
                + width
                + f64::from(self.layout.metrics.caption_width)
                + f64::from(self.layout.metrics.gap);
            // The fitted aperture and metrics are f32. Accommodate rounding
            // at their shared edge without admitting another physical item.
            used <= aperture + aperture * f64::from(2.0 * f32::EPSILON)
        })
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
            let placement_changed = match (&dock.projection, &projection) {
                (Some(old), Some(new)) => {
                    old.target != new.target
                        || old.layout.display != new.layout.display
                        || old.layout.column != new.layout.column
                        || old.layout.metrics != new.layout.metrics
                        || old.aspect_ratios != new.aspect_ratios
                }
                _ => true,
            };
            dock.accepts = accepts.to_vec();
            dock.capacity = capacity;
            dock.auto_clear_ms = auto_clear_ms;
            dock.projection = projection;
            if placement_changed {
                dock.normalize();
            }
        } else if projection.is_some() {
            self.dock = Some(DockState {
                accepts: accepts.to_vec(),
                capacity,
                auto_clear_ms,
                rows: Vec::new(),
                projection,
                pointer: None,
                needs_compaction: false,
                motion: None,
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
        if !Self::dock_row_fits(projection, index, row) {
            return Hidden;
        }
        if projection.target.is_none() {
            return Registry;
        }
        let widget = &row.widgets[widget_index];
        let Some(x) = widget.x else {
            return Hidden;
        };
        let Some(width) = projection.widget_width(&widget.widget_type) else {
            return Hidden;
        };
        let caption = projection.layout.caption_at(row.position);
        Placed(Rect::new(x as f32, caption.y, width as f32, caption.height))
    }

    fn dock_row_fits(projection: &DockProjection, index: usize, row: &DockRow) -> bool {
        index < projection.layout.capacity
            && (projection.target.is_none() || row.slot < projection.layout.physical_capacity)
    }

    pub fn dock_caption_rect(&self, index: usize) -> Option<Rect> {
        let projection = self.dock_projection()?;
        let row = self.dock_rows().get(index)?;
        Self::dock_row_fits(projection, index, row)
            .then(|| projection.layout.caption_at(row.position))
    }

    /// The compositor certifies the exact display used by dock drawing.
    /// This column is passive and is never added to the hit/capture regions.
    pub fn certify_dock_passive_column(&mut self, target: &str, display: Rect) {
        if !self.widgets_shown() {
            return;
        }
        let Some(dock) = &mut self.dock else { return };
        let Some(projection) = &dock.projection else {
            return;
        };
        if dock.pointer.is_none()
            && !dock.rows.is_empty()
            && projection.target.as_deref() == Some(target)
            && projection.layout.display == display
            && projection.layout.capacity > 0
        {
            dock.pointer = Some(DockPassiveColumn {
                target: target.into(),
                display,
                column: projection.layout.column,
                contact: Arc::default(),
            });
        }
    }

    pub fn dock_passive_column(&self) -> Option<&DockPassiveColumn> {
        self.dock.as_ref()?.pointer.as_ref()
    }

    pub(crate) fn revoke_dock_passive_column(&mut self) {
        if let Some(dock) = &mut self.dock {
            dock.pointer = None;
            dock.motion = None;
        }
    }

    /// Advance the shared draw pose once, before the frame gate. A windowed
    /// surface without a current observation holds the last committed pose.
    pub fn advance_dock_compaction(
        &mut self,
        windowed: bool,
        duration_us: u64,
        ease: fn(f32) -> f32,
    ) -> bool {
        if !self.widgets_shown() || !self.dock.as_ref().is_some_and(|dock| dock.needs_compaction) {
            return false;
        }
        let now = self.now_monotonic_us();
        let dock = self.dock.as_mut().expect("dock compaction pending");
        let (outside, hold_edge) = if windowed {
            dock.pointer
                .as_ref()
                .map_or((false, 0), DockPassiveColumn::sample)
        } else {
            (true, 0)
        };
        if !outside {
            dock.motion = None;
            return false;
        }
        if dock
            .motion
            .is_some_and(|motion| motion.hold_edge != hold_edge)
        {
            dock.motion = None;
        }
        if dock.motion.is_none() && !dock.has_gap() {
            dock.needs_compaction = false;
            return false;
        }
        let motion = dock.motion.get_or_insert(DockMotion {
            started_us: now,
            duration_us,
            ease,
            progress: 0.0,
            hold_edge,
        });
        let elapsed = now.saturating_sub(motion.started_us);
        let terminal = elapsed >= motion.duration_us || duration_us == 0;
        let fraction = if terminal {
            1.0
        } else if motion.progress >= 1.0 {
            // An eased f32 can round to one before the clock reaches the end.
            // Preserve the terminal geometry acknowledgement on a later turn.
            0.0
        } else {
            let progress = (motion.ease)(elapsed as f32 / motion.duration_us as f32);
            let fraction = (progress - motion.progress) / (1.0 - motion.progress);
            motion.progress = progress;
            fraction
        };
        let changed = dock.compact(fraction, terminal);
        if terminal {
            dock.motion = None;
            dock.needs_compaction = false;
        }
        if changed || terminal {
            self.bump_geometry_epoch();
        }
        !terminal
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
        let row_slot = dock.next_row_slot();
        if row_index.is_none() && row_slot.is_none() {
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
            x: None,
        });
        self.commit_widget_publication(instance, &definition, record)?;
        if let Some(widget) = new_widget {
            self.dock
                .as_mut()
                .expect("configured dock")
                .append(row_index, row_slot, namespace, widget);
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
