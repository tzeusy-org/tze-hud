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
