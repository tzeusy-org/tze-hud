//! Passive authored dock hover writes follow actual presented widget targets.

use std::collections::HashMap;
use std::sync::{Arc, atomic::Ordering};
use std::time::{Duration, Instant};

use tze_hud_compositor::renderer::{DockHoverFrame, DockRuntimeHoverTarget, PresentedDockHover};
use tze_hud_scene::dock::{DockPassiveColumn, DockWidgetSlot};
use tze_hud_scene::{LeaseState, SceneGraph, SceneId, WidgetParamAudience, WidgetParameterValue};

use crate::pipeline::HitTestSnapshot;

/// The latest pointer point over one passive column attachment, shared by
/// the dock tooltip and authored-hover visits. A new attachment replaces it.
#[derive(Default)]
pub(super) struct PassiveSample(Option<(DockPassiveColumn, (f32, f32))>);

impl PassiveSample {
    pub(super) fn remember(
        &mut self,
        column: Option<&DockPassiveColumn>,
        point: Option<(f32, f32)>,
    ) {
        let Some((column, point)) = column.zip(point) else {
            self.0 = None;
            return;
        };
        match &mut self.0 {
            Some((previous, old_point)) if previous.shares_attachment(column) => *old_point = point,
            _ => self.0 = Some((column.clone(), point)),
        }
    }

    pub(super) fn get(&self) -> Option<&(DockPassiveColumn, (f32, f32))> {
        self.0.as_ref()
    }
}

#[derive(Clone)]
struct Target {
    frame: Arc<DockHoverFrame>,
    index: usize,
}

impl Target {
    fn data(&self) -> &DockRuntimeHoverTarget {
        &self.frame.runtime_targets[self.index]
    }

    fn same_visit(&self, other: &Self) -> bool {
        self.frame.column.shares_attachment(&other.frame.column)
            && same_visit(self.data(), other.data())
    }

    fn represented_by(&self, proof: &PresentedDockHover, pixels: bool) -> bool {
        proof.surface_epoch.load(Ordering::Acquire)
            && self.frame.column.shares_attachment(&proof.frame.column)
            && proof.frame.runtime_targets.iter().any(|painted| {
                same_visit(self.data(), painted)
                    && (!pixels || self.data().pixels == painted.pixels)
            })
    }
}

fn same_visit(a: &DockRuntimeHoverTarget, b: &DockRuntimeHoverTarget) -> bool {
    a.instance == b.instance
        && a.instance_name == b.instance_name
        && a.scene_bounds == b.scene_bounds
        && a.bounds == b.bounds
        && a.trigger == b.trigger
        && a.behavior == b.behavior
}

struct Visit {
    target: Target,
    epoch: Arc<std::sync::atomic::AtomicBool>,
    due: Instant,
    exact: bool,
    activated: bool,
}

struct LocalWrite {
    target: Target,
    desired: bool,
    applied: bool,
    epoch: Arc<std::sync::atomic::AtomicBool>,
}

/// Desired writes survive contention independently of what was actually applied.
#[derive(Default)]
pub(super) struct DockParamHover {
    sample: PassiveSample,
    visit: Option<Visit>,
    writes: HashMap<(SceneId, String), LocalWrite>,
    pub(super) deadline_changed: bool,
}

impl DockParamHover {
    pub(super) fn remember(
        &mut self,
        column: Option<&DockPassiveColumn>,
        point: Option<(f32, f32)>,
    ) {
        self.sample.remember(column, point);
    }

