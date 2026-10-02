# CLAUDE.md

Guidance for Claude Code working in this repository.

## What This Is

**tze_hud** — a well-designed, extremely performant MCP/gRPC layer that lets
models generate and manage the real-estate lifecycle of a HUD over the user's
screen (discover, claim, fill, interact, hold, release, reclaim). Surfaces: a
session portal, ambient zones, agent-owned tiles, and SVG widgets, on a
Windows overlay. Read `docs/vision.md` first, then `docs/api.md`, `docs/invariants.md`, and `docs/scope.md`.

## Status

The 2026-10-02 reset is complete (T0–T5 in `docs/scope.md`): the project was
cut back from a much larger "agent presence engine" vision, and `docs/api.md`
is the API reference. The old doctrine, RFCs, and
OpenSpec specs live only at git tag `pre-reset-2026-10-02` — history, not
requirements. Do not restore them or design against them.

## Technology

- Rust core; Tokio; tonic for gRPC; wgpu + winit for rendering and input; resvg for SVG.
- Two protocol planes: MCP (JSON-RPC over HTTP) for zone/widget publishing and
  portal tools; gRPC for resident tiles and streams. No media plane.
- Windows is the only deployment target. Linux builds are for headless CI.
- TypeScript/browser only for tooling, never in the runtime.

## Rules

- **LLMs are never in the frame loop.** Agents state intent; the runtime renders.
- **The runtime owns the pixels.** Geometry, z-order, and visibility are runtime decisions.
- **Local feedback first.** Hover, press, focus, and composer typing never wait on a remote round trip.
- **Idle costs ~nothing; work is proportional to change.** Don't re-render unchanged content.
- **Token-minimal LLM surfaces.** No layout, geometry, or styling payloads through model context; typed widget parameters are fine.
- **No hardcoded styling.** Use `[design_tokens]` via `RenderingPolicy`.
- **Trusted agents only.** PSK auth, a per-agent zone/widget allowlist, and leases with TTL that disconnect, expiry, or human override reclaim. Don't add policy engines, attention budgets, or privacy layers.
- **The API is the product.** Each lifecycle stage should be one or two small, deterministic calls; treat token cost per stage like latency.
- **Prefer deleting over generalizing.** New abstractions need a current user.

## Commands

See `AGENTS.md` for the `just` recipes (`just ci` mirrors the blocking CI gates)
and operational notes. Don't run `cargo test -p tze_hud_compositor` bare; the
pixel-readback GPU test deadlocks headless without Mesa llvmpipe.

## LLM Self-Projection

To project this session onto the HUD, use the **`hud-projection`** skill
(`.claude/skills/hud-projection/SKILL.md`). It is cooperative opt-in
projection through the MCP verbs on `portal:<id>`: `hud_publish` (the first
publish attaches), `hud_input` (replies, acked in the same call), and
`hud_clear` (detach). Send your agent's PSK as the MCP bearer; its
`[agents.<id>] allow` list in the HUD's `agents.toml` (PSK hashes only, beside
the config) must include `portal` (`scripts/quickstart.sh` pairs
`[agents.claude]` with `allow = ["*"]`). For one-shot
zone publishing, use **`th-hud-publish`**.

## Issue Tracking

Beads (`bd`) tracks work when its Dolt server is reachable. Use
`git worktree add .worktrees/<name> -b <branch>` for isolated workers rather
than switching branches in the main checkout.
