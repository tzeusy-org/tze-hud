# Vision

tze_hud is a local HUD for my own LLM agents: a transparent, always-on-top
Windows overlay where a handful of trusted agents can hold a little screen
space, keep it current, and talk back and forth with me — without becoming a
chat app or a dashboard.

This document replaced the 15-document doctrine, 14 RFCs, and ~37 OpenSpec
capability specs on 2026-10-02. Those are preserved at git tag
`pre-reset-2026-10-02` as history, not as requirements.

## What it is for

Four surfaces, in priority order:

1. **Session portal.** An LLM session (e.g. Claude Code) projects itself onto
   the overlay: live output stream, status, and a reply composer I can type
   into. The agent polls for my input; nothing scrapes a terminal.
2. **Ambient zones.** Fixed, named regions (subtitle, notification,
   status bar, ambient background). An agent publishes text to a zone with
   one MCP call and no knowledge of layout.
3. **Agent-owned tiles.** A resident agent creates and updates its own tiles
   (cards, small dashboards) over gRPC.
4. **SVG widgets.** User-authored SVG templates (gauges, progress bars) with
   typed parameters that agents set; the runtime rasterizes and animates.

## Principles that survive the reset

- **The model is never in the frame loop.** Agents state intent; the runtime
  lays out, renders, and animates.
- **The runtime owns the pixels.** Agents ask; the runtime decides geometry,
  z-order, and what is shown.
- **Local feedback first.** Hover, press, focus, and typing in the composer
  are handled locally and instantly; agents hear about it afterwards.
- **Cheap when idle, cheap per token.** An idle overlay should cost ~nothing.
  LLM-facing calls are few and small; layout and styling never pass through
  model context.
- **No hardcoded styling.** Colors, fonts, and spacing come from a flat
  `[design_tokens]` config section.

## Trust model

At most a few agents, all mine. A pre-shared key authenticates them. Each
agent owns what it creates; ownership is released when the agent disconnects.
There is no defense against hostile agents beyond that: no capability matrix,
policy arbitration, attention budgets, or viewer-aware privacy.

## Non-goals

- Live media (video, audio, WebRTC, GStreamer) and clocked sync.
- Mobile, glasses, VR, macOS/Linux deployment, accessibility bridges.
- Multi-tenant governance: policy engines, quiet hours, redaction.
- Swappable component profiles (tokens only).
- A window manager, browser shell, notification engine, or UI framework.

## Technology

Rust, Tokio, tonic (gRPC), wgpu + winit, resvg for SVG. Two protocol planes:
MCP (JSON-RPC over HTTP) for one-shot publishing and the portal tools; gRPC
for resident tiles and streaming. Windows (D3D12/Vulkan via wgpu) is the only
deployment target; Linux builds exist for headless CI.
