//! Passive tooltip dwell follows the latest drawable and actually presented card.

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::Instant;

use tze_hud_compositor::renderer::{
    DockHoverCard, DockTooltipHandle, DockTooltipRequest, PresentedDockHover,
};
use tze_hud_scene::{Rect, dock::DockPassiveColumn};

use crate::pipeline::HitTestSnapshot;

#[derive(Default)]
pub(super) struct DockTooltipVisit {
    sample: Option<(DockPassiveColumn, (f32, f32))>,
    visit: Option<Visit>,
    pub(super) deadline_changed: bool,
    repaint_owed: bool,
    pub(super) output: DockTooltipHandle,
}

struct Visit {
    column: DockPassiveColumn,
    /// Last exact confirmed card. Keep it while the next raster is pending.
    card: Arc<DockHoverCard>,
    surface_epoch: Arc<AtomicBool>,
    exact: bool,
    due: Option<Instant>,
}

impl DockTooltipVisit {
    pub(super) fn remember(
        &mut self,
        column: Option<&DockPassiveColumn>,
        point: Option<(f32, f32)>,
    ) {
        let Some((column, point)) = column.zip(point) else {
            self.sample = None;
            return;
        };
        if let Some((previous, old_point)) = &mut self.sample
            && previous.shares_attachment(column)
        {
            *old_point = point;
        } else {
            self.sample = Some((column.clone(), point));
        }
    }

    pub(super) fn reconcile(
        &mut self,
        snapshot: &HitTestSnapshot,
        presented: Option<&PresentedDockHover>,
        now: Instant,
    ) {
        let candidate = self.sample.as_ref().and_then(|(sampled, (x, y))| {
            let current = snapshot.dock_hover.as_deref()?;
            let proof = presented.filter(|proof| proof.surface_epoch.load(Ordering::Acquire))?;
            if !sampled.shares_attachment(&current.column) {
                return None;
            }
            let card = current
                .cards
                .iter()
                .find(|card| card.bounds.contains_point(*x, *y))?;
            Some((&current.column, card, proof))
        });
        if let Some((column, card, proof)) = candidate {
            let exact = proof.frame.presented_equivalent(column, card);
            if let Some(active) = &mut self.visit
                && active.column.shares_attachment(column)
                && Arc::ptr_eq(&active.surface_epoch, &proof.surface_epoch)
                && active.card.same_visit(card)
                && (exact || proof.frame.retains_visit(column, &active.card))
            {
                self.deadline_changed |=
                    active.exact != exact || (exact && !active.card.same_pixels(card));
                active.exact = exact;
                if exact && !active.card.same_pixels(card) {
                    active.card = card.clone();
                }
                return;
            }
            if exact {
                self.clear_visit();
                self.deadline_changed = true;
                self.visit = Some(Visit {
                    column: column.clone(),
                    card: card.clone(),
                    surface_epoch: proof.surface_epoch.clone(),
                    exact: true,
                    due: Some(now + card.delay),
                });
                return;
            }
        }
        self.clear_visit();
    }

    fn clear_visit(&mut self) {
        if self.visit.take().is_none() {
            return;
        }
        self.deadline_changed = true;
        if self.output.load().is_some() {
            self.output.store(None);
            self.repaint_owed = true;
        }
    }

    pub(super) fn next_due(&self) -> Option<Instant> {
        let visit = self.visit.as_ref().filter(|visit| visit.exact)?;
        let due = visit.due?;
        if self
            .output
            .load()
            .as_ref()
            .is_some_and(|request| request.is_presented() || request.card.same_pixels(&visit.card))
        {
            return None;
        }
        Some(due)
    }

    fn expire_dwell(&mut self, now: Instant, work_area: Option<Rect>) -> bool {
        if self.next_due().is_none_or(|due| now < due) {
            return false;
        }
        let visit = self.visit.as_mut().expect("checked by next_due");
        let Some(work_area) = work_area else {
            visit.due = None;
            if self.output.load().is_some() {
                self.output.store(None);
                return true;
            }
            return false;
        };
        // The deadline stays with the visit. An unshown request can be
        // reissued against new exact pixels without another 180 ms wait.
        self.output.store(Some(Arc::new(DockTooltipRequest::new(
            visit.column.clone(),
            visit.card.clone(),
            visit.surface_epoch.clone(),
            work_area,
        ))));
        true
    }
}

