//! Opt-in layout, widget raster and upload observations at headless boundaries.

use tze_hud_telemetry::WorkCounts;

use super::Compositor;

#[derive(Default)]
pub(super) struct WorkObservation {
    frame_start: WorkCounts,
    latest: Option<WorkCounts>,
}

impl WorkObservation {
    fn begin_frame(&mut self, totals: WorkCounts) {
        self.latest = None;
        self.frame_start = totals;
    }
}

impl Compositor {
    fn current_work_totals(&self) -> WorkCounts {
        let (raster, upload) = self
            .widget_renderer
            .as_ref()
            .map_or((0, 0), |renderer| renderer.work_totals());
        WorkCounts {
            layout: self.text_shape_call_count(),
            raster,
            upload,
        }
    }

    /// An actual frame discards undrained prior work, even if it aborts later.
    pub(super) fn begin_headless_work_observation(&mut self) {
        let totals = self.current_work_totals();
        self.work_observation.begin_frame(totals);
    }

    /// Record only after the full-frame submit/readback tail completes.
    pub(super) fn record_headless_work(&mut self) {
        let before = self.work_observation.frame_start;
        let after = self.current_work_totals();
        self.work_observation.latest = Some(WorkCounts {
            layout: after.layout - before.layout,
            raster: after.raster - before.raster,
            upload: after.upload - before.upload,
        });
    }

    /// Drain actual shaping and widget raster/upload work from the most recent
    /// completed headless frame. A second drain returns `None`.
    ///
    /// Rust-only dev/test hook; zero widget uploads does not mean no GPU work.
    pub fn take_work_counts(&mut self) -> Option<WorkCounts> {
        self.work_observation.latest.take()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_observation_clears_prior_frame_work_counts() {
        let mut state = WorkObservation {
            latest: Some(WorkCounts {
                layout: 1,
                raster: 2,
                upload: 1,
            }),
            ..WorkObservation::default()
        };

        state.begin_frame(WorkCounts::default());

        assert!(state.latest.is_none());
    }
}
