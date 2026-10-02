use super::*;

impl SceneGraph {
    pub(super) fn coerce_widget_param_value(
        widget_name: &str,
        param_name: &str,
        decl: &crate::types::WidgetParameterDeclaration,
        submitted_value: &crate::types::WidgetParameterValue,
    ) -> Result<crate::types::WidgetParameterValue, ValidationError> {
        use crate::types::{WidgetParamConstraints, WidgetParamType, WidgetParameterValue};

        let empty_constraints = WidgetParamConstraints::default();
        let constraints = decl.constraints.as_ref().unwrap_or(&empty_constraints);

        match (&decl.param_type, submitted_value) {
            (WidgetParamType::F32, WidgetParameterValue::F32(v)) => {
                if v.is_nan() {
                    return Err(ValidationError::WidgetParameterInvalidValue {
                        widget: widget_name.to_string(),
                        param: param_name.to_string(),
                        reason: "NaN is not a valid f32 parameter value".to_string(),
                    });
                }
                if v.is_infinite() {
                    return Err(ValidationError::WidgetParameterInvalidValue {
                        widget: widget_name.to_string(),
                        param: param_name.to_string(),
                        reason: "infinity is not a valid f32 parameter value".to_string(),
                    });
                }
                let clamped = match (constraints.f32_min, constraints.f32_max) {
                    (Some(mn), Some(mx)) => v.clamp(mn, mx),
                    (Some(mn), None) => v.max(mn),
                    (None, Some(mx)) => v.min(mx),
                    (None, None) => *v,
                };
                Ok(WidgetParameterValue::F32(clamped))
            }
            (WidgetParamType::F32, _) => Err(ValidationError::WidgetParameterTypeMismatch {
                widget: widget_name.to_string(),
                param: param_name.to_string(),
            }),
            (WidgetParamType::String, WidgetParameterValue::String(s)) => {
                let mut max_bytes = constraints.string_max_bytes.unwrap_or(1024) as usize;
                if max_bytes == 0 {
                    max_bytes = 1024;
                }
                if s.len() > max_bytes {
                    return Err(ValidationError::WidgetParameterInvalidValue {
                        widget: widget_name.to_string(),
                        param: param_name.to_string(),
                        reason: format!(
                            "string value of {} bytes exceeds max_length of {}",
                            s.len(),
                            max_bytes
                        ),
                    });
                }
                Ok(WidgetParameterValue::String(s.clone()))
            }
            (WidgetParamType::String, _) => Err(ValidationError::WidgetParameterTypeMismatch {
                widget: widget_name.to_string(),
                param: param_name.to_string(),
            }),
            (WidgetParamType::Color, WidgetParameterValue::Color(c)) => {
                let clamped_color = Rgba {
                    r: c.r.clamp(0.0, 1.0),
                    g: c.g.clamp(0.0, 1.0),
                    b: c.b.clamp(0.0, 1.0),
                    a: c.a.clamp(0.0, 1.0),
                };
                Ok(WidgetParameterValue::Color(clamped_color))
            }
            (WidgetParamType::Color, _) => Err(ValidationError::WidgetParameterTypeMismatch {
                widget: widget_name.to_string(),
                param: param_name.to_string(),
            }),
            (WidgetParamType::Enum, WidgetParameterValue::Enum(v)) => {
                if !constraints.enum_allowed_values.is_empty()
                    && !constraints.enum_allowed_values.contains(v)
                {
                    return Err(ValidationError::WidgetParameterInvalidValue {
                        widget: widget_name.to_string(),
                        param: param_name.to_string(),
                        reason: format!(
                            "enum value '{}' not in allowed set {:?}",
                            v, constraints.enum_allowed_values
                        ),
                    });
                }
                Ok(WidgetParameterValue::Enum(v.clone()))
            }
            (WidgetParamType::Enum, _) => Err(ValidationError::WidgetParameterTypeMismatch {
                widget: widget_name.to_string(),
                param: param_name.to_string(),
            }),
        }
    }

    // ─── Zone operations ─────────────────────────────────────────────────

    /// Register a zone definition in the zone registry.
    pub fn register_zone(&mut self, zone: ZoneDefinition) {
        self.zone_registry.register(zone);
        self.version += 1;
    }

