# Exemplar Manual Review Checklist

Manual UX/design review of all tze_hud exemplars.
Reviewed: 2026-04-09

## Exemplars

| # | Exemplar | Status | Notes |
|---|----------|--------|-------|
| 1 | [Subtitle](#1-subtitle) | **done** | Removed backdrop, fixed centering, fixed streaming reveal speed, added fade transitions |
| 2 | [Alert Banner](#2-alert-banner) | **done** | Fixed missing text, added 1px outline, taller banners |
| 3 | [Notification](#3-notification) | **deferred** | Basic rendering works; needs modern redesign (rounded corners, icons, dismiss, action buttons) |
| 4 | [Status Bar](#4-status-bar) | **deferred** | Repositioned to right edge, compact value-only format. Needs SVG widget redesign (hud-x2v1) |
| 5 | [Ambient Background](#5-ambient-background) | **deferred** | Fixed exemplar alpha 1.0→0.15; StaticImage rendering blocked on hud-4sk5 |
| 6 | [Dashboard Tile](#6-dashboard-tile) | pending | gRPC test client built; ready for visual review |
| 7 | [Presence Card](#7-presence-card) | **blocked** | 2026-04-16 live run attempt reached Windows host but failed preflight auth (`missing_psk` for `TZE_HUD_PSK`); manual visual sign-off remains pending |
| 8 | [Gauge Widget](#8-gauge-widget) | **done** | Automation batch + manual visual sign-off completed on 2026-04-09 (ultra-minimal track, hover-only readout/label, tuned spacing/colors) |
| 9 | [Progress Bar](#9-progress-bar) | **automation-pass** | Step + color-sweep batches passed on 2026-04-09; manual visual sign-off pending |
| 10 | [Status Indicator](#10-status-indicator) | **automation-pass** | Enum/theme/label/validation batches passed on 2026-04-09; manual visual sign-off pending |
| 11 | [Text Stream Portals](#11-text-stream-portals) | **phase-1-promoted** | **Owner PROMOTE decision 2026-07-05 (Tzeusy)** — `docs/reports/hud-qfyfg-text-stream-portal-phase1-promotion-gate-20260705.md`. Grey-frame blocker fixed (`hud-a328c`/#1075, off-black `#0A0D11`) and owner-verified live on the runtime-token path; collapsed-card grey darkened (`hud-0hj7f`/#1079). Composer interaction verified live; lease self-termination fixed (`hud-hk8kl`, soak ran 57.6 min) with flat memory (27–34 MiB); cadence within budget (`over_budget_count=0`). Full-duration soak (`hud-5kq8k`) and minor polish (`hud-n5bqp`, `hud-3nus3`, `hud-4e6c0`) accepted as tracked follow-ups. |

---

## 1. Subtitle

**Type:** Zone | **Contention:** latest-wins | **Position:** bottom-center

**Spec:** `openspec/changes/exemplar-subtitle/`
**Profile:** `profiles/exemplar-subtitle/`

### Design Review

- [ ] Text size and readability
- [ ] Backdrop opacity and color
- [ ] Word-wrap behavior at screen edge
- [ ] Streaming reveal timing feels natural
- [ ] TTL fade-out duration
- [ ] Positioning relative to other zones
- [ ] Multi-line spacing

### UX Tweaks

1. **Removed backdrop** — subtitle now uses outline-only readability (white text + black outline on transparent background). No grey rectangle.
2. **Fixed text centering** — cosmic-text `Align::Center` was stored but never applied to glyphon buffers. Now wired through `buf.lines.set_align()`.
3. **Fixed streaming reveal speed** — `STREAM_REVEAL_FRAMES_PER_SEGMENT` increased from 1 to 10 (~167ms per word at 60fps). Previously all words revealed in <200ms total.
4. **Added fade transitions** — `transition_in_ms=200`, `transition_out_ms=150` now set in subtitle token defaults.

**Files changed:**
- `crates/tze_hud_config/src/policy_builder.rs` — removed backdrop/opacity token defaults for subtitle, added transition defaults
- `crates/tze_hud_compositor/src/text.rs` — wired `TextAlign` → `cosmic_text::Align` on buffer lines
- `crates/tze_hud_compositor/src/renderer.rs` — increased streaming reveal dwell from 1 to 10 frames

---

## 2. Alert Banner

**Type:** Zone | **Contention:** stack | **Position:** top of screen

**Spec:** `openspec/changes/exemplar-alert-banner/`
**Profile:** `profiles/exemplar-alert-banner/`

### Design Review

- [ ] Severity color contrast (info/warning/critical)
- [ ] Text legibility on colored backdrops
- [ ] Stack ordering (newest on top?)
- [ ] Auto-dismiss timing per severity
- [ ] Banner height and padding
- [ ] Animation on appear/dismiss

### UX Tweaks

1. **Fixed missing text** — `stack_slot_height` used raw `font_size_px` but cosmic-text needs `font_size * 1.4` for line_height. Text bounds were too small to fit even one line.
2. **Added 1px black text outline** — white text on amber/warning backdrop was illegible. Added `outline_color` and `outline_width=1.0` to alert-banner token defaults, wired policy outline through to notification TextItems.
3. **Taller banners** — increased `margin_vertical` to 12px and `SLOT_BASELINE_GAP` to 4px for ~20% more height.

**Files changed:**
- `crates/tze_hud_compositor/src/renderer.rs` — fixed `stack_slot_height` to use `font_size * 1.4`, wired outline through notification TextItem creation
- `crates/tze_hud_config/src/policy_builder.rs` — added outline_color, outline_width, margin_vertical, margin_horizontal defaults for alert-banner

---

## 3. Notification

**Type:** Zone | **Contention:** stack | **Position:** configurable

**Spec:** `openspec/changes/exemplar-notification/`
**Profile:** `profiles/notification-stack-exemplar/`

### Design Review

- [ ] Urgency color differentiation
- [ ] Max-depth eviction behavior
- [ ] Fade-out on TTL expiry
- [ ] Stack spacing between notifications
- [ ] Text truncation for long messages
- [ ] Multi-agent visual distinction

### UX Tweaks

Basic rendering confirmed working (colored backdrops, text, stacking, TTL expiry). However, the current design is too minimal for production use. A modern notification redesign is needed:

1. **Rounded corner rectangles** — current renderer only draws axis-aligned quads
2. **Info/urgency icons** — `icon` field exists but is stubbed (no texture pipeline in v1)
3. **Two-line layout** — title (bold) + body text (regular weight)
4. **Dismiss button (×)** — requires input routing to zones
5. **Action button** — callback semantics for notification actions

**Reference:** [Dribbble notification banner design](https://cdn.dribbble.com/userupload/14638898/file/original-3cee189d1cc549f2e3dd0471e69c8562.jpg)

**Tracked as:** bead (see below)

---

## 4. Status Bar

**Type:** Zone | **Contention:** merge-by-key | **Position:** bottom edge

**Spec:** `openspec/changes/exemplar-status-bar/`
**Profile:** `profiles/exemplar-status-bar/`

### Design Review

- [ ] Key-value pair layout and spacing
- [ ] Font choice (monospace) readability
- [ ] Bar height and padding
- [ ] Multi-agent key coexistence display
- [ ] Update coalescing visual smoothness
- [ ] Contrast against content behind bar

### UX Tweaks

1. **Visual reset (2026-04-09)** — moved to an ultra-minimal gauge face:
   - single-track body (no wrapper rectangle, no tick ladder)
   - flat fill treatment (no reflective highlights)
   - compact severity pod and restrained label typography
2. **Interaction update** — added declarative hover tooltip support (`tooltip_visible` + `readout`) and a `level -> bar.y` binding so level transitions visibly move up/down.
3. **Regression verification** — reran gauge widget test suites plus live Windows publish cycle; publish path and bindings remain healthy after redesign.

**Files changed:**
- `assets/widgets/gauge/background.svg`
- `assets/widgets/gauge/fill.svg`
- `assets/widgets/gauge/widget.toml`
- `crates/tze_hud_widget/tests/gauge_param_validation.rs`
- `crates/tze_hud_widget/tests/gauge_interpolation.rs`
- `crates/tze_hud_widget/tests/gauge_perf_and_cache.rs`
- `crates/tze_hud_compositor/tests/gauge_binding_rendering.rs`

---

## 5. Ambient Background

**Type:** Zone | **Contention:** latest-wins | **Position:** full-screen

**Spec:** `openspec/changes/exemplar-ambient-background/`

### Design Review

- [x] Solid color rendering
- [ ] Static image scaling/positioning
- [x] Transition between backgrounds
- [x] Impact on foreground element readability
- [x] Default/fallback appearance

### UX Tweaks

1. **Fixed exemplar script alpha** — all colors changed from `a: 1.0` (fully opaque, obscures desktop) to `a: 0.15` (subtle tint). Added `DEFAULT_ALPHA` constant. Fixed log format to show actual alpha instead of hardcoded `1.0`. User confirmed 0.15 looks right for overlay mode.
2. **Increased phase 3 pause** — static image placeholder pause increased from 2s to 5s so the warm-gray is visible before phase 4 overwrites it.
3. **StaticImage renders placeholder (not acceptable for v1)** — warm-gray `(0.3, 0.3, 0.3, 1.0)` placeholder is not a real image. User requested actual GPU texture rendering. Created epic **hud-4sk5** (P1, 5 children) to implement the full wgpu texture pipeline. Spec confirms StaticImage is v1-mandatory — the placeholder was an implementation gap, not a spec gap.
4. **Instant transitions acceptable** — background swap at low alpha is not jarring. Crossfade deferred.

**Files changed:**
- `.claude/skills/user-test/scripts/ambient_background_exemplar.py` — DEFAULT_ALPHA=0.15, fixed log format, phase 3 pause 2s→5s

**Infrastructure built during this review:**
- gRPC server bind changed from `[::1]` to `0.0.0.0` (windowed.rs + headless.rs) — enables remote gRPC testing over Tailnet
- Python gRPC test client (`hud_grpc_client.py`) — session, lease, tile, node support via proto stubs
- Proto stubs generated in `proto_gen/` from `crates/tze_hud_protocol/proto/`

---

## 6. Dashboard Tile

**Type:** Tile | **Size:** 400x300 px

**Spec:** `openspec/changes/exemplar-dashboard-tile/`

### Design Review

- [ ] Tile size and proportions
- [ ] Dark background opacity
- [ ] Icon size and placement
- [ ] Markdown body rendering
- [ ] Button styling (Refresh/Dismiss)
- [ ] Button hit targets (touch-friendly?)
- [ ] Tile border/shadow treatment
- [ ] Orphan handling visual cue

### UX Tweaks

_(to be filled during review)_

---

## 7. Presence Card

**Type:** Tile | **Size:** 200x80 px | **Position:** bottom-left stack

**Spec:** `openspec/changes/exemplar-presence-card/`

### Design Review

- [ ] Card size — enough room for content?
- [ ] Avatar size and shape
- [ ] Agent name text size
- [ ] "Last active" timestamp readability
- [ ] Stack spacing when multiple agents
- [ ] Disconnect/offline visual treatment
- [ ] Card background contrast

### UX Tweaks

Live validation update (2026-04-16):
1. Attempted canonical resident `/user-test` run against `windows-host.example:50051`.
2. Reachability succeeded (`50051` and `9090` reachable), but scenario exited with `{"error":"missing_psk","psk_env":"TZE_HUD_PSK"}`.
3. Manual visual verdict is blocked until `TZE_HUD_PSK` is provisioned in the operator environment and the scenario is re-run.

Open closeout items:
1. Re-run `presence_card_exemplar.py` with valid `TZE_HUD_PSK` and archive transcript evidence.
2. Record PASS/FAIL for all seven visual steps in `docs/reports/exemplar-presence-card-user-test.md`.
3. Move status to `done` only after evidence-backed manual sign-off is recorded.

Spec sections awaiting live proof:
- `Requirement: gRPC Test Sequence`
  - `Scenario: Full single-agent lifecycle`
  - `Scenario: Three-agent concurrent lifecycle`
- `Requirement: User-Test Scenario`
  - `Scenario: User-test visual verification sequence`

---

## 8. Gauge Widget

**Type:** Widget | **Orientation:** vertical fill

**Spec:** `openspec/changes/exemplar-gauge-widget/`

### Design Review

- [x] Fill bar animation smoothness (linear interp)
- [x] Severity color mapping clarity
- [x] Label text placement and size
- [x] Background track contrast
- [x] Widget dimensions
- [x] Boundary behavior at 0% and 100%

### Automation + Manual Checkpoint (2026-04-09)

- `publish_widget_batch.py` with `gauge_cycle_test.json` completed successfully against Windows MCP endpoint (`:9090`) with no RPC transport failures.
- Published states applied in sequence on `main-gauge` with updated readout payloads and muted color set.
- Manual visual sign-off completed: no tick ladder/wrapper rectangle, hover tooltip carries label + `current/max (%)`, and level movement is visibly animated between states.

### UX Tweaks

1. Shifted to ultra-minimal gauge face and removed reflective treatment.
2. Moved label text into hover tooltip (no always-on face label).
3. Tuned tooltip spacing (top margin for label) and reduced fill color saturation.

---

## 9. Progress Bar

**Type:** Widget | **Size:** 300x20 px

**Spec:** `openspec/changes/exemplar-progress-bar/`

> **Note:** Asset bundle exists and is deployed (`widget.toml`, `track.svg`, `fill.svg`); remaining work is visual/manual acceptance.

### Design Review

- [ ] Bar height — visible enough?
- [ ] Fill animation smoothness
- [ ] Centered label readability over fill
- [ ] Fill color customization
- [ ] Empty vs full visual distinction
- [ ] Corner radius treatment

### Automation Checkpoint (2026-04-09)

- `publish_widget_batch.py` with `progress-bar-step.json` passed all 7 scripted updates on `main-progress` (0% -> 25% -> 50% -> 75% -> 100% -> green fill override -> clear/reset).
- `publish_widget_batch.py` with `progress-bar-color-sweep.json` passed scripted color transitions on `main-progress`.
- This confirms publish-path and parameter updates are healthy; animation/shape/readability criteria above still require manual sign-off.

### UX Tweaks

_(to be filled during review)_

---

## 10. Status Indicator

**Type:** Widget | **Size:** 48x48 px default

**Spec:** `openspec/changes/exemplar-status-indicator/`

### Design Review

- [ ] Circle size at default 48x48
- [ ] Color mapping clarity (online/away/busy/offline)
- [ ] Color-blind accessibility
- [ ] Optional label text placement
- [ ] Transition between states
- [ ] Visibility at small sizes

### Automation Checkpoint (2026-04-09)

- `publish_widget_batch.py` with `status-indicator-enum-cycle-test.json` passed for `online/away/busy/offline` on `main-status`.
- `publish_widget_batch.py` with `status-indicator-theme-cycle-test.json` and `status-indicator-theme-status-matrix-test.json` passed theme+state combinations.
- `publish_widget_batch.py` with `status-indicator-label-update-test.json` passed label changes.
- Validation fixture `status-indicator-validation-test.json` returned expected MCP error `WIDGET_PARAMETER_INVALID_VALUE` for `status=do-not-disturb`.
- This confirms publish-path, enum handling, and validation behavior; visual/accessibility criteria above still require manual sign-off.

### UX Tweaks

_(to be filled during review)_

---

## 11. Text Stream Portals

**Type:** Resident raw tile | **Transport:** primary `HudSession` gRPC stream | **Position:** content-layer (below chrome)

**Spec:** `openspec/specs/text-stream-portals/spec.md`
**Epic:** `hud-t98e` — closeout report in `docs/reports/hud-t98e-text-stream-portals.md`
**Archived change:** `openspec/changes/archive/2026-04-17-text-stream-portals/` (spec/RFC planning only; implementation tracked under `hud-t98e`)
**RFC:** 0013 (see `about/legends-and-lore/`)

**Status:** live-refinement / heavy user-iteration mode. Phase-0 raw-tile pilot shipped across `hud-t98e.1` / `.2` / `.3` / `.4` (PRs #427, #429, #432, #435), with follow-up closures `hud-r11x` (PR #437) and `hud-r9u0` (PR #438). Gen-2 reconciliation `hud-fomr` (PR #441) confirmed full 13/13 spec requirement coverage. The live `/user-test` exemplar at `.claude/skills/user-test/scripts/text_stream_portal_exemplar.py` now includes the scroll contract as the OUTPUT transcript pane plus header drag, focus, composer editing, paste, cleanup, deterministic composer caret smoke coverage, center-pane resize, bottom-right portal resize, and minimize-to-icon prototypes. The current caret/Space pass has fresh live transcript evidence, but the portal is explicitly not complete: smooth drag/resize/minimized-icon UX still needs runtime pointer-capture work and more operator-led passes.

**Latest Phase-1 evidence update (2026-06-19 / `hud-ofe76`):** the extended
exemplar-script adapter run completed on the live Windows HUD and archived
sanitized transcript evidence at
`docs/evidence/text-stream-portals/hud-ofe76-exemplar-script-live-2026-06-19.json`
with a companion summary at
`docs/evidence/text-stream-portals/hud-ofe76-exemplar-script-live-2026-06-19.md`.
The main run covered `markdown,overflow,composer-edit,diagnostic-input,cadence,profile-swap,window-mgmt`
and ended with `cleanup_errors=[]` plus successful explicit lease release. A
focused diagnostic supplement at
`docs/evidence/text-stream-portals/hud-ofe76-diagnostic-input-rerun-2026-06-19.json`
captured the expected OS-input `input:focus-gained`, `drag:start`, `drag:end`,
and `scroll:output` checkpoints after the full run's diagnostic phase missed
the drag checkpoints. Cadence remains the blocking axis: 20/20 appends
presented, but 5 exceeded the `16.6ms` runtime-overhead budget (`p95=21.033ms`,
`max=56.205ms`, `within_present_budget=false`). Human visual sign-off was not
present for this worker run; structured operator-evidence confirmations remain
`null`.

**Cooperative-projection evidence update (2026-06-19 / `hud-kylt0`):** the live
Windows HUD was reachable through SSH, `TzeHudOverlay` was running, Windows
localhost ports `9090` and `50051` were open, and MCP `list_zones` succeeded
after recovering the runtime credential from the scheduled-task XML. Sanitized
evidence is archived at
`docs/evidence/text-stream-portals/hud-kylt0-cooperative-projection-live-2026-06-19.md`.
The cooperative-projection facade did not complete the agent-ergonomics
lifecycle: `portal_projection_attach`, `portal_projection_publish`,
`portal_projection_get_pending_input`, `portal_projection_acknowledge_input`,
and `portal_projection_detach` all returned `CAPABILITY_REQUIRED` with
`hint.required_capability=resident_mcp`; `portal_projection_cleanup`,
`projection_operation`, and `portal_projection_publish_status` returned
`Method not found` on the deployed runtime. This matches the local
`hud-projection` caveat: cooperative projection remains blocked by
resident-capable ingress and deployed lifecycle method coverage, not by Windows
reachability.

**Promotion gate decision (2026-07-05 / `hud-qfyfg`):** RFC 0013 section 7.2
**PASSES — owner PROMOTE decision by Tzeusy.** The decision is recorded at
`docs/reports/hud-qfyfg-text-stream-portal-phase1-promotion-gate-20260705.md`
(supersedes the 2026-06-19 and 2026-07-04 machine FAIL assessments). The
grey-frame blocker was fixed and owner-verified live; lease/memory/cadence were
proven good; remaining items are accepted as tracked follow-ups. Promotion
authorizes first-class portal surface work (`hud-tc153`) to proceed.

### Phase-1 Promotion Sign-Off

| Gate item | Outcome | Evidence / follow-up |
|---|---|---|
| Overall RFC 0013 section 7.2 promotion decision | **PASS — owner PROMOTE (Tzeusy, 2026-07-05)** | `docs/reports/hud-qfyfg-text-stream-portal-phase1-promotion-gate-20260705.md`. |
| Visual sign-off (task 7.4) | **PASS (owner, live)** | Grey frame fixed (`hud-a328c`/#1075 → off-black `#0A0D11`) and owner-verified off-black live on the runtime-token path; collapsed-card grey darkened (`hud-0hj7f`/#1079 → `#12161C`). Evidence `liveverify-signoff-20260705-105700/`. |
| Owner promotion approval (task 7.7) | **GRANTED** | Tzeusy, 2026-07-05. |
| Exemplar-script adapter six-axis live run | **PASS** | Markdown, overflow, composer edit, OS-input window-mgmt, cadence, profile swap, cleanup all exercised live (`hud-ofe76` + 2026-07-05 reconcile pass); cadence now within budget. |
| Cooperative-projection adapter + agent ergonomics | **PASS** | Cooperative render recovered/durable; resident ingress confirmed; owner verified live. |
| C5 sustained soak evidence | **ACCEPTED PARTIAL** | Lease self-termination fixed (`hud-hk8kl`) — soak ran 57.6 min (13303 cycles) vs prior 608 s; memory flat (27–34 MiB, no trend). Full 3600 s completion deferred to `hud-5kq8k`; transient abort `hud-n5bqp`. Owner accepts partial. |
| Repeated raw-tile complexity criterion | **PASS** | Six raw tiles, split root/child mutation batches, capture tiles, drag shields, minimized-icon state, explicit cleanup, OS-input diagnostics — recurring complexity a first-class surface should absorb. |
| Governance stability criterion | **PASS (owner-accepted)** | Redaction/safe-mode/freeze/orphan covered by 9 passing integration tests; orphan/grace live-drivable. Owner accepts integration coverage for Phase-1. |
| Presence-thesis boundary | **PASS (owner)** | Content-layer, lease-governed, bounded, below chrome, subordinate to the presence model. |
| No terminal semantics | **PASS** | No PTY hosting, VT100/xterm compatibility, alternate screen, terminal mouse reporting, terminal keystroke passthrough, dedicated portal transport, or process lifecycle ownership is introduced. |

### Capability summary

A governed, low-latency **text stream portal** — not a terminal host, not a chat app. Transport-agnostic boundary: generic output streams, bounded viewer input, session identity, and session status metadata. Tmux, chat platforms, and LLM sessions are all valid adapters behind the same contract.

### Phase-0 pilot shape (delivered)

- Resident raw-tile pilot composed from existing `SolidColor`, `TextMarkdown`, `StaticImage`, and `HitRegion` primitives — no new dedicated node type.
- Portal traffic rides the existing primary bidirectional `HudSession` stream — no second long-lived portal stream per agent.
- Content-layer surface rendering below chrome — no chrome-hosted affordances or shell-owned portal controls.
- External adapter authenticates through existing capability grants — no implicit local trust, no runtime ownership of tmux/process lifecycle.

### Design Review (live refinement)

- [x] Tile size, viewport bounds, and stack position feel right for peripheral text presence
- [x] Output streaming reveal timing feels natural (incremental, not snapshot replace)
- [ ] Unread/activity indicator remains ambient — does not escalate interruption class on backlog growth
- [x] Scroll input feels local-first (offset updates before adapter ack)
- [x] Reply submission affordance reads as transactional, not coalescible
- [ ] Redaction treatment preserves portal geometry without leaking transcript content
- [ ] Safe-mode / freeze visual is indistinguishable from generic queue-pressure state (no portal-specific leakage)
- [x] Orphan/disconnect cleanup path removes stale portal tiles during ordinary `/user-test` exit
- [ ] Caret geometry remains stable after long markdown paste and near right-edge line ends
- [ ] Composer focus/input feel remains stable for normal typing (`hello world`) and Space handling

### Automated coverage (complete)

Integration tests in `tests/integration/`:

- `text_stream_portal_surface.rs` — raw-tile composition, bounded viewport, local-first scroll, ambient attention
- `text_stream_portal_adapter.rs` — transport-agnostic seam, tmux + non-tmux conformance, external adapter isolation
- `text_stream_portal_coalescing.rs` — retained-window coherence under backpressure
- `text_stream_portal_governance.rs` — redaction, safe-mode, freeze, orphan path, chrome exclusion

Evidence: `docs/evidence/text-stream-portals/validation-2026-04-16.md`. Spec-to-test requirement matrix: see the closeout report's compliance table.

### Live validation axes (partially signed off)

Integration tests prove structure; these axes still need operator-visible proof during a live HUD run:

| Axis | Spec requirement | What the operator should see |
|------|------------------|------------------------------|
| Streaming reveal | Low-Latency Text Interaction | PASS in live refinement — output arrives as ordered incremental updates, not snapshot replace |
| Local-first scroll | Transcript Interaction Contract | PASS in live refinement — OUTPUT pane scrolls independently of the overlay window |
| Bounded viewport | Bounded Transcript Viewport | PASS after scroll clipping/refinement — retained window stays within on-screen bounds as transcript grows |
| Coalescing coherence | Coherent Transcript Coalescing | Under rapid-publish pressure, retained window never collapses to only latest line |
| Markdown | Phase-1 Promotion Evidence Gate | 2026-06-19 `hud-ofe76` machine transcript PASS; human visual confirmation not collected |
| Overflow | Phase-1 Promotion Evidence Gate | 2026-06-19 `hud-ofe76` machine transcript PASS; human visual confirmation not collected |
| Composer edit | Reply Submission / Phase-1 Promotion Evidence Gate | 2026-06-19 `hud-ofe76` machine transcript PASS across empty, typed, delete, and clear states; human visual confirmation not collected |
| Window management via OS input | Transcript Interaction Contract / Phase-1 Promotion Evidence Gate | 2026-06-19 `hud-ofe76` PASS with supplemental diagnostic rerun: focus, drag start/end, and output scroll checkpoints captured; human visual confirmation not collected |
| Cadence with RTT | Low-Latency Text Interaction / Phase-1 Promotion Evidence Gate | 2026-06-19 `hud-ofe76` FAIL: cadence completed but runtime-overhead budget failed with 5/20 appends over `16.6ms` |
| Profile swap | Phase-1 Promotion Evidence Gate | 2026-06-19 `hud-ofe76` machine transcript PASS for compact, standard, expanded, restored-standard; human visual confirmation not collected |
| Cooperative projection adapter | Cooperative LLM Projection Adapter / Phase-1 Promotion Evidence Gate | 2026-06-19 `hud-kylt0` live reachability PASS, but full agent-ergonomics lifecycle BLOCKED: projection attach/publish/input/detach over HTTP MCP returned `resident_mcp` capability errors, and cleanup/status dispatcher methods were absent on the deployed runtime |
| 60-minute sustained soak | Sustained Streaming Cadence / Phase-1 Promotion Evidence Gate | NOT SIGNED OFF: PR #899 landed the `soak` phase and tests, but no completed 60-minute reference-host portal artifact proves `<=5 MiB` memory drift |
| RFC 0013 section 7.2 promotion gate | Phase-1 Promotion Evidence Gate | 2026-06-19 `hud-qfyfg` FAIL: raw-tile pilot remains authoritative; owner approval is not present |
| Redaction | Governance / Privacy / Override | Portal geometry preserved; transcript content suppressed under viewer policy |
| Safe mode | Governance / Privacy / Override | Portal updates suspend under safe mode like other content surfaces |
| Orphan path | Governance / Privacy / Override | PASS for ordinary `/user-test` cleanup; explicit orphan/grace behavior still needs a dedicated run |
| Ambient attention | Ambient Portal Attention Defaults | Unread backlog does not auto-escalate interruption class |
| Composer input | Reply Submission | 2026-04-28 evidence pass, still open for UX refinement — click focus, runtime Character events for `hello world`, Space key-down fallback, Enter submit, and deterministic long-paste caret smoke all produced clean transcript evidence |
| Caret geometry | Reply Submission | 2026-04-28 evidence pass, still open for UX refinement — composer-smoke long markdown-like paste rendered as 5 visual lines with no trailing-whitespace wrap lines and caret on the final wrapped row |
| Pane / portal resizing | Transcript Interaction Contract | 2026-04-28 live refinement in progress — center divider and bottom-right handle were added, black pane backgrounds resize consistently during drag, and scroll tile z-order conflicts were fixed; manual sign-off remains open because cursor-follow still stutters without runtime pointer capture for gRPC hit regions |
| Minimized icon | Transcript Interaction Contract | 2026-04-28 live refinement in progress — top-left minimize works, the icon is now split into immediate restore body + separate drag grip, and restore no longer depends on `pointer_up`; manual sign-off remains open because icon dragging still depends on incomplete runtime pointer capture and the final live split-target pass was interrupted by a Windows rig/HUD connection reset |
| Runtime stability | User-Test Scenario | 2026-04-28 blocker — latest split-target run reached baseline render, then gRPC reset with `StatusCode.UNAVAILABLE` / `recvmsg:Connection reset by peer`; subsequent ping, SSH, and port probes to `windows-host.example` timed out, so live verification could not be completed |

### Out of scope

- Terminal-emulator rendering (ANSI, cursor positioning, PTY control)
- Full transcript history storage in the scene graph
- Chrome-hosted portal affordances or shell-owned portal controls
- Portal-specific transport RPCs outside the primary session stream
- Runtime ownership of external process or tmux lifecycle

### Closeout path

1. Run `text_stream_portal_exemplar.py --phases baseline,scroll` against the live Windows HUD.
2. Record PASS/FAIL per live validation axis above, including the OUTPUT-pane scroll phase.
3. Re-test long markdown paste caret placement and ordinary `hello world` typing. Completed on 2026-04-28 with `test_results/text-stream-portal-hud-0ojis-baseline-scroll-20260428.json` and `test_results/text-stream-portal-hud-0ojis-composer-smoke-20260428.json`.
4. Re-run the minimized-icon split-target pass once the Windows rig is reachable; specifically verify click-to-restore on the icon body and drag-only behavior from the bottom-right grip.
5. Implement/verify runtime pointer capture for gRPC-created hit regions (`hud-rvmil`) so header drag, pane resize, portal resize, and icon grip follow the cursor continuously instead of relying on idle/watchdog behavior.
6. Continue UX improvement passes while this section remains `live-refinement`.
7. Move this row to `done` only after a human visual sign-off is recorded.

### UX Tweaks

- Merged the former standalone scroll contract exemplar into the text stream
  portal OUTPUT pane; scroll is now a subset of the portal contract.
- Added header drag-to-reposition for the content-layer portal surface.
- Added click-to-focus composer editing with placeholder hiding, blinking
  caret, submit/clear behavior, paste, arrow/Home/End movement, and word
  navigation/delete.
- Raised pane/text-window opacity to 95% black and kept the rest of the chrome
  lighter.
- Added cleanup for ordinary `/user-test` exits so stale portal/progress/status
  artifacts do not linger on the HUD.
- Fixed composer pre-wrap so word-boundary wraps do not leave trailing spaces
  that can make caret placement look one character behind near right-edge line
  ends. Added `--self-test` and `--phases composer-smoke` to validate Space
  fallback, `hello world`, and long-paste caret geometry.
- Kept the input composer mounted during output-only portal updates so scroll,
  streaming, and append phases do not reset composer focus/caret state.
- Added center-divider pane ratio resizing and a bottom-right portal resize
  handle. Follow-up pass reduced flicker by avoiding full node-tree remounts
  during pointer movement, widened drag hold timeouts for human resize gestures,
  and stopped discarding coalesced pointer moves inside a single event batch.
- Added a top-left minimize affordance and compact text-stream icon. Live pass
  confirmed minimize/restore and icon repositioning, but the icon/resize drag
  still stutters because gRPC-created hit regions do not receive continuous
  pointer capture after the cursor leaves the hit rect. Runtime follow-up is
  tracked in `hud-rvmil`.
- Refined minimized-icon behavior after live feedback: the icon body now
  restores immediately on pointer-down, while a small bottom-right grip is the
  only drag target. This avoids depending on `pointer_up`, which the current
  gRPC hit-region path can drop for icon gestures.
- Latest live pass was cut short by a rig/HUD connection reset and subsequent
  host reachability timeout. Future direction is to keep iterating the portal
  UX in `/user-test`, but prioritize runtime pointer capture and test-rig
  stability before trying to polish drag smoothness further in the Python
  exemplar alone.

---

## Summary of Changes

_(to be filled after all reviews complete)_
