# Scope and reset plan

The project is being cut back from a general "agent presence engine" to an
MCP/gRPC layer for the HUD real-estate lifecycle (see [vision.md](vision.md)).
Tranches T0–T4 removed what doesn't serve that; T5 redesigned the API that
remains. T6–T8 prove the result on Windows and shrink the code to match.
Each tranche must still build, pass tests, and boot the overlay.

## Keep

| Area | Where |
|---|---|
| Overlay window, compositor, text/markdown, images | `tze_hud_runtime` (windowed), `tze_hud_compositor` |
| Scene graph: tabs, tiles, nodes, zones | `tze_hud_scene` |
| Session portal + projection | `tze_hud_projection`, `tze_hud_runtime` portal modules, `tze_hud_input` |
| MCP tools (zones, widgets, portal) | `tze_hud_mcp` |
| gRPC resident session (tiles, events) | `tze_hud_protocol` |
| SVG widgets + asset store | `tze_hud_widget`, `tze_hud_resource` |
| Config + flat design tokens | `tze_hud_config` |
| Frame/idle telemetry | `tze_hud_telemetry` |
| App binary | `app/tze_hud_app` |

## Tranches

| Tranche | What | Status |
|---|---|---|
| T0 | Doctrine, RFCs, OpenSpec, curriculum, evidence/report docs, doctrine and OpenSpec agent skills, vocabulary lint | done |
| T1 | Unused crates: `tze_hud_a11y`, `tze_hud_media_apple`, `tze_hud_media_android`, `tze_hud_policy`; Android/iOS/Safari CI workflows | done |
| T2 | Media and cloud relay: GStreamer/`v2_preview` features, media ingress/admission, video surface, media signaling protobuf messages (field numbers reserved), media config and capability, real-decode and v2-preview CI, Python media exemplars | done |
| T3 | Governance: attention budget, quiet hours, privacy redaction and viewer classes, `[privacy]`/`[degradation]`/`[chrome]` config, admission controller, budget ladder (now plain hard caps), unwired lease state machine and suspension manager, degradation ladder down to one fallback (Normal ↔ Simplified). **Kept** the lease lifecycle (request, TTL, renew, release, revoke, disconnect grace). Capability-scope shrink moved to T5: it changes the session-init and lease wire contract | done |
| T4 | Scaffolding: `tze_hud_validation`, replay/trace recording, v1-thesis/Layer-4 artifact harness and their CI jobs; component profiles (flat `[design_tokens]` stay; profile sections are ignored); sync groups and clock-skew estimation (`compositor_timestamp_wall_us` stays for `present_at`/`expires_at`; wire fields reserved); hardware calibration (tests use a fixed `test_budget` slack; the benchmark keeps its CI factors); test scenes for removed features; reserved mobile display profile; unwired tab-switch trigger; redundant tests | done |
| T5 | API redesign ([api.md](api.md)): one verb set per lifecycle stage across MCP and gRPC (five MCP tools replace 22; gRPC `ClaimTile`/`Publish`/`Clear`/`Hold`/`Reclaimed`/`RequestResult`); per-agent PSK identity with an `allow` list replaces the capability vocabulary, resident principal, and portal owner token; one shared error-code set with hints; tile placement hints replace agent geometry; token budgets enforced in CI; dead wire removed and reserved; session-server constructors collapsed; invariant 1 and 4 breaks on the gRPC path fixed | done |
| T6 | Windows delivery without SSH, then prove the POC on it. (1) CI builds `x86_64-pc-windows-msvc` natively on a Windows runner, boots it under WARP with an MCP smoke, and publishes minisign-signed releases (rolling `dev` from main, `v*` tags). (2) The exe self-installs per user (`%LOCALAPPDATA%`, `HKCU` Run autostart, no admin). (3) It listens on loopback and the Tailscale address only. (4) First-run pairing: the HUD shows a one-time code, an agent host trades it for a per-agent PSK (`POST /pair`; only a hash is stored); delete `--psk`, the default PSK, and `--bind-all-interfaces`. (5) `/admin/*` operator endpoints (status, logs, screenshot of the HUD's own frame, restart, signed pull-only self-update with health handoff), off the MCP surface. (6) Rewrite `user-test`/`hud-projection` around the paired PSK; delete SSH deploy tooling. Then run POC acceptance on the owner's host; close stale draft PRs | in progress (1) |
| T7 | Portal as a runtime feature: projection state lives in the runtime keyed by PSK identity. Delete the external-authority design: owner tokens, audit log, provider-neutral contract, `projection_authority` binary, `resident_grpc` adapter. Fold the `PROJECTION_*` error codes into the shared set. Internal interfaces may break; the MCP surface and token footprint must not regress. Target: portal code (projection, driver, windowed portal, composer) under ~12k lines from ~45k | planned |
| T8 | Code and test diet, crate by crate (compositor, runtime, scene, protocol, input first): delete code without a current user, replace oversized test files with behavior tests tied to `invariants.md`, add `tze_hud_resource` tests, a local llvmpipe recipe for compositor tests, and strip pre-reset references (openspec, RFCs, doctrine) from comments and protos | planned |

## POC acceptance

The POC is done when this passes on the Windows overlay, each stage in one or
two calls within its [api.md](api.md) token budget:

- **Portal:** a session attaches, streams output, receives a typed reply
  (local echo, no round trip), and detaches; an abandoned portal is reclaimed.
- **Zones:** a notification shows for its TTL and disappears unattended;
  `delay_ms` content appears on schedule; a notification action reaches `hud_input`.
- **Widgets:** a typed parameter update re-renders only that widget.
- **Tiles:** a gRPC agent claims a tile with a placement hint in two round
  trips, updates it, and its tile is orphaned on disconnect and reclaimed
  after grace.
- **Override:** safe mode and dismiss win with a hung agent.
- **Idle:** an idle HUD with content on screen uses ~0% CPU and redraws nothing.

After the POC passes, use it before planning more.

## Working rules

- Prefer deleting over generalizing. New abstractions need a current user.
- Tests guard behavior a user would notice, not internal structure.
- Removals and redesigns must keep every contract in `invariants.md`; changing
  one is a deliberate decision recorded there, with its tests.
- `docs/` holds only this file, `vision.md`, `invariants.md`, `api.md`,
  `QUICKSTART.md`, and `operations/` runbooks. Investigation notes go in PR descriptions.
