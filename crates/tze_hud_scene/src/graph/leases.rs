use super::*;

impl SceneGraph {
    // ─── Lease operations ────────────────────────────────────────────────

    /// Default maximum suspension time before a suspended lease is revoked (ms).
    /// Default 300,000 ms (5 minutes).
    pub const DEFAULT_MAX_SUSPENSION_MS: u64 = 300_000;

    /// Default grace period for disconnected leases (ms).
    /// Default 30,000 ms (30 seconds).
    pub const DEFAULT_GRACE_PERIOD_MS: u64 = crate::lease::ORPHAN_GRACE_PERIOD_MS;

    /// Budget soft-limit threshold (80% of hard limit).
    pub const BUDGET_SOFT_LIMIT_PCT: f64 = 0.80;

    /// Maximum leases across all agents in the entire runtime (spec §Lease Caps).
    pub const MAX_RUNTIME_LEASES: usize = 64;

    /// Maximum tiles per lease (spec §Lease Caps).
    pub const MAX_TILES_PER_LEASE: u32 = 64;

    /// Maximum nodes per tile (spec §Lease Caps).
    pub const MAX_NODES_PER_TILE: u32 = 64;

    /// Grant a lease with a default (nil) session_id.
    ///
    /// Panics if caps are exceeded (use `try_grant_lease_for_session` for graceful errors).
    pub fn grant_lease(&mut self, namespace: &str, ttl_ms: u64) -> SceneId {
        self.grant_lease_for_session(namespace, SceneId::nil(), ttl_ms)
    }

    /// Grant a lease, enforcing runtime-wide and per-session caps.
    ///
    /// Panics if caps are exceeded (use `try_grant_lease_for_session` for graceful errors).
    pub fn grant_lease_for_session(
        &mut self,
        namespace: &str,
        session_id: SceneId,
        ttl_ms: u64,
    ) -> SceneId {
        self.try_grant_lease_for_session(namespace, session_id, ttl_ms)
            .expect("lease grant failed cap check")
    }

    /// Try to grant a lease, returning an error if runtime or session caps are exceeded.
    ///
    /// Enforces the lease caps:
    /// - Max 64 leases per runtime across all agents (`MAX_RUNTIME_LEASES`).
    pub fn try_grant_lease_for_session(
        &mut self,
        namespace: &str,
        session_id: SceneId,
        ttl_ms: u64,
    ) -> Result<SceneId, LeaseError> {
        self.try_grant_lease_for_session_with_budget(
            namespace,
            session_id,
            ttl_ms,
            ResourceBudget::default(),
        )
    }

    /// Try to grant a lease with the session's frozen effective resource budget.
    ///
    /// Leases carry no permissions: the protocol and MCP boundaries check the
    /// agent's allow list before calling into the scene.
    pub fn try_grant_lease_for_session_with_budget(
        &mut self,
        namespace: &str,
        session_id: SceneId,
        ttl_ms: u64,
        resource_budget: ResourceBudget,
    ) -> Result<SceneId, LeaseError> {
        // Check runtime-wide cap
        let non_terminal_count = self
            .leases
            .values()
            .filter(|l| !l.state.is_terminal())
            .count();
        if non_terminal_count >= Self::MAX_RUNTIME_LEASES {
            return Err(LeaseError::CapsExceeded(
                CapsError::MaxRuntimeLeasesExceeded {
                    current: non_terminal_count,
                    limit: Self::MAX_RUNTIME_LEASES,
                },
            ));
        }

        let id = SceneId::new();
        let now_ms = self.clock.now_millis();
        self.leases.insert(
            id,
            Lease {
                id,
                namespace: namespace.to_string(),
                session_id,
                state: LeaseState::Active,
                granted_at_ms: now_ms,
                ttl_ms,
                resource_budget,
                spatial_budget: Default::default(),
                suspended_at_ms: None,
                ttl_remaining_at_suspend_ms: None,
                disconnected_at_ms: None,
                grace_period_ms: Self::DEFAULT_GRACE_PERIOD_MS,
            },
        );
        self.version += 1;
        Ok(id)
    }

