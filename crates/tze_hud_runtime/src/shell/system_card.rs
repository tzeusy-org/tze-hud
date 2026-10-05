//! Runtime system card and toast model (hud-i2e10.4).
//!
//! A centred card (pairing code + address) or a timed toast, shown by the
//! compositor above all content. It lives here, outside the scene graph, so
//! no agent surface (SceneSnapshot, `hud_surfaces`, zone publish results) can
//! observe it (docs/invariants.md section 3). Time is injected: every method
//! that depends on the clock takes `now_wall_us`.
//!
//! Idle cost: the handle wakes the render loop once on `set`/`clear`; a card
//! with an expiry reports one deadline via [`SystemCardHandle::frame_state`]
//! and the loop sleeps until then, with no per-frame redraw.

use std::sync::{Arc, RwLock};

use tze_hud_compositor::{SystemCardKind, SystemCardModel};
use tze_hud_scene::render_wake::RenderWakeNotifier;

/// How long an update/event toast stays up.
pub const TOAST_TTL_US: u64 = 5_000_000;

/// A card the runtime wants on screen, with an optional wall-clock expiry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SystemCard {
    pub kind: SystemCardKind,
    pub title: String,
    pub lines: Vec<String>,
    pub expires_at_wall_us: Option<u64>,
}

impl SystemCard {
    /// A toast that expires `TOAST_TTL_US` after `now_wall_us`.
    pub fn toast(title: impl Into<String>, now_wall_us: u64) -> Self {
        Self {
            kind: SystemCardKind::Toast,
            title: title.into(),
            lines: Vec::new(),
            expires_at_wall_us: Some(now_wall_us.saturating_add(TOAST_TTL_US)),
        }
    }

    fn model(&self) -> SystemCardModel {
        SystemCardModel {
            kind: self.kind,
            title: self.title.clone(),
            lines: self.lines.clone(),
        }
    }
}

/// What the compositor thread needs for one loop iteration.
#[derive(Debug, PartialEq, Eq)]
pub struct SystemCardFrame {
    /// The card to draw now (`None` when absent or expired).
    pub model: Option<SystemCardModel>,
    /// Wall-clock instant the loop must wake at to expire the card.
    pub wake_at_wall_us: Option<u64>,
}

/// Shared slot for the current card, stored like `ChromeState`.
#[derive(Clone, Default)]
pub struct SystemCardHandle {
    slot: Arc<RwLock<Option<SystemCard>>>,
    render_wake: RenderWakeNotifier,
}

impl SystemCardHandle {
    pub fn new(render_wake: RenderWakeNotifier) -> Self {
        Self {
            slot: Arc::default(),
            render_wake,
        }
    }

    /// Show `card`, replacing any current one, and wake the render loop.
    pub fn set(&self, card: SystemCard) {
        *self.slot.write().unwrap_or_else(|e| e.into_inner()) = Some(card);
        self.render_wake.notify();
    }

    /// Replace the card with `new` only if `expected` is still the one showing
    /// (a newer code must not be overwritten by a late background result).
    pub fn replace_if_current(&self, expected: &SystemCard, new: SystemCard) -> bool {
        let mut slot = self.slot.write().unwrap_or_else(|e| e.into_inner());
        if slot.as_ref() != Some(expected) {
            return false;
        }
        *slot = Some(new);
        drop(slot);
        self.render_wake.notify();
        true
    }

    /// Remove the card, waking the render loop only if one was showing.
    pub fn clear(&self) {
        let had = self
            .slot
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .take()
            .is_some();
        if had {
            self.render_wake.notify();
        }
    }

