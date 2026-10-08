# tze_hud

An MCP/gRPC layer that lets models manage the real-estate lifecycle of a HUD
over the user's screen: claim space, fill it, keep it current, take input,
and give it back. Surfaces: a session portal (live output plus a reply
composer), ambient zones (one MCP call to put text on screen), agent-owned
tiles (gRPC), and SVG widgets, rendered on a Windows overlay. The runtime
owns layout, styling, and rendering; models only state intent.

See [`docs/vision.md`](docs/vision.md) for what this is and isn't, and
[`docs/scope.md`](docs/scope.md) for the in-progress scope reset.

---

# Quickstart — a Windows HUD with an agent in WSL

Start with **[docs/QUICKSTART.md](docs/QUICKSTART.md)**: install the Windows
`dev` release (or cross-build with `just build-windows`), pair from the HUD
card, connect the current MCP client, and project a real session onto the
overlay. The runtime runs on Windows; Linux/WSL provides headless checks,
agent tooling and Windows cross-builds. The rest of this README is the
build/test reference.

---

# Build/Test/Run Commands

The rest of this README covers headless Linux checks, Windows builds,
test categories, and publishing to the Windows overlay.

## Required Gates / CI

CI (`.github/workflows/ci.yml`) runs the gates below on every PR; `just ci`
reproduces the blocking set locally (requires [just](https://github.com/casey/just)).
`clippy-windows-gnu`, `cargo-deny` and `overlay-harness-contract` are tool-gated:
if the tool (windows-gnu target + mingw, cargo-deny, `pwsh`) is missing they print
`SKIPPED: <reason>` and pass, so a green local `just ci` does not prove them;
CI always runs them.
Each local recipe maps to a CI job:

| Local recipe (`just …`) | CI job | Checks |
|---|---|---|
| `check` | `check` | `cargo check` (fast compile gate) |
| `fmt` | `fmt` | `cargo fmt --check` |
| `clippy` | `clippy` | `cargo clippy --workspace --all-targets -D warnings` |
| `test` | `test-unit` | workspace tests (excludes `integration`), including the GPU compositor tests and `pixel_readback` on Mesa llvmpipe |
| `test-integration` | `test-integration` | headless integration suites |
| `token-footprint` | `test-integration` | deterministic LLM-facing token-footprint gate |
| `test-python` | `user-test-python-suite` | pure-Python suites (pytest + `scripts/ci` unittest) |
| `production-boot` | `production-boot-vertical-slice` | vertical-slice production-config boot |
| `canonical-app-boot` | `canonical-app-production-boot` | canonical app production-config boot |
| `dev-mode-guard` | `dev-mode-guard` | dev-mode not enabled in any package's default-build dependency closure (shipped binary included) |
| `deps-unused` | `check` | `cargo machete`: unused dependencies |
| `overlay-harness-contract` | `check` | pwsh fullscreen-vs-overlay harness contract test |
| `idle-efficiency-checker` | `check` | fail-closed idle artifact contract tests |
| `test-gpu` | — | GPU subset of `test` only (compositor + `pixel_readback`), llvmpipe-pinned with timeouts; already covered by `test`, so not a separate `just ci` step |
| `clippy-windows-gnu` | `clippy-windows-gnu` | clippy on the `x86_64-pc-windows-gnu` target for the crates carrying `cfg(windows)` code |
| `cargo-deny` | `cargo-deny` | dependency/advisory policy (`deny.toml`) |

Slower suites run weekly, on demand, or on PRs labelled `perf-assert`, never as
merge gates: `perf-budget.yml` (Windows performance budget, constrained-envelope
budget) and `perf-assert.yml` (p99 timing asserts).

The toolchain is pinned in `rust-toolchain.toml` (Rust 1.88, matching CI and the
`glyphon 0.8.x` / `wgpu 24.x` co-pin).

## Overview: Canonical Runtime App vs. Demo Binaries

**Important:** This project distinguishes between the **canonical runtime application binary** and demo/reference binaries.

### Canonical Runtime App Binary
- **Purpose**: Production-ready runtime executable for cross-machine deployment and MCP publishing operations.
- **Binary name**: `tze_hud` (from the `tze_hud_app` crate, canonical application binary, part of a non-demo binary target in Cargo workspace)
- **Windows artifact**: `target/x86_64-pc-windows-gnu/release/tze_hud.exe`
- **Configuration**: Supports TOML configuration file with windowed display settings and network endpoint configuration.
- **Network support**: Includes full `NetworkRuntime` with MCP HTTP listener lifecycle in windowed mode.
- **Use case**: Remote deployment, cross-machine validation, automated publish workflows.

Live media (GStreamer/WebRTC) is out of scope; see `docs/vision.md`.

### Demo and Reference Binaries
- `poc_demo` (`examples/poc_demo/`): The POC demo client; see [Demo](#demo). It drives a *running* `tze_hud`, it is not a runtime itself.
- `vertical_slice` (`examples/vertical_slice/`): Development reference: a headless resident gRPC agent running the lifecycle verbs (`ClaimTile`, `Publish`, `Hold`, `Clear`). **Not** intended for operations or remote deployment.
- `benchmark` (`examples/benchmark/`): Performance profiling reference.
- `render_artifacts` (`examples/render_artifacts/`): GPU rendering artifact generation.

**Rule**: Automation and cross-machine workflows MUST target the canonical app binary, not demo binaries.

## Demo

`poc_demo` drives a live app (MCP on `127.0.0.1:9090`, gRPC on `127.0.0.1:50051`)
through each lifecycle stage in one or two calls, narrates every stage in
`zone:subtitle`, and prints the model-visible tokens each MCP call costs
(compare `docs/api.md` "Token budgets"). Run the app with
`app/tze_hud_app/config/production.toml` (it has the zones and the
`main-gauge`/`main-progress` widgets), pair an agent
(`docs/operations/windows-install.md`), and hand its PSK over by environment
or file. A PSK is never printed.

```bash
export TZE_HUD_PSK_FILE=~/.config/tze_hud/claude.psk   # or TZE_HUD_PSK; a saved POST /pair reply works
# optional: TZE_HUD_TILE_PSK(_FILE) for a separate resident-tile agent, plus --agent <its id>
cargo run -p poc_demo -- zones           # notification TTL, delay_ms, action press -> hud_input
cargo run -p poc_demo -- widgets         # typed gauge/progress updates
cargo run -p poc_demo -- tile            # ClaimTile(placement+root), MutationBatch, Hold, Reclaimed
cargo run -p poc_demo -- override-hang   # claim a tile, stop reading; press close or the safe-mode chord
cargo run -p poc_demo -- snapshot        # print `tiles <n>` from a fresh session's SceneSnapshot (CI uses it)
cargo run -p poc_demo -- all             # all of the above, with the portal step in between
```

`override-hang` only idles for `--human-wait-s` with its tile claimed, not reading the stream and not flooding the session; it sets the scene for you to press close or the safe-mode chord and does not itself verify the override (`poc_acceptance`'s `HungAgent` does).

`--mcp`, `--grpc`, `--agent` (the id the tile PSK was paired as, default
`claude`), `--pace-ms` and `--human-wait-s` are in `--help`. Windows is the
target; from another machine point `--mcp`/`--grpc` at the HUD's Tailscale
address.

**Portal step (not simulated).** The portal demo is a real Claude Code session:
run the `hud-projection` skill (`.claude/skills/hud-projection/SKILL.md`). Its
first `hud_publish` to `portal:<id>` attaches, typed replies come back through
`hud_input`, and `hud_clear` detaches. `poc_demo all` prints this instead of
faking it.

`cargo test -p poc_demo` runs the zones, widgets and tile stages against the
GPU-free headless runtime.

## 1) Build on Linux / Windows

### Linux/WSL (Ubuntu/Debian) - Headless checks and cross-build prerequisites

```bash
# System deps (Rust toolchain deps + protobuf compiler + common windowing libs)
sudo apt update
sudo apt install -y \
  build-essential pkg-config protobuf-compiler \
  libx11-dev libxrandr-dev libxi-dev libxcursor-dev libxinerama-dev \
  libxkbcommon-dev libwayland-dev

# Rust toolchain (workspace requires Rust 1.88+)
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source "$HOME/.cargo/env"
rustup toolchain install 1.88.0
rustup default 1.88.0

# Build entire workspace
cargo build --workspace
cargo build --workspace --release

# Build canonical runtime app binary only
cargo build --bin tze_hud --release
```

### Linux to Windows Cross-Compile (for deployment automation)

```bash
# Install Windows toolchain target
rustup target add x86_64-pc-windows-gnu

# Install MinGW toolchain (for cross-compilation)
sudo apt install -y mingw-w64

# Build canonical app for Windows target
cargo build --bin tze_hud --release --target x86_64-pc-windows-gnu

# Output artifact path:
# target/x86_64-pc-windows-gnu/release/tze_hud.exe
```

### Windows (PowerShell) - Native Build

```powershell
# Install toolchain/deps (run in elevated PowerShell)
winget install -e --id Rustlang.Rustup
winget install -e --id ProtocolBuffers.Protobuf
winget install -e --id Microsoft.VisualStudio.2022.BuildTools

# Rust toolchain
rustup toolchain install 1.88.0-x86_64-pc-windows-msvc
rustup default 1.88.0-x86_64-pc-windows-msvc

# Build (from repo root)
cargo build --workspace
cargo build --workspace --release

# Build canonical runtime app binary only
cargo build --bin tze_hud --release
```

If `cl.exe` is not found, run the build in **Developer PowerShell for VS 2022**.

**Output artifact path (Windows):**
```
target\x86_64-pc-windows-msvc\release\tze_hud.exe
```

## 1.1) Configuration for Canonical Runtime App

The canonical `tze_hud` binary uses the `TzeHudConfig` loader schema.

Minimal valid config requirements:
- `[runtime]` with a `profile` field
- at least one `[[tabs]]` entry

Schema version and compatibility policy:
- An optional top-level `schema_version` (integer) declares the config schema the
  document targets.
- **Absent** → treated as the current supported version, so existing v1 configs
  load unchanged.
- **Within the supported range** → loads and proceeds to normal validation.
- **Newer than the runtime supports** → fail-closed startup error
  `CONFIG_SCHEMA_VERSION_UNSUPPORTED` naming the supported range; no port is bound.

Canonical operator config path:
- `app/tze_hud_app/config/production.toml` (deploy this as `tze_hud.toml` beside the binary)

Window mode and endpoint ports are controlled via CLI flags / environment
variables (`--window-mode`, `--grpc-port`, `--mcp-port`) rather than
legacy config tables. Agents are paired in `agents.toml` beside the config,
which stores only the SHA-256 of each agent's PSK (`docs/api.md`). Legacy `[display]`/`[network]` tables are not part of the
current loader schema.

Canonical startup is fail-closed:
- missing or unreadable config is a hard startup error
- invalid loader-schema config is a hard startup error
- an unreadable or invalid `agents.toml` is a hard startup error

Development-only escape hatch:
- `TZE_HUD_DEV_ALLOW_INSECURE_STARTUP=1` is honored only in debug builds
- release builds ignore this override; do not use it for production operators

**Minimal schema example** (`tze_hud.toml`):

```toml
[runtime]
profile = "full-display"

[[tabs]]
name = "Main"
default_tab = true
```

**Windows runtime usage** (PowerShell, explicit arguments for an in-place build):

```powershell
.\tze_hud.exe --config tze_hud.toml --window-mode overlay

# Overlay with explicit endpoint settings
.\tze_hud.exe --config tze_hud.toml --window-mode overlay --grpc-port 50051 --mcp-port 9090
```

To install on Windows, see [Windows install and operation](#12-windows-install-and-operation) below.

CI-backed checks for the canonical path:
- `app/tze_hud_app/tests/canonical_config_schema.rs` validates `app/tze_hud_app/config/production.toml`.
- CI `test-unit` job runs pinned Nextest0.9.114 with `cargo nextest run --workspace --all-targets --exclude integration --features tze_hud_runtime/dev-mode`, which includes that test.

## 1.2) Windows install and operation

CI builds a signed `tze_hud.exe` for every merge to `main` (`dev`) and every
`v*` tag. Download it, double-click it, and pair an agent from the code on the
HUD card. Install, pairing, update, restart, uninstall, and the `/admin`
operator endpoints are in
[`docs/operations/windows-install.md`](docs/operations/windows-install.md).

The canonical binary is `tze_hud.exe` (built natively on Windows MSVC, one
file). Release assets include `tze_hud.exe.sha256` and `tze_hud.exe.minisig`;
verify before running (see the runbook).

## 3) Run tests (all categories)

### Fast baseline (workspace tests except `integration` package)

Install pinned `cargo-nextest`0.9.114 (`just bootstrap` on Linux/WSL, or
`cargo install --locked --version 0.9.114 cargo-nextest`) and verify
`cargo nextest --version`. Then run the software/required-GPU harness:

```bash
just test
```

CI and `just test` use the same runner arguments:
`cargo nextest run --workspace --all-targets --exclude integration --features tze_hud_runtime/dev-mode`.
The default profile assigns GPU-owning packages' non-bench tests to one Nextest
test group with capacity one, conservatively including some CPU cases. Other
CPU packages and the five known CPU Criterion targets remain parallel. The
separate integration and focused libtest recipes below remain unchanged.

### Scene/property tests

```bash
cargo test -p tze_hud_scene --test proptest_invariants -- --nocapture
cargo test -p tze_hud_scene --test fuzz_scene_graph -- --nocapture
```

### Protocol/session tests

```bash
cargo test -p tze_hud_protocol -- --nocapture
```

### Runtime/render validation tests

```bash
just test-gpu
```

### Integration tests

```bash
cargo test -p integration --tests
```

Multi-agent integration tests:

```bash
cargo test -p integration --test multi_agent -- --nocapture
```

## 4) Trigger publish-to-server + UI-control/overlay checks

### A. Explicit server publish path (gRPC session server)

This test sends a zone `Publish` to the session server and checks its `RequestResult`:

```bash
cargo test -p tze_hud_protocol test_durable_zone_publish_result -- --nocapture
cargo test -p tze_hud_protocol test_ephemeral_zone_no_publish_result -- --nocapture
```

### B. Development/Reference Demo (vertical_slice - NOT for operations)

The `vertical_slice` example is a **reference implementation** of a resident gRPC agent (see `docs/api.md`).
**It is NOT intended for production operations or remote deployment.**

Run it locally for development/testing (headless; no display needed):

```bash
cargo run -p vertical_slice
```

You should see logs for:
- the session handshake,
- `ClaimTile` (a filled tile in one round trip),
- a `Publish` to `zone:status-bar`,
- `Hold` and `Clear` of the tile.

**For operational workflows**, use the **canonical runtime app binary** instead. See [Windows install and operation](#12-windows-install-and-operation) and the [Windows-first Quickstart](docs/QUICKSTART.md).
