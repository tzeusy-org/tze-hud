# tze_hud

An MCP/gRPC layer that lets models manage the real-estate lifecycle of a HUD
over the user's screen: claim space, fill it, keep it current, take input,
and give it back. Surfaces: a session portal (live output plus a reply
composer), ambient zones (one MCP call to put text on screen),
and SVG widgets, rendered on a Windows overlay. The runtime
owns layout, styling, and rendering; models only state intent.

- [vision](docs/vision.md): intent and boundaries
- [api](docs/api.md): the MCP and gRPC contracts
- [scope](docs/scope.md): tranche status and open owner checks
- [architecture](docs/architecture.md): which code owns what
- [AGENTS.md](AGENTS.md): `just` recipes and contributor gotchas
- [Windows development](docs/development/windows.md): native Windows and WSL setup

## Install

Start with **[docs/QUICKSTART.md](docs/QUICKSTART.md)**: install the Windows
`dev` release, pair an agent from the code on the HUD card, connect the MCP
client, and project a real session onto the overlay.

[Windows CI](.github/workflows/windows.yml) publishes a signed `tze_hud.exe`
after successful non-docs pushes to `main` (`dev`) and on `v*` tags; docs-only
pushes skip it, so `dev` can lag the latest commit. Release assets include
`tze_hud.exe.sha256` and `tze_hud.exe.minisig`; verify before running.
Install, pairing, update, restart, uninstall and the `/admin` operator
endpoints are in the [Windows install runbook](docs/operations/windows-install.md).

The runtime runs on Windows. Linux/WSL provides headless checks, agent
tooling and Windows cross-builds.

## Build from source

The toolchain is pinned in `rust-toolchain.toml` (Rust 1.88). Protocol
generation uses bundled protoc 31.1 (`protoc-bin-vendored =3.2.0`) fetched by
Cargo; no system protoc is needed.

Linux/WSL (Ubuntu/Debian), for headless checks and the Windows cross-build
(needs [just](https://github.com/casey/just)):

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
just bootstrap        # apt packages (incl. mingw-w64), nextest, Python venv, pre-push hook
just build-windows    # target/x86_64-pc-windows-gnu/release/tze_hud.exe
just ci               # the blocking CI gates, locally
```

Windows (PowerShell), native MSVC build:

```powershell
winget install -e --id Rustlang.Rustup
winget install -e --id Microsoft.VisualStudio.2022.BuildTools
cargo build --bin tze_hud --release   # target\release\tze_hud.exe
```

If `cl.exe` is not found, build in **Developer PowerShell for VS 2022**.

## Configuration

`app/tze_hud_app/config/production.toml` is the canonical config (deployed as
`tze_hud.toml` beside the binary). Window mode and ports are flags
(`--window-mode`, `--grpc-port`, `--mcp-port`). Agents are paired into
`agents.toml` beside the config, which stores only each PSK's SHA-256 and its
`allow` list. Startup is fail-closed: a missing, unreadable or invalid config,
an unreadable or invalid `agents.toml`, or a `schema_version` newer than the
runtime supports (`CONFIG_SCHEMA_VERSION_UNSUPPORTED`) stops startup before
any port binds.
`TZE_HUD_DEV_ALLOW_INSECURE_STARTUP=1` relaxes that in debug builds only.

```powershell
.\tze_hud.exe --config tze_hud.toml --window-mode overlay --grpc-port 50051 --mcp-port 9090
```

## Demo

`poc_demo` drives a live app (MCP on `127.0.0.1:9090`)
through each lifecycle stage in one or two calls, narrates every stage in
`zone:subtitle`, and prints the model-visible tokens each MCP call costs
(compare `docs/api.md` "Token budgets"). Run the app with
`app/tze_hud_app/config/production.toml` (it has the zones and the
`main-gauge`/`main-progress` widgets), pair an agent, and hand its PSK over by
environment or file. A PSK is never printed.

```bash
export TZE_HUD_PSK_FILE=~/.config/tze-hud/<host>.psk   # from hud_pair.py; or TZE_HUD_PSK
cargo run -p poc_demo -- zones           # notification TTL, delay_ms, action press -> hud_input
cargo run -p poc_demo -- widgets         # typed gauge/progress updates
cargo run -p poc_demo -- all             # zones, widgets and portal instructions
```

`--mcp`, `--pace-ms` and `--human-wait-s` are in `--help`; from another
machine point `--mcp` at the HUD's Tailscale address. The portal stage is a
real Claude Code session running the `hud-projection` skill: its first
`hud_publish` to `portal:<id>` attaches, typed replies come back through
`hud_input`, and `hud_clear` detaches. `poc_demo all` prints this instead of
faking it. `cargo test -p poc_demo` runs the zones and widgets stages against
the GPU-free headless runtime.

The other examples are development references, never deployment targets:
`vertical_slice` (a headless resident gRPC agent running `Publish`, `Hold`,
`Clear`), `benchmark` and `widget_publish_load_harness` (performance), and
`render_artifacts` (`just render-scene`, offline rendering to PNG).
Automation targets the canonical `tze_hud` binary.
