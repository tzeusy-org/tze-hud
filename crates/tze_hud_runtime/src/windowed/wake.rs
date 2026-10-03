//! Event-driven wake scheduling for the production windowed runtime.
//!
//! The generation counter is the load-bearing missed-wakeup guard: callers
//! checkpoint before inspecting render work, then wait against that checkpoint.
//! A notification that races with inspection advances the generation, so the
//! waiter observes it without parking.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use winit::event_loop::{ControlFlow, EventLoopProxy};

use crate::idle_efficiency::{IdleEfficiencyCounters, RuntimeWakeupSource};

/// One explicit instant at which runtime-owned work becomes due.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Deadline {
    pub(super) at: Instant,
    pub(super) source: RuntimeWakeupSource,
}

impl Deadline {
    pub(super) const fn new(at: Instant, source: RuntimeWakeupSource) -> Self {
        Self { at, source }
    }
}

/// What a cursor-entry check may observe changing; any difference between two
/// checks is a hover/capture transition that deserves a full turn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct CursorEntrySignature {
    /// The overlay should capture (cursor over a region or portal affordance).
    pub(super) capture: bool,
    /// Focused portal whose resize grip the cursor is over.
    pub(super) grip: Option<tze_hud_scene::SceneId>,
    /// Dismissible tile whose viewer close button the cursor is over.
    pub(super) close_target: Option<tze_hud_scene::SceneId>,
    /// Number of widget hover regions containing the cursor.
    pub(super) widget_regions: u32,
}

/// Cursor-entry bookkeeping kept between event-loop turns.
#[derive(Debug, Default)]
pub(super) struct CursorEntryState {
    /// The turn in progress began with a raw mouse-motion event while armed.
    pub(super) tick: bool,
    /// Raw mouse input is currently being requested from the OS
    /// (`RIDEV_INPUTSINK`); mirrors [`cursor_entry_armed`].
    pub(super) listening: bool,
    /// Signature at the last full turn or cursor check.
    pub(super) signature: Option<CursorEntrySignature>,
}

/// Result of servicing one raw mouse-motion event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum CursorEntryTurn {
    /// Nothing changed: the loop sleeps until `next` without touching the
    /// scene or the compositor.
    Quiet { next: Option<Deadline> },
    /// State changed or the turn has other work: run the full turn, which owns
    /// exactly one compositor wake (main work is already marked pending).
    FullTurn,
}

/// Select the winit sleep policy from explicit deadlines.
///
/// Static idle has no deadline and therefore uses `Wait`. Due work is expressed
/// as `WaitUntil(now)` rather than `Poll`, keeping the production loop free of a
/// correctness dependency on bounded spinning.
pub(super) fn control_flow_for_deadlines(
    _now: Instant,
    deadlines: impl IntoIterator<Item = Deadline>,
) -> ControlFlow {
    deadlines
        .into_iter()
        .min_by_key(|deadline| deadline.at)
        .map_or(ControlFlow::Wait, |deadline| {
            ControlFlow::WaitUntil(deadline.at)
        })
}

/// Whether raw mouse input must wake the loop: an overlay in passthrough (the
/// OS delivers no pointer messages to the window) with interactive regions to
/// enter. Otherwise nothing is listened for, so an idle HUD with nothing
/// clickable parks and receives zero input wakeups.
pub(super) fn cursor_entry_armed(
    overlay: bool,
    has_interactive_regions: bool,
    in_passthrough: bool,
) -> bool {
    overlay && has_interactive_regions && in_passthrough
}

/// The listen mode to apply when the armed state changes; `None` if unchanged.
pub(super) fn device_events_transition(
    was_listening: bool,
    armed: bool,
) -> Option<winit::event_loop::DeviceEvents> {
    (was_listening != armed).then_some(if armed {
        winit::event_loop::DeviceEvents::Always
    } else {
        winit::event_loop::DeviceEvents::Never
    })
}