    /// Unregister a zone by name. Returns the removed definition if found.
    pub fn unregister_zone(&mut self, name: &str) -> Option<ZoneDefinition> {
        let removed = self.zone_registry.unregister(name);
        if removed.is_some() {
            self.version += 1;
        }
        removed
    }

    /// Publish content to a zone. Applies contention policy.
    ///
    /// Token validation is out-of-scope for the pure scene graph layer;
    /// callers (e.g., the gRPC server) must validate the token before calling this.
    ///
    /// # Arguments
    /// - `zone_name` — zone type name, resolved in the global `zone_registry` (v1: publishes
    ///   are global, not tab-scoped; tab-scoped zone instances are a post-v1 feature)
    /// - `content` — content payload; must match one of the zone's accepted_media_types
    /// - `publisher_namespace` — the publishing agent's namespace
    /// - `merge_key` — key for MergeByKey contention (ignored for other policies)
    /// - `expires_at_wall_us` — optional wall-clock expiry (µs since epoch)
    /// - `content_classification` — optional opaque content classification tag
    pub fn publish_to_zone(
        &mut self,
        zone_name: &str,
        content: ZoneContent,
        publisher_namespace: &str,
        merge_key: Option<String>,
        expires_at_wall_us: Option<u64>,
        content_classification: Option<String>,
    ) -> Result<(), ValidationError> {
        // Check zone exists and content type is accepted
        let (contention_policy, max_publishers, accepted) = {
            let zone = self.zone_registry.get_by_name(zone_name).ok_or_else(|| {
                ValidationError::ZoneNotFound {
                    name: zone_name.to_string(),
                }
            })?;
            let accepted = Self::content_media_type(&content)
                .map(|mt| zone.accepted_media_types.contains(&mt))
                .unwrap_or(true);
            (zone.contention_policy, zone.max_publishers, accepted)
        };

        if !accepted {
            return Err(ValidationError::ZoneMediaTypeMismatch {
                zone: zone_name.to_string(),
            });
        }

        let now_us = self.clock.now_us();

        // Auto-dismiss: if no explicit expires_at is provided and the content is a
        // Notification, derive expires_at from the urgency level.
        //
        // Urgency → default TTL mapping (per NotificationPayload semantics):
        //   0 (low)         → 8 s
        //   1 (normal)      → 8 s
        //   2 (urgent)      → 15 s
        //   3+ (critical)   → 30 s
        //
        // A publisher-supplied expires_at always takes precedence.
        let effective_expires_at = expires_at_wall_us.or_else(|| {
            if let ZoneContent::Notification(ref payload) = content {
                let ttl_us: u64 = match payload.urgency {
                    0 | 1 => Self::NOTIFICATION_TTL_INFO_US,
                    2 => Self::NOTIFICATION_TTL_WARNING_US,
                    _ => Self::NOTIFICATION_TTL_CRITICAL_US,
                };
                Some(now_us.saturating_add(ttl_us))
            } else {
                None
            }
        });

        let record = ZonePublishRecord {
            zone_name: zone_name.to_string(),
            publisher_namespace: publisher_namespace.to_string(),
            content,
            published_at_wall_us: now_us,
            merge_key: merge_key.clone(),
            expires_at_wall_us: effective_expires_at,
            content_classification,
            breakpoints: Vec::new(),
        };

        let publishes = self
            .zone_registry
            .active_publishes
            .entry(zone_name.to_string())
            .or_default();

        apply_contention(
            publishes,
            record,
            contention_policy,
            max_publishers,
            |max| ValidationError::ZoneMaxPublishersReached {
                zone: zone_name.to_string(),
                max,
            },
        )?;

        self.version += 1;
        Ok(())
    }