    pub fn revoke_lease(&mut self, lease_id: SceneId) -> Result<(), ValidationError> {
        {
            let lease = self
                .leases
                .get_mut(&lease_id)
                .ok_or(ValidationError::LeaseNotFound { id: lease_id })?;
            if lease.state.is_terminal() {
                return Err(ValidationError::LeaseNotFound { id: lease_id });
            }
            lease.state = LeaseState::Revoked;
        }
        // Remove all tiles associated with this lease.
        let orphaned_tiles: Vec<SceneId> = self
            .tiles
            .values()
            .filter(|t| t.lease_id == lease_id)
            .map(|t| t.id)
            .collect();
        for tile_id in orphaned_tiles {
            self.remove_tile_and_nodes(tile_id);
        }
        // Spec §Requirement: Lease Revocation Clears Zone Publications
        // (lines 235–242): clear this lease's publications on revocation.
        self.clear_publications_for_lease(lease_id);
        self.version += 1;
        Ok(())
    }

    /// Viewer dismiss of a tile: the human override reclaims the tile's whole
    /// lease (invariant 3). Every tile and zone/widget publication the lease
    /// owns is removed and the lease ends `Revoked`, in any non-terminal state
    /// (Active, Orphaned, Suspended), with no agent veto.
    ///
    /// Returns the `LeaseExpiry` the runtime forwards to the owning session
    /// (which sends `Reclaimed{OVERRIDE}`), or `None` for an unknown tile or an
    /// already-terminal lease.
    pub fn viewer_dismiss_tile(&mut self, tile_id: SceneId) -> Option<LeaseExpiry> {
        let lease_id = self.tiles.get(&tile_id)?.lease_id;
        if self.leases.get(&lease_id)?.state.is_terminal() {
            return None;
        }
        let expiry = self.reap_lease(lease_id, LeaseState::Revoked);
        self.version += 1;
        Some(expiry)
    }

    /// Whether a lease is present AND in the `Active` state (mutations allowed).
    ///
    /// Returns `false` for an unknown lease, or for a lease in any non-Active
    /// state (Suspended, Orphaned, Expired, Revoked). This is the correct
    /// liveness predicate for "may I still create/modify tiles under this
    /// lease?" — a lease still resident in the map may be terminal (Expired
    /// leases that `expire_leases` left in place after grace-period reaping).
    ///
    /// Used by the portal driver's lease-reuse decision so that a session
    /// attaching *after* its prior lease's grace period has already expired
    /// gets a FRESH lease rather than re-using the dead one (which would make
    /// `create_tile` fail its `require_active_lease` check and silently produce
    /// no portal).
    pub fn lease_is_active(&self, lease_id: &SceneId) -> bool {
        self.leases.get(lease_id).is_some_and(|l| l.is_active())
    }

    /// Whether a lease is present AND in the `Orphaned` state (disconnected,
    /// inside its grace window). Returns `false` for an unknown lease or any
    /// other state.
    ///
    /// The portal driver uses this to distinguish "owner dropped, grace running"
    /// (reconnect within grace resumes the SAME surface) from a terminal
    /// Expired/Revoked lease (post-grace: a fresh portal under a new lease).
    pub fn lease_is_orphaned(&self, lease_id: &SceneId) -> bool {
        self.leases
            .get(lease_id)
            .is_some_and(|l| l.state == LeaseState::Orphaned)
    }

    pub fn renew_lease(
        &mut self,
        lease_id: SceneId,
        new_ttl_ms: u64,
    ) -> Result<(), ValidationError> {
        let lease = self
            .leases
            .get_mut(&lease_id)
            .ok_or(ValidationError::LeaseNotFound { id: lease_id })?;
        if !lease.is_active() {
            return Err(ValidationError::LeaseNotFound { id: lease_id });
        }
        lease.granted_at_ms = self.clock.now_millis();
        lease.ttl_ms = new_ttl_ms;
        self.version += 1;
        Ok(())
    }

    /// Suspend a lease (safe mode entry). Blocks mutations, preserves state.
    pub fn suspend_lease(&mut self, lease_id: &SceneId, now_ms: u64) -> Result<(), LeaseError> {
        let lease = self
            .leases
            .get_mut(lease_id)
            .ok_or(LeaseError::LeaseNotFound(*lease_id))?;
        lease.suspend(now_ms)?;
        self.version += 1;
        Ok(())
    }

    /// Resume a suspended lease (safe mode exit). Re-enables mutations.
    pub fn resume_lease(&mut self, lease_id: &SceneId, now_ms: u64) -> Result<(), LeaseError> {
        let lease = self
            .leases
            .get_mut(lease_id)
            .ok_or(LeaseError::LeaseNotFound(*lease_id))?;
        lease.resume(now_ms)?;
        self.version += 1;
        Ok(())
    }

