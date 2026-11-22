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

For notifications with actions, omitted expiry uses the owner's live saved
answer window (default one minute). MCP explicit `ttl_ms`, including held
zero, wins; gRPC absolute expiry then positive TTL wins. Admission snapshots
one value into the absolute deadline for immediate and scheduled content;
panel changes only affect later accepted publications, never existing cards.
`hud_publish` and `hud_surfaces` report that stored lifetime, including delay.
The setting itself neither polls nor wakes the renderer.

Exception: a notification with actions that uses the owner's answer window (no
explicit expiry) is not removed while a pointer press that began on one of its
controls before its deadline is still down. The press's own activation on that
control (its release, or a completed destructive hold) is accepted; any other
ending — release elsewhere, cancel, or 5 s after the deadline — expires the
card at once. Hover and keyboard focus never hold a card; explicit expiry,
replace, clear and trim win immediately, and a later `hud_hold` retimes it as
usual. Tests: `answer_window_press_holds_card_until_release`,
`press_hold_release_off_control_expires_card`,
`hud_hold_retimes_a_press_held_card`,
`armed_wait_returns_action_for_press_held_past_deadline`,
`press_held_across_answer_window_deadline_answers_on_release`.

Zone/widget Hold accepts the full unsigned 64-bit millisecond range using
saturating duration and absolute-expiry arithmetic; positive TTLs cannot wrap to
an immediate expiry. Zero remains indefinite and widget owner caps still apply.
The existing MCP [`widget_publish_hold_clear`](../crates/tze_hud_mcp/src/server/tests.rs),
gRPC [`hold_zone_publication`](../crates/tze_hud_protocol/src/session_server/tests/lifecycle_verbs.rs)
and [`test_durable_widget_publish_receives_result`](../crates/tze_hud_protocol/src/session_server/tests/widget_publish.rs)
parents cover active and pending boundary values through the real transports.

A waiting choice card adds no timer or periodic layout work. Choosing a callback action
retains that exact publication only for its finite local exit, makes all controls
noninteractive and cannot be prolonged by hold. Clear, expiry, replacement,
dismiss and owner hiding prune local focus and feedback without an answer.
[`idle_with_notification_renders_only_at_fade`](../crates/tze_hud_compositor/src/renderer/tests/idle_gate.rs)
and [`release_activates_only_the_button_the_press_began_on`](../crates/tze_hud_runtime/src/windowed/event_loop_harness.rs)
cover the waiting and terminal transitions.

- `tze_hud_mcp` [`portal_flow_attach_publish_poll_ack_clear`](../crates/tze_hud_mcp/src/server/tests.rs) (atomic publish + portal hold),
  [`test_hud_publish_zone_ttl_sets_content_expiry_and_is_swept`](../crates/tze_hud_mcp/src/server/tests.rs),
  [`delay_ms_holds_content_until_due`](../crates/tze_hud_mcp/src/server/tests.rs), [`notification_ttl_zero_is_held_and_hold_retimes_it`](../crates/tze_hud_mcp/src/server/tests.rs)
- `tze_hud_scene` [`graph::widget_tests::widget_ttl_only_expired_publication_removed_when_mixed`](../crates/tze_hud_scene/src/graph/widget_tests.rs)
- `tze_hud_compositor` [`test_publication_ttl_ms_uses_expires_at_wall_us`](../crates/tze_hud_compositor/src/renderer/tests/zone_layers.rs),
  [`hold_moves_the_fade_deadline_and_ttl_zero_never_fades`](../crates/tze_hud_compositor/src/renderer/tests/idle_gate.rs)
- `tze_hud_protocol` (gRPC session) [`grpc_zone_publish_ttl_sets_expiry_and_is_swept`](../crates/tze_hud_protocol/src/session_server/tests/invariants.rs),
  [`grpc_zone_publish_expires_at_is_swept`](../crates/tze_hud_protocol/src/session_server/tests/invariants.rs), [`grpc_zone_publish_present_at_is_held_until_due`](../crates/tze_hud_protocol/src/session_server/tests/invariants.rs)
- `integration` (POC acceptance, MCP end to end) [`poc_zone_notification_ttl_disappears_unattended`](../tests/integration/poc_acceptance.rs),
  [`poc_zone_delay_ms_appears_on_schedule`](../tests/integration/poc_acceptance.rs)

The config writer's runtime round-trip, invalid-value and failed-rename
parents, the Agents `answer_window_saves_applies_and_resets_only_after_success`
row test, and `integration::poc_panel_runtime_rows_apply_save_and_stage_a_restart`
cover saved/default source, save-first failure behavior and both live server
bridges. Source tests are distinct from Windows owner interaction evidence.

Admin `holdings`/`warnings` are content-free observations of existing owners.
Their independent nonblocking snapshots cannot renew, apply or sweep retained
content, advance portal liveness, consume input or wake rendering. Busy/missing
sources and omissions are explicit; due retained records remain `cleanup_due`
until normal cleanup. This cross-agent metadata remains behind `admin_guard`,
separate from ordinary `hud_surfaces` and agent snapshots. Existing admin-status
and authenticated HTTP publish/readback tests cover privacy, expiry and bounds;
startup warning and doctor tests cover actual producers and bounded consumption.
These observations make no claim about owner visual acceptance or pixels.

## 2. Three message classes, kept distinct

Transactional (create, lease, release: reliable, ordered, acknowledged),
state-stream (content updates: reliable, coalesced, latest wins), and
ephemeral (pointer moves, hover and scroll notifications: droppable on lag).
A transport or API change must not give one class another's delivery semantics.

