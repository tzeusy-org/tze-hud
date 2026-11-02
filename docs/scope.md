# Scope and implementation status

The project is being cut back from a general "agent presence engine" to an
MCP/gRPC layer for the HUD real-estate lifecycle (see [vision.md](vision.md)).
Tranches T0–T4 removed what doesn't serve that; T5 redesigned the API that
remains. T6–T8 prove the result on Windows and shrink the code to match.
T9–T11 are owner-directed additions: per-monitor overlays, a swappable
visual design, and an in-HUD control panel.
Each tranche must still build, pass tests, and boot the overlay.

Source snapshot: `855f8397` (latest `origin/main` fetched on 2026-10-13).
The tranche descriptions below retain the approved direction; the status and
[evidence](#current-implementation-evidence) distinguish implemented pieces
from outstanding work. [Observed] means inspected source or tracker state,
[Inferred] means a conclusion from it, and [Unknown] means unverified here.
This refresh checked documentation against source; it did not run Windows
acceptance or certify the performance targets. Update this snapshot and its
source links when implementation changes the status.

## Keep

| Area | Where |
|---|---|
| Overlay window, compositor, text/markdown, images | `tze_hud_runtime` (windowed), `tze_hud_compositor` |
| Scene graph: tabs, tiles, nodes, zones | `tze_hud_scene` |
| Session portal + projection | `tze_hud_projection`, `tze_hud_runtime` portal modules, `tze_hud_input` |
| MCP tools (zones, widgets, portal) | `tze_hud_mcp` |
| gRPC resident session (zones, widgets, uploads, events) | `tze_hud_protocol` |
| SVG widgets + asset store | `tze_hud_widget`, `tze_hud_resource` |
| Config, flat design tokens, built-in themes | `tze_hud_config` |
| Frame/idle telemetry | `tze_hud_telemetry` |
| App binary | `app/tze_hud_app` |

## Tranches

| Tranche | What | Status |
|---|---|---|
| T0 | Doctrine, RFCs, OpenSpec, curriculum, evidence/report docs, doctrine and OpenSpec agent skills, vocabulary lint | done |
| T1 | Unused crates: `tze_hud_a11y`, `tze_hud_media_apple`, `tze_hud_media_android`, `tze_hud_policy`; Android/iOS/Safari CI workflows | done |
| T2 | Media and cloud relay: GStreamer/`v2_preview` features, media ingress/admission, video surface, media signaling protobuf messages (field numbers reserved), media config and capability, real-decode and v2-preview CI, Python media exemplars | done |
| T3 | Governance: attention budget, quiet hours, privacy redaction and viewer classes, `[privacy]`/`[degradation]`/`[chrome]` config, admission controller, budget ladder (now plain hard caps), unwired lease state machine and suspension manager, degradation ladder down to one fallback (Normal ↔ Simplified). **Kept** the lease lifecycle (request, TTL, renew, release, revoke, disconnect grace). Capability-scope shrink moved to T5: it changes the session-init and lease wire contract | done |
| T4 | Scaffolding: `tze_hud_validation`, replay/trace recording, v1-thesis/Layer-4 artifact harness and their CI jobs; component profiles (flat `[design_tokens]` stay; removed profile sections are rejected); sync groups and clock-skew estimation (`compositor_timestamp_wall_us` stays for `present_at`/`expires_at`; wire fields reserved); hardware calibration (tests use a fixed `test_budget` slack; the benchmark keeps its CI factors); test scenes for removed features; reserved mobile display profile; unwired tab-switch trigger; redundant tests | done |
| T5 | API redesign ([api.md](api.md)): one verb set per lifecycle stage across MCP and gRPC (five MCP tools replace 22; gRPC `Publish`/`Clear`/`Hold`/`RequestResult`); per-agent PSK identity with an `allow` list replaces the capability vocabulary, resident principal, and portal owner token; one shared error-code set with hints; geometry remains runtime-owned; token budgets enforced in CI; dead wire removed and reserved; session-server constructors collapsed; invariant 1 and 4 breaks on the gRPC path fixed; owner-directed public tile removal on 2026-10-14 (deferred spec hud-x1wuq) | done |
| T6 | Windows delivery without SSH, then prove the POC on it. (1) CI builds `x86_64-pc-windows-msvc` natively on a Windows runner, boots it under WARP with an MCP smoke, and publishes minisign-signed releases (rolling `dev` from main, `v*` tags). (2) The exe self-installs per user (`%LOCALAPPDATA%`, `HKCU` Run autostart, no admin). (3) It listens on loopback and the Tailscale address only (owner reaffirmed 2026-10-06: no loopback-only default, no relay). The HUD detects when Windows Firewall blocks tailnet inbound and says so on the pairing card and in `/admin/status`; an opt-in elevated helper adds one program-scoped rule (bead `hud-lgb6c`). (4) First-run pairing: the HUD shows a one-time code, an agent host trades it for a per-agent PSK (`POST /pair`; only a hash is stored); delete `--psk`, the default PSK, and `--bind-all-interfaces`. (5) `/admin/*` operator endpoints (status, logs, screenshot of the HUD's own frame, restart, signed pull-only self-update with health handoff), off the MCP surface. (6) Rewrite `user-test`/`hud-projection` around the paired PSK; delete SSH deploy tooling. Then run POC acceptance on the owner's host; close stale draft PRs | implemented delivery paths; Windows owner acceptance outstanding |
| T7 | Portal as a runtime feature: projection state lives in the runtime keyed by PSK identity. Delete the external-authority design: owner tokens, audit log, provider-neutral contract, `projection_authority` binary, `resident_grpc` adapter. Fold the `PROJECTION_*` error codes into the shared set. Internal interfaces may break; the MCP surface and token footprint must not regress. Target: portal code (projection, driver, windowed portal, composer) under ~12k lines from ~45k | in progress: shared `PortalHub` and direct scene renderer are live; portal-only wire fields 13–17 are reserved; rename, final size reconciliation and owner acceptance remain |
| T8 | Code and test diet, crate by crate (compositor, runtime, scene, protocol, input first): delete code without a current user, replace oversized test files with behavior tests tied to `invariants.md`, add `tze_hud_resource` tests, a local llvmpipe recipe for compositor tests, and strip pre-reset references (openspec, RFCs, doctrine) from comments and protos | in progress: resource behavior tests and llvmpipe recipes exist; diet and reconciliation continue |
| T9 | Owner-directed 2026-10-05, alongside T6 rather than after the POC. Per-monitor overlays: one window and surface per monitor sharing one scene, the default with no flag; delete `--monitor` and monitor cycling; logical (dp) token sizes scaled per monitor (bead `hud-1pt5h`). Owner-directed 2026-10-06: `[displays.<key>]` is keyed by EDID model and serial, plus the GPU connector path when two monitors are ambiguous; `/admin/status` and the log show each key (bead `hud-avr7s`). One global `ui.scale` token multiplies every logical size on top of each monitor's Windows scale factor; there are no per-monitor overrides (bead `hud-h51u7.3`) | partial: overlay windows exist; logical (dp) sizing and `ui.scale` landed (`hud-h51u7.3`); stable keys remain |
| T10 | Owner-directed 2026-10-05. Swappable visual design: themes as a token layer between canonical defaults and `[design_tokens]` overrides, with Tonal Glass as the default; semantic token families (surfaces, roles, state, motion, shape, spacing, type); configurable fonts (bundled IBM Plex, fonts beside the config, Windows fonts by name); live theme and font switching from the config file and from an in-HUD picker (now the T11 control panel's Appearance section), which writes the choice back to the config (owner-directed 2026-10-06; agents never set themes); every element restyled to use tokens only (bead `hud-h51u7`). This reverses T4's removal of component profiles for themes only: themes hold token values, never layout | partial: startup themes/fonts and live switching (config file and control panel; `hud-h51u7.5`) exist; `ui.scale` landed (`hud-h51u7.3`); restyling remains |
| T11 | Owner-directed 2026-10-15. In-HUD control panel: one keyboard-first, full-width, Spotlight-style pane (search, quick actions such as Hide all, section cards that scroll vertically) of every owner-tunable mechanism (agents and trust including a copy-pastable connect prompt, visibility, appearance, layout, chords, network, diagnostics, system, every design token), each row showing its value source and whether it applies live or after a restart. The safe-mode chord opens it with agents paused; a second chord opens it without pausing; closing resumes. Hiding a surface class is session-only and silent to agents (no draw, input or raster work; state unchanged). Agents never see the panel; no MCP or gRPC change. Supersedes the T10 settings card (epic `hud-vniri`) | partial: shell landed (two chords, search, quick tiles, keyboard and pointer, Agents/Input/System rows; `hud-vniri.2`); Visibility rows, Hide all and the hidden-content pill landed (`hud-vniri.4`); the agent roster with Clear, Unpair and allow-list grants, and Connect an agent with its copyable prompt, landed (`hud-vniri.6`); Network, Diagnostics and System rows (ports staged behind Restart now, live log level, zone outlines and frame cap, firewall, update, autostart; value sources from flags, env and `[runtime]`) landed (`hud-vniri.7`, part A); Layout (widget box and clears-after, live and saved; display zones read-only) and live chord rebinding landed (`hud-vniri.7`, part B); Appearance (theme, fonts) and Advanced tokens (every token, Reset all overrides), live and saved, landed (`hud-vniri.5`); Zoom (`ui.scale`, live and saved, widgets included) landed (`hud-h51u7.3`); the display profile and window show read-only with their source; the chords keep F-key defaults under `panel_hotkey` (Ctrl+Shift+F11) and `pairing_hotkey` (Ctrl+Shift+F10), not the epic's Ctrl+Shift+P under `settings_hotkey` (owner decision 2026-11-01: a global letter chord steals editors' command palette); the header state line counts agents and holdings and flags a pending restart, whose Restart now stays in System (one place to restart; the mockup's header has none), and the panel keeps its scrim whether or not agents are paused (the pane always owns the pointer over it); a changed MCP port asks first; changed rows offer Reset (Delete or the row's link) where the panel can restore the default live (Visibility, Appearance, tokens, chords, Diagnostics), while ports and widget boxes take a typed value (a port Reset would skip the MCP confirmation; a widget's default box is its asset's); the theme previews on Left/Right and Alt+T, Enter keeps it, leaving restores it, and a click keeps at once (the spec's "preview on focus, commit on Left/Right or Enter" assumed an option list; the row is a ‹ › choice, so the keyboard step is the focus and Enter the commit); chord rows name their source; the Connect firewall line links to Network › Remote agents; restart timing, UAC, autostart and Explorer checks are Windows owner checks; Windows focus check outstanding |
| T12 | Owner-directed 2026-11-02. Multiple-choice prompts as a choice card: notification actions up to 4, stacked labels, keyboard, chosen state; an owner-set answer window (default 1 min, control panel); Claude Code AskUserQuestion answered from the card by a PreToolUse hook. No new widget, surface or MCP field. Epic hud-bz9bv. | planned |

## Current implementation evidence

- **T6 — delivery [Observed].** The [Windows workflow](../.github/workflows/windows.yml)
  builds, tests, smoke-checks, and publishes signed releases. The runtime has
  [self-install](../crates/tze_hud_runtime/src/operator/install.rs),
  [pairing](../crates/tze_hud_runtime/src/pairing.rs),
  [operator routes](../crates/tze_hud_runtime/src/http.rs) (`route`),
  [restart handoff](../crates/tze_hud_runtime/src/operator/handoff.rs),
  [signed update](../crates/tze_hud_runtime/src/operator/update.rs), and
  [firewall diagnostics/helper](../crates/tze_hud_runtime/src/firewall.rs).
  Both transports use the shared [listener manager](../crates/tze_hud_runtime/src/net_addrs.rs)
  to follow every later Tailscale address via OS notifications and retry only
  missing binds, including failed restart/update handoff binds. The deterministic
  `listener_lifecycle_follows_changes_and_retries_only_missing_binds` gate covers
  address events, partial failures, deduplication and absence of idle polling;
  static address selection remains a separate gate. Existing MCP fan-out and
  gRPC handoff fixtures additionally inject a late v6 address and failed-then-
  successful bind, then connect to the real transport over IPv6 loopback;
  MCP also verifies that status lists only successful binds. Pairing uses each
  transport's successful bind list independently, with `grpc: null` before a
  gRPC listener is adopted; the pairing fixture covers unavailable and later
  successful gRPC addresses.
  This source/automated evidence does not establish Windows owner acceptance.
  [Unknown] Owner-host acceptance remains unverified in this refresh;
  `hud-i2e10.13`, `hud-lgb6c.3`, and `hud-jm8nq.18` are still open.
- **T7 — portal [Observed].** MCP tools now share
  [`PortalHandle`](../crates/tze_hud_mcp/src/server.rs) directly with the
  runtime's [portal driver](../crates/tze_hud_runtime/src/portal.rs).
  [`PortalHub`](../crates/tze_hud_projection/src/hub.rs) owns transcripts,
  replies, liveness, pacing, cue deadlines and retained unread state under
  `(agent, portal id)`. The [pure renderer](../crates/tze_hud_projection/src/render.rs)
  builds scene nodes from `PortalView` and resolved tokens; the driver applies
  them directly. The resident adapter and protobuf render round trip are gone.
  The [projection crate](../crates/tze_hud_projection/src/lib.rs) no longer exports
  the external authority, contract or coalescer. Portal-only `MutationProto`
  fields 13–17 and their names are reserved; their wire messages, converters and
  scene mutation variants are removed. Native rendering retains the checked
  direct scene methods. This is source evidence, not Windows owner acceptance.
  The runtime module/type is now `portal.rs` / `PortalDriver`, with one binding
  map and immediate scene-owned lease revocation. Scene, MCP and pause control
  share the injected clock; healthy portal idle/hold allowances survive pause
  while input expiry, prior orphan grace and maximum suspension continue.
  Scene bounds now supply the live fractional render/follow-tail viewport;
  the obsolete retained geometry cache is gone. Windowed submission routes text
  through the complete handle, keeps known-portal refusal out of raw echo, and
  resets history once in the common batch route after raw history seeding.
  Resize broadcast/persistence completion and detached store persistence each
  have one post-lock owner.

  Physical line inventory (comments/blanks included; production-prefix / test
  module / whole file) against `6ef4bf1a`, after the windowed deduplication:

  | File | Before | After |
  |---|---:|---:|
  | runtime/windowed/portal.rs | 2207 / 6734 / 8941 | 2171 / 4087 / 6258 |
  | runtime/windowed/keyboard.rs | 2072 / 319 / 2391 | 2044 / 1706 / 3750 |
  | runtime/windowed/lifecycle.rs | 3087 / 501 / 3588 | 3052 / 1336 / 4388 |
  | runtime/windowed/test_support.rs | 216 / 1046 / 1262 | 226 / 1351 / 1577 |
  | runtime/portal.rs | 986 / 1511 / 2497 | 949 / 1521 / 2470 |
  | projection/hub.rs | 1178 / 681 / 1859 | 1096 / 565 / 1661 |
  | input/portal_resize.rs (comment only) | 1067 / 1375 / 2442 | 1064 / 1375 / 2439 |
  | projection/Cargo.toml | 16 total | 14 total |

  The finite combined surface includes both move destinations and all of
  `test_support.rs`: 22,996 → 22,557 lines (439 fewer). Docs and the single
  removed lockfile dependency edge are outside that aggregate. These are
  physical module boundaries, not semantic production LOC: `test_support.rs`
  is entirely test-only, driver prefix retains a test-only hub accessor, and
  lifecycle prefix retains test-only imports. The feature-gated event-loop
  harness is unchanged and is not counted as production reduction.
  Thirteen keyboard and fourteen lifecycle test parents moved unchanged in
  name; shared factories moved once into test support. Five dimension parents
  became one five-row table and three pruning parents became one table at the
  real app method, including busy locks and idempotence. Five cache-only
  parents disappeared with their obsolete types. Tests: +2 ~3 -13 (net -11);
  moves are zero delta. Driver's 21 unique parents remain.
  Approximate windowed targets of 1700 / 2600 / 4300 are missed by
  471 / 1487 / 1958 lines; group resize/drag, raw-history seeding and every
  unique retained regression remain in scope. Driver's approximate 750 / 1100
  targets are also missed (949 / 1521). Moving tests is ownership cleanup,
  not combined deletion. This source inventory does not establish executed
  verification or physical Windows acceptance. Composer reduction, crate
  rename, final four-group size reconciliation and Windows owner acceptance
  remain (`hud-lirh2`); source evidence cannot close those owner checks.
- **T8 — diet [Observed].** The [local harness](../justfile) includes
  llvmpipe GPU recipes and Nextest; resource acceptance has a compact
  [`store_behavior`](../crates/tze_hud_resource/tests/store_behavior.rs) suite.
  Obsolete overlay/benchmark harnesses were removed in PR #1457. Unsupported
  root/tab zone configuration and the unused sealed-image upload workflow are
  removed; built-in zones, display assignments and offline render-scene and
  render-widget authoring remain supported. No Criterion benchmark targets
  are currently declared. The orphaned windowed benchmark mode and unbounded
  frame history are removed; telemetry keeps cumulative session totals and
  bounded recent latency samples. Headless benchmarks and the windowed
  quiescent-efficiency artifact remain. Pre-reset references remain in the projection and
  protocol crate docs. `hud-bstmy` and its reconciliation work remain open;
  a passing test inventory alone does not establish that the diet is complete.
- **T9 — monitors [Observed].**
  [`windowed/displays.rs`](../crates/tze_hud_runtime/src/windowed/displays.rs)
  creates per-monitor overlay windows sharing a scene in physical desktop
  pixels. Config still assigns zones by OS name such as `DISPLAY6`
  ([`zone_display_assignments`](../crates/tze_hud_config/src/displays.rs)).
  [EDID identity discovery](../crates/tze_hud_runtime/src/windowed/monitor_identity.rs)
  logs identity data; it does not yet replace those config keys.
  Token sizes are dp at each monitor's scale × `ui.scale`
  ([`set_px_scale`](../crates/tze_hud_config/src/tokens.rs), `hud-h51u7.3`).
  Stable-key integration (`hud-avr7s.2`) and mixed-DPI window sizing
  (`hud-670ds`) remain open. The vision describes their
  intended end state, not current deployment guarantees.
  Overlay sleep/wake recovery (`hud-jm8nq.25`) reports native visibility, iconic,
  topmost and DWM cloaking state per display, and requests no-activate reassertion
  on display/resume/power-on events or a status observation of drift. The decision
  has deterministic unit coverage. The owner's overnight monitor sleep/wake check
  remains pending; source checks do not establish that the original invisible
  overlay failure was repaired.
- **T10 — visuals [Observed].**
  [`resolve_config_tokens`](../crates/tze_hud_config/src/themes.rs) selects
  Tonal Glass, Blueprint or Classic at startup; [font loading](api.md#fonts)
  supports bundled, config-adjacent and named Windows fonts.
  Live switching (`hud-h51u7.5`) applies `[design_tokens]` edits to the
  config file and the control panel's Appearance and Advanced tokens choices
  without a restart ([`appearance.rs`](../crates/tze_hud_runtime/src/appearance.rs));
  the panel saves them with
  [`set_design_tokens`](../crates/tze_hud_config/src/config_write.rs). Widget
  text still uses the bundled fonts. The remaining restyle/owner-signoff work
  in `hud-h51u7` is outstanding. The Windows workflow captures theme fixtures and reports
  pixel differences; that evidence is not an owner visual signoff.

**Performance qualification [Observed].** The
[demand-layout investigation](operations/demand-layout-capability.md) records
NO-GO for complete-word deferral as a solution to the named giant-word
workload. Its prototype parity/performance gates were not executed, and the
prior measured Stage 4 failures remain failures. This documentation landing
must not be read as closing the performance outcome.

## Documentation shape after the reset

[Observed] T0 deliberately retired `about/`, OpenSpec and their navigation
skills. The generic th-projects shape scanner therefore reports
`UNSHAPED`, `DOCTRINE_LAYOUT=NONE`, and a failed formal traceability gate.
That measures the absent standard layout, not the absence of project guidance.
Use the compact equivalents below; archived specs are history, not current
requirements ([reset commit](https://github.com/tzeusy-org/tze-hud/commit/e32c619458f7ee527d738d17215286a632b32322)).

| Project question | Current home |
|---|---|
| Why, boundaries and principles | [vision.md](vision.md) |
| API and design contracts | [api.md](api.md), with wire definitions in [protocol](../crates/tze_hud_protocol/proto/) |
| Required behavior and regression evidence | [invariants.md](invariants.md); approved tranche direction above |
| Components, ownership and data flow | [architecture.md](architecture.md) |
| How to change and verify it | [README](../README.md), [AGENTS.md](../AGENTS.md), [justfile](../justfile), [Windows development](development/windows.md) |

[Inferred] The main maintenance risk is calling a tranche done because one
implementation or test landed. Keep source implementation, automated evidence
and owner acceptance distinct. [Unknown] This refresh is not an exhaustive
requirement-by-requirement audit of every public interface. Existing T6–T10
reconciliation and owner-check beads remain the follow-up record; this refresh
does not close them or authorize new product scope.

## POC acceptance

The POC is done when this passes on the Windows overlay, each stage in one or
two calls within its [api.md](api.md) token budget:

- **Portal:** a session attaches, streams output, receives a typed reply
  (local echo, no round trip), and detaches; an abandoned portal is reclaimed.
- **Zones:** a notification shows for its TTL and disappears unattended;
  `delay_ms` content appears on schedule; a notification action reaches `hud_input`.
  The [latest-wins count guard](../crates/tze_hud_compositor/src/renderer/tests/zone_stack.rs)
  checks warm/unchanged/one-zone-update/unchanged shaping work (2/0/1/0).
  These are frame-path shaping counts, not all CPU layout or presentation costs;
  zero widget raster/upload work still leaves full-frame rendering and GPU work.
- **Widgets:** a typed parameter update re-rasterizes only that widget's SVG
  (the frame is still re-presented in full; no damage tracking).
- **Override:** safe mode and dismiss win with a hung agent.
- **Idle:** an idle HUD with content on screen uses ~0% CPU and redraws nothing.

After the POC passes, use it before planning more.

Adopt windowed damage tracking only if `hud-w2t3x` / `hud-ddm2g`
publish-to-present measurements identify full re-present as the bottleneck;
`hud-ko552`'s historical ~5 µs per `collect_text_items` call measures isolated
text collection, not a frame or present cost.

Public agent-owned tiles were removed on 2026-10-14 for lack of a user.
The deferred spec and entry criterion remain in hud-x1wuq; internal scene
tiles, leases and native portal rendering remain.

## Merge gate and testability

The merge queue is the development loop, so it is held to targets, not just
kept green. Changing a required check updates the `main merge queue` ruleset,
the [`ci.yml`](../.github/workflows/ci.yml) header and this section together.

- **Fast.** A merge-group run's required checks finish within 6 minutes of the
  run starting. Suites that cannot fit run in the weekly lanes
  (`perf-budget.yml`, `perf-assert.yml`) or in `windows.yml`, never in the
  required set. [Observed 2026-10-14] p50 4.1 min, max 5.2 min.
- **Not queued.** Wall time is run time plus the wait for a hosted runner, and
  every job, PR push, merge-group build and main push draws on one shared
  pool. Required checks are three jobs (`static checks`, `cargo clippy`,
  `cargo test`); a new gate is a step in one of them, not a new job.
  Superseded PR runs are cancelled, and `windows.yml` keeps its
  informational idle measurement off PRs and runs its theme captures only
  when a PR touches rendered paths. [Observed 2026-11-01, nine jobs per run]
  jobs ran ≤3.3 min but waited 4–15 min for a runner; PR and merge-group wall
  time reached 13–18 min.
- **Repeatable.** A correct change passes on the first attempt. Nextest runs
  with `retries = 0`; a per-test retry override in
  [`.config/nextest.toml`](../.config/nextest.toml) names its reason and is
  tracked debt, not a fix. [Observed 2026-10-14] 25 of 25 merge-group runs
  green on the first attempt.
- **Unattended.** No required check waits on a person, the owner's hardware or
  a live HUD. Pixels are checked by machine on llvmpipe (`just test-gpu`,
  `just render-scene`); the Windows build and WARP smoke in `windows.yml` are
  informational. Owner-host checks, including POC acceptance above, are never
  a merge gate.
- **Mirrored locally.** `just ci` runs the required gates in CI order;
  `just pre-push` runs fmt and the exact CI clippy command.

Open work toward these targets: `hud-1dnk5` (`just dev-run`, Windows-first
quickstart, capture diff) and `hud-51ujj.7` (live smoke against the paired
HUD).

## Working rules

- Prefer deleting over generalizing. New abstractions need a current user.
- Tests guard behavior a user would notice, not internal structure.
- Removals and redesigns must keep every contract in `invariants.md`; changing
  one is a deliberate decision recorded there, with its tests.
- Keep `docs/` compact: vision, current scope, architecture, invariants, API,
  quickstart, development guidance and operational runbooks. Put investigation
  logs in PR descriptions; retain only reusable operational decisions in runbooks.
- Behavior changes update the affected API/invariant/operational docs in the same
  change. Status changes update this file with source evidence and any remaining
  owner acceptance; do not restore the retired doctrine/spec trees.
