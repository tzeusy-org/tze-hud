//! Read-only ownership metadata. No content, cleanup, lease renewal or clock writes.

use super::SceneGraph;
use super::publish::Publication;
use crate::types::{LeaseState, SceneId, ZONE_TILE_Z_MIN};

/// One borrowed ownership record. `id` omits the canonical kind prefix.
/// A visitor can bound its own result without cloning the scene or content.
pub struct Holding<'a> {
    pub owner: &'a str,
    pub id: &'a str,
    pub kind: &'static str,
    pub phase: &'static str,
    pub expiry_kind: &'static str,
    pub deadline_wall_us: Option<u64>,
    pub now_wall_us: u64,
}

struct Lifetime {
    phase: &'static str,
    basis: &'static str,
    deadline: Option<u64>,
}

impl Lifetime {
    fn bound(&mut self, at: u64, basis: &'static str) {
        // Callers visit content, lease, orphan, suspension in tie-break order.
        if self.deadline.is_none_or(|old| at < old) {
            self.deadline = Some(at);
            self.basis = basis;
        }
    }
}

impl SceneGraph {
    /// Visits accepted surface metadata using one clock observation. In particular,
    /// delayed relative widget TTL starts at application, not its planned time.
    pub fn visit_holdings(&self, mut visit: impl FnMut(Holding<'_>)) {
        let now = self.now_wall_us();
        for record in self.zone_registry.active_publishes.values().flatten() {
            let life = self.holding_lifetime(
                record.expires_at_wall_us,
                record.lease_id,
                record.publication_origin,
                "active",
                false,
                now,
            );
            emit_holding(
                &record.publisher_namespace,
                &record.zone_name,
                "zone",
                life,
                now,
                &mut visit,
            );
        }
        for record in self.widget_registry.active_publishes.values().flatten() {
            let paused = self.widget_time_paused();
            let limit = self
                .widget_registry
                .instances
                .get(&record.widget_name)
                .and_then(|i| i.auto_clear_ms);
            let content = super::zone_ops::effective_expiry(record, limit);
            let life = self.holding_lifetime(
                if paused { None } else { content },
                record.lease_id,
                record.publication_origin,
                if paused { "suspended" } else { "active" },
                paused && content.is_some(),
                now,
            );
            emit_holding(
                &record.publisher_namespace,
                &record.widget_name,
                "widget",
                life,
                now,
                &mut visit,
            );
        }
        for pending in &self.scheduled_publications {
            let (id, kind, expiry, uncertain) = match &pending.publication {
                Publication::Zone {
                    zone_name,
                    expires_at_wall_us,
                    content,
                    held,
                    ..
                } => (
                    zone_name,
                    "zone",
                    *expires_at_wall_us,
                    !held
                        && expires_at_wall_us.is_none()
                        && matches!(content, crate::types::ZoneContent::Notification(_)),
                ),
                Publication::Widget {
                    widget_name,
                    expires_at_wall_us,
                    ttl_us,
                    ..
                } => {
                    let auto_clear = self
                        .widget_registry
                        .instances
                        .get(widget_name)
                        .and_then(|i| i.auto_clear_ms);
                    (
                        widget_name,
                        "widget",
                        *expires_at_wall_us,
                        self.widget_time_paused()
                            || (expires_at_wall_us.is_none() && ttl_us.is_some())
                            || auto_clear.is_some(),
                    )
                }
            };
            let paused = kind == "widget" && self.widget_time_paused();
            let content = if uncertain {
                None
            } else {
                expiry.map(|at| at.max(pending.present_at_wall_us))
            };
            let life = self.holding_lifetime(
                content,
                pending.publisher.lease_id,
                pending.publisher.origin,
                if paused { "suspended" } else { "scheduled" },
                uncertain,
                now,
            );
            emit_holding(
                &pending.publisher.namespace,
                id,
                kind,
                life,
                now,
                &mut visit,
            );
        }
        for tile in self.tiles.values() {
            // Zone backing tiles occupy the runtime band; authenticated portal
            // backing tiles have their own runtime namespace, not the agent's.
            if tile.z_order >= ZONE_TILE_Z_MIN || tile.namespace == "tze_hud_portal_driver" {
                continue;
            }
            let life = self.holding_lifetime(
                None,
                Some(tile.lease_id),
                None,
                if tile.present_at.is_some_and(|at| at > now) {
                    "scheduled"
                } else {
                    "active"
                },
                false,
                now,
            );
            emit_holding(
                &tile.namespace,
                &tile.id.to_string(),
                "tile",
                life,
                now,
                &mut visit,
            );
        }
    }

    fn holding_lifetime(
        &self,
        content: Option<u64>,
        lease_id: Option<SceneId>,
        origin: Option<SceneId>,
        phase: &'static str,
        uncertain: bool,
        now_us: u64,
    ) -> Lifetime {
        let mut life = Lifetime {
            phase,
            basis: if uncertain { "unknown" } else { "until_clear" },
            deadline: None,
        };
        if let Some(at) = content {
            life.bound(at, "content");
        }
        if let Some(id) = lease_id {
            if let Some(lease) = self.leases.get(&id) {
                match lease.state {
                    LeaseState::Active | LeaseState::Orphaned => {
                        life.bound(
                            lease
                                .granted_at_ms
                                .saturating_add(lease.ttl_ms)
                                .saturating_mul(1000),
                            "lease",
                        );
                        if lease.state == LeaseState::Orphaned {
                            life.phase = "orphaned";
                            if let Some(at) = lease.disconnected_at_ms {
                                life.bound(
                                    at.saturating_add(lease.grace_period_ms)
                                        .saturating_mul(1000),
                                    "orphan_grace",
                                );
                            }
                        }
                    }
                    LeaseState::Suspended => {
                        life.phase = "suspended";
                        if let Some(at) = lease.suspended_at_ms {
                            life.bound(
                                at.saturating_add(Self::DEFAULT_MAX_SUSPENSION_MS)
                                    .saturating_mul(1000),
                                "suspension_limit",
                            );
                        } else if life.deadline.is_none() {
                            life.basis = "unknown";
                        }
                    }
                    _ => {
                        life.phase = "unknown";
                        if life.deadline.is_none() {
                            life.basis = "unknown";
                        }
                    }
                }
                // This is the same due predicate used by production cleanup,
                // including TTL=0. Never borrow scheduler comments as policy.
                if Self::lease_terminal_state(lease, now_us / 1000, Self::DEFAULT_MAX_SUSPENSION_MS)
                    .is_some()
                    && life.deadline.is_none_or(|at| at > now_us)
                {
                    life.bound(now_us, "lease");
                }
            } else {
                life.phase = "unknown";
                if life.deadline.is_none() {
                    life.basis = "unknown";
                }
            }
        }
        if let Some(at) = origin.and_then(|id| self.publication_orphans.get(&id)) {
            life.phase = "orphaned";
            life.bound(at.saturating_mul(1000), "orphan_grace");
        }
        life
    }
}

fn emit_holding(
    owner: &str,
    id: &str,
    kind: &'static str,
    life: Lifetime,
    now: u64,
    visit: &mut impl FnMut(Holding<'_>),
) {
    visit(Holding {
        owner,
        id,
        kind,
        phase: if life.deadline.is_some_and(|at| at <= now) {
            "cleanup_due"
        } else {
            life.phase
        },
        expiry_kind: life.basis,
        deadline_wall_us: life.deadline,
        now_wall_us: now,
    });
}