    pub(super) fn reconcile(
        &mut self,
        snapshot: &HitTestSnapshot,
        presented: Option<&PresentedDockHover>,
        now: Instant,
    ) {
        let candidate = self.sample.get().and_then(|(column, (x, y))| {
            let frame = snapshot.dock_hover.as_ref()?;
            if !column.shares_attachment(&frame.column) {
                return None;
            }
            let index = frame
                .runtime_targets
                .iter()
                .position(|target| target.trigger.contains_point(*x, *y))?;
            Some(Target {
                frame: frame.clone(),
                index,
            })
        });
        if let Some(target) = candidate
            && let Some(proof) =
                presented.filter(|proof| proof.surface_epoch.load(Ordering::Acquire))
        {
            let exact = target.represented_by(proof, true);
            if let Some(visit) = &mut self.visit
                && visit.target.same_visit(&target)
                && Arc::ptr_eq(&visit.epoch, &proof.surface_epoch)
                && (exact || visit.target.represented_by(proof, false))
            {
                self.deadline_changed |= visit.exact != exact;
                visit.exact = exact;
                visit.target = target.clone();
                let key = (
                    target.data().instance,
                    target.data().behavior.visibility_param.clone(),
                );
                if let Some(write) = self.writes.get_mut(&key) {
                    write.target = target;
                }
                return;
            }
            if exact {
                self.clear_visit();
                self.deadline_changed = true;
                let due = now + Duration::from_millis(u64::from(target.data().behavior.delay_ms));
                self.visit = Some(Visit {
                    target,
                    epoch: proof.surface_epoch.clone(),
                    due,
                    exact,
                    activated: false,
                });
                return;
            }
        }
        self.clear_visit();
    }

    fn clear_visit(&mut self) {
        self.deadline_changed |= self.visit.take().is_some();
        for write in self.writes.values_mut() {
            self.deadline_changed |= write.desired;
            write.desired = false;
        }
        // An unapplied positive is cancelled, not replayed after the pointer left.
        self.writes.retain(|_, write| write.applied);
    }

    pub(super) fn next_due(&self) -> Option<Instant> {
        self.visit
            .as_ref()
            .filter(|visit| visit.exact && !visit.activated)
            .map(|visit| visit.due)
    }

    /// Re-sample after presentation for both a newly due transition and a
    /// retained positive that can finally commit on this availability turn.
    pub(super) fn needs_positive_sample(&self, now: Instant) -> bool {
        self.next_due().is_some_and(|due| now >= due)
            || (self.visit.as_ref().is_some_and(|visit| visit.exact)
                && self
                    .writes
                    .values()
                    .any(|write| write.desired && !write.applied))
    }

    pub(super) fn has_debt(&self) -> bool {
        self.writes.values().any(|write| {
            write.desired != write.applied
                && (!write.desired || self.visit.as_ref().is_some_and(|visit| visit.exact))
        })
    }

    pub(super) fn expire_dwell(&mut self, now: Instant) {
        if self.next_due().is_none_or(|due| now < due) {
            return;
        }
        let visit = self.visit.as_mut().expect("checked by next_due");
        let target = visit.target.clone();
        let key = (
            target.data().instance,
            target.data().behavior.visibility_param.clone(),
        );
        self.writes
            .entry(key)
            .and_modify(|write| {
                write.target = target.clone();
                write.epoch = visit.epoch.clone();
                write.desired = true;
            })
            .or_insert(LocalWrite {
                target,
                desired: true,
                applied: false,
                epoch: visit.epoch.clone(),
            });
        // Contention is retried by availability, never an already-expired timer.
        visit.activated = true;
    }

    fn apply(&mut self, scene: &mut SceneGraph) -> bool {
        let mut changed = false;
        let mut rejected = false;
        let visit = self.visit.as_ref();
        self.writes.retain(|_, write| {
            let target = write.target.data();
            let Some(instance) = scene.widget_registry.instances.get(&target.instance_name) else {
                return false;
            };
            let Some(def) = scene
                .widget_registry
                .get_definition(&instance.widget_type_name)
            else {
                return false;
            };
            let runtime_param = def.parameter_schema.iter().any(|param| {
                param.name == target.behavior.visibility_param
                    && param.audience == WidgetParamAudience::Runtime
            });
            if instance.id != target.instance
                || !scene.is_dock_widget(&target.instance_name)
                || !runtime_param
            {
                return false;
            }
            // A successful pending repaint preserves dwell but does not grant an
            // unapplied positive until its exact pixels actually present.
            if write.desired && !write.applied && visit.is_some_and(|visit| !visit.exact) {
                return true;
            }
            if write.desired
                && (!write.epoch.load(Ordering::Acquire) || !positive_allowed(scene, &write.target))
            {
                rejected = true;
                write.desired = false;
            }
            if write.desired != write.applied {
                let value = WidgetParameterValue::F32(if write.desired {
                    target.behavior.visible_value
                } else {
                    target.behavior.hidden_value
                });
                if instance
                    .current_params
                    .get(&target.behavior.visibility_param)
                    != Some(&value)
                {
                    if let Err(error) = scene.set_widget_param_local(
                        &target.instance_name,
                        &target.behavior.visibility_param,
                        value,
                    ) {
                        tracing::debug!(
                            %error, widget = target.instance_name,
                            "dock hover local write rejected"
                        );
                        rejected = true;
                        return false;
                    }
                    changed = true;
                }
                write.applied = write.desired;
            }
            write.desired || write.applied
        });
        if rejected {
            self.clear_visit();
        }
        changed
    }
}