pub(super) fn deadline_from_wall_us(
    deadline_wall_us: u64,
    source: RuntimeWakeupSource,
) -> Deadline {
    let now_wall_us = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros() as u64;
    Deadline::new(
        Instant::now() + Duration::from_micros(deadline_wall_us.saturating_sub(now_wall_us)),
        source,
    )
}

#[derive(Debug)]
struct WakeState {
    generation: u64,
    source: RuntimeWakeupSource,
}

impl Default for WakeState {
    fn default() -> Self {
        Self {
            generation: 0,
            source: RuntimeWakeupSource::SceneChange,
        }
    }
}

/// Race-free compositor-thread wake primitive.
#[derive(Clone, Debug, Default)]
pub(super) struct CompositorWake {
    inner: Arc<(Mutex<WakeState>, Condvar)>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct WakeObservation {
    pub(super) generation: u64,
    pub(super) source: RuntimeWakeupSource,
    pub(super) timed_out: bool,
}

impl CompositorWake {
    /// Snapshot before inspecting work. Pass this value to `wait` after the
    /// inspection so a notification during that interval cannot be missed.
    pub(super) fn checkpoint(&self) -> u64 {
        self.inner
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .generation
    }

    pub(super) fn notify(&self, source: RuntimeWakeupSource) {
        let (state_lock, ready) = &*self.inner;
        let mut state = state_lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.generation = state.generation.wrapping_add(1);
        state.source = source;
        drop(state);
        ready.notify_one();
    }

