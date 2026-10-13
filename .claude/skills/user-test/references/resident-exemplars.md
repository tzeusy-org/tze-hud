# Resident gRPC Exemplar Scenarios

The Presence Card scenario exercises resident tiles through gRPC, using the
existing paired key selected by `HUD_HOST`. It does not pair, install, update,
or restart the HUD. The agent needs `allow = ["tiles"]` or `["*"]`; `tiles`
includes image upload, scene topology and input events. Admin access is separate
and is only needed if the operator uses the screenshot tools in the main skill.

## Run against an already-paired HUD

From the repository root in Linux/WSL, replace `<paired-host>` with the hostname
used when pairing:

```bash
HUD_HOST=<paired-host> uv run .claude/skills/user-test/scripts/presence_card_exemplar.py --transcript-out test_results/presence-card-latest.json
```

`uv run` reads the script's PEP 723 header and installs `grpcio`, `protobuf`,
`Pillow` and `blake3` in an isolated environment. For an existing venv instead:

```bash
python3 -m venv .venv
.venv/bin/python -m pip install -r scripts/requirements-dev.txt
HUD_HOST=<paired-host> .venv/bin/python .claude/skills/user-test/scripts/presence_card_exemplar.py --transcript-out test_results/presence-card-latest.json
```

The PSK stays in the existing private `~/.config/tze-hud/<hostname>.psk` file.
Do not print it or pass it on the command line. By default the client sends no
claimed agent ID and the HUD derives identity from the PSK. `--agent <paired-id>`
optionally asserts that identity; a mismatch returns `AUTH_FAILED`. The three
labels `agent-alpha`, `agent-beta` and `agent-gamma` are cosmetic card labels,
not three paired identities. All three streams share the authoritative
`SessionEstablished.namespace`, with separate leases and lifecycles.

The client defaults `GRPC_DNS_RESOLVER` to `native` **before importing gRPC**,
allowing WSL's native resolver to handle Tailscale short hostnames. An explicitly
set resolver wins. If an existing `ares` override prevents connection, unset it
or set `GRPC_DNS_RESOLVER=native` for this command. A handshake timeout alone
does not prove DNS failed; check the named phase and the connection separately.

Optional flags:

- `--host` selects the paired host instead of `HUD_HOST`.
- `--target` overrides the gRPC host:port; use `--host` to select the paired key
  if that endpoint uses a different hostname.
- `--update-wait-s` defaults to `30` seconds before the first content update.
- `--orphan-grace-s` defaults to `30`; match the deployed HUD's configured grace.
- `--observe-badge-s` defaults to `1.0` seconds for the badge observation pause.
- `--transcript-out` selects the JSON receipt; the command above names its default.

## What the command proves

The script uploads three 32x32 PNG avatars over `ResourceUploadStart`. Each
`ClaimTile` uses a bottom-left placement hint and an inline 13-node card tree:
root, sheen, accent, avatar plate, avatar, eyebrow, name, body, chip background,
chip text, dismiss background, dismiss text and dismiss hit region. Handshake
plus filled claim take two round trips; the client then sets Capture input mode.
The runtime chooses placement and stacking. Display dimensions come from the
scene snapshot; there is no hardcoded tab height or agent-supplied z-order.

The client retains the returned IDs, then sends `UpdateNodeContent` for the
body and chip. `NodeProto.id` is little-endian; mutation references and
`RequestResult.ids` use big-endian bytes. The sequential `set_tile_root` and
`add_node` helpers also return acknowledged IDs. Never use an outgoing
`NodeProto.id` as an `AddNode.parent_id` or update target.

Gamma stops heartbeats and drops its transport **without `SessionClose` or
`Clear`**. Alpha and beta keep sending heartbeats and receive further update
acknowledgements after the grace wait. At the end the script clears only its
known surviving tiles and closes their streams. Gamma is left to runtime
reclaim. A fresh invocation creates new sessions; there is no automatic resume
or replay. Structural operations and exemplar updates do not automatically
retry an uncertain acknowledgement, even if the shared client's retry policy
was configured elsewhere.

Stdout contains JSON step events plus brief client diagnostics. The JSON file
retains partial progress on error, including the endpoint, paired namespace,
card labels, lease/tile/node IDs and accepted update target/text. `status` is
`started`, `acknowledged`, `transport_dropped`, `observation_pending`,
`protocol_complete`, or `failed`. Every event's `visual_observation` is
`unverified`: an acknowledgement establishes accepted protocol work, not pixels.
Failures exit nonzero and do not mark the protocol complete. Ctrl+C attempts
cleanup and records interruption; an unknown remote outcome is not replayed.

## Owner-host visual acceptance

Offline Python tests and a completed command are separate from these Windows
observations. Record a screenshot or owner observation for each step:

| Step | Owner should observe |
|---|---|
| Create | Three stacked cards, each with visible name/body/avatar |
| Update | Body and chip text change after the first wait |
| Hard disconnect | Gamma gets a disconnection badge; alpha and beta remain live |
| Grace | Gamma disappears after grace, with survivors in place |
| Final cleanup | This run's surviving cards disappear; unrelated surfaces remain |

Without these observations, visible text changes, the badge and reclamation
remain pending. Successful body updates with missing or unchanged name/body
pixels belong to `hud-pcscu`; a grey avatar after successful image upload belongs
to `hud-o5elz`. Preserve the accepted IDs and expected text with that rendering
report. Do not report those cases as a visual pass or change rendering in this
tooling scenario.