    /// Resolve the "best" lease state for a given namespace.
    ///
    /// Selection priority (per spec §Zone Publish Requires Active Lease):
    ///
    /// 1. `Active` — if any Active lease exists, return it immediately.
    /// 2. First non-terminal lease — for accurate error reporting when no
    ///    Active lease exists but the agent is still in-flight (e.g. Orphaned,
    ///    Suspended).
    /// 3. First terminal lease — for accurate error reporting when all leases
    ///    are terminal.
    /// 4. `None` — the namespace has never held a lease.
    ///
    /// This helper is the canonical source of namespace→lease-state resolution
    /// used by all lease-enforcing zone publish paths.
    fn resolve_lease_state_for_namespace(&self, publisher_namespace: &str) -> Option<LeaseState> {
        let all_leases_for_ns: Vec<_> = self
            .leases
            .values()
            .filter(|l| l.namespace == publisher_namespace)
            .map(|l| l.state)
            .collect();

        if all_leases_for_ns.is_empty() {
            None
        } else {
            // Prefer Active; fall back to the first non-terminal; otherwise use first terminal.
            all_leases_for_ns
                .iter()
                .copied()
                .find(|&s| s == LeaseState::Active)
                .or_else(|| all_leases_for_ns.iter().copied().find(|s| !s.is_terminal()))
                .or_else(|| all_leases_for_ns.first().copied())
        }
    }

    /// Publish content to a zone with lease-state enforcement.
    ///
    /// This is the lease-aware variant of `publish_to_zone`. It looks up the
    /// active lease for `publisher_namespace` and enforces spec
    /// §Requirement: Zone Publish Requires Active Lease (lines 213–242):
    ///
    /// - ACTIVE lease → accepted.
    /// - ORPHANED lease → rejected with `ZonePublishLeaseOrphaned`; existing
    ///   content remains visible with stale badge (spec lines 231–233).
    /// - SUSPENDED lease → rejected with `ZonePublishSafeModeActive`
    ///   (spec line 227).
    /// - Terminal or missing lease → rejected with `ZonePublishLeaseNotFound`
    ///   or `ZonePublishLeaseNotActive`.
    ///
    /// Callers that do not hold a lease (e.g., system/chrome publishers) should
    /// use the unchecked `publish_to_zone` directly.
    ///
    /// `ttl_us` is the caller-supplied content time-to-live (microseconds). When
    /// `Some`, it is converted here into an absolute `expires_at_wall_us`
    /// (`clock.now_us() + ttl_us`) and stored on the publish record so the
    /// per-frame expiry sweep clears the content at its deadline. Passing the
    /// TTL as a relative duration (rather than an absolute time computed by the
    /// caller) keeps the expiry in the scene's own clock domain, which the sweep
    /// reads via the same `clock`. `None` means no content expiry (the record
    /// persists until overwritten or the zone default applies).
    pub fn publish_to_zone_with_lease(
        &mut self,
        zone_name: &str,
        content: ZoneContent,
        publisher_namespace: &str,
        merge_key: Option<String>,
        ttl_us: Option<u64>,
    ) -> Result<(), ValidationError> {
        use crate::lease::orphan::ZonePublishResult;

        let lease_state = self.resolve_lease_state_for_namespace(publisher_namespace);

        match lease_state {
            None => {
                // No lease whatsoever (namespace has never held a lease).
                return Err(ValidationError::ZonePublishLeaseNotFound {
                    namespace: publisher_namespace.to_string(),
                });
            }
            Some(state) => {
                let result = match state {
                    LeaseState::Active => ZonePublishResult::Accepted,
                    LeaseState::Orphaned => ZonePublishResult::RejectedLeaseOrphaned,
                    LeaseState::Suspended => ZonePublishResult::RejectedSafeModeActive,
                    _ => ZonePublishResult::RejectedLeaseTerminal,
                };
                match result {
                    ZonePublishResult::Accepted => {} // fall through to publish
                    ZonePublishResult::RejectedLeaseOrphaned => {
                        return Err(ValidationError::ZonePublishLeaseOrphaned {
                            namespace: publisher_namespace.to_string(),
                        });
                    }
                    ZonePublishResult::RejectedSafeModeActive => {
                        return Err(ValidationError::ZonePublishSafeModeActive {
                            namespace: publisher_namespace.to_string(),
                        });
                    }
                    ZonePublishResult::RejectedLeaseTerminal => {
                        return Err(ValidationError::ZonePublishLeaseNotActive {
                            namespace: publisher_namespace.to_string(),
                            state: format!("{state:?}"),
                        });
                    }
                }
            }
        }

        // Lease is Active — delegate to unchecked publish. Convert the relative
        // TTL into an absolute wall-clock deadline in the scene's clock domain so
        // `drain_expired_zone_publications` (which reads the same clock) sweeps it.
        let expires_at_wall_us = ttl_us.map(|t| self.clock.now_us().saturating_add(t));
        self.publish_to_zone(
            zone_name,
            content,
            publisher_namespace,
            merge_key,
            expires_at_wall_us,
            None,
        )
    }

