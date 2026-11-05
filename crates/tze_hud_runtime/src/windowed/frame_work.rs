//! The compositor turn's idle gate, frame builds, and primary-submit ledger.
//!
//! Capture shares the real build path but has no presentation side effects.
//! Keeping this boundary independent of a window lets the event-loop harness
//! drive it with only the final GPU/swapchain operations substituted.

use tze_hud_compositor::renderer::frame::{WindowedFrameBuild, WindowedPresentOutcome};
use tze_hud_compositor::{CaptureError, CapturedFrame, Compositor};
use tze_hud_scene::graph::SceneGraph;

use super::displays::FramePlan;
use super::lifecycle::windowed_frame_needs_render;
use super::wake::WindowedWake;
use crate::operator::screenshot::{CaptureInbox, CaptureRequest};

pub(super) struct WindowedFrameWork {
    last_rendered: (u64, u64),
    wake: WindowedWake,
    frame_ready: crate::channels::FrameReadyTx,
}

impl WindowedFrameWork {
    pub fn new(wake: WindowedWake, frame_ready: crate::channels::FrameReadyTx) -> Self {
        // Both content and position-only mutations must render on startup.
        Self {
            last_rendered: (u64::MAX, u64::MAX),
            wake,
            frame_ready,
        }
    }
}

#[derive(Default)]
pub(super) struct RenderTriggers {
    pub composer: bool,
    pub surface: bool,
}

pub(super) struct PreparedFrameWork {
    pub plan: FramePlan,
    pub render: Option<WindowedFrameBuild>,
    pub capture: Option<PreparedCapture>,
}

pub(super) struct PreparedCapture {
    request: CaptureRequest,
    build: WindowedFrameBuild,
}

impl WindowedFrameWork {
    pub fn prepare(
        &mut self,
        compositor: &mut Compositor,
        scene: &mut SceneGraph,
        size: (u32, u32),
        inbox: &CaptureInbox,
        triggers: RenderTriggers,
        plan: impl FnOnce(bool) -> FramePlan,
    ) -> PreparedFrameWork {
        // Decide BEFORE capture builds: a build may step an animation's final
        // state, but must never consume the screen's corresponding repaint.
        let needs_render = windowed_frame_needs_render(
            scene.version != self.last_rendered.0,
            scene.geometry_epoch != self.last_rendered.1,
            compositor.has_inflight_animation(scene),
            triggers.composer,
            triggers.surface,
        );
        let plan = plan(needs_render);
        let capture = inbox.next_live().map(|request| {
            tracing::debug!(
                target: "tze_hud_delay_diagnostic",
                event = "capture_build_begin",
                scene_version = scene.version,
            );
            let build = compositor.build_windowed_frame(scene, size.0, size.1);
            tracing::debug!(
                target: "tze_hud_delay_diagnostic",
                event = "capture_build_end",
                scene_version = scene.version,
            );
            PreparedCapture { request, build }
        });
        let render = (plan.full || plan.retry_only)
            .then(|| compositor.build_windowed_frame(scene, size.0, size.1));
        PreparedFrameWork {
            plan,
            render,
            capture,
        }
    }

    /// Only ordinary primary submissions may advance the idle gate or signal
    /// main-thread presentation. Acquisitions/submits count even if a later
    /// operation failed and cleared the aggregate stage-success sentinel.
    pub fn record_primary_submission(
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
            self.last_rendered = built_versions;
            let _ = self.frame_ready.send(true);
            self.wake
                .notify_main(crate::idle_efficiency::RuntimeWakeupSource::FrameReady);
        }
        submitted
    }

    #[cfg(test)]
    pub fn last_rendered(&self) -> (u64, u64) {
        self.last_rendered
    }
}

impl PreparedCapture {
    /// Complete the real inbox drain after releasing the scene lock. The
    /// callback owns only offscreen rendering, never swapchain presentation.
    pub fn finish_with(
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
