# Zone Exemplar Scenarios

Per-zone live-HUD exemplar scenarios published via MCP `hud_publish`. Each
section is self-contained: CLI, phases/sequence, visual checklist, and payload
shape. Referenced from [../SKILL.md](../SKILL.md). Every script reads `HUD_HOST` and the PSK file written by `hud_pair.py`.

## Subtitle Exemplar Scenario

Use `scripts/subtitle_exemplar.py` to exercise the subtitle zone on a live HUD.
The script validates streaming breakpoint reveal, single-line baseline rendering,
multi-line word-wrap, rapid-replacement contention, and TTL auto-clear — all using
the `exemplar-test` namespace.

### CLI

```bash
python3 .claude/skills/user-test/scripts/subtitle_exemplar.py \
  --ttl 10000
```

Optional: `--ttl` (ms, default 10000).

All messages are published to `zone:subtitle` under the PSK's agent namespace.

### Phases

| Phase | What happens | Pause |
|-------|-------------|-------|
| 1 — Streaming reveal | stream_text with breakpoints at word boundaries; compositor reveals word-by-word | TTL hold (10s default) |
| 2 — Single line | "Hello world — exemplar subtitle test"; baseline rendering | 4s |
| 3 — Multi-line | Long text forcing word-wrap and backdrop sizing | 4s |
| 4 — Rapid replacement | 3 publishes 100ms apart; only the third survives | 3s |
| 5 — TTL expiry | Subtitle with fixed 3s TTL; watch auto-clear fade-out | TTL + 0.3s safety + 1.0s margin + 2s confirmation (~6.3s total) |
| 6 — Streaming repeat | Same streaming reveal again for final sign-off | TTL hold (10s default) |

### Human Acceptance Criteria

Verify each criterion visually during the run:

| # | Criterion | Phase |
|---|-----------|-------|
| AC1 | White text with visible black outline on semi-transparent dark backdrop | 2 (single line) |
| AC2 | Text centered horizontally near bottom of screen | 2 (single line) |
| AC3 | Multi-line text wraps cleanly within backdrop bounds | 3 (multi-line) |
| AC4 | Rapid replacement transitions are smooth (no blank frames) | 4 (rapid replace) |
| AC5 | Content disappears after TTL with visible fade-out | 5 (TTL expiry) |
| AC6 | Streaming text reveals word-by-word | 1 and 6 (streaming) |

All six criteria must pass for the subtitle exemplar to be accepted.

### Named Test Group: subtitle-full-sequence

`subtitle-full-sequence.json` is provided as a batch sequence file and can be invoked
alongside other zone tests. Use with `--delay-ms 4000` so each scenario group has time
to render before the next publish fires:

```bash
python3 .claude/skills/user-test/scripts/publish_zone_batch.py \
  --messages-file .claude/skills/user-test/scripts/subtitle-full-sequence.json \
  --delay-ms 4000 \
  --list-surfaces
```

The sequence runs: single line → multi-line → rapid replacement (×3) → TTL expiry → streaming.
All messages use the PSK's agent namespace.

Use `--delay-ms 100` when running `subtitle-rapid-replace.json` alone to exercise
contention at a speed that actually triggers the latest-wins logic.

### Subtitle payload shape

```json
{"zone": "subtitle", "content": "Hello world", "ttl_ms": 10000}
```

For streaming with word-by-word breakpoints:

```json
{
  "zone": "subtitle",
  "content": "The quick brown fox jumps over the lazy dog",
  "breakpoints": [3, 9, 15, 19, 25, 30, 34, 38],
  "ttl_ms": 10000
}
```

The `breakpoints` array contains byte offsets of word boundaries in `content`. The
compositor reveals text progressively at each breakpoint at its own frame rate — the
agent does not control reveal timing.

---

## Notification Stack Exemplar Scenario

Use `scripts/notification_exemplar.py` to exercise the notification-area zone
on a live HUD. The script simulates 3 agents (alpha, beta, gamma) publishing
notifications with mixed urgency levels across 4 phases.

### CLI

```bash
python3 .claude/skills/user-test/scripts/notification_exemplar.py \
  --ttl 8000
```

Optional: `--ttl` (ms, default 8000).

### Phases

| Phase | What happens | Pause |
|-------|-------------|-------|
| 1 — Initial burst | alpha (urgency 0), beta (urgency 1), gamma (urgency 2) published in order | 2s |
| 2 — Stack growth | alpha (urgency 3), beta (urgency 1) — stack reaches max_depth=5 | 2s |
| 3 — TTL expiry | waits remaining phase-1 TTL plus ~650ms (150ms fade-out + 500ms margin) for phase-1 batch to auto-dismiss | 1s |
| 4 — Max depth eviction | 6 rapid notifications; 1st is evicted instantly (no fade) when 6th arrives | 3s |