    /// Publish streaming `StreamText` content to a zone with breakpoints and
    /// lease-state enforcement.
    ///
    /// This is the breakpoint-aware variant of `publish_to_zone_with_lease`. It
    /// performs the same lease validation and then stores the breakpoints in the
    /// `ZonePublishRecord` so the compositor can reveal the text progressively.
    ///
    /// Per spec §Subtitle Streaming Word-by-Word Reveal: breakpoints are
    /// byte-offset indices in the UTF-8 text where the compositor pauses reveal.
    /// An empty `breakpoints` vec reveals all text immediately.
    ///
    /// Non-`StreamText` content types MUST pass `breakpoints = Vec::new()`.
    ///
    /// `ttl_us` behaves as in [`publish_to_zone_with_lease`]: when `Some`, it is
    /// converted into an absolute `expires_at_wall_us` in the scene clock domain
    /// so the per-frame sweep clears the streamed content at its deadline.
    pub fn publish_to_zone_with_lease_and_breakpoints(
        &mut self,
        zone_name: &str,
        content: ZoneContent,
        publisher_namespace: &str,
        merge_key: Option<String>,
        ttl_us: Option<u64>,
        breakpoints: Vec<u64>,
    ) -> Result<(), ValidationError> {
        use crate::lease::orphan::ZonePublishResult;

        let lease_state = self.resolve_lease_state_for_namespace(publisher_namespace);

        match lease_state {
            None => {
                return Err(ValidationError::ZonePublishLeaseNotFound {
                    namespace: publisher_namespace.to_string(),
                });
            }
            Some(state) => {
                let result = match state {
                    LeaseState::Active => ZonePublishResult::Accepted,
                    LeaseState::Orphaned => ZonePublishResult::RejectedLeaseOrphaned,
                    LeaseState::Suspended => ZonePublishResult::RejectedSafeModeActive,
                    _ => ZonePublishResult::RejectedLeaseTerminal,
                };
                match result {
                    ZonePublishResult::Accepted => {}
                    ZonePublishResult::RejectedLeaseOrphaned => {
                        return Err(ValidationError::ZonePublishLeaseOrphaned {
                            namespace: publisher_namespace.to_string(),
                        });
                    }
                    ZonePublishResult::RejectedSafeModeActive => {
                        return Err(ValidationError::ZonePublishSafeModeActive {
                            namespace: publisher_namespace.to_string(),
                        });
                    }
                    ZonePublishResult::RejectedLeaseTerminal => {
                        return Err(ValidationError::ZonePublishLeaseNotActive {
                            namespace: publisher_namespace.to_string(),
                            state: format!("{state:?}"),
                        });
                    }
                }
            }
        }

        // Lease is Active — publish with breakpoints. Convert the relative TTL
        // into an absolute wall-clock deadline in the scene's clock domain.
        let expires_at_wall_us = ttl_us.map(|t| self.clock.now_us().saturating_add(t));
        self.publish_to_zone_with_breakpoints(
            zone_name,
            content,
            publisher_namespace,
            merge_key,
            expires_at_wall_us,
            None,
            breakpoints,
        )
    }