impl super::WinitApp {
    pub(super) fn reconcile_dock_tooltip(&mut self, snapshot: &HitTestSnapshot, now: Instant) {
        let presented = snapshot.dock_hover.as_ref().and_then(|hover| {
            self.state.dock_tooltip.sample.as_ref()?;
            self.presented_dock_hover(&hover.column)
        });
        self.state
            .dock_tooltip
            .reconcile(snapshot, presented.as_deref(), now);
    }

    pub(super) fn settle_dock_tooltip(&mut self, now: Instant) -> bool {
        let snapshot = self.state.pipeline.hit_test_snapshot.load_full();
        self.reconcile_dock_tooltip(&snapshot, now);
        let mut contact_changed = false;
        if self
            .state
            .dock_tooltip
            .next_due()
            .is_some_and(|due| now >= due)
        {
            let area = snapshot
                .dock_hover
                .as_ref()
                .and_then(|hover| self.dock_tooltip_work_area(&hover.column));
            // Main-thread presentation may have blocked after the ordinary
            // cursor refresh. Check the actual point again just before issue.
            #[cfg(target_os = "windows")]
            {
                contact_changed = self.refresh_dock_cursor_for_activation();
            }
            contact_changed |= self.state.dock_tooltip.expire_dwell(now, area);
        }
        self.state.dock_tooltip.deadline_changed = false;
        std::mem::take(&mut self.state.dock_tooltip.repaint_owed) || contact_changed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{num::NonZeroU64, time::Duration};
    use tze_hud_compositor::renderer::DockHoverFrame;
    use tze_hud_scene::{
        SceneId,
        dock::{DockLayout, DockMetrics, DockProjection},
        graph::SceneGraph,
    };

    #[test]
    fn clipped_visits_are_passive_and_require_current_presented_card() {
        let mut scene = SceneGraph::new(800.0, 500.0);
        // A real publication is required to certify the passive column.
        crate::scene_startup::run_scene_startup(
            &Default::default(),
            None,
            &mut scene,
            Default::default(),
        )
        .unwrap();
        let target = Rect::new(0.0, 0.0, 800.0, 500.0);
        let layout = DockLayout::new(
            target,
            5,
            DockMetrics {
                row_height: 52.0,
                caption_width: 80.0,
                gap: 8.0,
            },
            [160.0 / 52.0],
        );
        scene.set_dock_projection(
            &["countdown".into()],
            Some(DockProjection {
                target: Some("DISPLAY1".into()),
                layout,
                pixels_per_dp: 1.0,
                aspect_ratios: vec![("countdown".into(), 160.0 / 52.0)],
            }),
            NonZeroU64::new(5_000).unwrap(),
        );
        scene
            .publish_to_dock("countdown", Default::default(), "agent", None, 0, 0)
            .unwrap();
        scene.certify_dock_passive_column("DISPLAY1", target);
        let column = scene.dock_passive_column().unwrap().clone();
        let mut snapshot = HitTestSnapshot::from_scene(&scene);
        let dwell = Duration::from_millis(180);
        let id = scene.dock_instance("agent", "countdown").unwrap().id;
        let card = |instance, label, top, pixels| {
            Arc::new(DockHoverCard {
                instance,
                label: Arc::from(label),
                bounds: Rect::new(column.column.x + 100.0, column.column.y + top, 160.0, 52.0),
                delay: dwell,
                pixels,
            })
        };
        let a = card(id, "Full A", 0.0, [1; 32]);
        let b = card(SceneId::new(), "Full B", 60.0, [2; 32]);
        let frame = |a, b| {
            Arc::new(DockHoverFrame {
                column: column.clone(),
                cards: vec![a, b],
                runtime_targets: Vec::new(),
            })
        };
        let old_frame = frame(a.clone(), b.clone());
        snapshot.dock_hover = Some(old_frame.clone());
        let receipt = PresentedDockHover {
            frame: old_frame,
            surface_epoch: Arc::new(AtomicBool::new(true)),
        };
        let point = |card: &DockHoverCard| (card.bounds.x + 1.0, card.bounds.y + 1.0);

        let start = Instant::now();
        let mut hover = DockTooltipVisit::default();
        hover.remember(Some(&column), Some(point(&a)));
        hover.reconcile(&snapshot, None, start);
        assert_eq!(hover.next_due(), None, "encode alone cannot start dwell");
        let updated_a = card(a.instance, "Full A", 0.0, [3; 32]);
        snapshot.dock_hover = Some(frame(updated_a, b.clone()));
        hover.reconcile(&snapshot, Some(&receipt), start);
        assert_eq!(
            hover.next_due(),
            None,
            "old same-label pixels cannot start dwell"
        );
        snapshot.dock_hover = Some(receipt.frame.clone());
        hover.reconcile(&snapshot, Some(&receipt), start);
        assert_eq!(hover.next_due(), Some(start + dwell));
        assert!(hover.deadline_changed);
        assert!(
            !hover.repaint_owed,
            "pending dwell never invalidates render"
        );
        hover.deadline_changed = false;
        hover.reconcile(&snapshot, Some(&receipt), start + Duration::from_millis(50));
        assert!(!hover.deadline_changed);

        let entered_b = start + Duration::from_millis(80);
        hover.remember(Some(&column), Some(point(&b)));
        hover.reconcile(&snapshot, Some(&receipt), entered_b);
        assert_eq!(hover.next_due(), Some(entered_b + dwell));
        assert!(!hover.repaint_owed);
        let due_b = entered_b + dwell;
        assert!(!hover.expire_dwell(due_b - Duration::from_millis(1), Some(target)));
        let tick_b = card(b.instance, "Full B", 60.0, [4; 32]);
        snapshot.dock_hover = Some(frame(a.clone(), tick_b.clone()));
        hover.reconcile(&snapshot, Some(&receipt), due_b - Duration::from_millis(1));
        assert_eq!(hover.visit.as_ref().unwrap().due, Some(due_b));
        assert_eq!(
            hover.next_due(),
            None,
            "a pending tick does not spin an expired deadline"
        );
        assert!(!hover.expire_dwell(due_b, Some(target)));
        let mut current_receipt = PresentedDockHover {
            frame: snapshot.dock_hover.clone().unwrap(),
            surface_epoch: receipt.surface_epoch.clone(),
        };
        let after_present = due_b + Duration::from_millis(1);
        hover.reconcile(&snapshot, Some(&current_receipt), after_present);
        assert_eq!(
            hover.next_due(),
            Some(due_b),
            "native proof restores the original due time"
        );

        // A movement after the pre-present sample cannot use its earned time.
        let mut left_during_present = DockTooltipVisit::default();
        left_during_present.remember(Some(&column), Some(point(&tick_b)));
        left_during_present.reconcile(&snapshot, Some(&current_receipt), entered_b);
        assert_eq!(left_during_present.next_due(), Some(due_b));
        left_during_present.remember(None, None);
        left_during_present.reconcile(&snapshot, Some(&current_receipt), after_present);
        assert!(!left_during_present.expire_dwell(after_present, Some(target)));
        assert!(left_during_present.output.load().is_none());

        assert!(hover.expire_dwell(after_present, Some(target)));
        let issued = hover.output.load_full().unwrap();
        assert_eq!(issued.card.pixels, tick_b.pixels);
        assert_eq!(issued.card.label.as_ref(), "Full B");
        assert!(!issued.is_presented());

        let tick_again = card(b.instance, "Full B", 60.0, [5; 32]);
        snapshot.dock_hover = Some(frame(a.clone(), tick_again.clone()));
        hover.reconcile(&snapshot, Some(&current_receipt), after_present);
        assert!(!hover.expire_dwell(after_present, Some(target)));
        current_receipt.frame = snapshot.dock_hover.clone().unwrap();
        hover.reconcile(&snapshot, Some(&current_receipt), after_present);
        assert!(hover.expire_dwell(after_present, Some(target)));
        let confirmed = hover.output.load_full().unwrap();
        assert!(!Arc::ptr_eq(&confirmed, &issued));
        assert_eq!(confirmed.card.pixels, tick_again.pixels);
        confirmed.acknowledge_for_testing();

        let further_tick = card(b.instance, "Full B", 60.0, [6; 32]);
        snapshot.dock_hover = Some(frame(a.clone(), further_tick));
        hover.reconcile(&snapshot, Some(&current_receipt), after_present);
        assert_eq!(hover.next_due(), None);
        assert!(!hover.repaint_owed);
        assert!(Arc::ptr_eq(&confirmed, &hover.output.load_full().unwrap()));

        let relabelled = card(b.instance, "B's new full label", 60.0, tick_again.pixels);
        snapshot.dock_hover = Some(frame(a.clone(), relabelled.clone()));
        let relabelled_at = start + Duration::from_millis(300);
        hover.reconcile(&snapshot, Some(&current_receipt), relabelled_at);
        assert!(hover.output.load().is_none());
        assert!(std::mem::take(&mut hover.repaint_owed));
        assert_eq!(
            hover.next_due(),
            Some(relabelled_at + dwell),
            "same-pixel label changes re-dwell"
        );

        let recovered = PresentedDockHover {
            frame: snapshot.dock_hover.clone().unwrap(),
            surface_epoch: Arc::new(AtomicBool::new(true)),
        };
        let recovered_at = relabelled_at + Duration::from_millis(50);
        hover.reconcile(&snapshot, Some(&recovered), recovered_at);
        assert_eq!(
            hover.next_due(),
            Some(recovered_at + dwell),
            "drawable recovery retires old dwell"
        );
        assert!(hover.expire_dwell(recovered_at + dwell, Some(target)));
        assert_eq!(
            hover.output.load().as_ref().unwrap().card.label.as_ref(),
            "B's new full label"
        );

        snapshot.dock_hover = None;
        hover.reconcile(&snapshot, Some(&recovered), recovered_at + dwell);
        assert!(
            hover.output.load().is_none(),
            "fit/hide invalidates old native receipt"
        );
        assert_eq!(hover.next_due(), None);
        snapshot.dock_hover = Some(recovered.frame.clone());
        hover.remember(None, None);
        hover.reconcile(&snapshot, Some(&recovered), recovered_at + dwell);
        assert_eq!(hover.next_due(), None, "leave disarms stationary receipt");

        // Re-create the Scene contact after a coalesced hide/show, without an
        // intervening frame that could otherwise clear the old hover sample.
        use tze_hud_scene::graph::SurfaceClasses;
        let after_leave = recovered_at + dwell;
        hover.remember(Some(&column), Some(point(&a)));
        hover.reconcile(&snapshot, Some(&recovered), after_leave);
        assert_eq!(hover.next_due(), Some(after_leave + dwell));
        scene.set_owner_hidden(SurfaceClasses {
            widgets: true,
            ..SurfaceClasses::NONE
        });
        scene.set_owner_hidden(SurfaceClasses::NONE);
        scene.certify_dock_passive_column("DISPLAY1", target);
        let visible_again = scene.dock_passive_column().unwrap().clone();
        assert!(!visible_again.shares_attachment(&column));
        let same_geometry = Arc::new(DockHoverFrame {
            column: visible_again.clone(),
            cards: vec![a.clone()],
            runtime_targets: Vec::new(),
        });
        let second_present = PresentedDockHover {
            frame: same_geometry.clone(),
            surface_epoch: recovered.surface_epoch.clone(),
        };
        snapshot.dock_hover = Some(same_geometry);
        hover.reconcile(&snapshot, Some(&second_present), after_leave);
        assert_eq!(
            hover.next_due(),
            None,
            "coalesced visibility retires the old sample"
        );
        hover.remember(Some(&visible_again), Some(point(&a)));
        hover.reconcile(&snapshot, Some(&second_present), after_leave);
        assert_eq!(hover.next_due(), Some(after_leave + dwell));

        scene.release_scope("agent");
        scene
            .publish_to_dock("countdown", Default::default(), "agent", None, 0, 0)
            .unwrap();
        scene.certify_dock_passive_column("DISPLAY1", target);
        let recreated = scene.dock_passive_column().unwrap();
        let replacement = scene.dock_instance("agent", "countdown").unwrap().id;
        assert_ne!(a.instance, replacement);
        assert!(!recreated.shares_attachment(&visible_again));
        let new_frame = Arc::new(DockHoverFrame {
            column: recreated.clone(),
            cards: vec![card(replacement, "Full A", 0.0, a.pixels)],
            runtime_targets: Vec::new(),
        });
        snapshot.dock_hover = Some(new_frame.clone());
        let new_present = PresentedDockHover {
            frame: new_frame,
            surface_epoch: recovered.surface_epoch.clone(),
        };
        hover.reconcile(&snapshot, Some(&new_present), after_leave + dwell);
        assert_eq!(
            hover.next_due(),
            None,
            "remove/recreate cannot replay dwell"
        );
    }
}
