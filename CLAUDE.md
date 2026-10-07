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
  Developing from Windows: `docs/development/windows.md`.
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
and operational notes. Run GPU tests with `just test-gpu` (llvmpipe-only), not bare
`cargo test -p tze_hud_compositor`. Set `PROTOC` if `/usr/bin/protoc` is older than 3.15.

## LLM Self-Projection

To project this session onto the HUD, use the **`hud-projection`** skill
(`.claude/skills/hud-projection/SKILL.md`). It is cooperative opt-in
projection through the MCP verbs on `portal:<id>`: `hud_publish` (the first
publish attaches), `hud_input` (replies, acked in the same call), and
`hud_clear` (detach). Pair once with the code the HUD shows:
`python3 .claude/skills/user-test/scripts/hud_pair.py --host <HUD-address[:port]> --code <on-screen-code>`.
The PSK stays in `~/.config/tze-hud/<hostname>.psk`; a private nonsecret endpoint
record retains the actual pairing port. The project MCP stdio adapter discovers
one paired host without `HUD_HOST`; multiple hosts require explicit `HUD_HOST`.
An old key without metadata uses 9090; an old custom port needs re-pairing or
explicit selection. Ordinary Python clients still use `HUD_HOST`. The adapter
runs on Linux/WSL; native Windows/macOS client lifetime support is not claimed.
Fresh authenticated Claude discovery, including five upstream tools, remains
the live acceptance check; cached tools alone are insufficient. The paired agent gets
`allow = ["*"]`; an `[agents.<id>] allow` list in the HUD's `agents.toml` (PSK
hashes only, beside the config) must include `portal`. For one-shot
zone publishing, use **`th-hud-publish`**.

## Issue Tracking

Beads (`bd`) tracks work when its Dolt server is reachable. Use
`git worktree add .worktrees/<name> -b <branch>` for isolated workers rather
than switching branches in the main checkout.