    /// Mark a lease as disconnected (agent disconnect, enters grace period).
    ///
    /// Spec §Orphan Handling Grace Period (lines 132–145):
    /// - Lease transitions ACTIVE → ORPHANED.
    /// - TTL clock continues running.
    /// - All tiles owned by this lease receive `TileVisualHint::DisconnectionBadge`
    ///   (compositor must display the badge within 1 frame, spec line 133).
    pub fn disconnect_lease(&mut self, lease_id: &SceneId, now_ms: u64) -> Result<(), LeaseError> {
        let lease = self
            .leases
            .get_mut(lease_id)
            .ok_or(LeaseError::LeaseNotFound(*lease_id))?;
        lease.disconnect(now_ms)?;
        // Set disconnection badge on all tiles owned by this lease.
        // Compositor must render the badge within 1 frame (spec line 133).
        for tile in self.tiles.values_mut() {
            if tile.lease_id == *lease_id {
                tile.visual_hint = crate::lease::TileVisualHint::DisconnectionBadge;
            }
        }
        self.version += 1;
        Ok(())
    }

    /// Reconnect a disconnected lease (agent reconnect within grace period).
    ///
    /// Spec §Orphan Handling Grace Period (lines 139–141):
    /// ORPHANED → ACTIVE; disconnection badges cleared within 1 frame.
    pub fn reconnect_lease(&mut self, lease_id: &SceneId, now_ms: u64) -> Result<(), LeaseError> {
        let lease = self
            .leases
            .get_mut(lease_id)
            .ok_or(LeaseError::LeaseNotFound(*lease_id))?;
        lease.reconnect(now_ms)?;
        // Clear disconnection badge on all tiles owned by this lease.
        // Compositor must clear the badge within 1 frame (spec line 141).
        for tile in self.tiles.values_mut() {
            if tile.lease_id == *lease_id {
                tile.visual_hint = crate::lease::TileVisualHint::None;
            }
        }
        self.version += 1;
        Ok(())
    }

    /// Suspend all active leases (safe mode entry).
    pub fn suspend_all_leases(&mut self, now_ms: u64) {
        let active_ids: Vec<SceneId> = self
            .leases
            .values()
            .filter(|l| l.state == LeaseState::Active)
            .map(|l| l.id)
            .collect();
        for id in active_ids {
            if let Some(lease) = self.leases.get_mut(&id) {
                let _ = lease.suspend(now_ms);
            }
        }
        self.version += 1;
    }

    /// Resume all suspended leases (safe mode exit).
    pub fn resume_all_leases(&mut self, now_ms: u64) {
        let suspended_ids: Vec<SceneId> = self
            .leases
            .values()
            .filter(|l| l.state == LeaseState::Suspended)
            .map(|l| l.id)
            .collect();
        for id in suspended_ids {
            if let Some(lease) = self.leases.get_mut(&id) {
                let _ = lease.resume(now_ms);
            }
        }
        self.version += 1;
    }

    /// Expire all leases past their TTL, handle grace period expiry for
    /// disconnected leases, and handle suspension timeout.
    ///
    /// Returns detailed information about each expired/cleaned-up lease.
    pub fn expire_leases(&mut self) -> Vec<LeaseExpiry> {
        self.expire_leases_with_max_suspend(Self::DEFAULT_MAX_SUSPENSION_MS)
    }

    /// Like `expire_leases` but with a configurable max suspension time.
    pub fn expire_leases_with_max_suspend(&mut self, max_suspend_ms: u64) -> Vec<LeaseExpiry> {
        let now = self.clock.now_millis();

        // Collect leases that need cleanup
        let to_process: Vec<(SceneId, LeaseState)> = self
            .leases
            .values()
            .filter_map(|l| Self::lease_terminal_state(l, now, max_suspend_ms).map(|s| (l.id, s)))
            .collect();

        let mut expiries = Vec::with_capacity(to_process.len());
        for (id, terminal_state) in to_process {
            expiries.push(self.reap_lease(id, terminal_state));
        }

        if !expiries.is_empty() {
            self.version += 1;
        }
        expiries
    }

    /// Earliest time-dependent lease transition in the scene clock domain.
    /// Terminal leases and indefinite (`ttl_ms == 0`) leases contribute no TTL
    /// deadline. Orphan grace and suspension safety bounds remain explicit.
    pub fn next_lease_deadline_ms(&self, max_suspend_ms: u64) -> Option<u64> {
        self.leases
            .values()
            .filter_map(|lease| match lease.state {
                LeaseState::Active | LeaseState::Requested => {
                    (lease.ttl_ms > 0).then(|| lease.granted_at_ms.saturating_add(lease.ttl_ms))
                }
                LeaseState::Orphaned => {
                    let ttl = (lease.ttl_ms > 0)
                        .then(|| lease.granted_at_ms.saturating_add(lease.ttl_ms));
                    let grace = lease
                        .disconnected_at_ms
                        .map(|at| at.saturating_add(lease.grace_period_ms));
                    ttl.into_iter().chain(grace).min()
                }
                LeaseState::Suspended => lease
                    .suspended_at_ms
                    .map(|at| at.saturating_add(max_suspend_ms)),
                LeaseState::Denied
                | LeaseState::Revoked
                | LeaseState::Expired
                | LeaseState::Released => None,
            })
            .min()
    }