Accepted human notification events use a per-publisher FIFO and retained-until-ACK
ids without oldest-first eviction. A publisher is an agent plus its MCP session
(unscoped calls and gRPC are the agent's own session): FIFO isolation and
notification key slots are per session. Input is retained until ACK or until
its session is released (`DELETE /mcp`, or the 64-session bound), when no
consumer for it can exist any more; release is explicit, so §4 does not apply. Destructive scene transfer has no cancellation
point before retained commit. Callback and runtime-authored human-X kinds remain
distinct; coalesced readiness wakes carry no record ownership. These in-memory
guarantees do not claim process-crash durability or add a gRPC action transport.
An armed `hud_publish wait_ms` reserves only its exact admitted publication over
that same FIFO. Accepted callback/human-X wins before local removal; replacement,
expiry, stack trim and clear latch only the first unanswered terminal reason.
A full notification stack trims its oldest card that is not an unanswered
question, on both admission planes; only an all-question stack drops its oldest
question. The shared regular-stack layout presents unanswered questions first,
newest-first within each group, using the same scene predicate as admission.
Drawing, hit regions, keyboard order and operator geometry use those same slots;
answered, expired and URL-only cards have no question priority. A fixed-height
zone may omit later cards, but cannot put a retained question behind status cards.
Alert banners retain severity ordering. A trimmed, unanswered, unexpired card queues exactly one `displaced`
FIFO item for its own publisher, unless an armed wait on that exact card takes
the displacement instead; expiry, clear and same-key replacement queue nothing.
Receipts never grow at publish rate: one per key and at most
`DISPLACED_RECEIPT_CAP` (5) per publisher, dropping only older receipts, never
accepted human events.
Scene rollback also rolls back terminal notification debt and queued receipts. Readiness callbacks
run after scene unlock. The reserved input is consumed after successful reply
encoding, with no intervening await; cancellation before encoding releases it
for ordinary stable-id `hud_input`/ACK. Other publications and portal input keep
their own ownership. Deadline release returns to the ordinary retained-until-ACK
path. The delayed scheduler carries a private admission identity, without wire
or session-origin changes. Waiting publication wakes rendering before awaiting
input; it never adds a periodic poll to an idle runtime.

A notification key names one exact-byte slot within a zone and authenticated
publisher. Replacing it retains its position and zone enter progress while
issuing fresh publication identity. Selective clear removes only matching active
and delayed notifications while preserving sibling keys and foreign
publications. Activation snapshots its original optional key before removal, and its FIFO record and stable ACK id
survive replacement, clear and expiry. A reused key is not a generation nonce;
owner option D permits stale cancellation, never a stale answer. The broker
owns per-request callback routing and its accepted-race proof.

Notification hover/press is ephemeral runtime state: it never enters the
accepted input FIFO or changes ACK ownership. One matching live callback activation
produces one callback; unclicked, foreign, replaced, and stale controls produce
none. Existing end-to-end coverage:
[`poc_notification_action_reaches_hud_input`](../tests/integration/poc_acceptance.rs).
A card an agent puts under a resting pointer drops pointer presses on its
controls for `notification.press_guard.ms` (bounded at five intervals of
restarts) with no hover and the arrow cursor; a pointer that moved there,
the keyboard and the owner's own re-layout are never delayed, and only a
guarded card under the pointer arms a wake
([`a_card_arriving_under_a_resting_pointer_drops_the_press`](../crates/tze_hud_runtime/src/windowed/event_loop_harness/tests/press_guard.rs),
[`dropped_presses_restart_the_guard_up_to_five_intervals`](../crates/tze_hud_runtime/src/windowed/event_loop_harness/tests/press_guard.rs),
[`a_pointer_that_moved_there_is_never_guarded`](../crates/tze_hud_runtime/src/windowed/event_loop_harness/tests/press_guard.rs),
[`keyboard_answers_are_never_guarded`](../crates/tze_hud_runtime/src/windowed/event_loop_harness/tests/press_guard.rs),
[`the_owners_own_relayout_starts_no_guard`](../crates/tze_hud_runtime/src/windowed/event_loop_harness/tests/press_guard.rs),
[`press_guard_is_not_zeroed_by_os_motion`](../crates/tze_hud_config/src/tokens.rs)).

A notification action has one resolved callback, HTTPS URL, local copy
or typed reply target. Both planes validate it before mutation or scheduling.
On MCP a targetless action with a non-empty label is a callback named by that label.
Admission rejects a defaulted callback id that repeats another callback id
or is `dismiss`. MCP counts explicit empty strings as present. gRPC treats its
plain empty callback and URL strings as absent and requires a target.
Multiple targets on either plane or a missing target on gRPC use
`action needs one of callback_id, url, copy or reply`. URLs require a parsed HTTPS
host and at most 2048 supplied UTF-8 bytes. Copy text requires 1 through
2048 UTF-8 bytes without NUL. All action kinds share the four-action
admission cap.

Each card can open one typed reply field. Only a submitted line reaches the
publisher FIFO, with its callback id and committed text. A claimed wait consumes
the reply after encoding, as for a fixed action. Resident-origin replies use
the authenticated agent's unscoped MCP namespace. Scoped MCP replies stay with
their publishing session.

Optional notification progress is finite and within 0 through 1 on both
planes. A same-key update retains its visual slot and enter timeline.
Copy activation resolves the exact live card and button, then writes after
releasing scene guards. It preserves the card and its expiry, queues no
publisher input, and updates only finite owner-local feedback. Link and copy
actions do not give a card question priority.
Full-text reveal follows the painted ellipsis geometry. It preserves card
and button bounds, exposes no text in admin geometry, and arms a single
dwell deadline only for a hovered eligible field.

Links resolve by exact publication/publisher/zone and action index, never by
label or callback text. Permitted pointer and keyboard activation leaves the
card/lifetime/hold and controls intact and queues no input, readiness, human-X
or chosen exit. Transient monotonic two-second per-button debounce stamps
before attempting a launch, including failure; retirement removes it without
a timer or repaint. Safe mode, native permission and local cancellation remain
fail-closed. The owned parser-encoded target leaves all scene/shared guards
before a dedicated COM/ShellExecuteExW worker; blocking it cannot block scene
or frame progress. Opener failures neither expose URLs nor change chrome state.
Press feedback ("Opening…", then "Opened" or "Couldn't open") is one
main-thread slot drawn inside the pressed control's existing bounds: the
worker sets a completion flag and wakes the loop once, the shown result has
one deadline, and a handoff that never returns arms nothing. It never moves
hit geometry and never reaches an agent
(`card_link_feedback_shows_opening_then_the_worker_result`,
`link_worker_that_never_returns_arms_no_wake`).
The existing event-loop action/keyboard/idle parents and compositor layout/scale
parents exercise these contracts; physical Windows browser foreground remains
separate acceptance.

- `tze_hud_mcp` [`action_presses_are_delivered_until_acked`](../crates/tze_hud_mcp/src/server/tests.rs)
  (>256 interleaved events, isolation, typed dismiss, stable ACK/max/backlog),
  [`portal_flow_attach_publish_poll_ack_clear`](../crates/tze_hud_mcp/src/server/tests.rs)
  (real armed wait, cancellation/error retention and finite overlapping polls)

- `tze_hud_mcp` [`test_hud_publish_zone_contention_policy_latest_wins`](../crates/tze_hud_mcp/src/server/tests.rs)
- `tze_hud_compositor` [`test_latest_wins_zone_renders_only_latest_publication`](../crates/tze_hud_compositor/src/renderer/tests/zone_stack.rs)

Local pointer and command processing keep their feedback and focus ownership.
Portal input stays in its retained-until-ACK MCP queue. Scroll feedback and local
frame coalescing do not wait on a resident input feed.

- `tze_hud_runtime` native keyboard, focus, activation and pointer/scroll cases in
  [the real event-loop harness](../crates/tze_hud_runtime/src/windowed/event_loop_harness.rs)
- `tze_hud_input` [`test_scroll_changed_event_contains_current_offset`](../crates/tze_hud_input/src/scroll.rs)
  (latest queued request per frame); `integration`
  [`portal_scroll_coalesces_commits_without_resetting_visible_window`](../tests/integration/text_stream_portal_surface.rs)
  (local viewport survives coalesced frame commits)

Session scoping is tested by `tze_hud_mcp`
[`input_reaches_only_the_session_that_asked`](../crates/tze_hud_mcp/src/server/tests/scopes.rs),
[`ending_a_session_releases_everything_it_owns`](../crates/tze_hud_mcp/src/server/tests/scopes.rs)
and `tze_hud_scene`
[`release_scope_ends_only_that_session`](../crates/tze_hud_scene/src/graph/agents.rs).

The ask-and-wait deadline is classified when the scene accepts an event, in
its monotonic clock domain. An early answer retains its reservation until
encoding or cancellation even when dispatch resumes after the deadline; a
new event at or after the deadline cannot acquire that reservation. A busy
post-admission scene cannot extend the call's wait: bounded contention uses
the existing input-state error and releases ownership for ordinary polling.

## 3. The runtime owns the screen; the human override always wins

Visible zone publications traverse Background, Content, then Chrome, with
ascending zone names inside each layer. Flat geometry, rounded SDF shapes and
text share that order, independent of registration or map seed. Each zone keeps
its existing publication, urgency and slot order; owner-hidden zones contribute
no drawing or controls. Existing primitive-pass boundaries still apply: this
does not interleave all primitives into an atomic per-zone composite.

Overlapping notification controls choose the later-painted zone first while
preserving control order within each card. Keyboard tab order remains ascending
by zone name, and runtime close/jump controls retain their existing priority.
The existing compositor parents
[`test_three_pass_ordering_independent_of_registration_order`](../crates/tze_hud_compositor/src/renderer/tests/zone_layers.rs)
and [`test_notification_card_border_is_an_sdf_border`](../crates/tze_hud_compositor/src/renderer/tests/zone_notification.rs)
cover flat/SDF/text order, real overlap pixels, visibility and pointer/keyboard
precedence. These source and readback checks do not establish owner visual
acceptance.

Safe mode and dismiss work without any agent's cooperation and take
effect even when agents are hung. Agents never see or address chrome, and
shell state exposes no portal identity or transcript. A subsequent portal
publish intentionally attaches fresh after runtime reclaim and reports
`reattached: true`; agents cannot publish attached/detached or availability
as status labels. Discovery includes caller-owned scheduled zone holdings.

Owner decision (2026-10-07): freeze promise removed; safe mode already pauses agents; updates during a pause are rejected, not buffered.

Each Windows overlay starts hidden and requires `WS_EX_NOACTIVATE` and
`WS_EX_TOOLWINDOW` with `WS_EX_APPWINDOW` clear before its first show. The native
observer preserves those bits across extended-style changes, including cursor
hit-test changes. Windows overlay close requests leave the runtime running.
Tray Quit and development window close use the normal shutdown path.
`WM_MOUSEACTIVATE` keeps ordinary clicks local without
activation; an accepted composer or panel text-field hit, or the start of a
portal OUTPUT selection (so Ctrl+C can copy it), explicitly takes focus
outside scene guards. Each explicit take records one outer external foreground;
nested primary/secondary takes preserve it. A keyboard answer or outer Esc may
return only while an installed HUD HWND still owns the foreground and the saved
origin is live. User app switches win. A sent reply retains composer focus.

Notification publication never activates an OS window. The owner's press on
a card's Go to control can activate one eligible whole-title-token match,
with no position or size chosen by the runtime. The presentation lookup can
only decorate the card. A press repeats the lookup and goes through the
ordinary exact-publication input guard and local two-second limit. Queued
keyboard presses require an installed HUD window in the foreground. A newer
foreground app cancels the native handoff. Successful activation clears saved
return focus. A replaced or expired card cannot
activate a window from a pending lookup, and neither a lookup nor a switch
queues agent input.
`go_to_looks_up_once_after_present_and_rechecks_owner_pointer_and_keys` and
`go_to_drops_a_replaced_press_while_another_card_rechecks` cover the runtime
paths. The hosted `go-to-session` gesture checks a real switch and unchanged
window bounds.

The fourth owner chord (`answer_hotkey`, Ctrl+Shift+F9) pins the newest rendered
unanswered question card. With no card it targets the newest waiting portal's
composer; no target is a no-op; safe mode still captures input.
Its digits 1–4, arrows, Tab/Shift+Tab and Enter/Space cannot retarget stale
publications or leak text/repeats/releases to an editor. URL digits keep the
existing per-action two-second limiter and card lifetime. Chord hints reserve
geometry in both states. Esc removes only the innermost selection or
nested card/control state before an outer exit; it never removes draft text. An owned HWND handoff retains
card focus; an external loss cancels it. A running panel yields to an explicit
answer request only after a question is found, without restoring the origin
underneath the card.

Activating a reply action gives the inline field keyboard focus on its display.
The compositor publishes the drawn caret position for native IME placement.
Committed text enters the card's local draft. Escape first clears selection,
then collapses the field, returns focus and keeps the draft for that exact
card. Safe mode suspends editing and submission. A focused nonempty field
defers expiry without changing the original deadline. Leaving the field after
that deadline expires the card. Replacement, clear, trim and dismissal drop
the exact card's draft.

Existing runtime parent
[`notification_keyboard_cycle_activation_and_return_preserve_portal_editor`](../crates/tze_hud_runtime/src/windowed/event_loop_harness.rs)
executes these logical keyboard routes; `outer_focus_return_preserves_origin_and_user_switches`
checks the outer origin policy. Existing config, registration and panel parents
cover the fourth chord, conflicts and rebinding. The compositor's
[`notification_actions_layout_matches_hit_regions`](../crates/tze_hud_compositor/src/renderer/tests/zone_notification.rs)
checks hint visibility and unchanged label/hit geometry. The owned tier-W
`overlay-pointer` scenario exercises actual overlay styles, Notepad typing,
registered hotkey/answer/return (including the chord reaching a waiting
portal), nonactivating pane and card clicks, third-app Alt-Tab and a closed
origin; the informational `tray-answer` covers the tray click and its return. Source and headless checks do not substitute for
that required Windows pass or the epic's owner check of how focus return feels.

A matching human notification × removes that exact publication locally and
queues one typed dismiss for its owner. Callback auto-removal, TTL and explicit
clear do not impersonate a human ×. Accepted portal input and truthful rejection
feedback bypass output pacing once in the submitting maintenance turn; ordinary
agent output retains 100 ms cadence. This is scene readiness, not physical-display
or Windows-owner acceptance.

Notification feedback clears on leave, release, cancellation or capture loss,
and cannot outlive clear, expiry, replacement or dismiss. Terminal cleanup
retries on existing scene availability, never a polling timer; only an actual
flag change invalidates geometry. Agents cannot hold this chrome state or turn
cancellation into an action. Existing local owner coverage:
[`action_button_hover_and_press_are_local_and_frame_only_on_change`](../crates/tze_hud_runtime/src/windowed/event_loop_harness.rs).
Windows held-press/release acceptance remains separate.

A destructive notification action (MCP `role:"destructive"`) is never taken by
one click, Enter, Space or digit: it fires once only when a press is held for
`notification.action.hold.ms`, and nothing takes any answer automatically. The
hold is local feedback; agents see nothing until it completes. Release, leave,
capture or keyboard-focus loss, another key, hide-all, safe mode, and clear,
expiry or replacement end it with no callback, and a held key cannot act on
another card. The first non-destructive action is the primary (filled; the
answer chord's initial focus). Runtime coverage:
[`pointer_hold_fires_once_at_the_token_duration`](../crates/tze_hud_runtime/src/windowed/event_loop_harness/tests/hold.rs)
and its siblings; the owner's Windows feel check of the hold length remains
separate.

Existing zone grips move the zone slot for the runtime session. The effective
scene policy feeds every draw, text, layout-cache and hit consumer. Moves/reset
change only geometry readiness, with unchanged values remaining idle; no Zone
agent event, tile creation or disk override is produced. Clear, expiry, chosen
exit and publisher replacement retain placement. An existing gesture may finish
on an empty live slot; new Down requires a shown, emitted live handle. Hidden
zones, removed/replaced definitions or lost original window allocations cancel
without resurrection. Capture remains pinned to the press window, and deferred
Up settles once on availability without another ring enqueue. Reset restores
the default in-session; fresh bootstrap retains IDs and starts with defaults.
Regression owners (existing parents extended, Tests: +0 ~6 -0):

- Runtime [`windowed_post_render_refresh_updates_drag_handles_before_snapshot`](../crates/tze_hud_runtime/src/windowed/hittest.rs): actual rendered handle, runtime drag and refreshed grip/X/action translation.
- Hover-only move pills: the same post-render parent checks painted notification
  hover bounds, frontmost overlapping body selection and the card-to-grip gap
  without body/corridor capture;
  `armed_cursor_entry_never_wakes_compositor_until_hover_changes`
  exercises passthrough entry/exit and unchanged-target quiet turns. Compositor
  `drag_handle_opacity_switches_to_active_on_hover_state`, `drag_visual_feedback_applied_during_active_drag`
  and `idle_render_gate_renders_for_composer_echo_and_caret_blink` cover semantic
  geometry, active feedback and one-frame mirror invalidation.
- Runtime [`header_band_anchor_is_the_frame_and_band_drag_is_position_only`](../crates/tze_hud_runtime/src/windowed/portal.rs): Tile controls and Zone deferred terminal, replacement, cancellation and no persistence/event.
- Runtime [`action_button_hover_and_press_are_local_and_frame_only_on_change`](../crates/tze_hud_runtime/src/windowed/event_loop_harness.rs): moved four-action controls, lifetime and epoch accounting.
- Runtime [`bootstrap_round_trip_reuses_ids_across_restart`](../crates/tze_hud_runtime/src/element_store.rs): clear/republish retains placement; restart restores defaults without new saved overrides.
- Compositor [`drag_handle_regions_cover_visible_tile_and_zone_but_not_widget`](../crates/tze_hud_compositor/src/renderer/tests/hit_drag.rs): actual monitor-assignment/scale producer matrix, including connected-secondary controls without a grip.
- Compositor [`zone_hit_regions_and_slot_pitch_scale_with_px_scale`](../crates/tze_hud_compositor/src/renderer/tests/hit_drag.rs): moved controls, shared human reset and no-op reset at existing scales.

The pointer is a hand exactly where a primary press would act on a HUD control,
read from the last frame's control rects (answered cards and card controls in
safe mode keep the arrow); it outranks the drag handle's open hand and follows
controls that appear or leave under a resting pointer on the next turn, with no
polling. `/admin/status` `cursor` reports the applied shape, `null` while a
click-through overlay leaves the pointer to the app beneath. Owners: runtime
[`pressable_control_beats_drag_handle_cursor`](../crates/tze_hud_runtime/src/windowed/hittest.rs),
`pressable_cursor_follows_refusal_geometry_and_active_drag`,
`pressable_regions_follow_what_a_press_would_do` and
`admin_cursor_is_null_while_the_app_beneath_owns_the_pointer`.

Grip feedback uses `zone_slot_layout.iter_visible` to enclose the occupied
notification slots, not the reserved capacity. It changes neither zone geometry
nor layout admission. The single-item Reset menu publishes one frame-resolved
paint/action/capture rect, measured with the renderer's font owner and clamped
to the originating display. Its text has a separate prepared renderer sharing
the same font system/atlas, above its plate and below safe-mode/system chrome;
no stale glyphs replay after close. Menu Down/Up and captured moves stay local,
with matching menu/window identity required to reset and cancellation consumed.
Menu open/close dirties geometry, using the existing deadline and availability
wake paths rather than polling. The existing compositor drag feedback, chrome
pixel and target-signature parents and runtime hit-region/real-drag parents
cover these source seams (Tests: +0 ~5 -0 for the grip-chrome repair).

Expanded portal text leaves its backdrop to the pane, rather than emitting
zero-alpha flat fills that erase it in the overlay REPLACE pass. The existing
compositor `hud_w41ef_portal_backdrop_opaque_after_resize_grow_no_fade` parent
captures the production rounded portal through Normal/Simplified/recovery and
settled activation/move/release. This prevents transparent body holes when drag
work triggers simplified rendering; intended token alpha and fades remain.
The generated composer is installed separately from its render tree. Its hover
and press tint the existing expanded input pane or collapsed text background before fade/alpha conversion,
preserving the pane's coverage and paint order in normal and simplified frames.
The same parent checks actual composer pixels, leave, partial/zero fades, a
custom translucent/transparent panes, distinct and zero-alpha style overrides,
collapsed feedback and overlap with a higher tile. A fully transparent pane
does not gain a simplified fill that could erase the frame behind it.
The windowed pointer path advances the geometry epoch for a nonempty local
feedback patch, so composer hover/leave and press/release repaint without
waiting for a caret blink or invalidating content caches. Unchanged moves stay
idle. The existing runtime `action_button_hover_and_press_are_local_and_frame_only_on_change`
parent covers these transitions through the production pointer entry point.

These are source/headless proofs. Exact-deployed-SHA Windows motion/drop,
reset/restart, native capture/focus and four-action pointer/keyboard owner
acceptance remain pending; title passthrough is not a defect claim.

Notification controls join bare Tab / Shift+Tab traversal at the existing
owner's boundary: X followed by live actions, with Enter or Space activating
through the same exact-publication path as a pointer release. Publishing never
steals focus or activates an OS window. Hidden, stale or already chosen controls
cannot answer. Focus loss preserves the editor and quarantines matching consumed
key releases; deferred cleanup also settles before a returned Character event.
[`notification_keyboard_cycle_activation_and_return_preserve_portal_editor`](../crates/tze_hud_runtime/src/windowed/event_loop_harness.rs)
covers traversal, activation, hidden controls and focus return.

Portal composer edits stay local; accepted submissions alone atomically enter
input and native reply history. Repeated empty Enter, whitespace, Esc,
unknown tiles and rejected submits add neither accepted input nor readiness.
Blur and Esc keep each composer's nonempty draft (hud-ohgc9.4): a peer's
draft never displaces it, the unfocused draft is drawn without a caret and
reported by length only, and removed nodes cannot revive saved text. No Escape
key or command reaches an agent while a composer is active
([`bare_escape_in_composer_never_reaches_agent`](../crates/tze_hud_runtime/src/windowed/event_loop_harness.rs),
[`portal_escape_keeps_draft_and_reading_keys_scroll_output`](../crates/tze_hud_runtime/src/windowed/event_loop_harness.rs)). Focus and the
unsent draft survive portal re-renders (sends and streamed output): a tile keeps
one composer node across root replacements. Regression owners:
[`manager_draft_persists_across_focus_loss_and_restores`](../crates/tze_hud_input/src/composer_draft.rs),
[`composer_focus_lost_on_blur_preserves_keyed_draft`](../crates/tze_hud_input/src/lib.rs),
[`removing_focused_composer_tile_clears_local_echo`](../crates/tze_hud_runtime/src/windowed/lifecycle.rs),
[`streamed_output_keeps_the_focused_draft`](../crates/tze_hud_runtime/src/windowed/keyboard.rs),
and [`composer_interaction_hit_region_survives_consecutive_transcript_republishes`](../crates/tze_hud_scene/src/graph/portal_tests.rs).
No wire message carries a node, so no agent can mark a hit region as a
composer: [`removed_client_request_numbers_decode_to_empty_payload`](../crates/tze_hud_protocol/src/session_server/tests/session_handshake.rs).


Native portal INPUT history and OUTPUT have independent local scroll tracking.
Typing/submit resets only INPUT. A busy scene/shared-state lock retains the
post-edit reset in the existing keyboard FIFO; recovery neither replays the
accepted edit nor sends a reply. Retained wheel input and keyboard work keep
arrival order through the existing pointer FIFO, so a later wheel wins over
an earlier reset and a later edit resets after an earlier wheel. Appends retain
the OUTPUT reading anchor and fixed pane scissor. Bounds come from the actual full wrapped paint buffer, not
combined raw newline counts. Unchanged measured layouts schedule no frame.
The existing compositor regression
[`expanded_portal_chrome_stays_fixed_while_document_content_scrolls_after_resize`](../crates/tze_hud_compositor/src/renderer/tests/scroll.rs)
covers actual pane wheel routing, retained visible-row y through append/rolling
head trim, full layout versus clipping, and input reset/output jump-to-latest.
Unique visible lines can retain identity across trimming; repeated ambiguous
lines keep a clamped pixel position. Evicted text is outside the retained window.

A pointer-down on a tile's text/background transfers focus to that tile and
blurs its composer, preserving the keyed unsent draft for refocus. This makes
Ctrl+Shift+Arrow directional portal resize reachable from OUTPUT; the same
chord retains text-selection behavior while the composer is active.

Native unread counts accumulate new output units across render batches. Keyed
replacements and viewer replies do not add unseen units. Reaching the measured
OUTPUT tail acknowledges only the installed output ordinal for that attachment;
a racing newer publish remains unread. Backscroll alone cannot restore a read
count. Identical shaped content still reconciles a newer ordinal. MCP reports
the same native count as `unread` (omitted at zero) on the portal's
`hud_surfaces` entry and its publish replies; it never adds a second
counter ([`portal_unread_is_reported_until_the_viewer_reaches_the_tail`](../crates/tze_hud_mcp/src/server/tests.rs)).
This is the one owner-approved exception to "agents never see the panel"
below and to §10 (hud-ljhku.5 R5.4 = B, owner 2026-11-22): output is read
only when the viewer frame reaches the OUTPUT tail, so while the panel is
open or the portal hidden the count keeps growing, and an agent can infer
that its output was not presented. Reading does
not renew agent liveness, and repeated reconciliation schedules no idle repaint.
For a first-class portal, zero unread hides the jump badge's fill, label and hit
target together. Scrolling through read output cannot leave an empty fill behind;
ordinary non-portal jump controls retain their independent visibility policy.
The jump pill defaults to opaque fill/text and draws above OUTPUT glyphs on a
separate bounded text layer; display translation and its shared hit geometry
remain aligned. Header drag yields to that tile's runtime close control, while
empty header space and legacy grips keep their drag behavior. The real pointer
Down/Up harness checks dismissal, reclaim, unaffected input and later reattach;
this source proof is separate from Windows owner acceptance. Secondary-window
signatures include the pill fill and text. Higher effective-z cards clip both
the pill and its hit targets; partial coverage preserves the full label placement.


- `tze_hud_runtime` [`release_activates_only_the_button_the_press_began_on`](../crates/tze_hud_runtime/src/windowed/event_loop_harness.rs),
  [`authority_attached_submit_snaps_scrolled_back_history_to_tail`](../crates/tze_hud_runtime/src/windowed/keyboard.rs),
  [`portal_composer_submission_enters_pending_input_queue_and_can_be_acknowledged`](../crates/tze_hud_runtime/src/portal.rs)
- `tze_hud_portal` [`take_due_paces_renders_at_the_render_interval`](../crates/tze_hud_portal/src/hub.rs)

- `tze_hud_runtime` [`shell::safe_mode`](../crates/tze_hud_runtime/src/shell/safe_mode.rs) [`test_enter_safe_mode_suspends_active_leases`](../crates/tze_hud_runtime/src/shell/safe_mode.rs),
  [`test_mutations_rejected_via_shared_state_flag`](../crates/tze_hud_runtime/src/shell/safe_mode.rs)
- `tze_hud_runtime` control panel (global pause chord, default
  Ctrl+Shift+F12, `[runtime].safe_mode_hotkey`, handled on the network
  runtime, never the winit loop or an agent). Windows tray Pause uses the
  same panel task and safe-mode control even when another app owns the chord;
  Resume closes the panel. The native icon and WTS state share the existing
  display-watcher window/thread and schedule no idle frames.
  [`tray_presentation_and_session_transitions`](../crates/tze_hud_runtime/src/windowed/shell_presence.rs),
  [`tray_svg_uses_taskbar_contrast_and_semantic_state_tokens`](../crates/tze_hud_runtime/src/windowed/shell_presence.rs),
  `integration` [`poc_override_safe_mode_wins_with_hung_grpc_agent`](../tests/integration/poc_acceptance.rs),
  [`portal_liveness_paused_during_safe_mode`](../crates/tze_hud_runtime/src/windowed/panel_input.rs),
  [`panel_chord_opens_running_and_escape_closes_and_resumes`](../crates/tze_hud_runtime/src/windowed/panel_input.rs),
  [`panel_owns_its_opening_monitor_until_close`](../crates/tze_hud_runtime/src/windowed/panel_input.rs),
  [`winit_routing_keys_and_pointer_reach_the_open_panel_only`](../crates/tze_hud_runtime/src/windowed/panel_input.rs),
  [`refused_focus_shows_a_notice_and_chords_still_work`](../crates/tze_hud_runtime/src/windowed/panel_input.rs),
  [`an_idle_open_panel_does_not_wake_on_refresh_ticks`](../crates/tze_hud_runtime/src/windowed/panel_input.rs);
  agents never see the panel (except through the portal's native `unread`
  count, above):
  `tze_hud_compositor` [`panel_draws_over_the_frame_and_capture_leaves_it_out`](../crates/tze_hud_compositor/src/renderer/tests/control_panel.rs),
  [`secondary_panel_draws_at_its_scale_and_leaves_pairing_on_primary`](../crates/tze_hud_compositor/src/renderer/tests/control_panel.rs),
  [`capture_leaves_the_closed_notice_out`](../crates/tze_hud_compositor/src/renderer/tests/control_panel.rs),
  `integration` [`poc_agents_see_the_same_with_the_panel_open_or_content_hidden`](../tests/integration/poc_acceptance.rs)
- `tze_hud_config` [`safe_mode_hotkey_defaults_overrides_and_rejects_garbage`](../crates/tze_hud_config/src/tests.rs),
  [`hud_chords_must_be_distinct`](../crates/tze_hud_config/src/tests.rs)
- `integration` [`shell_dismiss_override_removes_portal_tile`](../tests/integration/text_stream_portal_governance.rs),
  [`shell_status_snapshot_exposes_no_portal_identity_or_transcript`](../tests/integration/text_stream_portal_governance.rs)
- viewer dismiss (hover close button): `tze_hud_scene`
  [`viewer_dismiss_tile_revokes_lease_in_any_live_state`](../crates/tze_hud_scene/src/graph/tests.rs); `tze_hud_runtime`
  [`viewer_dismiss_portal_detaches_and_next_publish_reattaches`](../crates/tze_hud_runtime/src/portal.rs),
  [`viewer_close_button_dismisses_hovered_tile`](../crates/tze_hud_runtime/src/windowed/event_loop_harness.rs); `integration`
  [`poc_portal_viewer_dismiss_then_mcp_verbs`](../tests/integration/poc_acceptance.rs);
  `tze_hud_compositor` [`tile_close_button_draw_and_hit_region_share_token_geometry`](../crates/tze_hud_compositor/src/renderer/tests/hit_drag.rs)
- hung agents (a raw HTTP/2 client that stops reading until the server's send
  buffer to it is full): `integration`
  [`poc_override_safe_mode_wins_with_hung_grpc_agent`](../tests/integration/poc_acceptance.rs),
  [`poc_override_exit_safe_mode_with_agent_hung_during_it`](../tests/integration/poc_acceptance.rs); a responsive agent
  is told, [`poc_override_notifies_the_agent`](../tests/integration/poc_acceptance.rs) (`SessionSuspended`,
  `SessionResumed`)
- `tze_hud_mcp` [`hud_publish_in_safe_mode_returns_safe_mode_active`](../crates/tze_hud_mcp/src/server/tests.rs),
  [`safe_mode_does_not_regrant_suspended_mcp_lease`](../crates/tze_hud_mcp/src/server/tests.rs),
  [`resume_restores_mcp_publishing`](../crates/tze_hud_mcp/src/server/tests.rs)
- `tze_hud_scene` [`graph::widget_tests::widget_publish_with_suspended_lease_is_safe_mode_active`](../crates/tze_hud_scene/src/graph/widget_tests.rs)
- `tze_hud_compositor` [`test_chrome_always_above_max_zorder_tile`](../crates/tze_hud_compositor/src/renderer/tests/surface.rs),
  [`windowed_frame_draws_chrome_overlay_in_safe_mode`](../crates/tze_hud_compositor/src/renderer/safe_mode_overlay.rs)

The control panel's per-agent Clear and Unpair are the same override at agent
scope: Clear revokes every live lease the agent holds (in any state) with its
tiles, removes all of its zone and widget publications under any lease or
none, drops its scheduled content and reclaims its portals; nothing waits
on the agent, and its next publish is
served (portals reattach). Unpair first removes its PSK hash from
`agents.toml` (so no new request authenticates, and a gRPC handshake that
authenticated against the old directory is refused when it registers), then
ends its gRPC sessions by dropping their handlers wherever they wait, waits
for those tasks to end, then clears.

- `tze_hud_scene` [`reclaim_agent_clears_everything_it_holds_and_nothing_else`](../crates/tze_hud_scene/src/graph/agents.rs);
  `tze_hud_portal` [`reclaim_agent_reclaims_only_that_agents_portals`](../crates/tze_hud_portal/src/hub.rs)
- `integration` [`poc_override_clear_and_unpair_with_hung_grpc_agent`](../tests/integration/poc_acceptance.rs),
  [`poc_panel_clear_reattaches_and_grants_apply_to_the_next_request`](../tests/integration/poc_acceptance.rs),
  [`poc_panel_roster_updates_live_without_moving_focus`](../tests/integration/poc_acceptance.rs);
  `tze_hud_protocol` [`test_handshake_unpaired_mid_flight_is_refused_at_registration`](../crates/tze_hud_protocol/src/session_server/tests/session_handshake.rs)
- `agents.toml` writers (pairing and the panel) serialize, re-read and keep
  comments and hand edits: `tze_hud_runtime` [`racing_writers_lose_nothing_and_keep_comments`](../crates/tze_hud_runtime/src/agent_store.rs),
  [`hand_edits_survive_panel_changes_and_reach_the_live_directory`](../crates/tze_hud_runtime/src/agent_store.rs);
  `tze_hud_config` [`edits_keep_comments_and_other_agents`](../crates/tze_hud_config/src/agents_file.rs)

Portal rendering is native scene data. The renderer's fixed visual fixtures and
integration token/composer checks preserve presentation independently of wire
encoding: [`render_visual_goldens`](../crates/tze_hud_portal/src/render_goldens.rs),
[`portal_node_uses_injected_visual_tokens`](../tests/integration/text_stream_portal_styling.rs),
[`local_echo_does_not_depend_on_adapter_round_trip`](../tests/integration/text_stream_portal_composer.rs).
The driver preserves the first-class surface and focusable composer in
[`drain_declares_first_class_portal_surface_on_tile`](../crates/tze_hud_runtime/src/portal.rs).

## 4. Resident termination releases only its own publications

Local portal attachments and existing internal lease owners retain their orphan
grace and restoration rules. A disconnected lease keeps its content for its
grace period; expiry reclaims it without agent help. Reclaiming a lease clears
only publications made under that lease, never the agent's whole namespace.

A resident stream ends on transport termination, explicit close, the existing
receive-timeout handling or unpair. Cleanup clears only the active and
scheduled zone and widget publications still owned by that origin, aborts its
pending uploads and returns admission capacity. Other streams, MCP, newer
replacements and independently leased content keep their own lifetimes.
Reconnects establish fresh origins through `SessionInit` and republish desired
state. No public scene snapshot exposes an origin or scene nodes.

- `tze_hud_protocol` [`disconnect_transitions_to_orphaned_and_sets_disconnection_badge`](../crates/tze_hud_protocol/tests/lease_governance.rs),
  [`grace_period_expiry_removes_tile_and_nodes`](../crates/tze_hud_protocol/tests/lease_governance.rs)
- `tze_hud_runtime` [`portal_degrade_then_publish_within_grace_resumes_same_tile`](../crates/tze_hud_runtime/src/portal.rs)
- `tze_hud_protocol` (gRPC session) [`grpc_termination_clears_owned_publications_without_touching_peer`](../crates/tze_hud_protocol/src/session_server/tests/invariants.rs)
- `tze_hud_runtime` (headless frame sweep) [`render_frame_reclaims_orphaned_lease_after_grace`](../crates/tze_hud_runtime/src/headless.rs)
- `tze_hud_compositor` (badge draw command) [`orphaned_tile_emits_disconnection_badge_draw_cmd`](../crates/tze_hud_compositor/src/renderer/tests/focus_chrome.rs)
- `integration` (POC acceptance, over gRPC and MCP with `production.toml`)
  [`poc_portal_abandoned_is_reclaimed`](../tests/integration/poc_acceptance.rs)
- `tze_hud_scene` [`tile_lease_reap_keeps_same_namespace_mcp_publications`](../crates/tze_hud_scene/src/graph/behavior_tests.rs),
  [`revoked_lease_clears_only_its_publications`](../crates/tze_hud_scene/src/graph/behavior_tests.rs)

## 5. Leases hold time; safe mode pauses it

Every lease has a TTL and expires on its own. Suspension (safe mode) pauses
the TTL clock and preserves lease identity; resuming continues it. Healthy
portal attachments also retain their remaining idle allowance and finite hold,
including attachments awaiting first paint; indefinite holds stay indefinite.
A genuine input poll during pause refreshes its own portal normally, without
also crediting the elapsed pause on resume. Repeated transitions are no-ops.
Output updates are rejected, never buffered for replay. Input TTL, finite poll
ownership, prior orphan grace and the maximum scene suspension still expire.
Blocked render/cue work stays pending until resume without periodic retries.
Widget publication TTLs and owner auto-clear allowances also pause, without
moving their original publication timestamp. Countdown and elapsed parameter
deadlines are absolute wall time: they keep passing, retain their last pixels
without retries while paused, and refresh to the current time on resume
([`widget_elapsed_presentation_retention_cadence_and_safe_mode`](../crates/tze_hud_scene/src/graph/widget_tests.rs)).
Already accepted scheduled widget publications wait through
pause, without an overdue wake or materializing under a suspended lease. On
resume their deadline and relative TTL start at actual presentation; new incoming
updates remain rejected while paused. Fresh zone/widget Hold is also refused
before changing active or pending publications, expiries, scene version or
leases, including a request queued behind pause at the scene lock. A suspended
MCP lease refuses Hold before mutation even if its early pause flag was false.
Resident termination still clears its active and pending publications while paused.

An already completed gRPC sequence replays its cached result without retiming
content; normal finite and indefinite Hold work again after resume.
Terminal lease cleanup still removes its queued publications during pause,
including revocation at the maximum suspension bound. Other owners' queued
publications survive.

- `tze_hud_runtime` [`test_ttl_excluded_during_suspension`](../crates/tze_hud_runtime/src/shell/safe_mode.rs),
  [`test_lease_identity_preserved_across_suspend_resume`](../crates/tze_hud_runtime/src/shell/safe_mode.rs),
  [`portal_liveness_paused_during_safe_mode`](../crates/tze_hud_runtime/src/windowed/panel_input.rs)
- `tze_hud_scene` [`test_lease_suspend_from_active`](../crates/tze_hud_scene/src/graph/tests.rs), [`test_lease_resume_from_suspended`](../crates/tze_hud_scene/src/graph/tests.rs)
- `tze_hud_mcp` [`safe_mode_does_not_regrant_suspended_mcp_lease`](../crates/tze_hud_mcp/src/server/tests.rs),
  [`resume_restores_mcp_publishing`](../crates/tze_hud_mcp/src/server/tests.rs),
  [`hud_publish_in_safe_mode_returns_safe_mode_active`](../crates/tze_hud_mcp/src/server/tests.rs)
- `tze_hud_protocol` [`test_safe_mode_rejects_mutations`](../crates/tze_hud_protocol/src/session_server/tests/safe_mode.rs)

## 6. Degradation changes drawing, never state

Under load the runtime switches to Simplified rendering (downscaled textures,
opaque fills, snapped animations). It never hides tiles, revokes leases, or
mutates the scene. Enforced structurally: `DegradationController` has no
scene access and `CompositorDegradationPolicy` carries no suppression set.
Widget dock overflow follows the same rule: when a display or scale change
leaves the column too short, the bottom rows stop drawing and are listed in
`/admin/status` `dock.overflow`, but no publication is removed, updates
keep landing, and the rows draw again when room returns.

- `tze_hud_compositor` [`significant_degradation_preserves_hidden_tiles_and_opaques_visible_tiles`](../crates/tze_hud_compositor/src/renderer/tile_render.rs)
- Dock overflow: `tze_hud_scene` [`dock_identity_and_order_survive_updates_and_rejected_admission`](../crates/tze_hud_scene/src/graph/dock_tests.rs)
  and [`dock_revokes_stale_samples_and_keeps_existing_rows_on_display_loss`](../crates/tze_hud_scene/src/graph/dock_tests.rs),
  `tze_hud_compositor` [`dock_draws_only_on_its_accepted_display_and_parks_overflow`](../crates/tze_hud_compositor/tests/widget_raster_count.rs),
  `tze_hud_runtime` [`reports_dock_capacity_overflow_and_widget_session`](../crates/tze_hud_runtime/src/operator/surface_status.rs)
- `tze_hud_runtime` [`degradation::tests::sustained_overload_does_not_escalate_past_simplified`](../crates/tze_hud_runtime/src/degradation.rs)

## 7. Agent-facing caps bound retained work

gRPC session admission enforces separate resident and guest capacity pools. A full pool refuses a new session with its recovery hint. MCP bounds its scoped sessions per agent and releases the least recently used scope when it needs space.

Zone admission honors the configured contention, publisher caps, and stack depth. A full stack follows its published trim policy and keeps a bounded set of cards. The widget dock's `max_rows` is a hard cap on session rows, as is the number of rows its column fits: a publish that needs another row fails `CONTENT_REJECTED` and changes nothing, and no instance is evicted to admit another session's. Portal publish bounds each payload and its retained transcript, and coalesces render work at its render interval. Its reply queue bounds count and bytes, refuses overflow, and keeps accepted replies until acknowledgement or expiry.

Resident resource upload enforces runtime-wide texture memory and resource count, per-resource raw and decoded size, concurrent upload slots, and the upload byte rate. A rejected resource adds no stored object, and its upload reservation is released on failed completion or abort. The upload lane applies byte-rate backpressure while the resident session can still acknowledge heartbeats.

Resident raster first grants validate bytes, type and dimensions and commit under
current pairing authority with storage and CPU admission. Equal validated images
share one retained pixel allocation and CPU charge. Immediate and scheduled image
admission require that pairing's grant and the zone's static-image allowlist; due
application and retained draw encoding recheck current authority. Unpair or re-pair
revokes only that incarnation, and old resident operations cannot grant or mutate its replacement's
content. Typed/wire snapshots retain hashes, not raster authority. GPU textures use
the existing budget/degradation policy in the configured layer and stay isolated
from generic tile, icon and MCP lookup.


At capacity, MCP releases the least recently called idle scope first. If each
scope owns state, it releases the least recently called scope overall.

- gRPC admission and recovery hints are covered by
  [`guest_sessions_use_a_separate_enforced_pool`](../crates/tze_hud_runtime/src/mutation_budget_bridge.rs)
  and [`test_session_registration_rejections_carry_recovery_hints`](../crates/tze_hud_protocol/src/session_server/tests/session_handshake.rs).
- MCP eviction is covered through the server by
  [`the_session_table_is_bounded_per_agent`](../crates/tze_hud_mcp/src/server/tests/scopes.rs).
- Zone and widget admission keep their publisher caps. See
  [`test_contention_zone_max_publishers_rejected`](../crates/tze_hud_scene/src/graph/tests.rs),
  [`test_contention_widget_max_publishers_rejected`](../crates/tze_hud_scene/src/graph/tests.rs)
  and [`full_stack_trims_oldest_status_and_keeps_the_question_on_both_planes`](../crates/tze_hud_scene/src/graph/stack_trim_tests.rs).
- The dock row cap is covered by
  [`dock_identity_and_order_survive_updates_and_rejected_admission`](../crates/tze_hud_scene/src/graph/dock_tests.rs),
  [`dock_refusal_preserves_sparse_rows_and_mcp_leases`](../crates/tze_hud_mcp/src/server/tests/scopes.rs)
  and, at both signed display sizes,
  [`shipped_dock_fills_both_signed_display_capacities`](../crates/tze_hud_compositor/tests/widget_raster_count.rs).
- Portal payload, retention and render pacing are covered by
  [`publish_attaches_implicitly_and_lists_for_owner_only`](../crates/tze_hud_portal/src/hub.rs),
  [`transcript_retention_trims_head_past_256k_keeps_16k_visible`](../crates/tze_hud_portal/src/hub.rs)
  and [`take_due_paces_renders_at_the_render_interval`](../crates/tze_hud_portal/src/hub.rs).
  [`portal_input_queue_matches_its_model`](../crates/tze_hud_portal/tests/input_queue.rs)
  covers reply bounds, acknowledgement and expiry.
- Resource admission is covered by
  [`runtime_wide_texture_cap_is_shared_across_agents`](../crates/tze_hud_resource/tests/store_behavior.rs),
  [`resource_count_cap_rejects_the_extra_resource`](../crates/tze_hud_resource/tests/store_behavior.rs),
  [`per_resource_size_cap_rejects_chunked_upload_at_start`](../crates/tze_hud_resource/tests/store_behavior.rs),
  [`rejections_carry_stable_wire_code_and_actionable_detail`](../crates/tze_hud_resource/tests/store_behavior.rs),
  [`upload_slot_cap_is_per_agent_and_freed_by_abort`](../crates/tze_hud_resource/tests/store_behavior.rs)
  and [`chunked_upload_rejected_at_complete_frees_its_slot_and_stores_nothing`](../crates/tze_hud_resource/tests/store_behavior.rs).
  [`test_resource_upload_backpressure_keeps_heartbeat_responsive`](../crates/tze_hud_protocol/src/session_server/tests/resource_upload.rs)
  covers transport throttling.
- Current-pairing raster admission, independent grants, revocation races and
  disconnect-before-worker-join are covered through real resident streams in
  [`raster_tests`](../crates/tze_hud_runtime/src/shell/control_panel/raster_tests.rs).
  Store-allocation reuse, isolated lookup, stale draw denial and foreground
  layering have GPU readback witnesses in
  [`retained_image`](../crates/tze_hud_compositor/src/renderer/tests/retained_image.rs).


Internal scene tiles keep their structural guards, covered by
[`tile_limit_1024_per_tab_enforced`](../crates/tze_hud_scene/src/graph/limits_tests.rs)
and [`node_limit_64_per_tile_enforced`](../crates/tze_hud_scene/src/graph/limits_tests.rs).

## 8. Errors are affordances for the model

Every rejection carries a stable error code and a `hint` that tells the model
what to do next. Codes are part of the API and don't change silently. A good
error saves a model a round trip and its tokens.

Typed widget values are rejected rather than silently clamped (a fractional
stepper count is refused, not floored:
[`stepper_counts_are_whole_numbers`](../crates/tze_hud_widget/tests/shipped_bundles.rs),
[`integer_counts_are_discovered_and_fractions_refused`](../crates/tze_hud_mcp/src/server/tests.rs));
invalid mixed
updates leave publication/current parameters unchanged. Both transports use
the same scene admission check. Widget audiences default to model; owner/runtime
parameters fail before any publication, lease or partial update, including a
gRPC parameter with an absent value. Errors name the writer, list enum choices
or the expected type/range, and stay within the 60-token result ceiling.
Non-model instance values survive every publication policy and clear/expiry;
only runtime-local hover writes tooltip visibility. LatestWins/Stack/MergeByKey
partial updates retain omitted ordinary model fields in the exact prior
namespace/key/lease/preclaim-origin record; incoming validated values win.
Lease-bound origins normalize away, while unleased preclaim origins stay
separate. No other identity or shared instance value seeds that record. Peer
clear, TTL, lease and origin cleanup restore its retained values through
occupancy. Replace's ordinary map and the separate deadline snapshot rules
are unchanged. MCP urgency and status entries reject wrong
types, and notification TTL is top-level only.
Successful replies follow the same rule: a publish whose text the renderer
would cut reports, per cut field, how many characters are drawn, so one
shortened republish lands; a reply where everything fits keeps its bytes.
Refusals name the field and limit to fix, and no hint carries an internal id.

- `tze_hud_scene` [`widget_audiences_reject_agent_writes_and_survive_publication_lifecycle`](../crates/tze_hud_scene/src/graph/widget_tests.rs),
  [`clear_widget_for_publisher_refreshes_current_params_from_remaining_publish`](../crates/tze_hud_scene/src/graph/widget_tests.rs),
  [`widget_ttl_only_expired_publication_removed_when_mixed`](../crates/tze_hud_scene/src/graph/widget_tests.rs)
- `tze_hud_widget` [`status_indicator_latest_publisher_wins_and_partial_updates_keep_other_params`](../crates/tze_hud_widget/tests/shipped_bundles.rs)
- `integration` [`poc_widget_discovery_and_errors_are_self_sufficient`](../tests/integration/poc_acceptance.rs)
- `tze_hud_scene` [`widget_publish_out_of_range_values_rejected`](../crates/tze_hud_scene/src/graph/widget_tests.rs), [`widget_publish_f32_nan_rejected`](../crates/tze_hud_scene/src/graph/widget_tests.rs)
- `tze_hud_mcp` [`widget_publish_hold_clear`](../crates/tze_hud_mcp/src/server/tests.rs), [`user_test_zone_fixtures_and_documented_examples_match_production_parser`](../crates/tze_hud_mcp/src/server/tests.rs)
- `tze_hud_protocol` [`test_durable_widget_publish_receives_result`](../crates/tze_hud_protocol/src/session_server/tests/widget_publish.rs)
- `clipped` counts: `integration` [`poc_publish_reports_clipped_text_only_when_cut`](../tests/integration/poc_acceptance.rs),
  `tze_hud_compositor` [`actions_report_the_smallest_cut_label`](../crates/tze_hud_compositor/src/renderer/text_fit.rs),
  [`cut_status_value_counts_what_republishes_whole`](../crates/tze_hud_compositor/src/renderer/text_fit.rs),
  [`status_entry_past_a_full_bar_counts_zero`](../crates/tze_hud_compositor/src/renderer/text_fit.rs),
  [`clipped_text_params_reports_labels_past_the_canvas`](../crates/tze_hud_compositor/src/widget.rs),
  `tze_hud_mcp` [`notification_reply_counts_cut_fields_and_is_unchanged_when_it_fits`](../crates/tze_hud_mcp/src/server/tests.rs),
  [`status_reply_counts_cut_values_only`](../crates/tze_hud_mcp/src/server/tests.rs)
- Refusal hints: `tze_hud_mcp` [`portal_refusals_name_the_field_and_its_limit`](../crates/tze_hud_mcp/src/server/tests.rs),
  [`scene_refusal_hints_are_fixed_id_free_and_short`](../crates/tze_hud_mcp/src/server/tests.rs)

- `tze_hud_mcp` [`test_structured_error_has_hint_field`](../crates/tze_hud_mcp/src/server/tests.rs), [`error_codes_are_unique_and_documented`](../crates/tze_hud_mcp/src/error.rs),
  [`every_returned_code_is_in_the_closed_set`](../crates/tze_hud_mcp/src/server/tests.rs)
- `tze_hud_resource` `store_behavior`: [`rejections_carry_stable_wire_code_and_actionable_detail`](../crates/tze_hud_resource/tests/store_behavior.rs),
  [`chunk_protocol_errors_have_stable_codes`](../crates/tze_hud_resource/tests/store_behavior.rs)

## 9. Time is injected

Anything with a deadline (TTL, grace periods, degradation windows, expiry, widget countdowns and elapsed timers)
reads time from an injectable clock (`TestClock`, or `*_at(now)` methods), so
its tests are deterministic and never sleep. Runtime composition shares one
clock object between scene, MCP, portal driver and pause control before cloning
handles. Wall deadlines use `now_us`; monotonic readings and `Instant` are only
elapsed-time and OS-wait domains. The panel parent exercises divergent domains,
wall rewind and saturated deadlines.
The compositor converts scene deadlines into OS waits using one paired injected
wall-time/`Instant` observation, carried across scene unlock; it never resamples
host wall time for that conversion.

- `tze_hud_scene` [`countdown_format_and_next_change_share_rounded_boundaries`](../crates/tze_hud_scene/src/widget_deadline.rs),
  [`elapsed_format_and_next_change_share_floored_boundaries`](../crates/tze_hud_scene/src/widget_deadline.rs),
  [`widget_deadline_presentation_retention_cadence_and_safe_mode`](../crates/tze_hud_scene/src/graph/widget_tests.rs),
  [`widget_elapsed_presentation_retention_cadence_and_safe_mode`](../crates/tze_hud_scene/src/graph/widget_tests.rs),
  [`scheduled_widget_snapshots_at_late_apply_and_clear_cancels_only_its_publisher`](../crates/tze_hud_scene/src/graph/widget_tests.rs)
- `tze_hud_runtime` [`portal_liveness_paused_during_safe_mode`](../crates/tze_hud_runtime/src/windowed/panel_input.rs),
  [`earliest_future_deadline_selects_wait_until`](../crates/tze_hud_runtime/src/windowed/wake.rs),
  [`building_an_idle_frame_leaves_the_idle_gate_inputs_unchanged`](../crates/tze_hud_runtime/src/windowed/event_loop_harness.rs)

- `integration` (POC acceptance: zone TTL, `delay_ms`, and portal liveness
  and lease grace all advanced by one `TestClock`) [`poc_portal_abandoned_is_reclaimed`](../tests/integration/poc_acceptance.rs),
  [`poc_zone_notification_ttl_disappears_unattended`](../tests/integration/poc_acceptance.rs), [`poc_zone_delay_ms_appears_on_schedule`](../tests/integration/poc_acceptance.rs)

An admin screenshot builds and reads back without advancing the last-rendered
scene/geometry ledger or primary presentation counters. An ordinary scene
change still presents, including after a capture alongside a failed acquire.

A ready signal is a submission notification, not a successful-present receipt.
If encoding defers presentation, the runtime retains that obligation and waits
for the actual per-window encoding dependency; another window cannot hold up
its completion wake. This wake adds no scene work or compositor generation.
The cursor-only quiet path services ready/deferred images even if raw mouse
motion follows the user event. New ready generations arriving during either
primary or secondary presentation remain unread for the next turn. Empty or
invalidated images retire only the existing obligation; unchanged held content
returns to quiescence. None of this proves Windows owner-visible pixels or
closes the separate overnight visibility acceptance.

- `tze_hud_runtime` [`building_an_idle_frame_leaves_the_idle_gate_inputs_unchanged`](../crates/tze_hud_runtime/src/windowed/event_loop_harness.rs)
  drives real MCP immediate/delayed held publication, encoding-guard completion,
  per-window coalescing, endpoint/inbox, frame build, idle ledger and main-thread
  primary/secondary present accounting with a fixed clock and queue synchronization. Only GPU
  readback and swapchain operations are synthetic here; actual capture pixels
  remain covered by compositor `capture_windowed_frame_reads_back_the_built_frame`
  in [surface.rs](../crates/tze_hud_compositor/src/renderer/tests/surface.rs).

- `tze_hud_runtime` [`armed_cursor_entry_never_wakes_compositor_until_hover_changes`](../crates/tze_hud_runtime/src/windowed/event_loop_harness.rs)
  also routes actual raw mouse motion before the present gate, preserving zero
  scene-work generations for presentation-only service.

## 10. Owner hiding changes the view, never state

From the control panel the owner can hide portals, zones, widgets, or all of them (Hide all: Alt+H in the panel, the hide chord or the tray; also how to present a shared
screen, or turn on Hide from screen capture). Hidden content is not drawn, catches no pointer input, holds no
keyboard focus (a focused composer blurs), and costs no raster or upload work;
its fades, reveals and scroll catch-up schedule no frames, and widgets shown
again draw their newest values directly. Hidden countdowns and elapsed timers
keep their absolute wall time but schedule no display-change wake; showing them
refreshes once
([`elapsed_rasterizes_once_per_display_change_and_fits_its_longest_strings`](../crates/tze_hud_compositor/tests/widget_raster_count.rs)
covers one raster per change).
The widget dock belongs to the widgets class: hiding widgets, or Hide all,
hides its rows, captions and tooltip, and its rows, capacity and overflow
are unchanged.
Leases, publications and agents are untouched and nothing tells an agent:
publishes keep landing and draw, as they are now, when shown again. The one
owner-approved exception is a hidden portal's native `unread` count (§3),
which keeps growing until the portal is shown and its OUTPUT tail is reached. The set is
runtime-only overlay state beside the scene model, never serialized and never
in `CompositorDegradationPolicy` (§6); a restart shows everything. While
anything is hidden and the panel is closed, a pill at the bottom right of the
primary display names what is hidden and the chord that opens the panel.
When both panel and pause chords are refused it names Tray → Open panel;
the tray's Hide all / Show all uses the same visibility action as Alt+H. A
word-by-word reveal under way when its zone was hidden finishes after the
zone is shown again.

- `tze_hud_scene` [`classes_hide_their_own_tiles_only`](../crates/tze_hud_scene/src/graph/visibility.rs), [`hidden_tiles_pass_pointer_input_through`](../crates/tze_hud_scene/src/graph/visibility.rs),
  [`a_change_bumps_the_version_once_and_keeps_content`](../crates/tze_hud_scene/src/graph/visibility.rs)
- `tze_hud_input` [`hidden_tile_loses_focus_and_leaves_the_cycle`](../crates/tze_hud_input/src/focus.rs)
- `tze_hud_runtime` [`hiding_focused_composer_tile_blurs_it_and_passes_clicks_through`](../crates/tze_hud_runtime/src/windowed/lifecycle.rs),
  [`hidden_tile_does_not_register_capture_region`](../crates/tze_hud_runtime/src/windowed/hittest.rs),
  [`hide_all_class_rows_and_the_closed_notice`](../crates/tze_hud_runtime/src/windowed/panel_input.rs),
  [`hide_chord_toggles_hide_all_like_the_tile`](../crates/tze_hud_runtime/src/windowed/panel_input.rs),
  [`switch_excludes_every_overlay_and_off_restores_them`](../crates/tze_hud_runtime/src/capture_exclusion.rs),
  [`reopen_chord_falls_back_to_the_pause_chord`](../crates/tze_hud_runtime/src/shell/control_panel/mod.rs),
  [`hidden_names_read_as_a_phrase`](../crates/tze_hud_runtime/src/shell/control_panel/sections/visibility.rs)
- `tze_hud_compositor` [`hidden_content_is_not_drawn_and_the_closed_notice_is`](../crates/tze_hud_compositor/src/renderer/tests/control_panel.rs),
  [`hidden_widgets_do_no_raster_work_and_draw_nothing`](../crates/tze_hud_compositor/tests/widget_raster_count.rs),
  [`hidden_content_animations_schedule_no_frames`](../crates/tze_hud_compositor/src/renderer/tests/idle_gate.rs),
  [`hidden_notification_sets_no_fade_deadline`](../crates/tze_hud_compositor/src/renderer/tests/idle_gate.rs),
  [`capture_leaves_the_closed_notice_out`](../crates/tze_hud_compositor/src/renderer/tests/control_panel.rs)
- Dock: `tze_hud_compositor` [`builtin_compact_countdown_keeps_standalone_pixels_and_timer_cadence`](../crates/tze_hud_compositor/tests/widget_raster_count.rs)
  (no raster or wake while hidden),
  [`dock_session_caption_follows_only_its_accepted_display`](../crates/tze_hud_compositor/tests/widget_raster_count.rs)
  (no caption or row draws while hidden), `tze_hud_runtime`
  [`reports_dock_capacity_overflow_and_widget_session`](../crates/tze_hud_runtime/src/operator/surface_status.rs)
  (state unchanged) and
  [`clipped_visits_are_passive_and_require_current_presented_card`](../crates/tze_hud_runtime/src/windowed/dock_tooltip.rs)
  (hiding ends the tooltip visit)
- `integration` [`poc_agents_see_the_same_with_the_panel_open_or_content_hidden`](../tests/integration/poc_acceptance.rs)

## Deferred ideas (notes, not code)

- **Interruption control.** If agents get noisy, use zone policy
  (latest-wins, stack depth) and an urgency field on notifications, not an
  attention budget.
- **Glasses/VR.** A non-goal, but "cheap when idle" (`docs/vision.md`) keeps
  that door open at no cost.


Notification capture delivery (source proposal; Windows owner acceptance separate):
The UI-thread observer retains per-installation/per-press cancellation history
until the corresponding application edge is delivered. Every delivered left
edge consumes its receipt before control-panel routing, including panel-owned
presses and releases that never reach the scene. This prevents a panel click
from leaving history that disables the next notification action. Normal native Up and
valid synthesized release activate once; cancelled or retired-window edges and
late Up do not impersonate an action. Callback code borrows no scene/runtime
owner and forwards native messages; local feedback cleanup uses the existing
settle and lock-availability continuation, rendering only after actual mutation.
Cancelled Zone drag feedback remains unsettled until pruning acquires the scene
and reconciles all remaining device gestures. A busy prune borrow reports a
deferred turn even when earlier chrome cleanup already consumed its pending flag;
the existing scene-availability wake retries it. Other devices keep their active
Tile feedback; cancelled late releases cannot move, persist or activate anything.
The existing runtime `action_button_hover_and_press_are_local_and_frame_only_on_change`
and `portal_resize_state_pruning_preserves_live_and_cleans_removed_tiles` parents
cover the split-borrow lock miss and both busy owners.
No new idle poll or timer is introduced. Ambiguous completed/active receipts,
mismatched history or bounded-admission failure pause notification actions.
The existing between-turn settle boundary resynchronizes history only after
cancellation reaches the scene, retained pointer edges drain while still inert,
and no recorded native delivery or application edge is active. It keeps cancelled
receipts until their matching application edges arrive: winit may still have
buffered those events during an earlier handler. Fault-era or queue-overflow
native edges retain bounded per-install cancellation debt; that window remains
inert until their application delivery drains it. Counter exhaustion stays
fail-closed. Recovery keeps old presses cancelled and preserves monotonic
installation/frame identities; stale Down/Up cannot borrow a fresh press's
receipts, including after reinstallation. Once cancelled delivery debt drains,
a complete fresh press can act without restarting.
Deferred pointer buttons retain their original notification authorization;
replay also checks current permission, so denied old input cannot inherit a
later press's receipt while the scene is busy.
Recovery neither installs a missing observer nor bypasses registration/receipt
bounds. Faulted, unobserved, full-queue and safe-mode notification buttons show no hover or
press feedback; drag handles keep their independent feedback. Valid reentrant
histories may trigger that availability limit; its frequency has not been
measured. Unrelated pointer input continues, and this source model does not
replace held-press/release owner proof.
