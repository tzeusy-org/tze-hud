# Quickstart: a Windows HUD with an agent in WSL

Run the HUD on your Windows desktop and keep your agent session in WSL. This
walkthrough installs the overlay, pairs the agent, and connects a real session
portal. Linux is used for headless checks and Windows cross-builds.

Projection is cooperative: the session chooses to publish its output and
collect replies. The HUD does not scrape your terminal. The runtime owns
placement, timing, composition and permissions.

## 1. Start the Windows HUD

Download the rolling `dev` release of `tze_hud.exe`, verify its signature, and
double-click it. Follow the existing
[Windows install runbook](operations/windows-install.md#download-and-verify).
This installs for the current user, creates the default config if needed,
and starts the overlay. No administrator rights are needed for installation.
An existing config and paired agents are preserved on upgrades.

If you are building from source in WSL instead, run this from the repo root
with the [build dependencies](../README.md#1-build-on-linux--windows) installed:

```bash
just build-windows
# Output: target/x86_64-pc-windows-gnu/release/tze_hud.exe
```

Copy that executable to a Windows development directory. Run it from your
interactive Windows desktop with explicit arguments, for example in
PowerShell from that directory:

```powershell
.\tze_hud.exe --config C:\path\to\repo\app\tze_hud_app\config\production.toml --window-mode overlay
```

Replace the example config path with your actual checkout path. A bare dev
launch installs the executable and can replace your installed HUD. Keep the
arguments for an in-place run, and stop an already-running instance first:
only one HUD runs per Windows user. See
[Windows development](development/windows.md#always-pass-arguments-to-a-dev-build).

The first-run card shows a pairing code and the Windows host's Tailscale
address. Use that address from WSL; WSL's loopback is not assumed to be the
Windows listener. The HUD binds loopback and its Tailscale addresses, never
a wildcard address.

If the card reports blocked tailnet access, the Windows owner can use the
explicit firewall helper for the executable being reached:

```powershell
& "$env:LOCALAPPDATA\Programs\tze_hud\tze_hud.exe" --allow-remote
# Undo that helper's change for the same executable:
& "$env:LOCALAPPDATA\Programs\tze_hud\tze_hud.exe" --disallow-remote
```

For an in-place build, use its actual executable path instead. Match any
custom MCP/gRPC port flags. The helper may request UAC approval; ordinary
startup does not change firewall policy. It creates a program-scoped tailnet
rule, not a general network bind. Read the
[remote-agent guidance](operations/windows-install.md#remote-agents) for
BLOCK-rule handling and undo limits.

## 2. Pair from WSL

Open a fresh WSL terminal in this checkout. Replace the placeholders below
with the host address and the current six-digit code from the HUD card:

```bash
python3 .claude/skills/user-test/scripts/hud_pair.py \
  --host '<Windows-Tailscale-address[:MCP-port]>' \
  --code '<six-digit-code-from-card>' --agent claude
```

The code expires after five minutes and works once. To obtain another code,
focus the HUD and press Ctrl+Shift+P, or use `tze_hud.exe --pair` on Windows.

The helper saves the PSK privately in `~/.config/tze-hud/<host>.psk` and stores
nonsecret endpoint metadata for the actual MCP port. It never prints the key.
The HUD stores only its SHA-256 in `agents.toml` beside the active config.
Pairing updates the running runtime without a restart. Do not copy the key
into `.mcp.json`, command arguments, transcripts, screenshots or Git.
Admin access is optional and is not needed for portal projection.

## 3. Connect a fresh agent session

Start Claude Code from this repo in a fresh WSL session and accept its normal
project MCP approval. The tracked `.mcp.json` uses the existing Linux/WSL
stdio adapter to the authenticated HTTP server; it contains no PSK.

- With one paired host and `HUD_HOST` unset, the adapter selects that host.
- An explicit `HUD_HOST` selects a host/port; multiple paired hosts require
  explicit selection. No paired host produces the pairing-command hint.
- Newly paired custom ports come from endpoint metadata. Legacy key-only
  pairs default to 9090; select an old custom port explicitly or re-pair.
  Invalid or stale metadata fails closed.

For explicit selection, set only the nonsecret address in the terminal
launching the client:

```bash
export HUD_HOST='<Windows-Tailscale-address[:MCP-port]>'
```

Confirm fresh authenticated `initialize`/`tools/list` discovery of all five
tools: `hud_surfaces`, `hud_publish`, `hud_hold`, `hud_clear`, and
`hud_input`. Cached tool names alone do not prove a connection.
Local/user MCP entries with the same name can shadow the project entry; use
the client's normal settings controls to resolve that without overwriting
trust or credentials. Other clients must use their supported authenticated
transport; this project entry is specifically for Claude Code.

The Windows executable can print redacted discovery information without
starting another window or exposing the PSK:

```powershell
& "$env:LOCALAPPDATA\Programs\tze_hud\tze_hud.exe" --print-attach-info
```

Use the actual dev path and `--config` for an in-place build. See the
[projection skill's MCP client instructions](../.claude/skills/hud-projection/SKILL.md#mcp-client)
for the current adapter and selection behavior.

## 4. Attach the session to a portal

In that connected session, say **“project this session to the HUD”**. The
[hud-projection skill](../.claude/skills/hud-projection/SKILL.md) uses a stable
`portal:<id>` owned by the paired agent:

1. Its first `hud_publish` attaches the portal and publishes session output.
2. Publish a prompt with `expects_reply: true`, then type a reply on the HUD.
   The composer echoes locally; `hud_input` retrieves the reply and its ID.
3. Acknowledge that input through the skill's normal acknowledgement path.
4. `hud_clear` detaches the portal when the session is finished.

Watch the actual output and typed reply on the Windows overlay. An HTTP
success alone does not establish that they appeared or that input worked.

Outside an MCP client, the existing
[portal client](../.claude/skills/hud-projection/scripts/portal_client.py)
uses explicit `HUD_HOST` and the same private paired-key file for publish,
poll, acknowledge and clear. It does not start a replacement runtime.

## 5. Run the demo against that same HUD

The default installed config has the demo zones and widgets. An in-place
build should use `app/tze_hud_app/config/production.toml`. In WSL, replace
each placeholder with the paired host, ports and key filename:

```bash
TZE_HUD_PSK_FILE="$HOME/.config/tze-hud/<host>.psk" \
  cargo run -p poc_demo -- all \
  --mcp '<Windows-Tailscale-address>:9090' \
  --grpc '<Windows-Tailscale-address>:50051' --agent claude
```

Use the actual ports if they differ. `poc_demo` reads the PSK from the file;
do not put its value in the environment or command. It drives the real
zones, widgets and resident tile and prints the portal instructions rather
than simulating a session. The `override-hang` stage waits for you to press
close or the safe-mode chord; its wait alone does not verify an override.
See [Demo](../README.md#demo) and [POC acceptance](scope.md#poc-acceptance).

## Troubleshooting

| Symptom | Next step |
|---|---|
| Pairing connection times out | Use the card's actual Tailscale address and MCP port; check the Windows card's firewall diagnosis and the runbook. |
| Pairing code rejected | Request a fresh card; codes expire and are single-use. |
| No host or multiple hosts selected | Pair first or select the intended host with `HUD_HOST`; retain the private key and matching endpoint metadata. |
| Stale metadata or unsafe key mode | Follow the adapter's refusal and re-pair privately; never print the key to debug it. |
| Authenticated call refused | Use the key for that host and the same agent; its `allow` list governs tools. See [identity and permissions](api.md#identity-and-permissions). |
| Dev executable exits as already running | Stop the previous Windows instance before the explicit in-place launch. |
| No overlay or typed reply visible | Verify the actual interactive Windows desktop, portal attachment and input acknowledgement; headless checks cannot prove these. |

Continue with the [API](api.md), [scope](scope.md), and
[Windows operation runbook](operations/windows-install.md).
