//! Which `RegisterHotKey` id holds each chord on the hotkey thread, and the
//! order a rebind takes: the new chord goes in under the role's other id
//! before the old id is released, so a refused chord leaves the old one
//! working. Kept free of Win32 so it is tested on every platform.

use tze_hud_scene::config::{ChordRole, Hotkey, HudChords};

/// Added to (or taken from) a role's base id on each rebind.
const ALT_ID: i32 = 10;

/// The role's base id; `WM_HOTKEY` carries an id back in `wParam`.
fn base_id(role: ChordRole) -> i32 {
    match role {
        ChordRole::Pause => 1,
        ChordRole::Panel => 2,
        ChordRole::Pairing => 3,
    }
}

fn slot(role: ChordRole) -> usize {
    ChordRole::ALL
        .iter()
        .position(|r| *r == role)
        .expect("every role is in ALL")
}

/// The chords and the id each is registered under now.
pub(super) struct Bindings {
    ids: [i32; 3],
    chords: HudChords,
}

impl Bindings {
    pub(super) fn new(chords: HudChords) -> Self {
        Self {
            ids: ChordRole::ALL.map(base_id),
            chords,
        }
    }

    pub(super) fn id(&self, role: ChordRole) -> i32 {
        self.ids[slot(role)]
    }

    pub(super) fn chord(&self, role: ChordRole) -> Hotkey {
        self.chords.get(role)
    }

    /// The role a `WM_HOTKEY` id belongs to (either of its ids).
    pub(super) fn role_for_id(id: usize) -> Option<ChordRole> {
        ChordRole::ALL
            .into_iter()
            .find(|role| base_id(*role) as usize == id % ALT_ID as usize)
    }

    /// Bind `role` to `hotkey`: `register` it under the role's other id, and
    /// only once that worked `unregister` the old id. On error nothing
    /// changes and the old chord stays registered.
    pub(super) fn rebind<E>(
        &mut self,
        role: ChordRole,
        hotkey: Hotkey,
        register: impl FnOnce(i32, Hotkey) -> Result<(), E>,
        unregister: impl FnOnce(i32),
    ) -> Result<(), E> {
        let old = self.id(role);
        let new = if old > ALT_ID {
            old - ALT_ID
        } else {
            old + ALT_ID
        };
        register(new, hotkey)?;
        unregister(old);
        self.ids[slot(role)] = new;
        self.chords.set(role, hotkey);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// Ids alternate per role, the new chord is in before the old id goes,
    /// a refused chord changes nothing, and presses on either id find the
    /// role.
    #[test]
    fn rebinding_registers_first_and_keeps_the_old_chord_on_refusal() {
        let mut bindings = Bindings::new(HudChords::default());
        let log = RefCell::new(Vec::new());
        let f9 = Hotkey::parse("Ctrl+Shift+F9").unwrap();
        let f8 = Hotkey::parse("Ctrl+Shift+F8").unwrap();
        let ok = |id, hk: Hotkey| -> Result<(), ()> {
            log.borrow_mut().push(format!("register {id} {hk}"));
            Ok(())
        };
        let release = |id| log.borrow_mut().push(format!("unregister {id}"));

        bindings.rebind(ChordRole::Panel, f9, ok, release).unwrap();
        assert_eq!(bindings.id(ChordRole::Panel), 12);
        assert_eq!(bindings.chord(ChordRole::Panel), f9);
        bindings.rebind(ChordRole::Panel, f8, ok, release).unwrap();
        assert_eq!(bindings.id(ChordRole::Panel), 2);
        assert_eq!(
            *log.borrow(),
            [
                "register 12 Ctrl+Shift+F9",
                "unregister 2",
                "register 2 Ctrl+Shift+F8",
                "unregister 12"
            ]
        );

        log.borrow_mut().clear();
        let refused = bindings.rebind(
            ChordRole::Panel,
            f9,
            |_, _| Err("in use"),
            |id| log.borrow_mut().push(format!("unregister {id}")),
        );
        assert_eq!(refused, Err("in use"));
        assert!(log.borrow().is_empty(), "the old chord is never released");
        assert_eq!(
            (
                bindings.id(ChordRole::Panel),
                bindings.chord(ChordRole::Panel)
            ),
            (2, f8)
        );
        assert_eq!(bindings.id(ChordRole::Pause), 1, "other roles untouched");

        for (id, role) in [
            (1, ChordRole::Pause),
            (12, ChordRole::Panel),
            (13, ChordRole::Pairing),
        ] {
            assert_eq!(Bindings::role_for_id(id), Some(role));
        }
        assert_eq!(Bindings::role_for_id(7), None);
    }
}