    pub(super) fn wait(&self, checkpoint: u64, deadline: Option<Deadline>) -> WakeObservation {
        let (state_lock, ready) = &*self.inner;
        let mut state = state_lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        while state.generation == checkpoint {
            let Some(deadline) = deadline else {
                state = ready
                    .wait(state)
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                continue;
            };

            let now = Instant::now();
            if now >= deadline.at {
                return WakeObservation {
                    generation: state.generation,
                    source: deadline.source,
                    timed_out: true,
                };
            }
            let timeout = deadline.at.saturating_duration_since(now);
            let (next, result) = ready
                .wait_timeout(state, timeout)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            state = next;
            if result.timed_out() && state.generation == checkpoint {
                return WakeObservation {
                    generation: state.generation,
                    source: deadline.source,
                    timed_out: true,
                };
            }
        }

        WakeObservation {
            generation: state.generation,
            source: state.source,
            timed_out: false,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub(super) struct RuntimeWakeEvent;

#[derive(Clone, Copy, Debug)]
pub(super) struct MainWorkCheckpoint {
    generation: u64,
    source: RuntimeWakeupSource,
}

#[derive(Debug)]
struct MainWorkState {
    generation: u64,
    finished_generation: u64,
    source: RuntimeWakeupSource,
}

#[derive(Debug)]
struct MainEventState {
    source: RuntimeWakeupSource,
    proxy_event_pending: bool,
}

impl Default for MainEventState {
    fn default() -> Self {
        Self {
            source: RuntimeWakeupSource::SceneChange,
            proxy_event_pending: false,
        }
    }
}

impl Default for MainWorkState {
    fn default() -> Self {
        Self {
            generation: 0,
            finished_generation: 0,
            source: RuntimeWakeupSource::SceneChange,
        }
    }
}

/// Single production owner of cross-thread wake delivery and wake counters.
#[derive(Clone)]
pub(super) struct WindowedWake {
    proxy: Option<EventLoopProxy<RuntimeWakeEvent>>,
    main_event: Arc<Mutex<MainEventState>>,
    main_work: Arc<Mutex<MainWorkState>>,
    /// Coalesces a lock-availability waiter for deferred main-thread work.
    main_availability_waiting: Arc<AtomicBool>,
    /// Coalesces a lock-availability waiter for a compositor `try_lock` miss.
    compositor_availability_waiting: Arc<AtomicBool>,
    compositor: CompositorWake,
    counters: Arc<IdleEfficiencyCounters>,
}

impl WindowedWake {
    pub(super) fn new(proxy: EventLoopProxy<RuntimeWakeEvent>) -> Self {
        Self {
            proxy: Some(proxy),
            main_event: Arc::new(Mutex::new(MainEventState::default())),
            main_work: Arc::new(Mutex::new(MainWorkState::default())),
            main_availability_waiting: Arc::new(AtomicBool::new(false)),
            compositor_availability_waiting: Arc::new(AtomicBool::new(false)),
            compositor: CompositorWake::default(),
            counters: Arc::new(IdleEfficiencyCounters::default()),
        }
    }

    #[cfg(any(test, feature = "test-harness"))]
    pub(super) fn disconnected() -> Self {
        Self {
            proxy: None,
            main_event: Arc::new(Mutex::new(MainEventState::default())),
            main_work: Arc::new(Mutex::new(MainWorkState::default())),
            main_availability_waiting: Arc::new(AtomicBool::new(false)),
            compositor_availability_waiting: Arc::new(AtomicBool::new(false)),
            compositor: CompositorWake::default(),
            counters: Arc::new(IdleEfficiencyCounters::default()),
        }
    }

    pub(super) fn render_notifier(&self) -> tze_hud_scene::render_wake::RenderWakeNotifier {
        let wake = self.clone();
        tze_hud_scene::render_wake::RenderWakeNotifier::new(move || {
            wake.notify_direct_render(RuntimeWakeupSource::SceneChange);
        })
    }

    /// Wake only the main-thread ingress owner. The owner records render work
    /// after its drain only when the scene actually changed, so side-effect-free
    /// ingress such as an empty portal input poll never reaches the compositor.
    pub(super) fn main_work_notifier(&self) -> tze_hud_scene::render_wake::RenderWakeNotifier {
        let wake = self.clone();
        tze_hud_scene::render_wake::RenderWakeNotifier::new(move || {
            wake.notify_main(RuntimeWakeupSource::SceneChange);
        })
    }

    /// Notify after a direct shared-scene mutation has already completed.
    ///
    /// The compositor must wake immediately, while the main thread is woken
    /// only to perform bookkeeping/present polling.  It must not create
    /// main-work debt: `about_to_wait` has no mutation to acknowledge here,
    /// and doing so would emit a duplicate compositor generation.
    pub(super) fn notify_direct_render(&self, source: RuntimeWakeupSource) {
        self.compositor.notify(source);
        self.notify_main(source);
    }

    pub(super) fn mark_main_work_pending(&self, source: RuntimeWakeupSource) -> MainWorkCheckpoint {
        let mut main_work = self
            .main_work
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        main_work.generation = main_work.generation.wrapping_add(1);
        main_work.source = source;
        MainWorkCheckpoint {
            generation: main_work.generation,
            source,
        }
    }

    /// Snapshot the producer generations that the next main-thread drain may
    /// acknowledge. Work arriving after this checkpoint remains unacknowledged
    /// for the following drain instead of being erased by this one.
    pub(super) fn main_work_checkpoint(&self) -> MainWorkCheckpoint {
        let main_work = self
            .main_work
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        MainWorkCheckpoint {
            generation: main_work.generation,
            source: main_work.source,
        }
    }

    /// True while a producer generation has not been published to the
    /// compositor yet.
    pub(super) fn has_unfinished_main_work(&self) -> bool {
        let main_work = self
            .main_work
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        main_work.generation > main_work.finished_generation
    }

    #[cfg(test)]
    pub(super) fn main_work_generation(&self) -> u64 {
        self.main_work
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .generation
    }

    /// Publish work created by the main thread after it drained an async
    /// producer or serviced a main-owned deadline.
    pub(super) fn finish_main_work(&self, checkpoint: MainWorkCheckpoint) -> bool {
        let should_notify = {
            let mut main_work = self
                .main_work
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if checkpoint.generation > main_work.finished_generation {
                main_work.finished_generation = checkpoint.generation;
                true
            } else {
                false
            }
        };
        if should_notify {
            self.compositor.notify(checkpoint.source);
            true
        } else {
            false
        }
    }

    /// Finish a main-thread settle pass without racing two wake sources for the
    /// same portal mutation. A typed producer/deadline checkpoint owns the
    /// attribution when present; otherwise a portal mutation discovered during
    /// the drain creates one `SceneChange` generation after all writes settle.
    pub(super) fn finish_main_work_after_settle(
        &self,
        checkpoint: MainWorkCheckpoint,
        portal_scene_changed: bool,
    ) -> bool {
        let finished_typed_work = self.finish_main_work(checkpoint);
        if portal_scene_changed && !finished_typed_work {
            self.notify_compositor(RuntimeWakeupSource::SceneChange);
            true
        } else {
            finished_typed_work
        }
    }

    pub(super) fn notify_main(&self, source: RuntimeWakeupSource) {
        self.notify_main_after_source_write(source, || {});
    }

    /// Arrange one main-thread wake after a contended scene dependency becomes
    /// available. The waiter is coalesced while it is blocked: neither this
    /// method nor the main loop creates a periodic retry deadline.
    ///
    /// The completion is main work because the wake lets `about_to_wait` retry
    /// deferred ingress. Its normal settle path owns the corresponding
    /// compositor generation only after that retry has made forward progress.
    pub(super) fn schedule_main_after_availability(
        &self,
        wait_until_available: impl FnOnce() + Send + 'static,
    ) {
        self.schedule_after_availability(
            Arc::clone(&self.main_availability_waiting),
            AvailabilityWakeTarget::Main,
            wait_until_available,
        );
    }

    /// Arrange one compositor wake after its `SceneGraph::try_lock` miss can
    /// make progress. This replaces frame-cadence lock polling with a single
    /// completion-driven retry.
    pub(super) fn schedule_compositor_after_availability(
        &self,
        wait_until_available: impl FnOnce() + Send + 'static,
    ) {
        self.schedule_after_availability(
            Arc::clone(&self.compositor_availability_waiting),
            AvailabilityWakeTarget::Compositor,
            wait_until_available,
        );
    }

    fn schedule_after_availability(
        &self,
        waiting: Arc<AtomicBool>,
        target: AvailabilityWakeTarget,
        wait_until_available: impl FnOnce() + Send + 'static,
    ) {
        if waiting.swap(true, Ordering::AcqRel) {
            return;
        }

        let wake = self.clone();
        let thread_waiting = Arc::clone(&waiting);
        let spawn_result = std::thread::Builder::new()
            .name("tze-scene-availability".to_string())
            .spawn(move || {
                wait_until_available();
                // Clear before delivery so a fresh contention observed while
                // this completion is queued can install its own waiter.
                thread_waiting.store(false, Ordering::Release);
                match target {
                    AvailabilityWakeTarget::Main => {
                        wake.mark_main_work_pending(RuntimeWakeupSource::SceneChange);
                        wake.notify_main(RuntimeWakeupSource::SceneChange);
                    }
                    AvailabilityWakeTarget::Compositor => {
                        wake.notify_compositor(RuntimeWakeupSource::SceneChange);
                    }
                }
            });
        if let Err(error) = spawn_result {
            waiting.store(false, Ordering::Release);
            tracing::error!(%error, "failed to start scene-availability wake waiter");
        }
    }

    fn notify_main_after_source_write(
        &self,
        source: RuntimeWakeupSource,
        after_source_write: impl FnOnce(),
    ) {
        let mut main_event = self
            .main_event
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        main_event.source = source;
        after_source_write();
        if !main_event.proxy_event_pending {
            main_event.proxy_event_pending = self
                .proxy
                .as_ref()
                .is_some_and(|proxy| proxy.send_event(RuntimeWakeEvent).is_ok());
        }
    }

    pub(super) fn has_pending_proxy_event(&self) -> bool {
        self.main_event
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .proxy_event_pending
    }

    pub(super) fn take_main_source(&self) -> RuntimeWakeupSource {
        let mut main_event = self
            .main_event
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        main_event.proxy_event_pending = false;
        main_event.source
    }

    pub(super) fn notify_compositor(&self, source: RuntimeWakeupSource) {
        self.compositor.notify(source);
    }

    pub(super) fn compositor(&self) -> &CompositorWake {
        &self.compositor
    }

    pub(super) fn counters(&self) -> &Arc<IdleEfficiencyCounters> {
        &self.counters
    }
}

#[derive(Clone, Copy)]
enum AvailabilityWakeTarget {
    Main,
    Compositor,
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use crate::idle_efficiency::{IdleEfficiencyCounters, RuntimeWakeupSource};

    use super::{
        CompositorWake, Deadline, WindowedWake, control_flow_for_deadlines, cursor_entry_armed,
    };

    #[test]
    fn static_idle_selects_wait_without_a_bounded_poll() {
        let now = Instant::now();
        assert_eq!(
            control_flow_for_deadlines(now, std::iter::empty()),
            winit::event_loop::ControlFlow::Wait
        );
    }

    #[test]
    fn earliest_future_deadline_selects_wait_until() {
        let now = Instant::now();
        let later = Deadline::new(
            now + Duration::from_millis(40),
            RuntimeWakeupSource::TtlDeadline,
        );
        let earlier = Deadline::new(
            now + Duration::from_millis(10),
            RuntimeWakeupSource::AnimationDeadline,
        );
        assert_eq!(
            control_flow_for_deadlines(now, [later, earlier]),
            winit::event_loop::ControlFlow::WaitUntil(earlier.at)
        );
    }

    #[test]
    fn already_due_work_uses_wait_until_not_poll() {
        let now = Instant::now();
        let due = Deadline::new(now, RuntimeWakeupSource::TtlDeadline);
        assert_eq!(
            control_flow_for_deadlines(now, [due]),
            winit::event_loop::ControlFlow::WaitUntil(now)
        );
    }

    #[test]
    fn benchmark_cadence_rearms_with_successive_wait_until_deadlines() {
        let start = Instant::now();
        let interval = Duration::from_millis(16);
        for frame in 1..=4 {
            let at = start + interval * frame;
            let deadline = Deadline::new(at, RuntimeWakeupSource::AnimationDeadline);
            assert_eq!(
                control_flow_for_deadlines(start, [deadline]),
                winit::event_loop::ControlFlow::WaitUntil(at)
            );
        }
    }

    #[test]
    fn device_events_transition_changes_only_on_edges() {
        use winit::event_loop::DeviceEvents::{Always, Never};
        assert_eq!(super::device_events_transition(false, true), Some(Always));
        assert_eq!(super::device_events_transition(true, false), Some(Never));
        assert_eq!(super::device_events_transition(true, true), None);
        assert_eq!(super::device_events_transition(false, false), None);
    }

    #[test]
    fn cursor_entry_armed_only_with_hit_regions_in_passthrough() {
        assert!(cursor_entry_armed(true, true, true));
        assert!(!cursor_entry_armed(true, false, true), "no regions: park");
        assert!(
            !cursor_entry_armed(true, true, false),
            "capturing: pointer events arrive"
        );
        assert!(
            !cursor_entry_armed(false, true, true),
            "fullscreen captures all"
        );
    }

    #[test]
    fn sixty_seconds_of_static_idle_has_zero_runtime_wakeups() {
        let counters = IdleEfficiencyCounters::default();
        let baseline = counters.snapshot();
        let now = Instant::now();
        for second in 0..60 {
            let simulated_now = now + Duration::from_secs(second);
            assert_eq!(
                control_flow_for_deadlines(simulated_now, std::iter::empty()),
                winit::event_loop::ControlFlow::Wait
            );
        }
        let delta = counters
            .snapshot()
            .delta_since(&baseline)
            .expect("monotonic counters");
        assert_eq!(delta.combined_runtime_wakeups(), 0);
        assert!(delta.combined_runtime_wakeups() <= 120);
    }

    #[test]
    fn notification_before_park_is_observed_from_the_pre_work_checkpoint() {
        let wake = CompositorWake::default();
        let checkpoint = wake.checkpoint();
        wake.notify(RuntimeWakeupSource::SceneChange);

        let observed = wake.wait(checkpoint, None);
        assert_eq!(observed.source, RuntimeWakeupSource::SceneChange);
        assert!(!observed.timed_out);
        assert!(observed.generation > checkpoint);
    }

    #[test]
    fn notification_during_park_wakes_the_waiter() {
        let wake = CompositorWake::default();
        let checkpoint = wake.checkpoint();
        let notifier = wake.clone();
        let thread = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(10));
            notifier.notify(RuntimeWakeupSource::SceneChange);
        });

        let observed = wake.wait(
            checkpoint,
            Some(Deadline::new(
                Instant::now() + Duration::from_secs(1),
                RuntimeWakeupSource::TtlDeadline,
            )),
        );
        thread.join().expect("notifier thread");
        assert_eq!(observed.source, RuntimeWakeupSource::SceneChange);
        assert!(!observed.timed_out);
    }