    /// Publish content to a zone with optional streaming breakpoints (unchecked).
    ///
    /// Like `publish_to_zone` but stores breakpoints in the publish record.
    /// Breakpoints identify byte offsets in the StreamText where the compositor
    /// pauses progressive reveal.
    #[allow(clippy::too_many_arguments)]
    pub fn publish_to_zone_with_breakpoints(
        &mut self,
        zone_name: &str,
        content: ZoneContent,
        publisher_namespace: &str,
        merge_key: Option<String>,
        expires_at_wall_us: Option<u64>,
        content_classification: Option<String>,
        breakpoints: Vec<u64>,
    ) -> Result<(), ValidationError> {
        // Check zone exists and content type is accepted
        let (contention_policy, max_publishers, accepted) = {
            let zone = self.zone_registry.get_by_name(zone_name).ok_or_else(|| {
                ValidationError::ZoneNotFound {
                    name: zone_name.to_string(),
                }
            })?;
            let accepted = Self::content_media_type(&content)
                .map(|mt| zone.accepted_media_types.contains(&mt))
                .unwrap_or(true);
            (zone.contention_policy, zone.max_publishers, accepted)
        };

        if !accepted {
            return Err(ValidationError::ZoneMediaTypeMismatch {
                zone: zone_name.to_string(),
            });
        }

        let now_us = self.clock.now_us();
        let record = ZonePublishRecord {
            zone_name: zone_name.to_string(),
            publisher_namespace: publisher_namespace.to_string(),
            content,
            published_at_wall_us: now_us,
            merge_key: merge_key.clone(),
            expires_at_wall_us,
            content_classification,
            breakpoints,
        };

        let publishes = self
            .zone_registry
            .active_publishes
            .entry(zone_name.to_string())
            .or_default();

        apply_contention(
            publishes,
            record,
            contention_policy,
            max_publishers,
            |max| ValidationError::ZoneMaxPublishersReached {
                zone: zone_name.to_string(),
                max,
            },
        )?;

        self.version += 1;
        Ok(())
    }

    /// Publish parameter values to a named widget instance.
    ///
    /// This is the scene-level implementation for both gRPC WidgetPublish and the
    /// MCP `hud_publish` tool (widget surfaces). It validates all parameters against the widget
    /// type's schema and applies the contention policy.
    ///
    /// # Capability
    ///
    /// This method does NOT check the `publish_widget:<widget_name>` capability —
    /// that check MUST be performed by the transport layer (gRPC session handler or
    /// MCP server) before calling this method.  The session layer checks capability
    /// strings directly against `session.capabilities` for gRPC, or grants a
    /// `PublishWidget` lease capability for MCP calls.
    ///
    /// # Parameter validation
    ///
    /// - Unknown parameter names → `WidgetUnknownParameter`
    /// - Type mismatch → `WidgetParameterTypeMismatch`
    /// - NaN/infinity (f32) → `WidgetParameterInvalidValue`
    /// - String exceeds max_length → `WidgetParameterInvalidValue`
    /// - Enum value not in allowed_values → `WidgetParameterInvalidValue`
    /// - f32 out of [min, max] range → clamped (NOT rejected)
    ///
    /// # Contention
    ///
    /// Follows the widget instance's contention policy (LatestWins, Replace,
    /// Stack, MergeByKey), parallel to zone publishing.
    pub fn publish_to_widget(
        &mut self,
        widget_name: &str,
        params: std::collections::HashMap<String, crate::types::WidgetParameterValue>,
        publisher_namespace: &str,
        merge_key: Option<String>,
        transition_ms: u32,
        expires_at_wall_us: Option<u64>,
    ) -> Result<bool, ValidationError> {
        // ── Step 1: Resolve the widget instance ──────────────────────────────
        let instance_name = widget_name;
        let instance = self
            .widget_registry
            .instances
            .get(instance_name)
            .ok_or_else(|| ValidationError::WidgetNotFound {
                name: widget_name.to_string(),
            })?
            .clone();

        let definition = self
            .widget_registry
            .definitions
            .get(&instance.widget_type_name)
            .ok_or_else(|| ValidationError::WidgetNotFound {
                name: widget_name.to_string(),
            })?
            .clone();

        let is_ephemeral = definition.ephemeral;

        // ── Step 2: Validate and coerce each submitted parameter ─────────────
        let mut validated_params: std::collections::HashMap<String, WidgetParameterValue> =
            std::collections::HashMap::new();

        for (param_name, submitted_value) in &params {
            // Look up declaration in schema
            let decl = definition
                .parameter_schema
                .iter()
                .find(|d| &d.name == param_name)
                .ok_or_else(|| ValidationError::WidgetUnknownParameter {
                    widget: widget_name.to_string(),
                    param: param_name.clone(),
                })?;

            let coerced =
                Self::coerce_widget_param_value(widget_name, param_name, decl, submitted_value)?;
            validated_params.insert(param_name.clone(), coerced);
        }

        // ── Step 3: Apply contention policy and record publication ────────────
        let contention_policy = instance
            .contention_override
            .unwrap_or(definition.default_contention_policy);
        let max_publishers = definition.max_publishers;

        let now_us = self.clock.now_us();

        let record = crate::types::WidgetPublishRecord {
            widget_name: widget_name.to_string(),
            publisher_namespace: publisher_namespace.to_string(),
            params: validated_params.clone(),
            published_at_wall_us: now_us,
            merge_key: merge_key.clone(),
            expires_at_wall_us,
            transition_ms,
        };

        let publishes = self
            .widget_registry
            .active_publishes
            .entry(widget_name.to_string())
            .or_default();

        apply_contention(
            publishes,
            record,
            contention_policy,
            max_publishers,
            |max| ValidationError::WidgetMaxPublishersReached {
                widget: widget_name.to_string(),
                max,
            },
        )?;

        // ── Step 4: Update current_params on the instance ─────────────────────
        // Merge new validated params over existing current_params.
        {
            let inst = self
                .widget_registry
                .instances
                .get_mut(instance_name)
                .expect("instance_name existence verified above");
            for (k, v) in &validated_params {
                inst.current_params.insert(k.clone(), v.clone());
            }
        }

        self.version += 1;
        // Return true for durable, false for ephemeral (caller decides whether to send ack)
        Ok(!is_ephemeral)
    }

