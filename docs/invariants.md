# Invariants

The contracts that must survive every refactor. Each names the tests that
enforce it; an invariant without a test is a gap to close, not a suggestion.
These preserve lessons from the former doctrine and RFCs (tag
`pre-reset-2026-10-02`).

Maintenance: behavior changes must update the affected contract and its test
references in the same change. Source links identify the test files; the
named functions are the anchors to verify after a refactor.

## 1. Arrival time is not presentation time

Content carries its own timing. A publication with a TTL or `expires_at`
disappears on schedule without the agent returning; content scheduled with
`present_at` is not shown early. This is what makes a HUD different from a
dashboard: an agent can say "show this for 8 seconds" in one call.

- `tze_hud_mcp` [`test_hud_publish_zone_ttl_sets_content_expiry_and_is_swept`](../crates/tze_hud_mcp/src/server/tests.rs),
  [`delay_ms_holds_content_until_due`](../crates/tze_hud_mcp/src/server/tests.rs), [`notification_ttl_zero_is_held_and_hold_retimes_it`](../crates/tze_hud_mcp/src/server/tests.rs)
- `tze_hud_scene` [`graph::widget_tests::widget_ttl_only_expired_publication_removed_when_mixed`](../crates/tze_hud_scene/src/graph/widget_tests.rs)
- `tze_hud_compositor` [`test_publication_ttl_ms_uses_expires_at_wall_us`](../crates/tze_hud_compositor/src/renderer/tests/zone_layers.rs),
  [`hold_moves_the_fade_deadline_and_ttl_zero_never_fades`](../crates/tze_hud_compositor/src/renderer/tests/idle_gate.rs)
- `tze_hud_protocol` (gRPC session) [`grpc_zone_publish_ttl_sets_expiry_and_is_swept`](../crates/tze_hud_protocol/src/session_server/tests/invariants.rs),
  [`grpc_zone_publish_expires_at_is_swept`](../crates/tze_hud_protocol/src/session_server/tests/invariants.rs), [`grpc_zone_publish_present_at_is_held_until_due`](../crates/tze_hud_protocol/src/session_server/tests/invariants.rs),
  [`grpc_batch_present_at_holds_content_until_due`](../crates/tze_hud_protocol/src/session_server/tests/invariants.rs), [`grpc_batch_expires_at_sweeps_tile`](../crates/tze_hud_protocol/src/session_server/tests/invariants.rs)
- `integration` (POC acceptance, MCP end to end) [`poc_zone_notification_ttl_disappears_unattended`](../tests/integration/poc_acceptance.rs),
  [`poc_zone_delay_ms_appears_on_schedule`](../tests/integration/poc_acceptance.rs)

## 2. Three message classes, kept distinct

Transactional (create, lease, release: reliable, ordered, acknowledged),
state-stream (content updates: reliable, coalesced, latest wins), and
ephemeral (pointer moves, hover: droppable, latest wins). A transport or API
change must not give one class another's delivery semantics.

- `tze_hud_mcp` [`test_hud_publish_zone_contention_policy_latest_wins`](../crates/tze_hud_mcp/src/server/tests.rs)
- `tze_hud_compositor` [`test_latest_wins_zone_renders_only_latest_publication`](../crates/tze_hud_compositor/src/renderer/tests/zone_stack.rs)
- `tze_hud_input` [`test_pointer_move_coalesced_in_batch`](../crates/tze_hud_input/src/batching.rs), [`test_coalesce_scroll_latest_wins`](../crates/tze_hud_input/src/coalescing.rs)

## 3. The runtime owns the screen; the human override always wins

Safe mode and dismiss work without any agent's cooperation and take
effect even when agents are hung. Agents never see or address chrome, and
shell state exposes no portal identity or transcript.

Owner decision (2026-10-07): freeze promise removed; safe mode already pauses agents; updates during a pause are rejected, not buffered.