    #[test]
    fn timeout_is_attributed_to_its_deadline_source() {
        let wake = CompositorWake::default();
        let checkpoint = wake.checkpoint();
        let observed = wake.wait(
            checkpoint,
            Some(Deadline::new(
                Instant::now() + Duration::from_millis(1),
                RuntimeWakeupSource::TtlDeadline,
            )),
        );
        assert_eq!(observed.source, RuntimeWakeupSource::TtlDeadline);
        assert!(observed.timed_out);
    }

    #[test]
    fn main_owned_work_gets_a_post_mutation_compositor_generation() {
        let wake = WindowedWake::disconnected();
        let before_main_mutation = wake.compositor().checkpoint();
        let checkpoint = wake.mark_main_work_pending(RuntimeWakeupSource::SceneChange);
        assert!(wake.finish_main_work(checkpoint));
        let post_mutation = wake.compositor().wait(before_main_mutation, None);
        assert!(post_mutation.generation > before_main_mutation);
    }

    #[test]
    fn direct_render_work_wakes_once_without_post_settle_debt() {
        let wake = WindowedWake::disconnected();
        let compositor_checkpoint = wake.compositor().checkpoint();

        wake.notify_direct_render(RuntimeWakeupSource::SceneChange);

        assert_eq!(
            wake.compositor().checkpoint(),
            compositor_checkpoint + 1,
            "a direct post-mutation notifier owns exactly one compositor generation"
        );
        assert!(
            !wake.finish_main_work(wake.main_work_checkpoint()),
            "the main bookkeeping wake must not manufacture a second render generation"
        );
        assert_eq!(wake.compositor().checkpoint(), compositor_checkpoint + 1);
    }