    /// Reap a **single** lease if — and only if — it is already past its TTL,
    /// grace period, or suspension timeout, returning the `LeaseExpiry` (with the
    /// removed tiles) or `None` if the lease is unknown or not yet due.
    ///
    /// This is the scoped counterpart to [`Self::expire_leases`]: it applies the
    /// identical orphan/grace/TTL predicate and the identical tile-removal and
    /// zone/widget-publication cleanup, but touches exactly the named lease. The
    /// portal driver uses it to bound the degraded window of an ungracefully
    /// dropped cooperative portal (grace expiry removes its surface) WITHOUT
    /// turning on a global lease sweep for every other subsystem's leases — the
    /// gRPC control plane relies on `renew_lease`, not on nothing ever calling
    /// `expire_leases`, so a global sweep here would change behaviour well beyond
    /// portals. Bumps the scene version when it reaps so the removal repaints.
    pub fn expire_lease(&mut self, lease_id: &SceneId) -> Option<LeaseExpiry> {
        let now = self.clock.now_millis();
        let terminal_state = self
            .leases
            .get(lease_id)
            .and_then(|l| Self::lease_terminal_state(l, now, Self::DEFAULT_MAX_SUSPENSION_MS))?;
        let expiry = self.reap_lease(*lease_id, terminal_state);
        self.version += 1;
        Some(expiry)
    }

    /// The terminal state a lease is due for at `now`, or `None` if it is not yet
    /// expired. Shared by [`Self::expire_leases_with_max_suspend`] (whole-scene
    /// sweep) and [`Self::expire_lease`] (single lease) so both apply the exact
    /// same TTL / grace / suspension predicate.
    fn lease_terminal_state(lease: &Lease, now: u64, max_suspend_ms: u64) -> Option<LeaseState> {
        // TTL-expired active/orphaned leases
        if (lease.state == LeaseState::Active || lease.state == LeaseState::Orphaned)
            && lease.is_expired(now)
        {
            return Some(LeaseState::Expired);
        }
        // Grace-period-expired orphaned leases
        if lease.state == LeaseState::Orphaned && lease.check_grace_expired(now) {
            return Some(LeaseState::Expired);
        }
        // Suspension-timeout leases
        if lease.state == LeaseState::Suspended
            && lease.check_suspension_expired(now, max_suspend_ms)
        {
            return Some(LeaseState::Revoked);
        }
        None
    }

    /// Drive a single lease to `terminal_state`: remove its tiles, clear its zone
    /// and widget publications, and record the terminal state in place. Does NOT
    /// bump `self.version` — the caller batches that so a multi-lease sweep bumps
    /// once. Shared reap body for [`Self::expire_leases_with_max_suspend`] and
    /// [`Self::expire_lease`].
    fn reap_lease(&mut self, id: SceneId, terminal_state: LeaseState) -> LeaseExpiry {
        // Collect the old state before mutating so terminal delivery can
        // report the actual transition.
        let previous_state = self
            .leases
            .get(&id)
            .map_or(terminal_state, |lease| lease.state);

        // Collect tile IDs that will be removed
        let removed_tiles: Vec<SceneId> = self
            .tiles
            .values()
            .filter(|t| t.lease_id == id)
            .map(|t| t.id)
            .collect();
        for tile_id in &removed_tiles {
            self.remove_tile_and_nodes(*tile_id);
        }
        if let Some(lease) = self.leases.get_mut(&id) {
            lease.state = terminal_state;
        }

        // Spec §Requirement: Lease Revocation Clears Zone Publications
        // (lines 235–242): When a lease is REVOKED or EXPIRED, all zone
        // publications made under that lease MUST be immediately cleared.
        // Widget publications are also cleared on lease expiry/revocation.
        if terminal_state.is_terminal() {
            self.clear_publications_for_lease(id);
        }

        LeaseExpiry {
            lease_id: id,
            previous_state,
            terminal_state,
            removed_tiles,
        }
    }
}