    /// Set a widget instance parameter from runtime-local behavior (non-agent publication).
    ///
    /// This updates `instance.current_params` directly after schema/type/constraint
    /// validation. No publication record is created and contention policy is not
    /// consulted. Intended for local runtime UI state such as hover/tooltip reveals.
    pub fn set_widget_param_local(
        &mut self,
        widget_name: &str,
        param_name: &str,
        value: crate::types::WidgetParameterValue,
    ) -> Result<(), ValidationError> {
        let instance = self
            .widget_registry
            .instances
            .get(widget_name)
            .ok_or_else(|| ValidationError::WidgetNotFound {
                name: widget_name.to_string(),
            })?
            .clone();

        let definition = self
            .widget_registry
            .definitions
            .get(&instance.widget_type_name)
            .ok_or_else(|| ValidationError::WidgetNotFound {
                name: widget_name.to_string(),
            })?
            .clone();

        let decl = definition
            .parameter_schema
            .iter()
            .find(|d| d.name == param_name)
            .ok_or_else(|| ValidationError::WidgetUnknownParameter {
                widget: widget_name.to_string(),
                param: param_name.to_string(),
            })?;

        let coerced = Self::coerce_widget_param_value(widget_name, param_name, decl, &value)?;

        if let Some(inst) = self.widget_registry.instances.get_mut(widget_name) {
            inst.current_params.insert(param_name.to_string(), coerced);
            self.version += 1;
            return Ok(());
        }

        Err(ValidationError::WidgetNotFound {
            name: widget_name.to_string(),
        })
    }

    /// Clear all active publishes for a zone (regardless of publisher).
    ///
    /// This removes ALL publications from the zone. For per-publisher clearing,
    /// use [`clear_zone_for_publisher`].
    pub fn clear_zone(&mut self, zone_name: &str) -> Result<(), ValidationError> {
        if !self.zone_registry.zones.contains_key(zone_name) {
            return Err(ValidationError::ZoneNotFound {
                name: zone_name.to_string(),
            });
        }
        self.zone_registry.active_publishes.remove(zone_name);
        self.version += 1;
        Ok(())
    }

    /// Clear all active publishes for a zone made by a specific publisher.
    ///
    /// Per spec: "ClearZone clears all publications by the agent in the specified zone."
    /// If no publications exist for the publisher, this is a no-op (but still succeeds).
    pub fn clear_zone_for_publisher(
        &mut self,
        zone_name: &str,
        publisher_namespace: &str,
    ) -> Result<(), ValidationError> {
        if !self.zone_registry.zones.contains_key(zone_name) {
            return Err(ValidationError::ZoneNotFound {
                name: zone_name.to_string(),
            });
        }
        if let Some(publishes) = self.zone_registry.active_publishes.get_mut(zone_name) {
            let before = publishes.len();
            publishes.retain(|r| r.publisher_namespace != publisher_namespace);
            if publishes.len() != before {
                self.version += 1;
            }
        }
        Ok(())
    }

