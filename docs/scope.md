# Scope and reset plan

The project is being cut back from a general "agent presence engine" to an
MCP/gRPC layer for the HUD real-estate lifecycle (see [vision.md](vision.md)).
Tranches T0–T4 remove what doesn't serve that; T5 redesigns the API that
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
| T3 | Governance: attention budget, quiet hours, privacy redaction, policy-shaped admission and budget ladder, degradation ladder down to one fallback. **Keep** the lease lifecycle (request, TTL, renew, release, revoke, disconnect grace); capability scopes shrink to a per-agent zone/widget allowlist | pending |
| T4 | Component profiles (keep tokens), sync groups / clock domains, replay, calibration, unused test scenes; `tze_hud_validation` + v1-thesis/Layer-4 artifact harness; shrink oversized test files | pending |
| T5 | API design pass: one coherent verb set per lifecycle stage (discover, claim, fill, interact, hold, release, reclaim) across MCP and gRPC; measure token cost per stage; collapse accreted constructors and per-feature parameter threading in the session server | pending |

## Working rules

- Prefer deleting over generalizing. New abstractions need a current user.
- Tests guard behavior a user would notice, not internal structure.
- `docs/` holds only this file, `vision.md`, `QUICKSTART.md`, and
  `operations/` runbooks. Investigation notes go in PR descriptions.
