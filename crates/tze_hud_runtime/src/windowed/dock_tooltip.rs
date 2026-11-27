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
    card: Arc<DockHoverCard>,
    surface_epoch: Arc<AtomicBool>,
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
            if !sampled.shares_attachment(&current.column) {
                return None;
            }
            let presented =
                presented.filter(|proof| proof.surface_epoch.load(Ordering::Acquire))?;
            current.cards.iter().find_map(|card| {
                (card.bounds.contains_point(*x, *y)
                    && presented.frame.presented_equivalent(&current.column, card))
                .then_some((&current.column, card, &presented.surface_epoch))
            })
        });
        if match (&self.visit, candidate) {
            (Some(active), Some((column, card, epoch))) => {
                active.column.shares_attachment(column)
                    && active.card.same_visit(card)
                    && Arc::ptr_eq(&active.surface_epoch, epoch)
            }
            (None, None) => true,
            _ => false,
        } {
            return;
        }
        self.deadline_changed = true;
        if self.output.load().is_some() {
            self.output.store(None);
            self.repaint_owed = true;
        }
        self.visit = candidate.map(|(column, card, epoch)| Visit {
            column: column.clone(),
            card: Arc::clone(card),
            surface_epoch: Arc::clone(epoch),
            due: Some(now + card.delay),
        });
    }

    pub(super) fn next_due(&self) -> Option<Instant> {
        self.visit.as_ref()?.due
    }

    fn expire_dwell(&mut self, now: Instant, work_area: Option<Rect>) -> bool {
        let Some(visit) = self.visit.as_mut() else {
            return false;
        };
        if visit.due.is_none_or(|due| now < due) {
            return false;
        }
        visit.due = None;
        let Some(work_area) = work_area else {
            return false;
        };
        self.output.store(Some(Arc::new(DockTooltipRequest {
            column: visit.column.clone(),
            card: Arc::clone(&visit.card),
            surface_epoch: Arc::clone(&visit.surface_epoch),
            work_area,
        })));
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
        self.state.dock_tooltip.deadline_changed = false;
        let repaint_owed = std::mem::take(&mut self.state.dock_tooltip.repaint_owed);
        if self
            .state
            .dock_tooltip
            .next_due()
            .is_none_or(|due| now < due)
        {
            return repaint_owed;
        }
        let area = snapshot
            .dock_hover
            .as_ref()
            .and_then(|hover| self.dock_tooltip_work_area(&hover.column));
        self.state.dock_tooltip.expire_dwell(now, area) || repaint_owed
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
        let column = scene.dock_passive_column().unwrap();
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
        hover.remember(Some(column), Some(point(&a)));
        hover.reconcile(&snapshot, None, start);
        assert_eq!(hover.next_due(), None, "encode alone cannot start dwell");
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
        hover.remember(Some(column), Some(point(&b)));
        hover.reconcile(&snapshot, Some(&receipt), entered_b);
        assert_eq!(hover.next_due(), Some(entered_b + dwell));
        assert!(!hover.repaint_owed);
        assert!(!hover.expire_dwell(start + dwell, Some(target)));
        assert!(hover.expire_dwell(entered_b + dwell, Some(target)));
        assert_eq!(
            hover.output.load().as_ref().unwrap().card.label.as_ref(),
            "Full B"
        );

        let relabelled = card(b.instance, "B's new full label", 60.0, b.pixels);
        snapshot.dock_hover = Some(frame(a.clone(), relabelled.clone()));
        let relabelled_at = start + Duration::from_millis(300);
        hover.reconcile(&snapshot, Some(&receipt), relabelled_at);
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
    }
}