    /// Dismiss a single notification by its publication key.
    ///
    /// Removes the publication identified by `(zone_name, published_at_wall_us,
    /// publisher_namespace)` from `zone_registry.active_publishes`.  This is the
    /// primitive used by the zone interaction layer when the user clicks or
    /// activates a notification's dismiss (×) button.
    ///
    /// Returns `true` if a matching publication was found and removed, `false` if
    /// the zone does not exist or no matching publication was found.
    ///
    /// # Local feedback
    ///
    /// Per doctrine ("local feedback first"), this method immediately removes
    /// matching hit regions from `zone_hit_regions` so that the stale dismiss
    /// affordance is gone before the next rendered frame.  Empty zone entries
    /// are pruned from `active_publishes` for consistency with other cleanup
    /// helpers.
    pub fn dismiss_notification(
        &mut self,
        zone_name: &str,
        published_at_wall_us: u64,
        publisher_namespace: &str,
    ) -> bool {
        let publishes = match self.zone_registry.active_publishes.get_mut(zone_name) {
            Some(v) => v,
            None => return false,
        };
        let before = publishes.len();
        publishes.retain(|r| {
            !(r.published_at_wall_us == published_at_wall_us
                && r.publisher_namespace == publisher_namespace)
        });
        let removed = publishes.len() < before;
        if removed {
            // Prune empty zone entry (consistent with other cleanup helpers).
            if publishes.is_empty() {
                self.zone_registry.active_publishes.remove(zone_name);
            }
            // Remove stale hit regions for this publication immediately so
            // pointer/keyboard events can no longer land on them this frame.
            self.overlay.zone_hit_regions.retain(|r| {
                !(r.zone_name == zone_name
                    && r.published_at_wall_us == published_at_wall_us
                    && r.publisher_namespace == publisher_namespace)
            });
            self.version += 1;
        }
        removed
    }

    /// Clear all widget publications from a given agent namespace across all widgets.
    ///
    /// Called on lease expiry/revocation to satisfy spec §Requirement: Lease
    /// Revocation Clears Widget Publications. Mirrors
    /// [`clear_zone_publications_for_namespace`] for the widget registry.
    pub fn clear_widget_publications_for_namespace(&mut self, namespace: &str) {
        let mut touched_widgets = Vec::new();
        for (widget_name, publishes) in self.widget_registry.active_publishes.iter_mut() {
            let before = publishes.len();
            publishes.retain(|r| r.publisher_namespace != namespace);
            if publishes.len() != before {
                touched_widgets.push(widget_name.clone());
            }
        }
        // Remove empty entries for cleanliness
        self.widget_registry
            .active_publishes
            .retain(|_, v| !v.is_empty());
        if !touched_widgets.is_empty() {
            for widget_name in touched_widgets {
                self.refresh_widget_current_params(&widget_name);
            }
            self.version += 1;
        }
    }

    /// Per spec: "ClearWidget clears all publications by the agent in the specified widget."
    /// If no publications exist for the publisher, this is a no-op (but still succeeds).
    /// When all publishers have been cleared the widget reverts to its default params.
    ///
    /// Returns `Err(WidgetNotFound)` if the widget instance is not registered.
    pub fn clear_widget_for_publisher(
        &mut self,
        widget_name: &str,
        publisher_namespace: &str,
    ) -> Result<(), ValidationError> {
        if !self.widget_registry.instances.contains_key(widget_name) {
            return Err(ValidationError::WidgetNotFound {
                name: widget_name.to_string(),
            });
        }
        if let Some(publishes) = self.widget_registry.active_publishes.get_mut(widget_name) {
            let before = publishes.len();
            publishes.retain(|r| r.publisher_namespace != publisher_namespace);
            if publishes.len() != before {
                if publishes.is_empty() {
                    self.widget_registry.active_publishes.remove(widget_name);
                }
                self.refresh_widget_current_params(widget_name);
                self.version += 1;
            }
        }
        Ok(())
    }