- `tze_hud_runtime` [`shell::safe_mode`](../crates/tze_hud_runtime/src/shell/safe_mode.rs) [`test_enter_safe_mode_suspends_active_leases`](../crates/tze_hud_runtime/src/shell/safe_mode.rs),
  [`test_mutations_rejected_via_shared_state_flag`](../crates/tze_hud_runtime/src/shell/safe_mode.rs)
- `tze_hud_runtime` `windowed::safe_mode_toggle`
  [`hotkey_event_enters_safe_mode_and_suspends_leases`](../crates/tze_hud_runtime/src/windowed/safe_mode_toggle.rs) (global hotkey, default
  Ctrl+Shift+F12, `[runtime].safe_mode_hotkey`)
- `tze_hud_config` [`safe_mode_hotkey_defaults_overrides_and_rejects_garbage`](../crates/tze_hud_config/src/tests.rs)
- `integration` [`shell_dismiss_override_removes_portal_tile`](../tests/integration/text_stream_portal_governance.rs),
  [`shell_status_snapshot_exposes_no_portal_identity_or_transcript`](../tests/integration/text_stream_portal_governance.rs)
- viewer dismiss (hover close button): `tze_hud_scene`
  [`viewer_dismiss_tile_revokes_lease_in_any_live_state`](../crates/tze_hud_scene/src/graph/tests.rs); `tze_hud_protocol`
  [`viewer_dismiss_tile_pushes_reclaimed_override`](../crates/tze_hud_protocol/src/session_server/tests/events.rs); `tze_hud_runtime`
  [`viewer_dismiss_portal_detaches_and_next_publish_reattaches`](../crates/tze_hud_runtime/src/portal_projection_driver.rs),
  [`viewer_close_button_dismisses_hovered_tile_and_notifies_owner`](../crates/tze_hud_runtime/src/windowed/event_loop_harness.rs); `integration`
  [`poc_portal_viewer_dismiss_then_mcp_verbs`](../tests/integration/poc_acceptance.rs);
  `tze_hud_compositor` [`tile_close_button_draw_and_hit_region_share_token_geometry`](../crates/tze_hud_compositor/src/renderer/tests/hit_drag.rs)
- hung agents (a raw HTTP/2 client that stops reading until the server's send
  buffer to it is full): `integration`
  [`poc_override_safe_mode_wins_with_hung_grpc_agent`](../tests/integration/poc_acceptance.rs),
  [`poc_override_exit_safe_mode_with_agent_hung_during_it`](../tests/integration/poc_acceptance.rs); a responsive agent
  is told, [`poc_override_notifies_the_agent`](../tests/integration/poc_acceptance.rs) (`SessionSuspended`,
  `SessionResumed`, `Reclaimed{OVERRIDE}`)
- `tze_hud_mcp` [`hud_publish_in_safe_mode_returns_safe_mode_active`](../crates/tze_hud_mcp/src/server/tests.rs),
  [`safe_mode_does_not_regrant_suspended_mcp_lease`](../crates/tze_hud_mcp/src/server/tests.rs),
  [`resume_restores_mcp_publishing`](../crates/tze_hud_mcp/src/server/tests.rs)
- `tze_hud_scene` [`graph::widget_tests::widget_publish_with_suspended_lease_is_safe_mode_active`](../crates/tze_hud_scene/src/graph/widget_tests.rs)
- `tze_hud_compositor` [`test_chrome_always_above_max_zorder_tile`](../crates/tze_hud_compositor/src/renderer/tests/surface.rs),
  [`windowed_frame_draws_chrome_overlay_in_safe_mode`](../crates/tze_hud_compositor/src/renderer/safe_mode_overlay.rs)

## 4. Disconnect is not release

A disconnected agent's leases become orphaned (badge shown, content kept) for
a grace period. Reconnecting within the grace period with the resume token
restores the same surfaces and their budget usage; when the grace period
ends, the runtime reclaims everything with no agent help. Reclaiming a lease
clears only the zone and widget publications made under that lease, never the
rest of the agent's namespace. Before a gRPC session's first successful tile
claim, zone and widget publications (including scheduled zone content) carry
that session's transient origin. A complete successful claim adopts only its
still-unleased publications into the granted lease; failed claims change no
publication ownership. Publication-only disconnects use the same finite resume
grace and idle deadline as orphaned leases. Authenticated resume within grace
restores the origin; grace expiry reclaims its current content and pending
publishes without touching concurrent sessions, overwritten content, MCP
publications, or independently leased content. This origin is in-memory only,
not a wire or snapshot field.

