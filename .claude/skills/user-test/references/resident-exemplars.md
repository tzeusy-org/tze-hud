# Resident gRPC Exemplar Scenarios

These scenarios exercise the resident gRPC session stream (not the MCP
zone/widget surface). Referenced from [../SKILL.md](../SKILL.md).

## Presence Card Exemplar Scenario

Use `scripts/presence_card_exemplar.py` to exercise the Presence Card raw-tile
resident flow on a live HUD. This scenario uses the resident gRPC session
stream, not the MCP zone/widget surface.

It drives the exact operator-visible lifecycle needed for the Presence Card
manual proof path:

1. Start 3 resident sessions (`agent-alpha`, `agent-beta`, `agent-gamma`)
2. Create 3 stacked bottom-left cards
3. Wait 30s and rebuild all 3 cards with updated `Last active` text
4. Disconnect `agent-gamma`
5. Pause for badge/orphan observation
6. Wait for orphan grace expiry while `agent-alpha` and `agent-beta` continue
7. Finish with 2 remaining cards and a JSON transcript artifact

Implementation note:
This scenario now uploads each 32x32 PNG avatar over the resident
`HudSession` stream (`ResourceUploadStart`), then applies the returned
`ResourceId` in the Presence Card `StaticImageNode`. The visual proof path
therefore covers stacked cards, periodic text updates, disconnect/orphan
observation, cleanup, and the real resident image-upload consumer contract.

### CLI

```bash
python3 .claude/skills/user-test/scripts/presence_card_exemplar.py \
  --target windows-host.example:50051 \
  --psk-env TZE_HUD_PSK \
  --tab-height 1080 \
  --transcript-out test_results/presence-card-latest.json
```

Optional flags:

- `--update-wait-s` (default `30`) — first periodic content-update wait
- `--heartbeat-timeout-s` (default `15`) — heartbeat-timeout reference for manual observation
- `--orphan-grace-s` (default `30`) — orphan grace-period wait
- `--observe-badge-s` (default `1.0`) — badge observation pause after disconnect

### Output

The script emits one JSON object per step to stdout and writes a transcript file
by default to `test_results/presence-card-latest.json`.

Each step includes:

- `code` — stable step identifier
- `title` — short operator-facing label
- `action` — what the script is doing
- `expected_visual` — what the operator should confirm on screen
- `status` — `started` or `completed`

### Human Acceptance Criteria

Verify the visible sequence in order:

| Step | Expected visual |
|---|---|
| Create | 3 stacked cards visible in the bottom-left corner |
| Update | All 3 cards show `Last active: 30s ago` |
| Disconnect | Only `agent-gamma` disconnects |
| Orphan observe | Disconnect badge appears on `agent-gamma` only |
| Cleanup | `agent-gamma` disappears after grace expiry |
| Final state | `agent-alpha` and `agent-beta` remain at original positions with no reflow |

This scenario is the repo-native execution surface for
`docs/reports/exemplar-presence-card-user-test.md`.

## Text Stream Portals Exemplar Scenario