    fn refresh_widget_current_params(&mut self, widget_name: &str) {
        let tab_id = match self.widget_registry.instances.get(widget_name) {
            Some(instance) => instance.tab_id,
            None => return,
        };
        let effective_params = match self.widget_registry.get_occupancy(widget_name, tab_id) {
            Some(occupancy) => occupancy.effective_params,
            None => return,
        };
        if let Some(instance) = self.widget_registry.instances.get_mut(widget_name) {
            instance.current_params = effective_params;
        }
    }

    /// Map ZoneContent to its ZoneMediaType, if deterministic.
    fn content_media_type(content: &ZoneContent) -> Option<ZoneMediaType> {
        match content {
            ZoneContent::StreamText(_) => Some(ZoneMediaType::StreamText),
            ZoneContent::Notification(_) => Some(ZoneMediaType::ShortTextWithIcon),
            ZoneContent::StatusBar(_) => Some(ZoneMediaType::KeyValuePairs),
            ZoneContent::SolidColor(_) => Some(ZoneMediaType::SolidColor),
            ZoneContent::StaticImage(_) => Some(ZoneMediaType::StaticImage),
        }
    }

    // ─── Zone publication expiry ─────────────────────────────────────────

    /// Remove zone publications whose `expires_at_wall_us` has passed.
    ///
    /// Per timing-model/spec.md §Requirement: Expiration Policy and
    /// `ZonePublishRecord` contract: "When present, the runtime MUST clear
    /// this publication at or before this time."
    ///
    /// Returns the number of expired publications removed.
    ///
    /// Call this once per frame before rendering (Stage 4: Scene Commit).
    pub fn drain_expired_zone_publications(&mut self) -> usize {
        let now_us = self.clock.now_us();
        let mut total_removed = 0usize;

        for publishes in self.zone_registry.active_publishes.values_mut() {
            let before = publishes.len();
            publishes.retain(|r| match r.expires_at_wall_us {
                Some(exp) => exp > now_us,
                None => true,
            });
            total_removed += before - publishes.len();
        }

        // Clean up empty entries and bump version if anything changed.
        if total_removed > 0 {
            self.zone_registry
                .active_publishes
                .retain(|_, v| !v.is_empty());
            self.version += 1;
        }

        total_removed
    }

    /// Earliest absolute wall-clock expiry across active zone and widget
    /// publications. The windowed scheduler uses this to arm one exact wake
    /// instead of polling the expiry sweeps every display interval.
    pub fn next_publication_expiry_wall_us(&self) -> Option<u64> {
        let zone_expiry = self
            .zone_registry
            .active_publishes
            .values()
            .flatten()
            .filter_map(|record| record.expires_at_wall_us)
            .min();
        let widget_expiry = self
            .widget_registry
            .active_publishes
            .values()
            .flatten()
            .filter_map(|record| record.expires_at_wall_us)
            .min();
        zone_expiry.into_iter().chain(widget_expiry).min()
    }

    // ─── Widget publication expiry ───────────────────────────────────────

    /// Remove widget publications whose `expires_at_wall_us` has passed.
    ///
    /// Mirrors `drain_expired_zone_publications` for the widget registry.
    /// Per timing-model/spec.md §Requirement: Expiration Policy and the
    /// `WidgetPublishRecord` contract: "When present, the runtime MUST clear
    /// this publication at or before this time."
    ///
    /// Returns the number of expired publications removed.
    ///
    /// Call this once per frame before rendering (Stage 4: Scene Commit),
    /// alongside `drain_expired_zone_publications`.
    pub fn drain_expired_widget_publications(&mut self) -> usize {
        let now_us = self.clock.now_us();
        let mut total_removed = 0usize;
        let mut touched_widgets = Vec::new();

        for (widget_name, publishes) in self.widget_registry.active_publishes.iter_mut() {
            let before = publishes.len();
            publishes.retain(|r| match r.expires_at_wall_us {
                Some(exp) => exp > now_us,
                None => true,
            });
            let removed = before - publishes.len();
            if removed > 0 {
                total_removed += removed;
                touched_widgets.push(widget_name.clone());
            }
        }

        // Clean up empty entries and bump version if anything changed.
        if total_removed > 0 {
            self.widget_registry
                .active_publishes
                .retain(|_, v| !v.is_empty());
            for widget_name in touched_widgets {
                self.refresh_widget_current_params(&widget_name);
            }
            self.version += 1;
        }

        total_removed
    }
}
