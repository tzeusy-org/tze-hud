//! Go to session is a local card control. Discover titles off-thread; only an
//! explicit press can carry a result back to the winit thread for activation.

use std::collections::HashMap;
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use tze_hud_scene::graph::overlay::{GoToCue, NotificationGeometry};
use tze_hud_scene::render_wake::RenderWakeNotifier;
use tze_hud_scene::{Rect, SceneGraph, SceneId};

use super::window_match::{self, Match, Target, Window};
use super::{WinitApp, lifecycle};

type Candidates = Vec<(SceneId, String)>;

struct Inspector {
    apps: Vec<String>,
    windows: Box<dyn Fn() -> Vec<Window> + Send + Sync>,
}

impl Inspector {
    fn matches(&self, windows: &[Window], token: &str) -> Match {
        window_match::find(windows, std::process::id(), &self.apps, token)
    }
}

fn inspect<T: Send + 'static>(
    work: impl FnOnce() -> T + Send + 'static,
    wake: RenderWakeNotifier,
) -> std::io::Result<mpsc::Receiver<T>> {
    let (tx, rx) = mpsc::sync_channel(1);
    std::thread::Builder::new()
        .name("HUD window lookup".into())
        .spawn(move || {
            let _ = tx.send(work());
            wake.notify();
        })?;
    Ok(rx)
}

fn take_result<T>(worker: &mut Option<mpsc::Receiver<T>>) -> Option<T> {
    match worker.as_ref()?.try_recv() {
        Ok(value) => {
            *worker = None;
            Some(value)
        }
        Err(mpsc::TryRecvError::Disconnected) => {
            *worker = None;
            None
        }
        Err(mpsc::TryRecvError::Empty) => None,
    }
}

#[derive(Clone, Copy)]
struct Press {
    id: SceneId,
    stamp: u64,
    bounds: Rect,
    foreground: Option<isize>,
    until: Instant,
}

struct PendingPress {
    press: Press,
    worker: mpsc::Receiver<Match>,
    timed_out: bool,
}

type Feedback = (Press, Option<GoToCue>);

#[derive(Default)]
pub(super) struct WindowGoTo {
    inspector: Option<Arc<Inspector>>,
    scan_due: bool,
    scan: Option<mpsc::Receiver<Vec<SceneId>>>,
    presses: Vec<PendingPress>,
    feedback: Vec<Feedback>,
    cue_deadlines: HashMap<SceneId, (u64, Instant)>,
    #[cfg(test)]
    test_activate: Option<Box<dyn FnMut(Target) -> bool>>,
    pub(super) activations: u64,
}

impl WindowGoTo {
    #[cfg(target_os = "windows")]
    pub(super) fn new(apps: Option<Vec<String>>) -> Self {
        let apps = apps.unwrap_or_else(|| {
            window_match::DEFAULT_APPS
                .iter()
                .map(|app| (*app).into())
                .collect()
        });
        if apps.is_empty() {
            return Self::default();
        }
        Self {
            inspector: Some(Arc::new(Inspector {
                apps,
                windows: Box::new(window_match::native::enumerate),
            })),
            ..Self::default()
        }
    }

    #[cfg(not(target_os = "windows"))]
    pub(super) fn new(_apps: Option<Vec<String>>) -> Self {
        Self::default()
    }

    pub(super) fn next_deadline(&self) -> Option<Instant> {
        self.cue_deadlines
            .values()
            .map(|(_, due)| *due)
            .chain(
                self.presses
                    .iter()
                    .filter(|pending| !pending.timed_out)
                    .map(|pending| pending.press.until),
            )
            .min()
    }

    #[cfg(test)]
    pub(super) fn press_pending_for_test(&self) -> bool {
        !self.presses.is_empty()
    }

    #[cfg(test)]
    pub(super) fn inject(
        &mut self,
        windows: impl Fn() -> Vec<Window> + Send + Sync + 'static,
        activate: impl FnMut(Target) -> bool + 'static,
    ) {
        self.inspector = Some(Arc::new(Inspector {
            apps: vec!["Code.exe".into()],
            windows: Box::new(windows),
        }));
        self.test_activate = Some(Box::new(activate));
    }
}

fn presented_candidates(scene: &SceneGraph) -> Candidates {
    scene
        .overlay
        .notification_geometry
        .iter()
        .filter(|card| {
            card.chosen.is_none()
                && card.has_window_caption
                && !scene
                    .overlay
                    .notification_go_to
                    .contains_key(&card.publication_id)
        })
        .filter_map(|card| card_token(scene, card).map(|token| (card.publication_id, token.into())))
        .collect()
}

