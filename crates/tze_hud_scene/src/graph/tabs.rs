use super::*;

/// Internal tab operations used by configured startup and shell navigation.
enum TabOperation<'a> {
    CreateTab { name: &'a str, display_order: u32 },
    SwitchActiveTab { tab_id: SceneId },
}

impl SceneGraph {
    // ─── Tab operations ──────────────────────────────────────────────────

    /// Create a tab for trusted scene setup.
    ///
    /// Tab name must be non-empty, ≤ 128 UTF-8 bytes.
    /// Scene must not already have 256 tabs (MAX_TABS).
    pub fn create_tab(
        &mut self,
        name: &str,
        display_order: u32,
    ) -> Result<SceneId, ValidationError> {
        self.apply_tab_operation(
            TabOperation::CreateTab {
                name,
                display_order,
            },
            None,
        )
    }

    /// Create a tab under an active lease.
    #[cfg(test)]
    pub(crate) fn create_tab_with_lease(
        &mut self,
        name: &str,
        display_order: u32,
        lease_id: SceneId,
    ) -> Result<SceneId, ValidationError> {
        self.apply_tab_operation(
            TabOperation::CreateTab {
                name,
                display_order,
            },
            Some(lease_id),
        )
    }

    /// Switch the active tab for trusted shell navigation.
    pub fn switch_active_tab(&mut self, tab_id: SceneId) -> Result<(), ValidationError> {
        self.apply_tab_operation(TabOperation::SwitchActiveTab { tab_id }, None)
            .map(|_| ())
    }

    /// Switch active tab with lease enforcement.
    #[cfg(test)]
    pub(crate) fn switch_active_tab_with_lease(
        &mut self,
        tab_id: SceneId,
        lease_id: SceneId,
    ) -> Result<(), ValidationError> {
        self.apply_tab_operation(TabOperation::SwitchActiveTab { tab_id }, Some(lease_id))
            .map(|_| ())
    }

    fn apply_tab_operation(
        &mut self,
        operation: TabOperation<'_>,
        lease_id: Option<SceneId>,
    ) -> Result<SceneId, ValidationError> {
        if let Some(lease_id) = lease_id {
            self.require_active_lease(lease_id)?;
        }
        match operation {
            TabOperation::CreateTab {
                name,
                display_order,
            } => self.insert_tab(name, display_order),
            TabOperation::SwitchActiveTab { tab_id } => {
                if !self.tabs.contains_key(&tab_id) {
                    return Err(ValidationError::TabNotFound { id: tab_id });
                }
                self.active_tab = Some(tab_id);
                self.version += 1;
                Ok(tab_id)
            }
        }
    }

    fn insert_tab(&mut self, name: &str, display_order: u32) -> Result<SceneId, ValidationError> {
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
