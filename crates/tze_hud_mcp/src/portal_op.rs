//! Portal operations the MCP server sends to the runtime's portal driver.
//!
//! The driver owns the [`PortalHub`] on the winit event-loop thread; the MCP
//! HTTP server runs on Tokio. Each `portal:<id>` verb becomes one [`PortalOp`]
//! on an unbounded channel, and the driver answers on the op's oneshot. The
//! type lives here because `tze_hud_runtime` depends on `tze_hud_mcp`, not
//! the other way around.
//!
//! Every op carries the authenticated agent id; portals are keyed by
//! (agent, portal id), so there is nothing secret to hand back to the model.

use tokio::sync::oneshot::Sender;
use tze_hud_projection::hub::{
    InputBatch, PortalError, PortalHub, PortalKey, PortalStatus, PortalSummary, Publish,
};

#[derive(Debug)]
pub enum PortalOp {
    /// The agent's live portals, for `hud_surfaces`.
    List {
        agent: String,
        reply: Sender<Vec<PortalSummary>>,
    },
    /// `hud_publish` to `portal:<portal>`; the first one attaches.
    Publish {
        agent: String,
        portal: String,
        display_name: Option<String>,
        text: Option<String>,
        key: Option<String>,
        expects_reply: bool,
        status: Option<String>,
        reply: Sender<Result<(), PortalError>>,
    },
    /// `hud_hold`: keep the portal for `ttl_ms` (0 = until cleared).
    Hold {
        agent: String,
        portal: String,
        ttl_ms: u64,
        reply: Sender<Result<(), PortalError>>,
    },
    /// `hud_input`: drop acked items, return queued input across the agent's
    /// portals.
    Input {
        agent: String,
        ack: Vec<String>,
        max_items: Option<usize>,
        reply: Sender<InputBatch>,
    },
    /// `hud_clear`: detach the portal.
    Clear {
        agent: String,
        portal: String,
        reply: Sender<Result<(), PortalError>>,
    },
}

impl PortalOp {
    /// Apply the op to `hub` and answer it. Returns the key of a portal this
    /// op cleared, so the caller can take its surface down.
    pub fn apply(self, hub: &mut PortalHub, now_us: u64) -> Option<PortalKey> {
        match self {
            Self::List { agent, reply } => {
                let _ = reply.send(hub.list(&agent));
                None
            }
            Self::Publish {
                agent,
                portal,
                display_name,
                text,
                key,
                expects_reply,
                status,
                reply,
            } => {
                let result = status
                    .as_deref()
                    .map(PortalStatus::parse)
                    .transpose()
                    .and_then(|status| {
                        let publish = Publish {
                            display_name,
                            text,
                            key,
                            expects_reply,
                            status,
                        };
                        hub.publish(&PortalKey::new(agent, portal), publish, now_us)
                    });
                let _ = reply.send(result);
                None
            }
            Self::Hold {
                agent,
                portal,
                ttl_ms,
                reply,
            } => {
                let _ = reply.send(hub.hold(&PortalKey::new(agent, portal), ttl_ms, now_us));
                None
            }
            Self::Input {
                agent,
                ack,
                max_items,
                reply,
            } => {
                let _ = reply.send(hub.poll_input(&agent, &ack, max_items, now_us));
                None
            }
            Self::Clear {
                agent,
                portal,
                reply,
            } => {
                let key = PortalKey::new(agent, portal);
                let result = hub.clear(&key);
                let cleared = result.is_ok().then_some(key);
                let _ = reply.send(result);
                cleared
            }
        }
    }
}