fn card_token<'a>(scene: &'a SceneGraph, card: &NotificationGeometry) -> Option<&'a str> {
    let now = scene.now_wall_us();
    scene
        .zone_registry
        .active_for_zone(&card.zone_name)
        .iter()
        .find(|record| {
            record.publication_id == card.publication_id
                && record.expires_at_wall_us.is_none_or(|expiry| expiry > now)
        })?
        .window_token
        .as_deref()
}

impl WinitApp {
    pub(super) fn go_to_presented(&mut self) {
        if self.state.window_go_to.inspector.is_some() {
            self.state.window_go_to.scan_due = true;
            if self.settle_go_to(Instant::now()) {
                self.state
                    .wake
                    .notify_compositor(crate::idle_efficiency::RuntimeWakeupSource::SceneChange);
            }
        }
    }

    pub(super) fn press_go_to(&mut self, id: SceneId, token: String, stamp: u64, bounds: Rect) {
        let state = &mut self.state.window_go_to;
        let Some(inspector) = state.inspector.clone() else {
            return;
        };
        let now = Instant::now();
        let press = Press {
            id,
            stamp,
            bounds,
            foreground: foreground(),
            until: now + Duration::from_secs(2),
        };
        if !state.presses.iter().any(|pending| pending.press.id == id) {
            let wake = self.state.wake.main_work_notifier();
            match inspect(
                move || inspector.matches(&(inspector.windows)(), &token),
                wake,
            ) {
                Ok(worker) => state.presses.push(PendingPress {
                    press,
                    worker,
                    timed_out: false,
                }),
                Err(_) => state.feedback.push((press, Some(GoToCue::Blocked))),
            }
        } else {
            state.feedback.push((press, Some(GoToCue::Blocked)));
        }
        self.state.wake.main_work_notifier().notify();
    }

    pub(super) fn settle_go_to(&mut self, now: Instant) -> bool {
        let state = &mut self.state.window_go_to;
        if !state.scan_due
            && state.scan.is_none()
            && state.presses.is_empty()
            && state.feedback.is_empty()
            && !state.cue_deadlines.values().any(|(_, at)| now >= *at)
        {
            return false;
        }
        let shared = Arc::clone(&self.state.shared_state);
        let Ok(shared) = shared.try_lock() else {
            self.schedule_shared_scene_availability_wake();
            return false;
        };
        let Ok(mut scene) = shared.scene.try_lock() else {
            self.schedule_shared_scene_availability_wake();
            return false;
        };
        let mut changed = false;
        if let Some(found) = take_result(&mut state.scan) {
            scene.prune_notification_feedback();
            for id in found {
                if let Some(availability) = scene.overlay.notification_go_to.get_mut(&id) {
                    availability.available = true;
                    changed = true;
                }
            }
        }
        if state.scan_due && state.scan.is_none() {
            state.scan_due = false;
            let candidates = presented_candidates(&scene);
            if !candidates.is_empty() {
                for (id, _) in &candidates {
                    scene.overlay.notification_go_to.entry(*id).or_default();
                }
                if let Some(probe) = state.inspector.clone() {
                    state.scan = inspect(
                        move || {
                            let windows = (probe.windows)();
                            candidates
                                .into_iter()
                                .filter(|(_, token)| {
                                    matches!(probe.matches(&windows, token), Match::One(_))
                                })
                                .map(|(id, _)| id)
                                .collect()
                        },
                        self.state.wake.main_work_notifier(),
                    )
                    .ok();
                }
            }
        }
        state.cue_deadlines.retain(|id, (stamp, due)| {
            let Some(control) = scene.overlay.notification_go_to.get_mut(id) else {
                return false;
            };
            if control.attempted_at_mono_us != Some(*stamp) {
                return false;
            }
            if now < *due {
                return true;
            }
            control.cue = None;
            changed = true;
            false
        });
        let hold = self
            .state
            .global_tokens
            .get("notification.goto.feedback.ms")
            .and_then(|value| value.trim().parse::<u32>().ok())
            .unwrap_or(1200);
        for (press, cue) in state.feedback.drain(..) {
            let Some(control) = scene.overlay.notification_go_to.get_mut(&press.id) else {
                continue;
            };
            if control.attempted_at_mono_us == Some(press.stamp) {
                control.cue = cue;
                if cue.is_some() {
                    state.cue_deadlines.insert(
                        press.id,
                        (press.stamp, now + Duration::from_millis(hold.into())),
                    );
                } else {
                    scene.focus_notification(None);
                }
                changed = true;
            }
        }
        let mut completed = Vec::new();
        state.presses.retain_mut(|pending| {
            let result = pending.worker.try_recv();
            if !pending.timed_out {
                if now >= pending.press.until {
                    completed.push((pending.press, Err(mpsc::TryRecvError::Disconnected)));
                    // Keep this card's worker slot until its exit so repeated
                    // timeouts cannot spawn more workers for one publication.
                    pending.timed_out = true;
                } else if !matches!(result, Err(mpsc::TryRecvError::Empty)) {
                    completed.push((pending.press, result));
                }
            }
            matches!(result, Err(mpsc::TryRecvError::Empty))
        });
        if !completed.is_empty() {
            scene.prune_notification_feedback();
            completed.retain(|(press, _)| {
                scene
                    .overlay
                    .notification_go_to
                    .get(&press.id)
                    .is_some_and(|control| control.attempted_at_mono_us == Some(press.stamp))
                    && scene.overlay.zone_hit_regions.iter().any(|region| {
                        region.publication_id == press.id
                            && region.kind == tze_hud_scene::ZoneInteractionKind::GoTo
                    })
            });
        }
        if changed {
            scene.bump_geometry_epoch();
        }
        drop(scene);
        drop(shared);

        for (press, result) in completed {
            let cue = match result {
                Ok(Match::None) => Some(GoToCue::NotFound),
                Ok(Match::Multiple(count)) => Some(GoToCue::Ambiguous(count)),
                Ok(Match::One(target))
                    if press.foreground == foreground()
                        && !self
                            .state
                            .safe_mode_atomic
                            .load(std::sync::atomic::Ordering::Acquire)
                        && self.activate_go_to(target, press.bounds) =>
                {
                    None
                }
                _ => Some(GoToCue::Blocked),
            };
            if cue.is_none() {
                lifecycle::discard_foreground_origin();
                self.state.answer_keyboard.focus = None;
                self.state.answer_keyboard.return_key = None;
                self.state.window_go_to.activations += 1;
                self.state
                    .surface_ui
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .window_activations = self.state.window_go_to.activations;
            }
            self.state.window_go_to.feedback.push((press, cue));
            self.state.wake.main_work_notifier().notify();
        }
        changed
    }

