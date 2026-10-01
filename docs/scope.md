# Scope and reset plan

The project is being cut back from a general "agent presence engine" to the
four surfaces in [vision.md](vision.md). The cut happens in place, in
tranches. Each tranche must still build, pass tests, and boot the overlay.

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

## Remove

| Tranche | What | Status |
|---|---|---|
| T0 | Doctrine, RFCs, OpenSpec, curriculum, evidence/report docs, doctrine and OpenSpec agent skills, vocabulary lint | done |
| T1 | Unused crates: `tze_hud_a11y`, `tze_hud_media_apple`, `tze_hud_media_android`, `tze_hud_policy`, `tze_hud_validation`; mobile/media CI workflows | pending |
| T2 | Media: GStreamer feature, media ingress/admission, video surface, `v2_preview`, real-decode CI | pending |
| T3 | Governance: attention budget, quiet hours, admission, budget ladder, capability matrix, lease TTL renewal, redaction; leases become ownership + disconnect cleanup | pending |
| T4 | Component profiles (keep tokens), sync groups / clock domains, replay, calibration, unused test scenes; shrink oversized test files | pending |

## Working rules

- Prefer deleting over generalizing. New abstractions need a current user.
- Tests guard behavior a user would notice, not internal structure.
- `docs/` holds only this file, `vision.md`, `QUICKSTART.md`, and
  `operations/` runbooks. Investigation notes go in PR descriptions.