- `tze_hud_protocol` [`disconnect_transitions_to_orphaned_and_sets_disconnection_badge`](../crates/tze_hud_protocol/tests/lease_governance.rs),
  [`grace_period_expiry_removes_tile_and_nodes`](../crates/tze_hud_protocol/tests/lease_governance.rs)
- `tze_hud_runtime` [`disconnect_then_reconnect_within_grace_resumes_same_surface_without_duplication`](../crates/tze_hud_runtime/src/portal_projection_driver.rs),
  [`resumed_session_restores_usage_before_accepting_new_mutations`](../crates/tze_hud_runtime/src/mutation_budget_bridge.rs)
- `tze_hud_protocol` (gRPC session) [`grpc_disconnect_orphans_leases_and_badges_tiles`](../crates/tze_hud_protocol/src/session_server/tests/invariants.rs),
  [`grpc_resume_within_grace_restores_same_lease_and_tile`](../crates/tze_hud_protocol/src/session_server/tests/invariants.rs),
  [`grpc_grace_expiry_reclaims_orphaned_lease_and_rejects_resume`](../crates/tze_hud_protocol/src/session_server/tests/invariants.rs)
- `tze_hud_runtime` (headless frame sweep) [`render_frame_reclaims_orphaned_lease_after_grace`](../crates/tze_hud_runtime/src/headless.rs)
- `tze_hud_compositor` (badge draw command) [`orphaned_tile_emits_disconnection_badge_draw_cmd`](../crates/tze_hud_compositor/src/renderer/tests/focus_chrome.rs)
- `integration` (POC acceptance, over gRPC and MCP with `production.toml`)
  [`poc_portal_abandoned_is_reclaimed`](../tests/integration/poc_acceptance.rs), [`poc_tile_claim_with_placement_two_round_trips`](../tests/integration/poc_acceptance.rs),
  [`poc_tile_update_then_orphan_then_reclaim_after_grace`](../tests/integration/poc_acceptance.rs)
- `tze_hud_scene` [`tile_lease_reap_keeps_same_namespace_mcp_publications`](../crates/tze_hud_scene/src/graph/spec_scenarios.rs),
  [`revoked_lease_clears_only_its_publications`](../crates/tze_hud_scene/src/graph/spec_scenarios.rs)

## 5. Leases hold time; safe mode pauses it

Every lease has a TTL and expires on its own. Suspension (safe mode) pauses
the TTL clock and preserves lease identity; resuming continues it.

- `tze_hud_runtime` [`test_ttl_excluded_during_suspension`](../crates/tze_hud_runtime/src/shell/safe_mode.rs),
  [`test_lease_identity_preserved_across_suspend_resume`](../crates/tze_hud_runtime/src/shell/safe_mode.rs),
  [`hotkey_event_enters_safe_mode_and_suspends_leases`](../crates/tze_hud_runtime/src/windowed/safe_mode_toggle.rs)
- `tze_hud_scene` [`test_lease_suspend_from_active`](../crates/tze_hud_scene/src/graph/tests.rs), [`test_lease_resume_from_suspended`](../crates/tze_hud_scene/src/graph/tests.rs)
- `tze_hud_mcp` [`safe_mode_does_not_regrant_suspended_mcp_lease`](../crates/tze_hud_mcp/src/server/tests.rs),
  [`resume_restores_mcp_publishing`](../crates/tze_hud_mcp/src/server/tests.rs)

## 6. Degradation changes drawing, never state

Under load the runtime switches to Simplified rendering (downscaled textures,
opaque fills, snapped animations). It never hides tiles, revokes leases, or
mutates the scene. Enforced structurally: `DegradationController` has no
scene access and `CompositorDegradationPolicy` carries no suppression set.