fn positive_allowed(scene: &SceneGraph, target: &Target) -> bool {
    let data = target.data();
    let Some(instance) = scene.widget_registry.instances.get(&data.instance_name) else {
        return false;
    };
    let DockWidgetSlot::Placed(scene_bounds) = scene.dock_widget_slot(&data.instance_name) else {
        return false;
    };
    if !scene.widgets_shown()
        || scene_bounds != data.scene_bounds
        || scene
            .dock_passive_column()
            .is_none_or(|column| !column.shares_attachment(&target.frame.column))
    {
        return false;
    }
    let presentation = scene.widget_dock_presentation(instance);
    let hover = match presentation {
        Some(presentation) if !presentation.uses_builtin_countdown_layout() => {
            presentation.hover_behavior.as_ref()
        }
        Some(_) => None,
        None => scene
            .widget_registry
            .get_definition(&instance.widget_type_name)
            .and_then(|def| def.hover_behavior.as_ref()),
    };
    if hover != Some(&data.behavior) {
        return false;
    }
    let now = scene.now_wall_us();
    scene
        .dock_rows()
        .iter()
        .find(|row| {
            row.widgets
                .iter()
                .any(|widget| widget.instance_name == data.instance_name)
        })
        .is_some_and(|row| {
            scene
                .widget_registry
                .active_publishes
                .get(&data.instance_name)
                .into_iter()
                .flatten()
                .any(|record| {
                    record.publisher_namespace == row.namespace
                        && scene
                            .widget_expiry(record)
                            .is_none_or(|expiry| expiry > now)
                        && record.lease_id.is_none_or(|id| {
                            scene.leases.get(&id).is_some_and(|lease| {
                                lease.namespace == row.namespace
                                    && lease.state == LeaseState::Active
                                    && lease.remaining_ms(now / 1000) > 0
                            })
                        })
                })
        })
}

impl super::WinitApp {
    pub(super) fn reconcile_dock_param_hover(&mut self, snapshot: &HitTestSnapshot, now: Instant) {
        let presented = snapshot
            .dock_hover
            .as_ref()
            .and_then(|frame| self.presented_dock_hover(&frame.column));
        self.state
            .dock_param_hover
            .reconcile(snapshot, presented.as_deref(), now);
    }

    pub(super) fn settle_dock_param_hover(&mut self, now: Instant) -> bool {
        let snapshot = self.state.pipeline.hit_test_snapshot.load_full();
        self.reconcile_dock_param_hover(&snapshot, now);
        let contact_changed = if self.state.dock_param_hover.needs_positive_sample(now) {
            // Present may have blocked after the ordinary cursor refresh. Its
            // newly observed Outside edge also owes passive compaction work.
            #[cfg(target_os = "windows")]
            let changed = self.refresh_dock_cursor_for_activation();
            #[cfg(not(target_os = "windows"))]
            let changed = false;
            self.state.dock_param_hover.expire_dwell(now);
            changed
        } else {
            false
        };
        let changed = self.apply_dock_param_hover();
        self.state.dock_param_hover.deadline_changed = false;
        changed || contact_changed
    }

