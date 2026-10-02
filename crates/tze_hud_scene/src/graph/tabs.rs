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
    pub fn create_tab_with_lease(
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
                tab_switch_on_event: None,
            },
        );
        if self.active_tab.is_none() {
            self.active_tab = Some(id);
        }
        self.version += 1;
        Ok(id)
    }

    /// Delete a tab. All tiles in the tab are also removed.
    ///
    /// Requires an active lease when one is provided.
    pub fn delete_tab(&mut self, tab_id: SceneId) -> Result<(), ValidationError> {
        self.delete_tab_checked(tab_id, None)
    }

    /// Delete a tab with lease enforcement.
    pub fn delete_tab_with_lease(
        &mut self,
        tab_id: SceneId,
        lease_id: SceneId,
    ) -> Result<(), ValidationError> {
        self.delete_tab_checked(tab_id, Some(lease_id))
    }

    fn delete_tab_checked(
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
        // Remove all tiles in this tab
        let tab_tiles: Vec<SceneId> = self
            .tiles
            .values()
            .filter(|t| t.tab_id == tab_id)
            .map(|t| t.id)
            .collect();
        for tile_id in tab_tiles {
            self.remove_tile_and_nodes(tile_id);
        }
        self.tabs.remove(&tab_id);
        if self.active_tab == Some(tab_id) {
            // Fall back to the tab with the lowest display_order
            self.active_tab = self
                .tabs
                .values()
                .min_by_key(|t| t.display_order)
                .map(|t| t.id);
        }
        self.version += 1;
        Ok(())
    }

    /// Rename a tab. Requires an active lease when one is provided.
    pub fn rename_tab(&mut self, tab_id: SceneId, new_name: &str) -> Result<(), ValidationError> {
        self.rename_tab_checked(tab_id, new_name, None)
    }

    /// Rename a tab with lease enforcement.
    pub fn rename_tab_with_lease(
        &mut self,
        tab_id: SceneId,
        new_name: &str,
        lease_id: SceneId,
    ) -> Result<(), ValidationError> {
        self.rename_tab_checked(tab_id, new_name, Some(lease_id))
    }

    fn rename_tab_checked(
        &mut self,
        tab_id: SceneId,
        new_name: &str,
        lease_id: Option<SceneId>,
    ) -> Result<(), ValidationError> {
        if let Some(lid) = lease_id {
            self.require_active_lease(lid)?;
        }
        if new_name.is_empty() {
            return Err(ValidationError::InvalidField {
                field: "name".into(),
                reason: "tab name must be non-empty".into(),
            });
        }
        if new_name.len() > MAX_TAB_NAME_BYTES {
            return Err(ValidationError::InvalidField {
                field: "name".into(),
                reason: format!(
                    "tab name exceeds maximum {} UTF-8 bytes (got {})",
                    MAX_TAB_NAME_BYTES,
                    new_name.len()
                ),
            });
        }
        let tab = self
            .tabs
            .get_mut(&tab_id)
            .ok_or(ValidationError::TabNotFound { id: tab_id })?;
        tab.name = new_name.to_string();
        self.version += 1;
        Ok(())
    }

    /// Change the display_order of a tab.
    pub fn reorder_tab(&mut self, tab_id: SceneId, new_order: u32) -> Result<(), ValidationError> {
        self.reorder_tab_checked(tab_id, new_order, None)
    }

    /// Change the display_order of a tab with lease enforcement.
    pub fn reorder_tab_with_lease(
        &mut self,
        tab_id: SceneId,
        new_order: u32,
        lease_id: SceneId,
    ) -> Result<(), ValidationError> {
        self.reorder_tab_checked(tab_id, new_order, Some(lease_id))
    }

    fn reorder_tab_checked(
        &mut self,
        tab_id: SceneId,
        new_order: u32,
        lease_id: Option<SceneId>,
    ) -> Result<(), ValidationError> {
        if let Some(lid) = lease_id {
            self.require_active_lease(lid)?;
        }
        if !self.tabs.contains_key(&tab_id) {
            return Err(ValidationError::TabNotFound { id: tab_id });
        }
        // display_order must be unique across tabs (excluding this tab)
        if self
            .tabs
            .values()
            .any(|t| t.id != tab_id && t.display_order == new_order)
        {
            return Err(ValidationError::DuplicateDisplayOrder { order: new_order });
        }
        let tab = self
            .tabs
            .get_mut(&tab_id)
            .expect("tab_id existence checked above");
        tab.display_order = new_order;
        self.version += 1;
        Ok(())
    }

    pub fn switch_active_tab(&mut self, tab_id: SceneId) -> Result<(), ValidationError> {
        self.switch_active_tab_checked(tab_id, None)
    }

    /// Switch active tab with lease enforcement.
    pub fn switch_active_tab_with_lease(
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

    /// Configure the `tab_switch_on_event` field for an existing tab.
    ///
    /// The bare event name (e.g. `"doorbell.ring"`) triggers automatic tab
    /// activation when any agent emits a matching bare name.  Pass `None` to
    /// clear the setting.
    ///
    /// The provided bare name (when `Some`) must:
    /// - Match `[a-z][a-z0-9_]*(\.[a-z][a-z0-9_]*)+` (validated by
    ///   [`crate::events::naming::validate_bare_name`]).
    /// - Not start with `"system."` or `"scene."` (reserved prefixes that
    ///   can never be emitted by agents and would never trigger).
    ///
    pub fn set_tab_switch_on_event(
        &mut self,
        tab_id: SceneId,
        bare_name: Option<String>,
    ) -> Result<(), ValidationError> {
        if let Some(ref name) = bare_name {
            crate::events::naming::validate_bare_name(name).map_err(|e| {
                ValidationError::InvalidField {
                    field: "tab_switch_on_event".into(),
                    reason: e.to_string(),
                }
            })?;
        }
        let tab = self
            .tabs
            .get_mut(&tab_id)
            .ok_or(ValidationError::TabNotFound { id: tab_id })?;
        tab.tab_switch_on_event = bare_name;
        self.version += 1;
        Ok(())
    }

    /// Find the tab configured with `tab_switch_on_event == bare_name`.
    ///
    /// Returns `None` if no tab is configured for this bare name.
    /// Excludes any tab whose configured value starts with `"system."` (cannot
    /// match agent events — runtime enforces this at tab configuration time,
    /// but defensive check is applied here too per spec line 250).
    ///
    /// If multiple tabs share the same `tab_switch_on_event` value, the tab
    /// with the lowest `display_order` (ties broken by `created_at_ms`) is
    /// chosen to guarantee deterministic behaviour across HashMap iteration
    /// orders.
    pub fn find_tab_for_event(&self, bare_name: &str) -> Option<SceneId> {
        self.tabs
            .values()
            .filter(|tab| {
                if let Some(trigger) = &tab.tab_switch_on_event {
                    // System events must NOT trigger tab switches (spec line 250).
                    !trigger.starts_with("system.") && trigger == bare_name
                } else {
                    false
                }
            })
            .min_by_key(|tab| (tab.display_order, tab.created_at_ms))
            .map(|tab| tab.id)
    }
}