### Visual Checklist (per phase)

**Phase 1:** Three notifications stacked newest-at-top. gamma (amber), beta
(dark blue), alpha (dark gray) backdrops with 1px border and body-font text.

**Phase 2:** Five notifications. Top two are phase-2 publishes; bottom three
are phase-1. All urgency-tinted correctly.

**Phase 3:** Phase-1 batch (urgency 0/1/2) has faded out; only 2 phase-2
notifications remain (urgency 3 and 1).

**Phase 4:** Exactly 5 notifications visible. "Burst A1" (oldest, urgency 0)
is gone with no fade — evicted instantly. "Burst C6" is at top.

### Notification payload shape

```json
{
  "body": "...",
  "urgency": 0,
  "title": "Optional heading",
  "actions": [
    {"label": "Open", "callback_id": "open"},
    {"label": "Dismiss", "callback_id": "dismiss"}
  ]
}
```

Published via MCP `hud_publish` to `zone:notification-area` with `ttl_ms`
from `--ttl`; the simulated agents share the PSK's namespace and differ by label
(`alpha`, `beta`, or `gamma`).

### Notification Full-Gamut Pass

After running `notification_exemplar.py`, run this additional batch to validate
the full v1 notification visual surface: two-line layout (`title` + `text`),
long-body containment, and action-button rows.

```bash
python3 .claude/skills/user-test/scripts/publish_zone_batch.py \
  --messages-file .claude/skills/user-test/scripts/notification-full-gamut.json \
  --delay-ms 250 \
  --list-surfaces
```

Coverage in `notification-full-gamut.json`:
- urgency gamut: low (0), normal (1), urgent (2), critical (3)
- two-line cards via `title` on all messages
- long-body critical text to verify card-height containment
- action rows: 2 actions on urgent card, 3 actions on critical card

Visual checks:
- no body text should escape its card backdrop
- urgency colors should progress low → normal → urgent → critical
- action rows should appear inside the card near the bottom edge
- stack ordering should remain newest-at-top under mixed payload shapes

## Alert-Banner Exemplar Scenario

Use `scripts/alert_banner_exemplar.py` to exercise the alert-banner zone on a
live HUD. The script publishes 3 alerts at increasing urgency levels with 3-second
delays between each, validating urgency-driven visual differentiation and simultaneous
multi-alert display.

### CLI

```bash
python3 .claude/skills/user-test/scripts/alert_banner_exemplar.py \
  --ttl 15000
```

Optional: `--ttl` (ms, default 15000).

### Sequence

| Step | Alert | Urgency | Text | Pause |
|------|-------|---------|------|-------|
| 1 | Info | 1 | "Info: system nominal" | 3s |
| 2 | Warning | 2 | "Warning: disk space low" | 3s |
| 3 | Critical | 3 | "CRITICAL: security breach detected" | — |

### Visual Checklist

After all 3 publishes, the alert-banner zone should show all three alerts
simultaneously:

- **Critical (red)** at top — "CRITICAL: security breach detected"
- **Warning (amber)** in middle — "Warning: disk space low"
- **Info (blue)** at bottom — "Info: system nominal"

All three remain visible until their TTL elapses. Confirm urgency-derived
color tinting is applied correctly at each level: blue for info (urgency=1),
amber for warning (urgency=2), red for critical (urgency=3).

### Alert payload shape

```json
{"body": "...", "urgency": 1}
```

Published via MCP `hud_publish` to `zone:alert-banner` with `ttl_ms` set to
`--ttl` (e.g. `--ttl 15000` → `ttl_ms = 15000`).

## Status-Bar Exemplar Scenario

Use `scripts/status_bar_exemplar.py` to exercise the status-bar zone on a live
HUD. The script simulates three independent agents (`agent-weather`, `agent-power`,
`agent-clock`) publishing merge-keyed entries, validating multi-agent coexistence,
key replacement, empty-value removal, and TTL-driven sweep.

### CLI

```bash
python3 .claude/skills/user-test/scripts/status_bar_exemplar.py \
  --battery-ttl 5000
```

Optional: `--ttl` (ms, default 60000 — long TTL for weather/time entries),
`--battery-ttl` (ms, default 15000 — TTL for battery entry; long enough to survive steps 4/6/8 visual checks but expires during step 9).

### 10-Step Sequence