    /// Resolve the card for this loop iteration. An expired card is dropped
    /// here, so its deadline is reported exactly once.
    pub fn frame_state(&self, now_wall_us: u64) -> SystemCardFrame {
        let mut slot = self.slot.write().unwrap_or_else(|e| e.into_inner());
        if slot
            .as_ref()
            .and_then(|c| c.expires_at_wall_us)
            .is_some_and(|at| at <= now_wall_us)
        {
            *slot = None;
        }
        SystemCardFrame {
            model: slot.as_ref().map(SystemCard::model),
            wake_at_wall_us: slot.as_ref().and_then(|c| c.expires_at_wall_us),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tze_hud_mcp::{CallerContext, McpConfig, McpServer};
    use tze_hud_scene::graph::SceneGraph;
    use tze_hud_scene::types::ZoneRegistry;

    const CODE: &str = "482913";

    fn pairing_card() -> SystemCard {
        SystemCard {
            kind: SystemCardKind::Pairing,
            title: "Pair an agent".into(),
            lines: vec![CODE.into(), "http://100.64.0.1:9090/pair".into()],
            expires_at_wall_us: None,
        }
    }

    fn counting_handle() -> (SystemCardHandle, Arc<AtomicUsize>) {
        let wakes = Arc::new(AtomicUsize::new(0));
        let w = Arc::clone(&wakes);
        let handle = SystemCardHandle::new(RenderWakeNotifier::new(move || {
            w.fetch_add(1, Ordering::SeqCst);
        }));
        (handle, wakes)
    }

    /// The card is not scene content: neither the scene snapshot an agent
    /// receives on connect nor MCP `hud_surfaces` / zone publish results
    /// contain its text.
    #[tokio::test]
    async fn system_card_absent_from_snapshot_surfaces_and_publish() {
        let (handle, _) = counting_handle();
        handle.set(pairing_card());

        let mut scene = SceneGraph::new(1920.0, 1080.0);
        scene.zone_registry = ZoneRegistry::with_defaults();
        scene.create_tab("Main", 0).unwrap();
        let snapshot_json = scene.take_snapshot(1, 1).to_json().unwrap();
        let snapshot_proto = tze_hud_protocol::proto::session::SceneSnapshot {
            snapshot_json: snapshot_json.clone(),
            ..Default::default()
        };
        let mut bytes = Vec::new();
        prost::Message::encode(&snapshot_proto, &mut bytes).unwrap();
        assert!(!bytes.windows(CODE.len()).any(|w| w == CODE.as_bytes()));
        assert!(!snapshot_json.contains("Pair an agent"));

        let server = McpServer::new(scene).with_config(McpConfig::with_psk("test-key"));
        let ctx = CallerContext::with_bearer("test-key");
        let call = |name: &str, args: serde_json::Value| {
            serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
                "params": {"name": name, "arguments": args}})
            .to_string()
        };
        let surfaces = server
            .dispatch(&call("hud_surfaces", serde_json::json!({})), &ctx)
            .await;
        let published = server
            .dispatch(
                &call(
                    "hud_publish",
                    serde_json::json!({"s": "zone:subtitle", "text": "hi"}),
                ),
                &ctx,
            )
            .await;
        for out in [&surfaces, &published] {
            assert!(
                !out.contains(CODE) && !out.contains("Pair an agent"),
                "{out}"
            );
        }
        assert!(
            surfaces.contains("zone:subtitle"),
            "server answered: {surfaces}"
        );
    }

    /// A toast schedules one deadline wake; once it fires the card is gone and
    /// no further deadline is reported.
    #[test]
    fn expiring_toast_schedules_exactly_one_deadline_wake() {
        let (handle, wakes) = counting_handle();
        handle.set(SystemCard::toast("Updated to dev-abc1234", 1_000));
        assert_eq!(wakes.load(Ordering::SeqCst), 1, "set wakes the loop once");

        let expiry = 1_000 + TOAST_TTL_US;
        let shown = handle.frame_state(2_000);
        assert!(shown.model.is_some());
        assert_eq!(shown.wake_at_wall_us, Some(expiry));

        let expired = handle.frame_state(expiry);
        assert_eq!(
            expired,
            SystemCardFrame {
                model: None,
                wake_at_wall_us: None
            }
        );
        assert_eq!(handle.frame_state(expiry + 1).wake_at_wall_us, None);
        assert_eq!(
            wakes.load(Ordering::SeqCst),
            1,
            "expiry rides the deadline, not a notify"
        );
    }

    /// A card without an expiry (pairing) never schedules a wake; clear wakes
    /// only when something was showing.
    #[test]
    fn pairing_card_has_no_deadline_and_clear_wakes_once() {
        let (handle, wakes) = counting_handle();
        handle.clear();
        assert_eq!(
            wakes.load(Ordering::SeqCst),
            0,
            "clearing nothing is silent"
        );
        handle.set(pairing_card());
        assert_eq!(handle.frame_state(u64::MAX - 1).wake_at_wall_us, None);
        handle.clear();
        assert_eq!(wakes.load(Ordering::SeqCst), 2);
        assert_eq!(handle.frame_state(0).model, None);
    }
}