    #[test]
    fn frame_ready_does_not_bypass_the_next_compositor_deadline() {
        let wake = WindowedWake::disconnected();
        wake.notify_main(RuntimeWakeupSource::FrameReady);
        assert!(!wake.finish_main_work(wake.main_work_checkpoint()));
    }

    #[test]
    fn repeated_main_only_ingress_does_not_advance_compositor_generation() {
        let wake = WindowedWake::disconnected();
        let notifier = wake.main_work_notifier();
        let compositor_checkpoint = wake.compositor().checkpoint();
        for _ in 0..=200 {
            notifier.notify();
        }
        assert_eq!(wake.compositor().checkpoint(), compositor_checkpoint);
        assert!(
            !wake.finish_main_work(wake.main_work_checkpoint()),
            "empty ingress drains must not synthesize post-drain render work"
        );
        assert_eq!(wake.compositor().checkpoint(), compositor_checkpoint);
    }

    #[test]
    fn main_source_and_pending_transition_cannot_be_split_by_event_consumption() {
        let wake = Arc::new(WindowedWake::disconnected());
        {
            let mut main_event = wake.main_event.lock().unwrap();
            main_event.source = RuntimeWakeupSource::FrameReady;
            main_event.proxy_event_pending = true;
        }
        let (source_written_tx, source_written_rx) = std::sync::mpsc::channel();
        let (resume_tx, resume_rx) = std::sync::mpsc::channel();
        let notifier = Arc::clone(&wake);
        let notify_thread = std::thread::spawn(move || {
            notifier.notify_main_after_source_write(RuntimeWakeupSource::TtlDeadline, || {
                source_written_tx.send(()).unwrap();
                resume_rx.recv().unwrap();
            });
        });
        source_written_rx.recv().unwrap();

        let (taken_tx, taken_rx) = std::sync::mpsc::channel();
        let consumer = Arc::clone(&wake);
        let take_thread = std::thread::spawn(move || {
            taken_tx.send(consumer.take_main_source()).unwrap();
        });
        assert!(
            taken_rx.recv_timeout(Duration::from_millis(20)).is_err(),
            "event consumption must remain blocked while source and pending state are updated"
        );
        resume_tx.send(()).unwrap();
        notify_thread.join().unwrap();
        assert_eq!(taken_rx.recv().unwrap(), RuntimeWakeupSource::TtlDeadline);
        take_thread.join().unwrap();
        assert!(!wake.has_pending_proxy_event());
    }