| Step | Agent | Action | Pause |
|------|-------|--------|-------|
| 1 | agent-weather | publish `weather` → `"72F Sunny"` | — |
| 2 | agent-power | publish `battery` → `"85%"` (short TTL) | — |
| 3 | agent-clock | publish `time` → `"3:42 PM"` | — |
| 4 | — | VISUAL CHECK: all 3 visible | 3s |
| 5 | agent-weather | update `weather` → `"75F Cloudy"` (key replacement) | — |
| 6 | — | VISUAL CHECK: weather updated; battery/time unchanged | 3s |
| 7 | agent-weather | publish empty value for `weather` (key removal) | — |
| 8 | — | VISUAL CHECK: weather gone; battery/time remain | 3s |
| 9 | — | wait for battery TTL to expire (remaining TTL + 500ms sweep margin) | — |
| 10 | — | VISUAL CHECK: battery gone; time remains | 3s |

### Visual Checklist

**Step 4:** Status bar shows all three key-value pairs in a horizontal row at the
bottom edge of the display with a dark opaque backdrop. Entries display in
monospace font using secondary text color. Order may vary by insertion time.

**Step 6:** Status bar still shows three entries. The `weather` value reads
`75F Cloudy` (replaced). `battery: 85%` and `time: 3:42 PM` are unchanged.

**Step 8:** Status bar shows two entries. The `weather` key is no longer visible
(empty-value convention suppresses rendering). `battery` and `time` remain.

**Step 10:** Status bar shows one entry. The `battery` key has been swept by
`sweep_expired_zone_publications` after its TTL elapsed. Only `time: 3:42 PM`
remains visible.

### Human Acceptance Criteria

| # | Criterion | Step |
|---|-----------|------|
| AC1 | Three distinct keys coexist — no key overwrites another | 4 |
| AC2 | Key replacement updates only the target key's value | 6 |
| AC3 | Empty-value publish removes that key from the visible display | 8 |
| AC4 | TTL expiry removes the key without explicit publish | 10 |
| AC5 | Chrome-layer bar is always visible above content tiles | all visual steps |
| AC6 | Monospace font and dark opaque backdrop visible throughout | all visual steps |

All six criteria must pass for the status-bar exemplar to be accepted.

### Status-bar payload shape

```json
{
  "zone": "status-bar",
  "content": {"type": "status_bar", "entries": {"weather": "72F Sunny"}},
  "key": "weather",
  "ttl_ms": 60000
}
```

Each simulated agent uses a distinct merge `key` (`agent-weather`, `agent-power`,
`agent-clock`). The `merge_key` matches the single entry key in `entries`.
Published via MCP `hud_publish`.

---

## Ambient Background Exemplar Scenario

Use `scripts/ambient_background_exemplar.py` to exercise the ambient-background
zone on a live HUD. The script publishes across 4 phases: solid-color fill,
latest-wins replacement, static-image placeholder, and rapid-replacement stress.

### CLI

```bash
python3 .claude/skills/user-test/scripts/ambient_background_exemplar.py
```

Both read `HUD_HOST` and the paired PSK file.

### Phases

| Phase | What happens | Pause |
|-------|-------------|-------|
| 1 — Dark blue | Publish `solid_color` dark navy blue (r=0.05, g=0.05, b=0.2) | 3s |
| 2 — Warm amber | Replace with warm amber (r=0.9, g=0.6, b=0.2); latest-wins Replace policy evicts dark blue | 3s |
| 3 — Static image | Publish `static_image` content type (64-char hex resource_id); runtime renders warm-gray placeholder in v1 | 2s |
| 4 — Rapid replace | 10 different solid colors in sequence without delay; query `hud_surfaces` to confirm `held: true` and visually confirm the final color is bright green | — |

### Visual Checklist

**Phase 1:** Entire HUD background should turn dark navy blue. No content tiles
are affected — background is behind all content-layer zones.

**Phase 2:** Background shifts instantly to warm amber. The previous dark blue
must be gone (Replace policy: latest-wins, exactly 1 active publication).

**Phase 3:** Background changes to a warm-gray placeholder quad (v1 behavior —
GPU texture upload is deferred). The zone must accept the publication without
error.

**Phase 4:** After all 10 rapid publishes, the background should settle on
bright green (last of the 10 colors). No other colors from the burst should
bleed through. `hud_surfaces` must report `held: true` for the
`ambient-background` zone.

### Background payload shapes

```json
{"type": "solid_color", "r": 0.05, "g": 0.05, "b": 0.2, "a": 1.0}
{"type": "solid_color", "r": 0.9, "g": 0.6, "b": 0.2, "a": 1.0}
{"type": "static_image", "resource_id": "<64-char-hex-blake3-hash>"}
```

All published via MCP `hud_publish` to `ambient-background` zone with
`namespace` set to `ambient-test-p<N>` per phase. TTL is omitted (defaults to
persistent — `auto_clear_ms=None` on this zone) for phases 1–3.