    pub(super) fn apply_dock_param_hover(&mut self) -> bool {
        if self.state.dock_param_hover.writes.is_empty() {
            return false;
        }
        let shared = self.state.shared_state.clone();
        let Ok(state) = shared.try_lock() else {
            return false;
        };
        let Ok(mut scene) = state.scene.try_lock() else {
            return false;
        };
        self.state.dock_param_hover.apply(&mut scene)
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;
    use tze_hud_scene::{
        Rect, TestClock,
        dock::{DockLayout, DockMetrics, DockProjection},
    };

    pub(in crate::windowed) fn fixture() -> (SceneGraph, Arc<TestClock>, Arc<DockHoverFrame>) {
        let clock = Arc::new(TestClock::new(1_000));
        let mut scene = SceneGraph::new_with_clock(1280.0, 720.0, clock.clone());
        crate::scene_startup::run_scene_startup(
            &Default::default(),
            None,
            &mut scene,
            Default::default(),
        )
        .unwrap();
        let display = Rect::new(0.0, 0.0, 1280.0, 720.0);
        scene.set_dock_projection(
            &["progress-bar".into()],
            Some(DockProjection {
                target: Some("SELECTED".into()),
                layout: DockLayout::new(
                    display,
                    2,
                    DockMetrics {
                        row_height: 52.0,
                        caption_width: 96.0,
                        gap: 8.0,
                    },
                    [6.4],
                ),
                pixels_per_dp: 1.0,
                aspect_ratios: vec![("progress-bar".into(), 6.4)],
            }),
            std::num::NonZeroU64::new(5_000).unwrap(),
        );
        for owner in ["a", "b"] {
            scene
                .publish_to_dock("progress-bar", Default::default(), owner, None, 0, 0)
                .unwrap();
        }
        scene.certify_dock_passive_column("SELECTED", display);
        let runtime_targets = ["a", "b"]
            .into_iter()
            .map(|owner| {
                let instance = scene.dock_instance(owner, "progress-bar").unwrap();
                let DockWidgetSlot::Placed(bounds) =
                    scene.dock_widget_slot(&instance.instance_name)
                else {
                    panic!("placed")
                };
                DockRuntimeHoverTarget {
                    instance: instance.id,
                    instance_name: instance.instance_name.clone(),
                    scene_bounds: bounds,
                    bounds,
                    trigger: bounds,
                    behavior: scene
                        .widget_registry
                        .get_definition("progress-bar")
                        .unwrap()
                        .hover_behavior
                        .clone()
                        .unwrap(),
                    pixels: [1; 32],
                }
            })
            .collect();
        let frame = Arc::new(DockHoverFrame {
            column: scene.dock_passive_column().unwrap().clone(),
            cards: Vec::new(),
            runtime_targets,
        });
        (scene, clock, frame)
    }

    pub(in crate::windowed) fn snapshot(frame: &Arc<DockHoverFrame>) -> HitTestSnapshot {
        HitTestSnapshot {
            dock_hover: Some(frame.clone()),
            dock_passive_column: Some(frame.column.clone()),
            ..HitTestSnapshot::empty()
        }
    }

    pub(in crate::windowed) fn proof(frame: &Arc<DockHoverFrame>) -> PresentedDockHover {
        PresentedDockHover {
            frame: frame.clone(),
            surface_epoch: Arc::new(AtomicBool::new(true)),
        }
    }

    fn repaint(frame: &Arc<DockHoverFrame>, byte: u8) -> Arc<DockHoverFrame> {
        Arc::new(DockHoverFrame {
            column: frame.column.clone(),
            cards: Vec::new(),
            runtime_targets: frame
                .runtime_targets
                .iter()
                .map(|target| DockRuntimeHoverTarget {
                    instance: target.instance,
                    instance_name: target.instance_name.clone(),
                    scene_bounds: target.scene_bounds,
                    bounds: target.bounds,
                    trigger: target.trigger,
                    behavior: target.behavior.clone(),
                    pixels: [byte; 32],
                })
                .collect(),
        })
    }

    #[test]
    fn current_presented_dwell_and_pending_repaint_keep_distinct_write_debt() {
        let (mut scene, _, frame) = fixture();
        let mut hover = DockParamHover::default();
        let now = Instant::now();
        let card = &frame.runtime_targets[0];
        hover.remember(
            Some(&frame.column),
            Some((card.bounds.x + 1.0, card.bounds.y + 1.0)),
        );
        hover.reconcile(&snapshot(&frame), None, now);
        assert!(hover.next_due().is_none(), "built-only target cannot arm");
        let mut presented = proof(&frame);
        hover.reconcile(&snapshot(&frame), Some(&presented), now);
        let due = hover.next_due().unwrap();
        hover.expire_dwell(due - Duration::from_nanos(1));
        assert!(!hover.has_debt());
        hover.expire_dwell(due);
        assert!(hover.has_debt());
        assert!(
            hover.next_due().is_none(),
            "expired write has no past timer"
        );
        let changed = repaint(&frame, 2);
        hover.reconcile(&snapshot(&changed), Some(&presented), due);
        assert!(
            !hover.has_debt(),
            "unpresented new raster waits on ACK, not lock retry"
        );
        assert!(!hover.apply(&mut scene));
        assert_eq!(
            scene.widget_registry.instances[&card.instance_name].current_params["tooltip_visible"],
            WidgetParameterValue::F32(0.0)
        );
        presented.frame = changed.clone();
        hover.reconcile(&snapshot(&changed), Some(&presented), due);
        assert!(hover.has_debt());
        let publications = scene.widget_registry.active_publishes.clone();
        let leases = scene.leases.clone();
        assert!(hover.apply(&mut scene));
        assert!(!hover.has_debt());
        assert_eq!(
            scene.widget_registry.instances[&card.instance_name].current_params["tooltip_visible"],
            WidgetParameterValue::F32(1.0)
        );
        assert_eq!(scene.widget_registry.active_publishes, publications);
        assert_eq!(scene.leases, leases);
        // The positive's own pixels and a successful equivalent restyle do not rearm.
        let own_repaint = repaint(&frame, 3);
        presented.frame = own_repaint.clone();
        hover.reconcile(
            &snapshot(&own_repaint),
            Some(&presented),
            due + Duration::from_secs(1),
        );
        assert!(hover.next_due().is_none());
        let version = scene.version;
        assert!(!hover.apply(&mut scene));
        assert_eq!(scene.version, version);
        hover.reconcile(&HitTestSnapshot::empty(), Some(&presented), due);
        assert!(
            hover.has_debt(),
            "denied or absent current target owes hide"
        );
        assert!(hover.apply(&mut scene));
        assert!(hover.writes.is_empty());
        assert_eq!(
            scene.widget_registry.instances[&card.instance_name].current_params["tooltip_visible"],
            WidgetParameterValue::F32(0.0)
        );
    }

    #[test]
    fn real_compaction_rejects_old_same_attachment_pose_then_accepts_current_pose() {
        let (mut scene, _, frame) = fixture();
        let old = &frame.runtime_targets[1];
        let presented = proof(&frame);
        let mut hover = DockParamHover::default();
        let now = Instant::now();
        hover.remember(
            Some(&frame.column),
            Some((old.bounds.x + 1.0, old.bounds.y + 1.0)),
        );
        hover.reconcile(&snapshot(&frame), Some(&presented), now);
        hover.expire_dwell(hover.next_due().unwrap());
        scene.clear_dock_widget("a", "progress-bar");
        frame
            .column
            .observe(tze_hud_scene::dock::DockPointer::Outside);
        let geometry = scene.geometry_epoch;
        scene.advance_dock_compaction(true, 0, |p| p);
        let DockWidgetSlot::Placed(moved) = scene.dock_widget_slot(&old.instance_name) else {
            panic!("surviving slot")
        };
        assert_ne!(moved, old.scene_bounds);
        assert!(scene.geometry_epoch > geometry);
        assert!(
            scene
                .dock_passive_column()
                .unwrap()
                .shares_attachment(&frame.column)
        );
        let publications = scene.widget_registry.active_publishes.clone();
        assert!(
            !hover.apply(&mut scene),
            "old exact frame/proof cannot authorize the moved Scene pose"
        );
        assert!(!hover.has_debt());
        assert_eq!(
            scene.widget_registry.instances[&old.instance_name].current_params["tooltip_visible"],
            WidgetParameterValue::F32(0.0)
        );
        // CPU presentation is injected from the actual new Scene slot. The
        // strict shipped raster companion checks that the collector carries it.
        let fresh = Arc::new(DockHoverFrame {
            column: frame.column.clone(),
            cards: Vec::new(),
            runtime_targets: vec![DockRuntimeHoverTarget {
                instance: old.instance,
                instance_name: old.instance_name.clone(),
                scene_bounds: moved,
                bounds: moved,
                trigger: moved,
                behavior: old.behavior.clone(),
                pixels: old.pixels,
            }],
        });
        let fresh_proof = PresentedDockHover {
            frame: fresh.clone(),
            surface_epoch: presented.surface_epoch.clone(),
        };
        hover.remember(Some(&fresh.column), Some((moved.x + 1.0, moved.y + 1.0)));
        hover.reconcile(
            &snapshot(&fresh),
            Some(&fresh_proof),
            now + Duration::from_secs(1),
        );
        hover.expire_dwell(hover.next_due().unwrap());
        assert!(hover.apply(&mut scene));
        assert_eq!(
            scene.widget_registry.instances[&old.instance_name].current_params["tooltip_visible"],
            WidgetParameterValue::F32(1.0)
        );
        assert_eq!(scene.widget_registry.active_publishes, publications);
    }

    #[test]
    fn positive_rechecks_live_identity_expiry_capacity_and_epoch_without_renewal() {
        for fault in [
            "expiry",
            "lease",
            "lease_expiry",
            "ownership",
            "cap",
            "hidden",
            "park",
            "source",
            "attachment",
            "clear",
            "epoch",
            "replacement",
        ] {
            let (mut scene, clock, frame) = fixture();
            let card = &frame.runtime_targets[0];
            let mut hover = DockParamHover::default();
            let presented = proof(&frame);
            let now = Instant::now();
            hover.remember(
                Some(&frame.column),
                Some((card.bounds.x + 1.0, card.bounds.y + 1.0)),
            );
            hover.reconcile(&snapshot(&frame), Some(&presented), now);
            hover.expire_dwell(hover.next_due().unwrap());
            match fault {
                "expiry" => clock.advance(5_001),
                "lease" => {
                    scene
                        .widget_registry
                        .active_publishes
                        .get_mut(&card.instance_name)
                        .unwrap()[0]
                        .lease_id = Some(SceneId::new())
                }
                "lease_expiry" => {
                    let id = scene.grant_lease("a", 1);
                    scene
                        .widget_registry
                        .active_publishes
                        .get_mut(&card.instance_name)
                        .unwrap()[0]
                        .lease_id = Some(id);
                    clock.advance(2);
                }
                "ownership" => {
                    scene
                        .widget_registry
                        .active_publishes
                        .get_mut(&card.instance_name)
                        .unwrap()[0]
                        .publisher_namespace = "intruder".into();
                }
                "park" => {
                    scene.set_dock_projection(
                        &["progress-bar".into()],
                        None,
                        std::num::NonZeroU64::new(5_000).unwrap(),
                    );
                }
                "source" => {
                    scene
                        .widget_registry
                        .definitions
                        .get_mut("progress-bar")
                        .unwrap()
                        .hover_behavior
                        .as_mut()
                        .unwrap()
                        .delay_ms += 1;
                }
                "attachment" => {
                    scene.set_owner_hidden(tze_hud_scene::graph::visibility::SurfaceClasses {
                        widgets: true,
                        ..Default::default()
                    });
                    scene.set_owner_hidden(Default::default());
                    scene.certify_dock_passive_column("SELECTED", frame.column.display);
                }
                "clear" => scene.clear_dock_widget("a", "progress-bar"),
                "cap" => {
                    let mut projection = scene.dock_projection().unwrap().clone();
                    projection.layout.capacity = 0;
                    scene.set_dock_projection(
                        &["progress-bar".into()],
                        Some(projection),
                        std::num::NonZeroU64::new(5_000).unwrap(),
                    );
                }
                "hidden" => {
                    scene.set_owner_hidden(tze_hud_scene::graph::visibility::SurfaceClasses {
                        widgets: true,
                        ..Default::default()
                    });
                }
                "epoch" => presented.surface_epoch.store(false, Ordering::Release),
                "replacement" => {
                    scene
                        .widget_registry
                        .instances
                        .get_mut(&card.instance_name)
                        .unwrap()
                        .id = SceneId::new();
                }
                _ => unreachable!(),
            }
            let publications = scene.widget_registry.active_publishes.clone();
            let leases = scene.leases.clone();
            assert!(!hover.apply(&mut scene), "{fault}");
            assert!(
                !hover.has_debt(),
                "{fault}: invalid positive cannot busy retry"
            );
            assert_eq!(
                scene.widget_registry.active_publishes, publications,
                "{fault}"
            );
            assert_eq!(scene.leases, leases, "{fault}");
            if let Some(instance) = scene.widget_registry.instances.get(&card.instance_name) {
                assert_eq!(
                    instance.current_params["tooltip_visible"],
                    WidgetParameterValue::F32(0.0),
                    "{fault}"
                );
            } else {
                assert_eq!(fault, "clear");
            }
        }
    }
}