**Status: implementation complete, live user-test exemplar available.** The
phase-0 raw-tile pilot shipped via epic `hud-t98e` (see
`docs/reports/hud-t98e-text-stream-portals.md`). All 13 normative requirements
in `openspec/specs/text-stream-portals/spec.md` are covered by integration
tests; gen-2 reconciliation (PR #441) confirmed 13/13 coverage. What remains
is recorded manual visual sign-off.

### Existing automated coverage (do not duplicate)

Integration tests in `tests/integration/`:

- `text_stream_portal_surface.rs` — raw-tile pilot composition, bounded
  viewport, local-first scroll, ambient attention
- `text_stream_portal_adapter.rs` — transport-agnostic seam, tmux and
  non-tmux adapter conformance, external adapter isolation
- `text_stream_portal_coalescing.rs` — retained-window coherence under
  backpressure
- `text_stream_portal_governance.rs` — safe-mode, freeze, orphan
  path, chrome exclusion

Evidence artifact: `docs/evidence/text-stream-portals/validation-2026-04-16.md`.

### Phase-0 pilot shape (recap)

- **Resident raw-tile pilot** — composed from existing text, solid-color,
  image, and hit-region primitives. No new dedicated node type.
- **Resident gRPC session** — portal traffic rides the existing primary
  bidirectional `HudSession` stream. No second long-lived portal stream.
- **Content-layer surface** — portal tile renders below chrome like any other
  content-layer zone. No chrome-hosted portal affordances.
- **External adapter, authenticated** — local adapter processes pass through
  existing capability grants; no implicit local trust.

### CLI

```bash
python3 .claude/skills/user-test/scripts/text_stream_portal_exemplar.py \
  --target windows-host.example:50051 \
  --psk-env TZE_HUD_PSK \
  --agent-id agent-alpha \
  --doc .claude/skills/user-test/references/exemplar-manual-review-checklist.md \
  --tab-width 1920 \
  --phases baseline,scroll \
  --baseline-hold-s 30 \
  --max-lines 80
```

By default the script explicitly releases its portal lease before closing the
resident session, so normal exit and Ctrl-C cleanup remove the portal tiles
without requiring a HUD restart. Use `--leave-lease-on-exit` only when
deliberately testing orphan/grace behavior.

Resize maxima in this exemplar are derived from the live `SceneSnapshot`
`display_area`. The runtime does not yet expose a portal-specific lease maximum
to the script, so display-area bounds are the interim harness limit while still
preserving the token-defined minimum legible size where the display can fit it.

Optional `--phases` values:

| Phase | What it exercises | Operator-visible proof |
|---|---|---|
| `baseline` | Two-pane INPUT/OUTPUT portal composition | Portal appears at right edge with header, composer, divider, transcript body, and footer |
| `scroll` | Transcript Interaction Contract | OUTPUT pane registers scroll, steps through transcript data, preserves mid-scroll window while tail lines append, then returns to latest output |
| `streaming` | Low-latency text interaction | OUTPUT body grows in ordered chunks |
| `rapid` | Coalescing coherence smoke | Rapid publish pressure does not collapse the retained window to one latest line |
| `diagnostic-input` | Live compositor/input path | Uses Windows OS input injection over SSH to click-focus the composer, drag the portal header, and wheel-scroll the OUTPUT pane |

For `diagnostic-input`, `--diagnostic-input-connect-timeout-s` controls the
SSH connect timeout separately from the overall injector timeout so unreachable
Windows hosts fail fast.

### Live Validation Axes

`text_stream_portal_exemplar.py` drives the resident raw-tile pilot against a
live HUD and produces operator-visible proof for axes that integration tests
cannot validate alone:

| Axis | Spec requirement | What the operator should see |
|------|------------------|------------------------------|
| Streaming reveal | Low-Latency Text Interaction | Output arrives as ordered incremental updates, not snapshot replace |
| Local-first scroll | Transcript Interaction Contract | Scroll offset visibly updates before any adapter ack |
| Bounded viewport | Bounded Transcript Viewport | Retained window stays within on-screen bounds as transcript grows |
| Coalescing coherence | Coherent Transcript Coalescing | Under rapid-publish pressure, retained window never collapses to only latest line |
| Safe mode | Governance, Privacy, and Override Compliance | Portal updates suspend under safe mode like other content surfaces |
| Orphan path | Governance, Privacy, and Override Compliance | Disconnected portal freezes at last coherent state; grace expiry removes it |
| Ambient attention | Ambient Portal Attention Defaults | Unread backlog does not auto-escalate interruption class |

### Out of scope for the live exemplar

- Terminal-emulator rendering (ANSI, cursor positioning, PTY control)
- Full transcript history storage in the scene graph
- Chrome-hosted portal affordances or shell-owned portal controls
- Portal-specific transport RPCs outside the primary session stream
- Runtime ownership of external process or tmux lifecycle

### Human Acceptance Criteria

- The portal stays in the content layer and remains below chrome.
- The OUTPUT pane text is readable and bounded within the portal.
- During `scroll`, the visible output window advances in steps, appended tail
  lines do not force an unsolicited jump, and return-to-tail shows the newest
  output.
- During `streaming`, output arrives incrementally rather than as a single
  snapshot replace.
- During `rapid`, the pane remains coherent under fast updates.
- During `diagnostic-input`, the JSON transcript includes `input:focus-gained`,
  `drag:start`/`drag:end`, and `scroll:output` checkpoints. The injector uses
  `SetCursorPos`, mouse events, wheel events, and `SendInput` Unicode text
  against the live overlay, so failures are runtime/input-path evidence rather
  than synthetic transcript success.
- Manual review notes and any UX tweaks are recorded in
  `.claude/skills/user-test/references/exemplar-manual-review-checklist.md` row 11.
