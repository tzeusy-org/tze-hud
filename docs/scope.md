# Scope and implementation status

The project is being cut back from a general "agent presence engine" to an
MCP/gRPC layer for the HUD real-estate lifecycle (see [vision.md](vision.md)).
Tranches T0–T4 removed what doesn't serve that; T5 redesigned the API that
remains. T6–T8 prove the result on Windows and shrink the code to match.
T9–T10 are owner-directed additions: per-monitor overlays and a swappable
visual design.
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
| gRPC resident session (tiles, events) | `tze_hud_protocol` |
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
| T5 | API redesign ([api.md](api.md)): one verb set per lifecycle stage across MCP and gRPC (five MCP tools replace 22; gRPC `ClaimTile`/`Publish`/`Clear`/`Hold`/`Reclaimed`/`RequestResult`); per-agent PSK identity with an `allow` list replaces the capability vocabulary, resident principal, and portal owner token; one shared error-code set with hints; tile placement hints replace agent geometry; token budgets enforced in CI; dead wire removed and reserved; session-server constructors collapsed; invariant 1 and 4 breaks on the gRPC path fixed | done |
| T6 | Windows delivery without SSH, then prove the POC on it. (1) CI builds `x86_64-pc-windows-msvc` natively on a Windows runner, boots it under WARP with an MCP smoke, and publishes minisign-signed releases (rolling `dev` from main, `v*` tags). (2) The exe self-installs per user (`%LOCALAPPDATA%`, `HKCU` Run autostart, no admin). (3) It listens on loopback and the Tailscale address only (owner reaffirmed 2026-10-06: no loopback-only default, no relay). The HUD detects when Windows Firewall blocks tailnet inbound and says so on the pairing card and in `/admin/status`; an opt-in elevated helper adds one program-scoped rule (bead `hud-lgb6c`). (4) First-run pairing: the HUD shows a one-time code, an agent host trades it for a per-agent PSK (`POST /pair`; only a hash is stored); delete `--psk`, the default PSK, and `--bind-all-interfaces`. (5) `/admin/*` operator endpoints (status, logs, screenshot of the HUD's own frame, restart, signed pull-only self-update with health handoff), off the MCP surface. (6) Rewrite `user-test`/`hud-projection` around the paired PSK; delete SSH deploy tooling. Then run POC acceptance on the owner's host; close stale draft PRs | implemented delivery paths; Windows owner acceptance outstanding |
| T7 | Portal as a runtime feature: projection state lives in the runtime keyed by PSK identity. Delete the external-authority design: owner tokens, audit log, provider-neutral contract, `projection_authority` binary, `resident_grpc` adapter. Fold the `PROJECTION_*` error codes into the shared set. Internal interfaces may break; the MCP surface and token footprint must not regress. Target: portal code (projection, driver, windowed portal, composer) under ~12k lines from ~45k | in progress: shared `PortalHub` is live; legacy renderer/contract cleanup remains |
| T8 | Code and test diet, crate by crate (compositor, runtime, scene, protocol, input first): delete code without a current user, replace oversized test files with behavior tests tied to `invariants.md`, add `tze_hud_resource` tests, a local llvmpipe recipe for compositor tests, and strip pre-reset references (openspec, RFCs, doctrine) from comments and protos | in progress: resource behavior tests and llvmpipe recipes exist; diet and reconciliation continue |
| T9 | Owner-directed 2026-10-05, alongside T6 rather than after the POC. Per-monitor overlays: one window and surface per monitor sharing one scene, the default with no flag; delete `--monitor` and monitor cycling; logical (dp) token sizes scaled per monitor (bead `hud-1pt5h`). Owner-directed 2026-10-06: `[displays.<key>]` is keyed by EDID model and serial, plus the GPU connector path when two monitors are ambiguous; `/admin/status` and the log show each key (bead `hud-avr7s`). One global `ui.scale` token multiplies every logical size on top of each monitor's Windows scale factor; there are no per-monitor overrides (bead `hud-h51u7.3`) | partial: overlay windows exist; stable keys and logical sizing remain |
| T10 | Owner-directed 2026-10-05. Swappable visual design: themes as a token layer between canonical defaults and `[design_tokens]` overrides, with Tonal Glass as the default; semantic token families (surfaces, roles, state, motion, shape, spacing, type); configurable fonts (bundled IBM Plex, fonts beside the config, Windows fonts by name); live theme and font switching from the config file and from an in-HUD settings card opened by a shortcut, which writes the choice back to the config (owner-directed 2026-10-06; agents never set themes); every element restyled to use tokens only (bead `hud-h51u7`). This reverses T4's removal of component profiles for themes only: themes hold token values, never layout | partial: startup themes/fonts exist; live switching and restyling remain |

## Current implementation evidence

- **T6 — delivery [Observed].** The [Windows workflow](../.github/workflows/windows.yml)
  builds, tests, smoke-checks, and publishes signed releases. The runtime has
  [self-install](../crates/tze_hud_runtime/src/operator/install.rs),
  [pairing](../crates/tze_hud_runtime/src/pairing.rs),
  [operator routes](../crates/tze_hud_runtime/src/http.rs) (`route`),
  [restart handoff](../crates/tze_hud_runtime/src/operator/handoff.rs),
  [signed update](../crates/tze_hud_runtime/src/operator/update.rs), and
  [firewall diagnostics/helper](../crates/tze_hud_runtime/src/firewall.rs).
  [Unknown] Owner-host acceptance remains unverified in this refresh;
  `hud-i2e10.13`, `hud-lgb6c.3`, and `hud-jm8nq.18` are still open.
- **T7 — portal [Observed].** MCP tools now share
  [`PortalHandle`](../crates/tze_hud_mcp/src/server.rs) directly with the
  runtime's [portal driver](../crates/tze_hud_runtime/src/portal_projection_driver.rs).
  [`PortalHub`](../crates/tze_hud_projection/src/hub.rs) owns transcripts,
  replies, liveness and pacing under `(agent, portal id)`. The driver still
  uses `ResidentGrpcPortalAdapter` and `ProjectedPortalState`; the
  [projection crate](../crates/tze_hud_projection/src/lib.rs) still exports
  `ProjectionAuthority` and the old contract. [Inferred] This is an active
  migration, not a finished deletion or rename (`hud-lirh2`). The ~12k-line
  target has not been certified by this refresh.
- **T8 — diet [Observed].** The [local harness](../justfile) includes
  llvmpipe GPU recipes and Nextest; resource acceptance has a compact
  [`store_behavior`](../crates/tze_hud_resource/tests/store_behavior.rs) suite.
  Obsolete overlay/benchmark harnesses were removed in PR #1457. Remaining
  pre-reset references are still visible in the projection and protocol crate
  docs. `hud-bstmy` and its reconciliation work remain open; a passing test
  inventory alone does not establish that the diet is complete.
- **T9 — monitors [Observed].**
  [`windowed/displays.rs`](../crates/tze_hud_runtime/src/windowed/displays.rs)
  creates per-monitor overlay windows sharing a scene in physical desktop
  pixels. Config still assigns zones by OS name such as `DISPLAY6`
  ([`zone_display_assignments`](../crates/tze_hud_config/src/displays.rs)).
  [EDID identity discovery](../crates/tze_hud_runtime/src/windowed/monitor_identity.rs)
  logs identity data; it does not yet replace those config keys.
  Stable-key integration (`hud-avr7s.2`), mixed-DPI sizing (`hud-670ds`), and
  global `ui.scale` (`hud-h51u7.3`) remain open. The vision describes their
  intended end state, not current deployment guarantees.
- **T10 — visuals [Observed].**
  [`resolve_config_tokens`](../crates/tze_hud_config/src/themes.rs) selects
  Tonal Glass, Blueprint or Classic at startup; [font loading](api.md#fonts)
  supports bundled, config-adjacent and named Windows fonts.
  [`set_design_tokens`](../crates/tze_hud_config/src/config_write.rs) can
  persist theme/font choices but has no production caller yet. Live switching
  (`hud-h51u7.5`) and the remaining restyle/owner-signoff work in `hud-h51u7`
  are outstanding. The Windows workflow captures theme fixtures and reports
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
- **Widgets:** a typed parameter update re-rasterizes only that widget's SVG
  (the frame is still re-presented in full; no damage tracking).
- **Tiles:** a gRPC agent claims a tile with a placement hint in two round
  trips, updates it, and its tile is orphaned on disconnect and reclaimed
  after grace.
- **Override:** safe mode and dismiss win with a hung agent.
- **Idle:** an idle HUD with content on screen uses ~0% CPU and redraws nothing.

After the POC passes, use it before planning more.

Adopt windowed damage tracking only if `hud-w2t3x` / `hud-ddm2g`
publish-to-present measurements identify full re-present as the bottleneck;
`hud-ko552`'s historical ~5 µs per `collect_text_items` call measures isolated
text collection, not a frame or present cost.

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