    #[test]
    fn portal_scene_change_without_typed_work_notifies_once_after_settle() {
        let wake = WindowedWake::disconnected();
        let settle_checkpoint = wake.main_work_checkpoint();
        let compositor_checkpoint = wake.compositor().checkpoint();

        assert!(wake.finish_main_work_after_settle(settle_checkpoint, true));
        assert_eq!(wake.compositor().checkpoint(), compositor_checkpoint + 1);
        let observed = wake.compositor().wait(compositor_checkpoint, None);
        assert_eq!(observed.source, RuntimeWakeupSource::SceneChange);
    }

    #[test]
    fn typed_deadline_owns_one_portal_scene_change_generation() {
        let wake = WindowedWake::disconnected();
        wake.mark_main_work_pending(RuntimeWakeupSource::TtlDeadline);
        let settle_checkpoint = wake.main_work_checkpoint();
        let compositor_checkpoint = wake.compositor().checkpoint();

        assert!(wake.finish_main_work_after_settle(settle_checkpoint, true));
        assert_eq!(wake.compositor().checkpoint(), compositor_checkpoint + 1);
        let observed = wake.compositor().wait(compositor_checkpoint, None);
        assert_eq!(observed.source, RuntimeWakeupSource::TtlDeadline);
    }

    #[test]
    fn arrival_after_drain_checkpoint_is_finished_by_the_next_drain() {
        let wake = WindowedWake::disconnected();
        wake.mark_main_work_pending(RuntimeWakeupSource::SceneChange);
        let first_drain = wake.main_work_checkpoint();

        // This producer arrives after the main thread has completed its drain
        // but before that drain acknowledges its checkpoint.
        wake.mark_main_work_pending(RuntimeWakeupSource::SceneChange);
        let late_arrival = wake.main_work_checkpoint();
        assert!(late_arrival.generation > first_drain.generation);

        let before_first_finish = wake.compositor().checkpoint();
        assert!(wake.finish_main_work(first_drain));
        let first_post_mutation = wake.compositor().wait(before_first_finish, None);

        // Finishing the earlier checkpoint must not consume the late arrival.
        assert!(wake.finish_main_work(late_arrival));
        let late_post_mutation = wake.compositor().wait(first_post_mutation.generation, None);
        assert!(late_post_mutation.generation > first_post_mutation.generation);
    }

