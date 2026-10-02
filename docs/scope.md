# Scope and reset plan

The project is being cut back from a general "agent presence engine" to an
MCP/gRPC layer for the HUD real-estate lifecycle (see [vision.md](vision.md)).
Tranches T0–T4 removed what doesn't serve that; T5 redesigned the API that
remains. Each tranche must still build, pass tests, and boot the overlay.

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

## Working rules

- Prefer deleting over generalizing. New abstractions need a current user.
- Tests guard behavior a user would notice, not internal structure.
- Removals and redesigns must keep every contract in `invariants.md`; changing
  one is a deliberate decision recorded there, with its tests.
- `docs/` holds only this file, `vision.md`, `invariants.md`, `api.md`,
  `QUICKSTART.md`, and `operations/` runbooks. Investigation notes go in PR descriptions.
