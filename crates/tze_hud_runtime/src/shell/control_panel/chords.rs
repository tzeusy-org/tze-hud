//! The global chords as they are now, and how the panel changes one.

use std::sync::{Arc, Mutex, RwLock};

use tze_hud_scene::config::{ChordRole, Hotkey, HudChords};

/// Registers `hotkey` for `role` with Windows in place of its current chord
/// (keeping the current one when Windows refuses). Blocking.
pub(crate) type Rebinder = Arc<dyn Fn(ChordRole, Hotkey) -> Result<(), String> + Send + Sync>;

/// The live chords, shared by the panel's rows, its hints and the closed
/// notice.
#[derive(Clone)]
pub(crate) struct ChordControl {
    current: Arc<RwLock<HudChords>>,
    /// Held across a rebind's clash check, registration and update, so two
    /// edits cannot both pass the check and bind one chord twice.
    changing: Arc<Mutex<()>>,
    /// `None`: chords cannot change while running (no hotkey thread).
    pub rebind: Option<Rebinder>,
}

impl ChordControl {
    pub(crate) fn new(chords: HudChords, rebind: Option<Rebinder>) -> Self {
        Self {
            current: Arc::new(RwLock::new(chords)),
            changing: Arc::default(),
            rebind,
        }
    }

    /// Chords that never change while running.
    #[cfg(test)]
    pub(crate) fn fixed(chords: HudChords) -> Self {
        Self::new(chords, None)
    }

    pub(crate) fn get(&self) -> HudChords {
        *self.current.read().unwrap_or_else(|e| e.into_inner())
    }

    /// Bind `role` to `hotkey` now: refused when another role uses it or
    /// Windows does; the current chord stays bound on failure.
    pub(crate) fn rebind(&self, role: ChordRole, hotkey: Hotkey) -> Result<(), String> {
        let rebind = self
            .rebind
            .as_ref()
            .ok_or("Chords are fixed in this build")?;
        let _changing = self.changing.lock().unwrap_or_else(|e| e.into_inner());
        let current = self.get();
        if current.get(role) == hotkey {
            return Ok(());
        }
        if let Some(other) = current.role_of(&hotkey) {
            return Err(format!("{hotkey} is already the {}", label(other)));
        }
        rebind(role, hotkey)?;
        self.current
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .set(role, hotkey);
        Ok(())
    }
}

/// "pause chord", "panel chord", "pairing chord".
pub(crate) fn label(role: ChordRole) -> &'static str {
    match role {
        ChordRole::Pause => "pause chord",
        ChordRole::Panel => "panel chord",
        ChordRole::Pairing => "pairing chord",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// A rebind reaches the registrar once, then shows; a chord another role
    /// has, or one Windows refuses, changes nothing.
    #[test]
    fn rebinding_refuses_clashes_and_keeps_the_old_chord_on_failure() {
        let calls: Arc<Mutex<Vec<String>>> = Arc::default();
        let seen = Arc::clone(&calls);
        let chords = ChordControl::new(
            HudChords::default(),
            Some(Arc::new(move |role, hotkey: Hotkey| {
                seen.lock().unwrap().push(format!("{role:?} {hotkey}"));
                if hotkey.to_string() == "Ctrl+Alt+F1" {
                    Err("Another program uses Ctrl+Alt+F1".into())
                } else {
                    Ok(())
                }
            })),
        );
        let parse = |s| Hotkey::parse(s).unwrap();

        chords
            .rebind(ChordRole::Panel, parse("Ctrl+Shift+F9"))
            .unwrap();
        assert_eq!(chords.get().panel.to_string(), "Ctrl+Shift+F9");
        let err = chords
            .rebind(ChordRole::Panel, parse("Ctrl+Shift+F12"))
            .unwrap_err();
        assert_eq!(err, "Ctrl+Shift+F12 is already the pause chord");
        let err = chords
            .rebind(ChordRole::Panel, parse("Ctrl+Alt+F1"))
            .unwrap_err();
        assert!(err.contains("Another program"));
        assert_eq!(chords.get().panel.to_string(), "Ctrl+Shift+F9", "kept");
        chords
            .rebind(ChordRole::Panel, parse("Ctrl+Shift+F9"))
            .unwrap();
        assert_eq!(
            *calls.lock().unwrap(),
            ["Panel Ctrl+Shift+F9", "Panel Ctrl+Alt+F1"]
        );

        let fixed = ChordControl::fixed(HudChords::default());
        assert!(
            fixed
                .rebind(ChordRole::Panel, parse("Ctrl+Shift+F9"))
                .is_err()
        );
    }
}