    fn activate_go_to(&mut self, target: Target, bounds: Rect) -> bool {
        #[cfg(test)]
        if let Some(injected) = self.state.window_go_to.test_activate.as_mut() {
            return injected(target);
        }
        #[cfg(target_os = "windows")]
        {
            let display = self.display_window_at(
                bounds.x + bounds.width * 0.5,
                bounds.y + bounds.height * 0.5,
            );
            let overlay = display
                .and_then(|index| self.state.secondaries.get(index))
                .map(|display| &display.window)
                .or(self.state.window.as_ref())
                .and_then(|window| lifecycle::hwnd_for_window(window));
            windows_activate(target, overlay)
        }
        #[cfg(not(target_os = "windows"))]
        {
            let _ = (target, bounds);
            false
        }
    }
}

fn foreground() -> Option<isize> {
    #[cfg(target_os = "windows")]
    {
        lifecycle::foreground_window()
    }
    #[cfg(not(target_os = "windows"))]
    {
        None
    }
}

#[cfg(target_os = "windows")]
fn windows_activate(target: Target, overlay: Option<windows::Win32::Foundation::HWND>) -> bool {
    use window_match::{Foreground, native};
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::{
        GetForegroundWindow, IsWindow, SW_RESTORE, SetForegroundWindow, ShowWindowAsync,
    };

    struct Win32 {
        overlay: Option<HWND>,
        previous: Option<isize>,
    }

    impl Foreground for Win32 {
        fn restore_minimized(&mut self, target: Target) -> bool {
            // SAFETY: native::hwnd verifies the destination's process.
            native::hwnd(target)
                .is_some_and(|hwnd| unsafe { ShowWindowAsync(hwnd, SW_RESTORE).as_bool() })
        }

        fn raise(&mut self, target: Target) -> bool {
            // SAFETY: native::hwnd verifies the destination's process.
            native::hwnd(target).is_some_and(|hwnd| unsafe {
                hwnd == GetForegroundWindow() || SetForegroundWindow(hwnd).as_bool()
            })
        }

        fn acquire_overlay(&mut self) -> bool {
            self.overlay
                .is_some_and(|hwnd| lifecycle::acquire_window_foreground(hwnd, || {}))
        }

        fn release_overlay(&mut self) {
            // SAFETY: Release only our temporary foreground. A subsequent
            // user switch wins, as for an ordinary explicit HUD exit.
            unsafe {
                if self.overlay == Some(GetForegroundWindow())
                    && let Some(previous) = self.previous
                {
                    let previous = HWND(previous as *mut std::ffi::c_void);
                    if IsWindow(previous).as_bool() {
                        let _ = SetForegroundWindow(previous);
                    }
                }
            }
        }
    }

    window_match::activate(
        &mut Win32 {
            overlay,
            previous: foreground(),
        },
        target,
    )
}