- `tze_hud_compositor` [`significant_degradation_preserves_hidden_tiles_and_opaques_visible_tiles`](../crates/tze_hud_compositor/src/renderer/tile_render.rs)
- `tze_hud_runtime` [`degradation::tests::sustained_overload_does_not_escalate_past_simplified`](../crates/tze_hud_runtime/src/degradation.rs)

## 7. Budgets are hard caps, not negotiations

Per-agent and runtime-wide limits on tiles, texture bytes, update rate, and
nodes per tile. An over-budget batch is rejected whole with a structured
error; nothing escalates, throttles, or partially applies.

- `tze_hud_runtime` [`mutation_budget_bridge::tests::registered_budget_rejects_mutation_above_tile_limit`](../crates/tze_hud_runtime/src/mutation_budget_bridge.rs),
  [`aggregate_limits_are_atomic_across_agents`](../crates/tze_hud_runtime/src/mutation_budget_bridge.rs)
- `tze_hud_resource` `store_behavior`: [`agent_texture_budget_rejects_whole_upload_and_stores_nothing`](../crates/tze_hud_resource/tests/store_behavior.rs),
  [`runtime_wide_texture_cap_is_shared_across_agents`](../crates/tze_hud_resource/tests/store_behavior.rs),
  [`resource_count_cap_rejects_the_extra_resource`](../crates/tze_hud_resource/tests/store_behavior.rs),
  [`per_resource_size_cap_rejects_chunked_upload_at_start`](../crates/tze_hud_resource/tests/store_behavior.rs),
  [`upload_slot_cap_is_per_agent_and_freed_by_abort`](../crates/tze_hud_resource/tests/store_behavior.rs),
  [`chunked_upload_rejected_at_complete_frees_its_slot_and_stores_nothing`](../crates/tze_hud_resource/tests/store_behavior.rs)

## 8. Errors are affordances for the model

Every rejection carries a stable error code and a `hint` that tells the model
what to do next. Codes are part of the API and don't change silently. A good
error saves a model a round trip and its tokens.

- `tze_hud_protocol` [`mutation_batch_oversized_rejected_with_structured_error`](../crates/tze_hud_protocol/tests/fuzz_protocol_boundary.rs)
- `tze_hud_mcp` [`test_structured_error_has_hint_field`](../crates/tze_hud_mcp/src/server/tests.rs), [`error_codes_are_unique_and_documented`](../crates/tze_hud_mcp/src/error.rs),
  [`every_returned_code_is_in_the_closed_set`](../crates/tze_hud_mcp/src/server/tests.rs)
- `tze_hud_resource` `store_behavior`: [`rejections_carry_stable_wire_code_and_actionable_detail`](../crates/tze_hud_resource/tests/store_behavior.rs),
  [`chunk_protocol_errors_have_stable_codes`](../crates/tze_hud_resource/tests/store_behavior.rs)

## 9. Time is injected

Anything with a deadline (TTL, grace periods, degradation windows, expiry)
reads time from an injectable clock (`TestClock`, or `*_at(now)` methods), so
its tests are deterministic and never sleep.

- `integration` (POC acceptance: zone TTL, `delay_ms`, and portal liveness
  and lease grace all advanced by one `TestClock`) [`poc_portal_abandoned_is_reclaimed`](../tests/integration/poc_acceptance.rs),
  [`poc_zone_notification_ttl_disappears_unattended`](../tests/integration/poc_acceptance.rs), [`poc_zone_delay_ms_appears_on_schedule`](../tests/integration/poc_acceptance.rs)

## Deferred ideas (notes, not code)

- **Screen-share hiding.** The owner's screen is sometimes seen by others. If
  needed, add one "presenting" flag that hides portal content; don't bring
  back viewer classes.
- **Interruption control.** If agents get noisy, use zone policy
  (latest-wins, stack depth) and an urgency field on notifications, not an
  attention budget.
- **Glasses/VR.** A non-goal, but "cheap when idle" (`docs/vision.md`) keeps
  that door open at no cost.
