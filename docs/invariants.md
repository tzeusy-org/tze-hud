# Invariants

The few contracts that must survive every refactor, including the T5 API
redesign. Each names the tests that enforce it; an invariant without a test is
a gap to close, not a suggestion. These are what the old doctrine and RFCs
(tag `pre-reset-2026-10-02`) got right, kept here in short form.

## 1. Arrival time is not presentation time

Content carries its own timing. A publication with a TTL or `expires_at`
disappears on schedule without the agent returning; content scheduled with
`present_at` is not shown early. This is what makes a HUD different from a
dashboard: an agent can say "show this for 8 seconds" in one call.

- `tze_hud_mcp` `test_publish_to_zone_ttl_sets_content_expiry_and_is_swept`
- `tze_hud_scene` `widget_ttl_only_expired_publication_removed_when_mixed`
- `tze_hud_compositor` `test_publication_ttl_ms_uses_expires_at_wall_us`
- Gap: `present_at` has parse/resolution tests (`timing::hints`) but no
  end-to-end "held until due" test.

## 2. Three message classes, kept distinct

Transactional (create, lease, release: reliable, ordered, acknowledged),
state-stream (content updates: reliable, coalesced, latest wins), and
ephemeral (pointer moves, hover: droppable, latest wins). A transport or API
change must not give one class another's delivery semantics.

- `tze_hud_mcp` `test_publish_to_zone_contention_policy_latest_wins`
- `tze_hud_compositor` `test_latest_wins_zone_renders_only_latest_publication`
- `tze_hud_input` `test_pointer_move_coalesced_in_batch`, `test_coalesce_scroll_latest_wins`

## 3. The runtime owns the screen; the human override always wins

Safe mode, freeze, and dismiss work without any agent's cooperation and take
effect even when agents are hung. Agents never see or address chrome, and
shell state exposes no portal identity or transcript.

- `tze_hud_runtime` `shell::safe_mode` `test_enter_safe_mode_suspends_active_leases`,
  `test_mutations_rejected_via_shared_state_flag`,
  `test_overlay_renders_from_chrome_state_only_after_critical_error`
- `integration` `shell_dismiss_override_removes_portal_tile`,
  `shell_status_snapshot_exposes_no_portal_identity_or_transcript`
- `tze_hud_compositor` `test_chrome_always_above_max_zorder_tile`

## 4. Disconnect is not release

A disconnected agent's leases become orphaned (badge shown, content kept) for
a grace period. Reconnecting within the grace period with the resume token
restores the same surfaces and their budget usage; when the grace period
ends, the runtime reclaims everything with no agent help.

- `tze_hud_protocol` `disconnect_transitions_to_orphaned_and_sets_disconnection_badge`,
  `grace_period_expiry_removes_tile_and_nodes`
- `tze_hud_runtime` `disconnect_then_reconnect_within_grace_resumes_same_surface_without_duplication`,
  `resumed_session_restores_usage_before_accepting_new_mutations`

## 5. Leases hold time; safe mode pauses it

Every lease has a TTL and expires on its own. Suspension (safe mode) pauses
the TTL clock and preserves lease identity; resuming continues it.

- `tze_hud_runtime` `test_ttl_excluded_during_suspension`,
  `test_lease_identity_preserved_across_suspend_resume`
- `tze_hud_scene` `test_lease_suspend_from_active`, `test_lease_resume_from_suspended`

## 6. Degradation changes drawing, never state

Under load the runtime switches to Simplified rendering (downscaled textures,
opaque fills, snapped animations). It never hides tiles, revokes leases, or
mutates the scene. Enforced structurally: `DegradationController` has no
scene access and `CompositorDegradationPolicy` carries no suppression set.

- `tze_hud_compositor` `significant_degradation_preserves_hidden_tiles_and_opaques_visible_tiles`
- `tze_hud_runtime` `degradation::tests::sustained_overload_does_not_escalate_past_simplified`

## 7. Budgets are hard caps, not negotiations

Per-agent and runtime-wide limits on tiles, texture bytes, update rate, and
nodes per tile. An over-budget batch is rejected whole with a structured
error; nothing escalates, throttles, or partially applies.

- `tze_hud_runtime` `mutation_budget_bridge::tests::registered_budget_rejects_mutation_above_tile_limit`,
  `aggregate_limits_are_atomic_across_agents`

## 8. Errors are affordances for the model

Every rejection carries a stable error code and a `hint` that tells the model
what to do next. Codes are part of the API and don't change silently. Keep
this through T5: a good error saves a model a round trip and its tokens.

- `tze_hud_protocol` `test_build_runtime_error_data_with_context_and_hint`,
  `mutation_batch_oversized_rejected_with_structured_error`, `mcp_error_codes_are_stable`
- `tze_hud_mcp` `test_structured_error_has_hint_field`

## 9. Time is injected

Anything with a deadline (TTL, grace periods, degradation windows, expiry)
reads time from an injectable clock (`TestClock`, or `*_at(now)` methods), so
its tests are deterministic and never sleep. Keep this even if replay
tooling goes in T4.

## Deferred ideas (notes, not code)

- **Screen-share hiding.** The owner's screen is sometimes seen by others. If
  needed, add one "presenting" flag that hides portal content; don't bring
  back viewer classes.
- **Interruption control.** If agents get noisy, use zone policy
  (latest-wins, stack depth) and an urgency field on notifications, not an
  attention budget.
- **Glasses/VR.** A non-goal, but "cheap when idle" (`docs/vision.md`) keeps
  that door open at no cost.
