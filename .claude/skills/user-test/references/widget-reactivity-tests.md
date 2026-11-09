# Widget Reactivity Tests (Steps 5–7)

These steps continue the Workflow in [../SKILL.md](../SKILL.md) after Step 4
(Publish Configurable Widget Messages). They verify widget re-rasterization on
param change for the gauge, status-indicator, and progress-bar widget instances.

### Step 5: Widget Reactivity Test (Gauge Cycling)

After the initial widget publish, cycle the gauge through a sequence of values with 3-second delays to verify widget reactivity (re-rasterization on param change).

Use `scripts/gauge_cycle_test.json` from this skill with `--delay-ms 3000`:

```bash
python3 .claude/skills/user-test/scripts/publish_widget_batch.py \
  --messages-file .claude/skills/user-test/scripts/gauge_cycle_test.json \
  --delay-ms 3000
```

The gauge should visually cycle through: blue 25% "Low" → yellow 50% "Medium" → red 95% "Critical!" → primary 42% "Normal". Report per-step success and whether the user confirmed visual updates.

### Step 6: Widget Reactivity Test (Status Indicator)

After the gauge cycle, confirm that a `status-indicator` widget instance is registered before proceeding. Use the `--list-surfaces` output from Step 4 (or re-run it) to verify that a widget named `status-indicator` appears in the list. If no such instance is present, skip the sub-steps below and report that the status-indicator widget is not deployed.

Run the status-indicator enum cycle to verify discrete color binding and re-rasterization on param change.

Use `scripts/status-indicator-enum-cycle-test.json` from this skill with `--delay-ms 1000`:

```bash
python3 .claude/skills/user-test/scripts/publish_widget_batch.py \
  --messages-file .claude/skills/user-test/scripts/status-indicator-enum-cycle-test.json \
  --delay-ms 1000 \
  --cleanup-on-exit
```

The status indicator should visually cycle through:
- `online` → green badge (`#4FB543`)
- `away` → amber badge (`#D97706`)
- `busy` → red badge (`#DC2626`)
- `offline` → gray badge (`#6B7280`)

Each transition is a discrete snap (no interpolation). Require human visual confirmation that both color and glyph change per state.

Status style is owner configuration, not an agent parameter. To review each
style, set `initial_params = { theme = "minimal" }` (then `system`, then
`friendly`) on the status instance in `[[tabs.widgets]]`, restart that owned
test HUD, and run the enum cycle for each. Confirm the configured style stays
fixed across all model status updates. Do not publish `theme` or
`tooltip_visible`; hover locally to reveal the tooltip.

Next, run the label-update sequence to verify text-content binding:

```bash
python3 .claude/skills/user-test/scripts/publish_widget_batch.py \
  --messages-file .claude/skills/user-test/scripts/status-indicator-label-update-test.json \
  --delay-ms 1000 \
  --cleanup-on-exit
```

Expected label progression: "Butler" → "Codex" → (empty). The badge remains online/green. Label changes are primarily visible in the tooltip content (not always-on icon text); verify by hovering long enough to reveal the tooltip.

Finally, run the validation fixture to confirm invalid enum rejection at the MCP surface:

```bash
python3 .claude/skills/user-test/scripts/publish_widget_batch.py \
  --messages-file .claude/skills/user-test/scripts/status-indicator-validation-test.json
```

Expected result: MCP returns an error response (`WIDGET_PARAMETER_INVALID`) for `status=do-not-disturb`. The widget display must not change. Report whether the error response matches expectation.

### Step 7: Widget Reactivity Test (Progress Bar)

After the status-indicator tests, confirm that a `progress-bar` widget instance is registered before proceeding. Use the `--list-surfaces` output from Step 4 (or re-run it) to verify that a widget named `progress-bar` appears in the list. If no such instance is present, skip the sub-steps below and report that the progress-bar widget is not deployed.

This is the **progress-bar-widget** user-test scenario. It animates a thin horizontal bar from 0 to 100% and confirms visual quality at each step.

#### 7a: 7-Step Sequence

Run `progress-bar-step.json` with `--delay-ms 1000` so the tester has ~1 second to observe each visual transition:

```bash
python3 .claude/skills/user-test/scripts/publish_widget_batch.py \
  --messages-file .claude/skills/user-test/scripts/progress-bar-step.json \
  --delay-ms 1000 \
  --cleanup-on-exit
```

At each step, prompt the tester to confirm the expected visual state:

| Step | Published params | What to confirm |
|------|-----------------|-----------------|
| 1 | `progress=0.0, label=""` | Bar is empty (zero width fill); no label text visible |
| 2 | `progress=0.25, label="25%"` | Fill animates smoothly to 25%; label reads "25%" centered on bar |
| 3 | `progress=0.5, label="50%"` | Fill animates smoothly to 50%; label reads "50%" |
| 4 | `progress=0.75, label="75%"` | Fill animates smoothly to 75%; label reads "75%" |
| 5 | `progress=1.0, label="100%"` | Fill animates smoothly to full width; label reads "100%" |
| 6 | `severity="info"` | Fill returns to the owner info override or primary token; progress/label unchanged |
| 7 | clear | Bar resets to empty with no visual artifacts |

**Human acceptance criteria at each step:**

- **(a) Pill/capsule shape** — The bar has visually rounded end-caps on both the track and the fill. No sharp corners.
- **(b) Smooth fill animation** — Each step 2-5 fills with a visible 200ms animation. No jumps or jank.
- **(c) Centered label** — Label text is horizontally and vertically centered on the bar at all non-empty steps.
- **(d) Correct fill color** — Info uses the owner override or primary token; warning/error use caution/error. Enum changes snap.
- **(e) Clean reset** — After the clear action, the bar is completely empty with no residual fill or label artifacts.

#### 7b: Color Sweep (Optional)

Optionally, run the color-sweep fixture to validate severity-driven colors with `--delay-ms 1000`:

```bash
python3 .claude/skills/user-test/scripts/publish_widget_batch.py \
  --messages-file .claude/skills/user-test/scripts/progress-bar-color-sweep.json \
  --delay-ms 1000 \
  --cleanup-on-exit
```

The fixture visits error -> warning -> info -> info, then clears. Severity changes snap; info uses the owner override or primary token. Confirm that the fill color matches expectations at each step before the next publish fires, and that after the final clear action the bar is fully empty with no residual fill or label.

Report pass/fail per step. A step fails if the tester observes: missing animation, wrong color, misaligned label, missing rounded end-caps, or visible artifacts after the reset/clear-to-empty step.

#### 7c: Rapid-Fire Stream Test (100 publishes / 5 seconds)

Use this fixture to simulate a dense progress-update stream and validate that the HUD stays responsive under frequent widget publishes.

```bash
python3 .claude/skills/user-test/scripts/publish_widget_batch.py \
  --messages-file .claude/skills/user-test/scripts/progress-bar-rapidfire-100-5s.json \
  --delay-ms 50 \
  --cleanup-on-exit --cleanup-delay-ms 3000
```

Fixture details (`progress-bar-rapidfire-100-5s.json`):
- 100 sequential updates (`1%` -> `100%`)
- publish cadence: 50ms between requests (~5s total sequence duration)
- per-message transition: 45ms
- fixed widget target: `main-progress`

Expected outcomes:
- No MCP transport or validation errors across the 100 publishes.
- Progress bar appears continuously animated without freezing/stalling.
- Final visible state settles at `100%`.
- No visual artifacts in label text during rapid updates.
