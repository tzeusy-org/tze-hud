# Vision

tze_hud is a well-designed, extremely performant MCP/gRPC layer that lets
models generate and manage the **real-estate lifecycle** of a HUD over the
user's screen. The renderer is a means; the product is the API through which
a model claims space, fills it, keeps it current, hears from the human, and
gives it back.

This document replaced the 15-document doctrine, 14 RFCs, and ~37 OpenSpec
capability specs on 2026-10-02. Those are preserved at git tag
`pre-reset-2026-10-02` as history, not requirements. The lessons they taught
are kept below.

## The real-estate lifecycle

Every surface a model touches goes through the same lifecycle, and the API
is organized around it:

| Stage | What the model does | What the runtime guarantees |
|---|---|---|
| Discover | List zones, widgets, and its own holdings | Cheap, deterministic answers sized for model context |
| Claim | Publish to a named zone, or lease a tile region | Admission is immediate and explicit; conflicts resolve by policy, not by the model |
| Fill / update | Send content or typed parameters | Latest-wins coalescing; work proportional to change; nothing re-rendered needlessly |
| Interact | Receive input; acknowledge it | Local feedback is instant; the model hears about it afterwards |
| Hold | Renew, or let TTL run | Leases expire on their own; nothing is held forever by accident |
| Release | Clear, release, or detach | Space is reclaimed immediately and cleanly |
| Reclaim | (runtime-initiated) | Disconnect, expiry, or human override revokes and cleans up without model help |

Surfaces on that lifecycle, in priority order:

1. **Session portal.** An LLM session projects itself: live output, status,
   and a reply composer. The agent polls and acknowledges input; nothing
   scrapes a terminal.
2. **Ambient zones.** Named slots (subtitle, notification, status bar,
   ambient background). One MCP call, no layout knowledge.
3. **Agent-owned tiles.** Leased regions a resident agent creates and updates
   over gRPC.
4. **SVG widgets.** User-authored templates with typed parameters (`f32`,
   `color`, `enum`, `string`) the model sets; the runtime rasterizes and
   animates.

## Design principles

- **The model is never in the frame loop.** Models state intent; the runtime
  lays out, renders, and animates.
- **The runtime owns the pixels.** Geometry, z-order, and visibility are
  runtime decisions. Models never send coordinates or styling.
- **Token cost is an API metric.** Each lifecycle stage should be one or two
  small, deterministic calls. Responses carry what the model needs next and
  nothing else.
- **Two planes, chosen by traffic shape.** MCP for discoverable one-shot calls
  (zones, widgets, portal); gRPC streams for resident sessions (tiles, input,
  high-rate updates). Message classes stay distinct: transactional (acked),
  state-stream (coalesced), ephemeral (droppable).
- **Cheap when idle.** An idle HUD costs ~nothing; a busy one costs in
  proportion to what changed.
- **Local feedback first.** Hover, press, focus, and composer typing never
  wait on a round trip.
- **No hardcoded styling.** Visuals come from a flat `[design_tokens]` table.

## Trust model

A few agents, all the owner's. A pre-shared key authenticates them. Each
agent may publish to an allowlist of zones and widgets and holds leases with
TTLs. Disconnect, expiry, and human override reclaim everything. No defense
against hostile agents beyond that.

## Lessons from the first build

- **Scaffolding outran the product.** Doctrine, RFCs, specs, reconciliation
  reports, and evidence docs grew to ~640k lines around ~260k lines of Rust
  whose real use was mostly one portal. Write specs after the code works,
  keep them short, and put investigation notes in PRs.
- **The portal is where real use went.** Most commits after mid-2026 were
  portal and projection work. That one agent-surface lifecycle (attach,
  publish, take input, detach) is the product's centre of gravity.
- **"Deferred" must mean deleted.** Media was declared deferred, yet ~10k
  lines of ingress, admission, decode, and video-surface code stayed compiled
  behind default-off config and feature flags, taxing every change. Git
  history is the archive.
- **Governance was sized for the wrong threat.** Privacy classes, attention
  budgets, quiet hours, and policy arbitration targeted a shared household
  wall display with untrusted agents. The real user is one owner with their
  own agents.
- **APIs accreted instead of being designed.** Signs: a session-server
  constructor named
  `from_shared_state_with_config_media_ingress_and_degradation_notices`, and
  one module per feature threading parameters through every handler. The
  API layer deserves a deliberate design pass once the dead weight is gone.
- **Test volume is not confidence.** Single test files reached 17k lines, and
  GPU tests that can't run on the dev box gave no local signal. Test what a
  model or user would notice.
- **What worked, and stays:** one-call zone publishing, typed widget
  parameters, runtime-owned layout, local-first input, idle efficiency, and
  cooperative (not scraped) session projection.

## Non-goals

- Live media (video, audio, WebRTC, GStreamer) and clocked media sync.
- Mobile, glasses, VR, macOS/Linux deployment, accessibility bridges.
- Multi-tenant governance: policy engines, quiet hours, privacy redaction.
- Swappable component profiles (tokens only).
- A window manager, browser shell, notification engine, or UI framework.

## Technology

Rust, Tokio, tonic (gRPC), wgpu + winit, resvg. Windows (D3D12/Vulkan via
wgpu) is the only deployment target; Linux builds exist for headless CI.
