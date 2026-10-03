# tze_hud Quickstart — portal as your primary LLM interface

**Goal:** in under 10 minutes, get the tze_hud runtime up and have a fresh
Claude or Codex session project itself onto your screen as a live text-stream
portal — so the portal becomes how you watch and talk to the session.

**Doctrine:** this is *cooperative opt-in projection*. The runtime owns the
screen (placement, timing, composition, permissions); the LLM session
*chooses* to attach and publish. Nothing is scraped from your terminal.

> Platform note: the steps below are for running the portal on your **own Linux
> desktop** (a local display server / X or Wayland session). For fullscreen wall
> displays, overlay mode on Windows, or cross-machine deployment, see the
> **Cross-Machine Deployment** and **TigerVNC** sections of the top-level
> [`README.md`](../README.md). A headless CI box cannot open a GUI window.

---

## TL;DR (the one command)

From the repo root (`mayor/rig/`):

```bash
# Build the runtime once (5–10 min the first time), then bootstrap + launch.
cargo build --bin tze_hud --release
scripts/quickstart.sh --window-mode overlay
```

`quickstart.sh` scaffolds a minimal config, generates a strong PSK, prints the
redacted **ATTACH INFO** block (MCP URL + credential instructions + a client
config template), and launches the runtime. Then jump to
[Step 4: attach a session](#step-4-attach-an-llm-session).

To create a ready-to-use client config without building or launching the
runtime, use the protected-file form (implemented by
[`scripts/quickstart.sh`](../scripts/quickstart.sh)):

```bash
scripts/quickstart.sh --emit-mcp-config=tze-hud.mcp.json
# Creates a new mode-600 file and refuses to overwrite an existing file.
```

To see the attach block **without** launching a window (e.g. to wire up your MCP
client first, or on a headless box):

```bash
scripts/quickstart.sh --print-attach-info
```

The binary also has this built in — no shell required, so it works on Windows
too. Once you have a `tze_hud` binary, ask it directly and it prints the same
attach block (MCP URL + resident-principal rule + paste-ready MCP config) and
exits **without** starting the runtime:

```bash
./target/release/tze_hud --print-attach-info
# honours --config / --mcp-port / --grpc-port so the printed info matches
# the runtime it describes; never prints the PSK value.
```

The rest of this doc is the same flow, step by step, explaining each piece.

---

## Prerequisites

- Linux with a display server (X11 or Wayland) — you need to be able to open a
  GPU window. Build deps and toolchain are in [`README.md`](../README.md)
  (§1 Build). In short: `build-essential pkg-config protobuf-compiler`, the X/
  Wayland `-dev` libs, and Rust 1.88 (pinned in `rust-toolchain.toml`).
- An LLM client that speaks **MCP over HTTP** and lets you set a bearer header —
  e.g. Claude Code or Codex with an MCP server entry.

---

## Step 1 — Build the runtime

```bash
cargo build --bin tze_hud --release
# → target/release/tze_hud
```

`tze_hud` is the **canonical runtime binary** (not a demo). It starts the
windowed compositor plus the gRPC and MCP listeners.

---

## Step 2 — Scaffold config + credentials

You can let `quickstart.sh` do this, or do it by hand.

**Automatic:**

```bash
scripts/quickstart.sh --print-attach-info
```

This writes two files in the current directory (both idempotent):

- `tze_hud.toml` — the minimal valid config: `[runtime]` + one default
  `[[tabs]]`. A text-stream portal renders into that **Main** tab using the
  runtime's built-in zero-config placement, size, and design-token defaults —
  no widget wiring is required for session projection.
- `tze_hud.psk` — a freshly generated strong pre-shared key (`chmod 600`),
  kept by the agent side as its MCP bearer.
- `agents.toml` — next to the config: pairs that PSK as agent `claude` with
  `allow = ["*"]`, storing only the PSK's SHA-256. The runtime never sees the
  PSK itself.

To scaffold those files and also create a secret-bearing MCP client config,
run `scripts/quickstart.sh --emit-mcp-config=tze-hud.mcp.json`. The output file
is created with mode `600`; the script fails rather than replacing an existing
client config. The bare `--emit-mcp-config` form instead writes exactly one JSON
document to stdout and exits, for piping into a client-specific merge command.
Treat that stdout as a secret: do not paste it into logs or tickets.

**Manual equivalent** (if you prefer):

```bash
cat > tze_hud.toml <<'TOML'
[runtime]
profile = "full-display"

[[tabs]]
name        = "Main"
default_tab = true
TOML

( umask 077; openssl rand -hex 24 > tze_hud.psk )
cat > agents.toml <<TOML
[agents.claude]
psk_sha256 = "$(tr -d '[:space:]' < tze_hud.psk | sha256sum | cut -d' ' -f1)"
allow      = ["*"]
TOML
```

> **Why a config and a paired agent are mandatory:** canonical startup is
> *fail-closed*. Launching with no readable config, or an invalid
> `agents.toml`, is a hard startup error. With no `agents.toml` the runtime
> starts but rejects every request until an agent is paired. The quickstart
> script sets up both for you.

---

## Step 3 — Launch

```bash
scripts/quickstart.sh --window-mode overlay
```

or equivalently, by hand:

```bash
./target/release/tze_hud \
  --config tze_hud.toml \
  --window-mode overlay \
  --mcp-port 9090 \
  --grpc-port 50051
```

The runtime takes no PSK: it authenticates each request against the hashes
in `agents.toml` next to `--config`.

A window opens. The MCP listener is on `http://127.0.0.1:9090/mcp` (the runtime
listens on loopback plus this host's Tailscale addresses, nothing else).

On launch the runtime also prints a short **startup banner** to stdout — once,
unconditionally, even when `TZE_HUD_LOG` is unset — so you can see where it is
listening without turning on logging:

```text
────────────────────────────────────────────────────────────────────
 tze_hud runtime ready
   gRPC   : 127.0.0.1:50051
   MCP    : http://127.0.0.1:9090/mcp   (auth: Authorization: Bearer <agent PSK>)
   attach : invoke the `hud-projection` skill in an LLM session, or run
            scripts/quickstart.sh — see docs/QUICKSTART.md
────────────────────────────────────────────────────────────────────
```

The banner is deliberately non-secret: it shows only the bound addresses and an
attach hint, never the PSK. (A disabled service — `--mcp-port 0` or
`--grpc-port 0` — shows as `disabled`.)

> **Identity is the PSK.** The MCP bearer identifies an agent, and that
> agent's `allow` list in `agents.toml` decides which tools it may call. The
> generated `agents.toml` pairs `[agents.claude]` with `allow = ["*"]`, so
> sending the PSK from `tze_hud.psk` as the MCP `Authorization: Bearer` gets
> you the portal tools. A disallowed call fails with `NOT_ALLOWED` and a hint
> naming the `allow` entry to add.

---

## Step 4 — Attach an LLM session

Generate a client config with the endpoint and bearer already wired:

```bash
scripts/quickstart.sh --emit-mcp-config=tze-hud.mcp.json
```

Merge the resulting `mcpServers.tze-hud-runtime` entry into your LLM client's
MCP settings. If you prefer to do that manually, the equivalent shape is:

```json
{
  "mcpServers": {
    "tze-hud-runtime": {
      "type": "url",
      "url": "http://127.0.0.1:9090/mcp",
      "headers": { "Authorization": "Bearer <your PSK>" }
    }
  }
}
```

`--print-attach-info` remains deliberately redacted. To get both its discovery
text and a protected credential file in one headless run, use:

```bash
scripts/quickstart.sh --print-attach-info \
  --emit-mcp-config=tze-hud.mcp.json
```

The bare stdout form is rejected when combined with `--print-attach-info`, so a
headless/redacted command can never print the PSK accidentally.

Then, inside that session, opt into projection. If your client supports the
bundled skill, just say **"project this session to the HUD"** — that loads the
[`hud-projection`](../.claude/skills/hud-projection/SKILL.md) skill. Otherwise
call the tools directly:

1. `hud_publish {"surface": "portal:<id>", "content": "...", "status": "active"}`
   — the first publish to a stable `<id>` attaches the portal; `display_name`
   is optional. Ownership is your agent identity: the runtime keeps the owner
   token server-side, so no call takes or returns one.
2. `hud_publish` again — publish output fragments; they render in the portal.
   Add `"expects_reply": true` to arm the composer.
3. `hud_input {"wait_ms": 30000}` — collect text typed at the HUD; pass the
   ids back in `ack` on the next call.
4. `hud_clear {"surface": "portal:<id>"}` — detach when done.

The full contract is in [`docs/api.md`](api.md).

You now have a session whose live output is on the screen and that can read
input typed at the HUD — the portal is your primary interface to it.

---

## Verify it works (no GUI needed)

Confirm the MCP endpoint is reachable and authenticating before debugging the
UI. `tools/list` should be *accepted* with your PSK and *rejected* without it:

```bash
# Reachable + authorized (expects a normal JSON-RPC result, not an auth error):
curl -s -X POST http://127.0.0.1:9090/mcp \
  -H "Content-Type: application/json" \
  -H "Authorization: Bearer $(cat tze_hud.psk)" \
  -d '{"jsonrpc":"2.0","method":"tools/list","params":{},"id":1}' | head -c 400
```

---

## Troubleshooting

| Symptom | Cause / fix |
|---|---|
| `canonical startup requires a readable config file` | No config resolved. Run from a dir containing `tze_hud.toml`, or pass `--config <path>`. `quickstart.sh` scaffolds one. |
| `agents.toml: invalid agents.toml` | A `psk_sha256` is not 64 hex characters or an `allow` entry is unknown; the message names the field. |
| Every MCP call is `-32004` unauthenticated | The bearer's SHA-256 is not in `agents.toml` next to the config. Re-run `quickstart.sh` or add the hash. |
| Nothing printed on stdout after launch | The runtime always prints a one-time non-secret startup banner (bind addrs + attach hint). *Structured* logs beyond it are gated behind the `TZE_HUD_LOG` env filter — run with `TZE_HUD_LOG=info` for detailed startup/bind logs. (`quickstart.sh` prints the attach block regardless.) |
| Portal call returns `NOT_ALLOWED` | The bearer's agent lacks `portal` in its `allow` list. Add it (or `*`) to that `[agents.<id>]` table in `agents.toml`, as the hint says, and restart. |
| `No active tab` on the autonomous test VM | WARP-VM-specific: the config's `[[tabs]]` did not materialize. Restart the HUD task; tabs are not creatable over MCP. Not seen on a normal GPU desktop. |
| Window won't open on a headless box | Expected — you need a real display server. Use overlay/fullscreen on a desktop, or the TigerVNC path in `README.md`. |

---

## Where to go next

- **Skill internals & full contract:** [`hud-projection` SKILL](../.claude/skills/hud-projection/SKILL.md)
- **One-shot zone/widget publishing** (no session lifecycle): the `th-hud-publish` skill
- **Full config surface** (widgets, agents, profiles): [`app/tze_hud_app/config/production.toml`](../app/tze_hud_app/config/production.toml)
- **All CLI flags / env vars:** `./target/release/tze_hud --help`
- **Cross-machine / Windows deployment:** [`README.md`](../README.md)