    #[test]
    fn main_work_checkpoint_preserves_each_timer_source_across_late_arrival() {
        let wake = WindowedWake::disconnected();
        wake.mark_main_work_pending(RuntimeWakeupSource::AnimationDeadline);
        let animation = wake.main_work_checkpoint();
        wake.mark_main_work_pending(RuntimeWakeupSource::TtlDeadline);
        let ttl = wake.main_work_checkpoint();

        let before_animation = wake.compositor().checkpoint();
        assert!(wake.finish_main_work(animation));
        let animation_observed = wake.compositor().wait(before_animation, None);
        assert_eq!(
            animation_observed.source,
            RuntimeWakeupSource::AnimationDeadline
        );

        assert!(wake.finish_main_work(ttl));
        let ttl_observed = wake.compositor().wait(animation_observed.generation, None);
        assert_eq!(ttl_observed.source, RuntimeWakeupSource::TtlDeadline);
    }

    #[test]
    fn os_event_generation_notifies_only_after_the_settle_checkpoint_finishes() {
        let wake = WindowedWake::disconnected();
        let compositor_checkpoint = wake.compositor().checkpoint();
        wake.mark_main_work_pending(RuntimeWakeupSource::SceneChange);
        let settle_checkpoint = wake.main_work_checkpoint();
        assert_eq!(wake.compositor().checkpoint(), compositor_checkpoint);

        assert!(wake.finish_main_work(settle_checkpoint));
        let post_settle = wake.compositor().wait(compositor_checkpoint, None);
        assert!(post_settle.generation > compositor_checkpoint);
    }

