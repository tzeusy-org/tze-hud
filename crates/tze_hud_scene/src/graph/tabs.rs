use super::*;

impl SceneGraph {
    // ─── Tab operations ──────────────────────────────────────────────────

    /// Create a new tab. Requires an active lease when `lease_id` is provided.
    ///
    /// Tab name must be non-empty, ≤ 128 UTF-8 bytes.
    /// Scene must not already have 256 tabs (MAX_TABS).
    pub fn create_tab(
        &mut self,
        name: &str,
        display_order: u32,
    ) -> Result<SceneId, ValidationError> {
        self.create_tab_checked(name, display_order, None)
    }

    /// Create a tab under an active lease.
    ///
    /// Pass `Some(lease_id)` to require an active lease. Pass `None` to skip
    /// the lease check (used by internal scene construction and tests).
    #[cfg(test)]
    pub(crate) fn create_tab_with_lease(
        &mut self,
        name: &str,
        display_order: u32,
        lease_id: SceneId,
    ) -> Result<SceneId, ValidationError> {
        self.create_tab_checked(name, display_order, Some(lease_id))
    }

    fn create_tab_checked(
        &mut self,
        name: &str,
        display_order: u32,
        lease_id: Option<SceneId>,
    ) -> Result<SceneId, ValidationError> {
        // Lease check
        if let Some(lid) = lease_id {
            self.require_active_lease(lid)?;
        }
        // Name validation: non-empty, ≤ 128 UTF-8 bytes
        if name.is_empty() {
            return Err(ValidationError::InvalidField {
                field: "name".into(),
                reason: "tab name must be non-empty".into(),
            });
        }
        if name.len() > MAX_TAB_NAME_BYTES {
            return Err(ValidationError::InvalidField {
                field: "name".into(),
                reason: format!(
                    "tab name exceeds maximum {} UTF-8 bytes (got {})",
                    MAX_TAB_NAME_BYTES,
                    name.len()
                ),
            });
        }
        // Scene-level tab count limit
        if self.tabs.len() >= MAX_TABS {
            return Err(ValidationError::BudgetExceeded {
                resource: format!("tabs (limit {MAX_TABS})"),
            });
        }
        // Check display_order uniqueness
        if self.tabs.values().any(|t| t.display_order == display_order) {
            return Err(ValidationError::DuplicateDisplayOrder {
                order: display_order,
            });
        }
        let id = SceneId::new();
        let now_ms = self.clock.now_millis();
        self.tabs.insert(
            id,
            Tab {
                id,
                name: name.to_string(),
                display_order,
                created_at_ms: now_ms,
            },
        );
        if self.active_tab.is_none() {
            self.active_tab = Some(id);
        }
        self.version += 1;
        Ok(id)
    }

    pub fn switch_active_tab(&mut self, tab_id: SceneId) -> Result<(), ValidationError> {
        self.switch_active_tab_checked(tab_id, None)
    }

    /// Switch active tab with lease enforcement.
    #[cfg(test)]
    pub(crate) fn switch_active_tab_with_lease(
        &mut self,
        tab_id: SceneId,
        lease_id: SceneId,
    ) -> Result<(), ValidationError> {
        self.switch_active_tab_checked(tab_id, Some(lease_id))
    }

    fn switch_active_tab_checked(
        &mut self,
        tab_id: SceneId,
        lease_id: Option<SceneId>,
    ) -> Result<(), ValidationError> {
        if let Some(lid) = lease_id {
            self.require_active_lease(lid)?;
        }
        if !self.tabs.contains_key(&tab_id) {
            return Err(ValidationError::TabNotFound { id: tab_id });
        }
        self.active_tab = Some(tab_id);
        self.version += 1;
        Ok(())
    }

    // ─── Lease helpers ───────────────────────────────────────────────────

    /// Check that the lease is currently active (not expired, not suspended).
    pub(super) fn require_active_lease(&self, lease_id: SceneId) -> Result<(), ValidationError> {
        let lease = self
            .leases
            .get(&lease_id)
            .ok_or(ValidationError::LeaseNotFound { id: lease_id })?;
        let now = self.clock.now_millis();
        if lease.is_expired(now) {
            return Err(ValidationError::LeaseExpired { id: lease_id });
        }
        if !lease.is_mutations_allowed() {
            return Err(ValidationError::InvalidField {
                field: "lease_state".into(),
                reason: format!(
                    "lease {} is in {:?} state; mutations require Active state",
                    lease_id, lease.state
                ),
            });
        }
        Ok(())
    }
}
