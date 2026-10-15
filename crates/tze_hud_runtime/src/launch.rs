//! Where each launch setting came from, so the control panel can say so.
//!
//! Precedence, highest first: a command-line flag, an environment variable,
//! `[runtime]` in `tze_hud.toml`, the default. A flag or variable pins the
//! value: the panel shows it read-only and names it, because a restart
//! reuses the same argv and environment and would put it back.

/// The origin of one setting's value.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Source {
    #[default]
    Default,
    /// `[runtime]` in the config file.
    Config,
    /// An environment variable, by name.
    Env(&'static str),
    /// A command-line flag, by name.
    Flag(&'static str),
}

impl Source {
    /// A flag or environment variable sets it; the panel cannot change it.
    pub fn is_pinned(self) -> bool {
        matches!(self, Self::Env(_) | Self::Flag(_))
    }

    /// The row's one-line "where this comes from".
    pub fn describe(self) -> String {
        match self {
            Self::Default => "Default".to_owned(),
            Self::Config => "Saved to config".to_owned(),
            Self::Env(name) | Self::Flag(name) => format!("Set by {name}"),
        }
    }

    /// Record a config value unless something higher already set one.
    /// Returns whether `config` was taken.
    pub fn take_config<T>(&mut self, value: &mut T, config: Option<T>) -> bool {
        match config {
            Some(v) if *self == Self::Default => {
                *value = v;
                *self = Self::Config;
                true
            }
            _ => false,
        }
    }
}

/// The sources of the settings the control panel shows.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LaunchSources {
    pub mcp_port: Source,
    pub grpc_port: Source,
    pub fps: Source,
    pub log_level: Source,
    pub debug_zones: Source,
}

/// The settings that change while the HUD runs: the frame rate cap and the
/// zone outlines. The control panel writes them; the compositor thread reads
/// them when [`LiveSettings::if_changed`] says they moved, so an unchanged
/// setting costs nothing per frame.
pub(crate) struct LiveSettings {
    fps: std::sync::atomic::AtomicU32,
    zone_outlines: std::sync::atomic::AtomicBool,
    generation: std::sync::atomic::AtomicU64,
    wake: tze_hud_scene::render_wake::RenderWakeNotifier,
}

impl LiveSettings {
    pub(crate) fn new(
        fps: u32,
        zone_outlines: bool,
        wake: tze_hud_scene::render_wake::RenderWakeNotifier,
    ) -> Self {
        Self {
            fps: fps.max(1).into(),
            zone_outlines: zone_outlines.into(),
            generation: 0.into(),
            wake,
        }
    }

    pub(crate) fn fps(&self) -> u32 {
        self.fps.load(std::sync::atomic::Ordering::Acquire)
    }

    pub(crate) fn zone_outlines(&self) -> bool {
        self.zone_outlines
            .load(std::sync::atomic::Ordering::Acquire)
    }

    /// Cap the frame rate at `fps` (at least 1) from the next frame.
    pub(crate) fn set_fps(&self, fps: u32) {
        self.fps
            .store(fps.max(1), std::sync::atomic::Ordering::Release);
        self.changed();
    }

    /// Draw or stop drawing zone outlines from the next frame.
    pub(crate) fn set_zone_outlines(&self, on: bool) {
        self.zone_outlines
            .store(on, std::sync::atomic::Ordering::Release);
        self.changed();
    }

    fn changed(&self) {
        self.generation
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        self.wake.notify();
    }

    /// The frame loop's step: when something changed since `seen`, re-pace
    /// `interval` and pass the outlines to `set_outlines`. Returns what
    /// `set_outlines` did, i.e. whether a frame must be drawn (a new cap only
    /// re-paces).
    pub(crate) fn apply(
        &self,
        seen: &mut u64,
        interval: &mut std::time::Duration,
        set_outlines: impl FnOnce(bool) -> bool,
    ) -> bool {
        self.if_changed(seen).is_some_and(|(fps, outlines)| {
            *interval = frame_interval(fps);
            tracing::info!(fps, outlines, "compositor: live settings changed");
            set_outlines(outlines)
        })
    }

    /// `(fps, zone_outlines)` when they changed since `seen`, updating it.
    pub(crate) fn if_changed(&self, seen: &mut u64) -> Option<(u32, bool)> {
        let now = self.generation.load(std::sync::atomic::Ordering::Acquire);
        (now != *seen).then(|| {
            *seen = now;
            (self.fps(), self.zone_outlines())
        })
    }
}

/// The time between frames at `fps` (at least 1).
pub(crate) fn frame_interval(fps: u32) -> std::time::Duration {
    std::time::Duration::from_micros(1_000_000 / u64::from(fps.max(1)))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A flag or variable keeps its value against config; the default does
    /// not; a missing config value changes nothing.
    #[test]
    fn config_fills_only_what_nothing_higher_set() {
        let (mut port, mut source) = (9090u16, Source::Default);
        assert!(source.take_config(&mut port, Some(9290)));
        assert_eq!((port, source), (9290, Source::Config));

        let (mut port, mut source) = (9191u16, Source::Env("TZE_HUD_MCP_PORT"));
        assert!(!source.take_config(&mut port, Some(9290)));
        assert_eq!(port, 9191);
        assert!(source.is_pinned());
        assert_eq!(source.describe(), "Set by TZE_HUD_MCP_PORT");

        let (mut port, mut source) = (9090u16, Source::Default);
        assert!(!source.take_config(&mut port, None));
        assert_eq!((port, source), (9090, Source::Default));
        assert!(!Source::Config.is_pinned());
    }

    /// The frame loop re-paces on a new cap without drawing, draws once when
    /// the outlines change, and does nothing when nothing moved.
    #[test]
    fn frame_loop_step_repaces_and_draws_only_for_outlines() {
        let live = LiveSettings::new(60, false, Default::default());
        let (mut seen, mut interval) = (0, frame_interval(60));
        let mut outlines = false;
        let mut set = |on: bool| std::mem::replace(&mut outlines, on) != on;

        assert!(!live.apply(&mut seen, &mut interval, &mut set));
        live.set_fps(30);
        assert!(
            !live.apply(&mut seen, &mut interval, &mut set),
            "a cap only re-paces"
        );
        assert_eq!(interval, std::time::Duration::from_micros(33_333));
        live.set_zone_outlines(true);
        assert!(
            live.apply(&mut seen, &mut interval, &mut set),
            "outlines draw"
        );
        assert!(!live.apply(&mut seen, &mut interval, &mut set), "once");
    }

    /// The frame loop sees a change once, and nothing when nothing moved.
    #[test]
    fn live_settings_report_each_change_once() {
        let live = LiveSettings::new(60, false, Default::default());
        let mut seen = 0;
        assert_eq!(live.if_changed(&mut seen), None);
        live.set_fps(30);
        live.set_zone_outlines(true);
        assert_eq!(live.if_changed(&mut seen), Some((30, true)));
        assert_eq!(live.if_changed(&mut seen), None);
        live.set_fps(0);
        assert_eq!(live.if_changed(&mut seen), Some((1, true)), "never zero");
    }
}
