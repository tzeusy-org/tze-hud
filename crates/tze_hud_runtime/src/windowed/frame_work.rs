//! The compositor turn's idle gate, frame builds, and primary-submit ledger.
//!
//! Capture shares the real build path but has no presentation side effects.
//! Keeping this boundary independent of a window lets the event-loop harness
//! drive it with only the final GPU/swapchain operations substituted.

use tze_hud_compositor::renderer::frame::{WindowedFrameBuild, WindowedPresentOutcome};
use tze_hud_compositor::{CaptureError, CapturedFrame, Compositor};
use tze_hud_scene::graph::SceneGraph;

use super::displays::FramePlan;
use super::wake::WindowedWake;
use crate::operator::screenshot::{CaptureInbox, CaptureRequest};
use crate::pipeline::{FrameTurnBuild, FrameTurnRequest, FrameTurnState};

pub(super) struct WindowedFrameWork {
    turn: FrameTurnState,
    wake: WindowedWake,
    frame_ready: crate::channels::FrameReadyTx,
}

impl WindowedFrameWork {
    pub(super) fn new(wake: WindowedWake, frame_ready: crate::channels::FrameReadyTx) -> Self {
        // Both content and position-only mutations must render on startup.
        Self {
            turn: FrameTurnState::default(),
            wake,
            frame_ready,
        }
    }
}

#[derive(Default)]
pub(super) struct RenderTriggers {
    pub surface: bool,
    pub degradation: Option<tze_hud_compositor::CompositorDegradationPolicy>,
}

pub(super) struct PreparedFrameWork {
    pub plan: FramePlan,
    pub render: Option<WindowedFrameBuild>,
    pub capture: Option<PreparedCapture>,
    pub markdown_prime_us: u64,
    pub next_wall_us: Option<u64>,
    pub next_animation_at: Option<std::time::Instant>,
}

pub(super) struct PreparedCapture {
    request: CaptureRequest,
    build: WindowedFrameBuild,
}

impl WindowedFrameWork {
    pub(super) fn prepare(
        &mut self,
        compositor: &mut Compositor,
        scene: &mut SceneGraph,
        size: (u32, u32),
        inbox: &CaptureInbox,
        triggers: RenderTriggers,
        plan: impl FnOnce(bool) -> FramePlan,
    ) -> PreparedFrameWork {
        let turn = self.turn.run_frame_turn(
            scene,
            Some(compositor),
            FrameTurnRequest {
                repaint: triggers.surface,
                degradation: triggers.degradation,
                ..Default::default()
            },
            |_| (),
            |compositor, scene, needs_render| {
                // Decide before capture builds: captures cannot acknowledge the
                // ordinary frame's animation/deadline repaint.
                let plan = plan(needs_render);
                let capture = inbox.next_live().map(|request| {
                    tracing::debug!(target: "tze_hud_delay_diagnostic", event = "capture_build_begin", scene_version = scene.version);
                    let build = compositor.build_windowed_frame(scene, size.0, size.1);
                    tracing::debug!(target: "tze_hud_delay_diagnostic", event = "capture_build_end", scene_version = scene.version);
                    PreparedCapture { request, build }
                });
                let render = (plan.full || plan.retry_only)
                    .then(|| compositor.build_windowed_frame(scene, size.0, size.1));
                FrameTurnBuild {
                    work: (plan, render, capture),
                    submitted: false,
                }
            },
        );
        let (plan, render, capture) = turn.work.expect("windowed renderer is attached");
        PreparedFrameWork {
            plan,
            render,
            capture,
            markdown_prime_us: turn.markdown_prime_us,
            next_wall_us: turn.next_wall_us,
            next_animation_at: turn.next_animation_at,
        }
    }

    /// Only ordinary primary submissions may advance the idle gate or signal
    /// main-thread presentation. Acquisitions/submits count even if a later
    /// operation failed and cleared the aggregate stage-success sentinel.
    pub(super) fn record_primary_submission(
        &mut self,
        outcome: &WindowedPresentOutcome,
        built_versions: (u64, u64),
    ) -> bool {
        if outcome.surface_acquired {
            self.wake.counters().record_surface_acquisition();
        }
        if outcome.gpu_submitted {
            self.wake.counters().record_gpu_submission();
        }
        let submitted = outcome.telemetry.stage7_gpu_submit_us > 0;
        tracing::debug!(
            target: "tze_hud_delay_diagnostic",
            event = "ordinary_primary_submission",
            scene_version = built_versions.0,
            geometry_epoch = built_versions.1,
            acquired = outcome.surface_acquired,
            gpu_submitted = outcome.gpu_submitted,
            ready = outcome.telemetry.stage7_gpu_submit_us > 0,
        );
        if submitted {
            self.turn.record_submission(true, built_versions);
            let _ = self.frame_ready.send(true);
            self.wake
                .notify_main(crate::idle_efficiency::RuntimeWakeupSource::FrameReady);
        }
        submitted
    }

    #[cfg(test)]
    pub(super) fn last_rendered(&self) -> (u64, u64) {
        self.turn.last_rendered()
    }
}

impl PreparedCapture {
    /// Complete the real inbox drain after releasing the scene lock. The
    /// callback owns only offscreen rendering, never swapchain presentation.
    pub(super) fn finish_with(
        self,
        render: impl FnOnce(&WindowedFrameBuild, Option<&str>) -> Result<CapturedFrame, CaptureError>,
    ) {
        let result = render(&self.build, self.request.display.as_deref());
        tracing::debug!(
            target: "tze_hud_delay_diagnostic",
            event = "capture_render_end",
            success = result.is_ok(),
        );
        let _ = self.request.reply.send(result);
    }
}