    #[test]
    fn held_scene_lock_has_no_retry_wake_and_one_main_render_after_release() {
        let wake = WindowedWake::disconnected();
        let lock = Arc::new(Mutex::new(()));
        let guard = lock.lock().expect("test lock must be available");
        let availability_lock = Arc::clone(&lock);
        let compositor_before = wake.compositor().checkpoint();

        wake.schedule_main_after_availability(move || {
            drop(
                availability_lock
                    .lock()
                    .expect("availability waiter must acquire after release"),
            );
        });
        // A duplicate contention observation must join the same completion
        // waiter rather than arm another timer or retry wake.
        let duplicate_lock = Arc::clone(&lock);
        wake.schedule_main_after_availability(move || {
            drop(
                duplicate_lock
                    .lock()
                    .expect("coalesced waiter must acquire after release"),
            );
        });

        assert_eq!(
            wake.compositor().checkpoint(),
            compositor_before,
            "a held lock must not create a periodic compositor retry"
        );
        assert_eq!(
            wake.main_work_checkpoint().generation,
            0,
            "a held lock must not manufacture main work before availability"
        );

        drop(guard);
        let deadline = Instant::now() + Duration::from_secs(1);
        while wake.main_work_checkpoint().generation == 0 && Instant::now() < deadline {
            std::thread::yield_now();
        }
        let checkpoint = wake.main_work_checkpoint();
        assert_eq!(
            checkpoint.generation, 1,
            "release must produce exactly one coalesced main-work completion"
        );
        assert!(
            wake.finish_main_work(checkpoint),
            "the availability event must drive the deferred work's render"
        );
        assert_eq!(
            wake.compositor().checkpoint(),
            compositor_before + 1,
            "release-driven forward progress renders exactly once"
        );
    }

    #[test]
    fn compositor_lock_miss_waits_for_release_instead_of_frame_cadence() {
        let wake = WindowedWake::disconnected();
        let lock = Arc::new(Mutex::new(()));
        let guard = lock.lock().expect("test lock must be available");
        let availability_lock = Arc::clone(&lock);
        let compositor_before = wake.compositor().checkpoint();

        wake.schedule_compositor_after_availability(move || {
            drop(
                availability_lock
                    .lock()
                    .expect("availability waiter must acquire after release"),
            );
        });

        assert_eq!(
            wake.compositor().checkpoint(),
            compositor_before,
            "the failed try_lock must park without a cadence retry"
        );

        drop(guard);
        let deadline = Instant::now() + Duration::from_secs(1);
        while wake.compositor().checkpoint() == compositor_before && Instant::now() < deadline {
            std::thread::yield_now();
        }
        assert_eq!(
            wake.compositor().checkpoint(),
            compositor_before + 1,
            "the compositor must resume once from the lock-release completion"
        );
    }
}
