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

# Quickstart — portal as your primary LLM interface

New here and just want an LLM session projecting onto your own screen? Start with
**[`docs/QUICKSTART.md`](docs/QUICKSTART.md)** (<10 minutes). One command from the
repo root:

```bash
cargo build --bin tze_hud --release
scripts/quickstart.sh --window-mode overlay   # scaffolds config + PSK, prints ATTACH INFO, launches
```

`scripts/quickstart.sh --print-attach-info` prints the MCP endpoint and a
redacted client-config template **without** opening a window. To create a
ready-to-use mode-600 config with the bearer already wired, run
`scripts/quickstart.sh --emit-mcp-config=tze-hud.mcp.json`. The rest of this
README is the deeper build/test/deploy reference.

---

# Build/Test/Run Commands

The rest of this README is command-first and focused on four workflows:

1. Build on Linux and Windows
2. Build/run on Linux inside TigerVNC and connect from Windows
3. Run all test categories
4. Trigger zone publishing to the server and verify UI-control/overlay path

## Required Gates / CI

CI (`.github/workflows/ci.yml`) runs the gates below on every PR; `just ci`
reproduces the blocking set locally except `clippy-windows-gnu` and `cargo-deny`
(requires [just](https://github.com/casey/just)).
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
| `idle-efficiency-checker` | `check` | fail-closed idle artifact contract tests |
| `test-gpu` | — | GPU subset of `test` only (compositor + `pixel_readback`), llvmpipe-pinned with timeouts; already covered by `test`, so not a separate `just ci` step |
| — | `clippy-windows-gnu` | clippy on the `x86_64-pc-windows-gnu` target for the crates carrying `cfg(windows)` code |
| — | `cargo-deny` | dependency/advisory policy (`deny.toml`) |

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
- `vertical_slice` (`examples/vertical_slice/`): Development reference showing scene/lease/zone publish semantics. **Not** intended for operations or remote deployment.
- `benchmark` (`examples/benchmark/`): Performance profiling reference.
- `render_artifacts` (`examples/render_artifacts/`): GPU rendering artifact generation.

**Rule**: Automation and cross-machine workflows MUST target the canonical app binary, not demo binaries.

## 1) Build on Linux / Windows

### Linux (Ubuntu/Debian) - Native Build

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

**Runtime usage:**

```bash
# Fullscreen (default) with config
./tze_hud --config tze_hud.toml

# Overlay with explicit endpoint settings
./tze_hud --config tze_hud.toml --window-mode overlay --grpc-port 50051 --mcp-port 9090

# Or on Windows with prebuilt binary
.\tze_hud.exe --config tze_hud.toml
```

For Windows deployment automation, see [Cross-Machine Deployment](#cross-machine-deployment) below.

CI-backed checks for the canonical path:
- `app/tze_hud_app/tests/canonical_config_schema.rs` validates `app/tze_hud_app/config/production.toml`.
- CI `test-unit` job runs `cargo test --workspace --all-targets --exclude integration`, which includes that test.

## 1.2) Cross-Machine Deployment

The canonical `tze_hud` binary is designed for automated cross-machine deployment using SSH+SCP.

### Prerequisites

- Linux host with built canonical app binary for Windows target
- Windows remote host reachable via SSH (tailnet or VPN)
- SSH key-based authentication configured

### Deployment Workflow

**Step 1: Build Windows artifact on Linux**

```bash
# From repo root
cargo build --bin tze_hud --release --target x86_64-pc-windows-gnu
WINDOWS_EXE="target/x86_64-pc-windows-gnu/release/tze_hud.exe"
echo "Artifact ready: $WINDOWS_EXE"
```

**Step 2: Deploy and launch via user-test automation**

See [Cross-Machine Validation via user-test](#cross-machine-validation-via-user-test) below for the full automation script.

**Key deployment points:**
1. Verify SSH connectivity BEFORE deploying
2. Build or locate prebuilt canonical app `.exe`
3. Use deployment script to copy and launch on Windows
4. **Verify MCP HTTP reachability gate BEFORE publish assertions**
5. Publish zone test messages via MCP HTTP once endpoint is live

### Deployment Artifact Identity

For automation purposes, the canonical app binary produces:

- **Artifact name**: `tze_hud.exe` (stable, deterministic)
- **Linux build output**: `target/x86_64-pc-windows-gnu/release/tze_hud.exe`
- **Windows remote path**: `C:\tze_hud\tze_hud.exe` (default deployment location)
- **Checksum / provenance**: the `release-provenance` workflow
  (`.github/workflows/release-provenance.yml`) cross-builds `tze_hud.exe` and
  publishes a pipeline-generated `tze_hud.exe.sha256` alongside it as a workflow
  artifact. Deployment automation MUST verify the artifact against the published
  checksum (`sha256sum -c tze_hud.exe.sha256`) before activation. (Signing is
  optional/deferred for v1.)

## 2) Linux + TigerVNC, then connect from Windows

### On Linux host (start VNC desktop)

```bash
# Install VNC server + lightweight desktop
sudo apt update
sudo apt install -y tigervnc-standalone-server tigervnc-common xfce4 xfce4-goodies

# Set VNC password (first run)
vncpasswd

# Create VNC startup script
cat > ~/.vnc/xstartup <<'XEOF'
#!/bin/sh
unset SESSION_MANAGER
unset DBUS_SESSION_BUS_ADDRESS
startxfce4 &
XEOF
chmod +x ~/.vnc/xstartup

# Start VNC display :1 (TCP 5901)
vncserver :1 -localhost no -geometry 1920x1080 -depth 24

# Run the windowed demo inside that display
export DISPLAY=:1
cargo run -p vertical_slice
```

### From Windows client

```powershell
# Secure option: tunnel VNC over SSH
ssh -L 5901:localhost:5901 <linux-user>@<linux-host>
```

Then open TigerVNC Viewer and connect to:

```text
localhost:5901
```

(Direct LAN option without tunnel: `<linux-host>:5901`.)

To stop VNC on Linux:

```bash
vncserver -kill :1
```

## 3) Run tests (all categories)

### Fast baseline (workspace tests except `integration` package)

```bash
cargo test --workspace --all-targets --exclude integration
```

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

The `vertical_slice` example is a **reference implementation** for understanding scene/lease/zone semantics.
**It is NOT intended for production operations or remote deployment.**

Run the demo locally for development/testing:

```bash
cargo run -p vertical_slice
```

You should see logs for:
- session + lease handshake,
- tile creation and hit-region input handling,
- zone publishes (`status-bar`, `notification-area`).

Headless variant (for server-side environments):

```bash
cargo run -p vertical_slice -- --headless
```

**For operational workflows**, use the **canonical runtime app binary** instead. See [Cross-Machine Deployment](#cross-machine-deployment) and [Cross-Machine Validation](#cross-machine-validation-via-user-test).

## 5) Cross-Machine Validation via user-test

For automated cross-machine deployment and MCP publish validation, use the `user-test` skill workflow.

### Workflow Overview

1. **Build canonical app for Windows target** (Linux cross-compile)
2. **Deploy to Windows** via SSH+SCP
3. **MCP Reachability Gate** - Verify endpoint is live before publish
4. **Publish test zones** - Validate MCP authentication and zone semantics
5. **Diagnostics** - Structured failure output on endpoint/auth mismatches

### Quick Start

**Prerequisites:**
- `~/.ssh/hud-ssh-key` SSH key (or override via `SSH_OPTS`)
- Windows host: `windows-host.example` (or override `--win-host`)
- Windows SSH user: `hud-user` (or override `--win-user`)
- MCP test PSK in environment: `export MCP_TEST_PSK="..."`

**Step 1: Verify SSH connectivity**

```bash
ssh -o BatchMode=yes -o IdentitiesOnly=yes -i ~/.ssh/hud-ssh-key \
  hud-user@windows-host.example "whoami"
```

Must return `hud-user`. Do not proceed without successful key auth.

**Step 2: Build canonical app for Windows**

```bash
cargo build --bin tze_hud --release --target x86_64-pc-windows-gnu
FULL_APP_EXE="target/x86_64-pc-windows-gnu/release/tze_hud.exe"
```

**Step 3: Deploy and launch with MCP reachability gate**

```bash
# From repo root
WIN_USER=hud-user \
SSH_OPTS='-i ~/.ssh/hud-ssh-key -o IdentitiesOnly=yes -o BatchMode=yes' \
.claude/skills/user-test/scripts/deploy_windows_hud.sh \
  --win-host windows-host.example \
  --full-app-exe "$FULL_APP_EXE" \
  --launch-mode auto \
  --tail
```

**Expected output:**
- Remote exe path: `C:\tze_hud\tze_hud.exe`
- Launcher logs tail (remote)

**Step 4: Verify MCP endpoint reachability (MCP Reachability Gate)**

```bash
# Test MCP HTTP endpoint
curl -s -X POST http://windows-host.example:9090/mcp \
  -H "Content-Type: application/json" \
  -H "Authorization: Bearer $MCP_TEST_PSK" \
  -d '{"jsonrpc":"2.0","method":"tools/list","params":{},"id":1}' | jq .
```

If the endpoint is unreachable, stop and investigate launch logs. Do not proceed to publish.

**Step 5: Publish test zone messages via MCP HTTP**

```bash
# Create test message JSON
cat > /tmp/hud-test-zones.json <<'EOF'
[
  {
    "zone": "status-bar",
    "content": {"entries": {"deploy": "live"}},
    "key": "deploy-status"
  },
  {
    "zone": "notification-area",
    "content": {"title": "MCP", "body": "publish validation successful"},
    "ttl_ms": 60000
  }
]
EOF

# Publish via MCP HTTP
python3 .claude/skills/user-test/scripts/publish_zone_batch.py \
  --url "http://windows-host.example:9090/mcp" \
  --psk-env MCP_TEST_PSK \
  --messages-file /tmp/hud-test-zones.json
```

### Troubleshooting

**Symptom**: SSH connectivity fails at step 1
- Verify `~/.ssh/hud-ssh-key` exists and has correct permissions
- Check Windows SSH server is running
- Verify firewall rules allow SSH (port 22)

**Symptom**: Deployment succeeds but MCP endpoint unreachable
- Check Windows target's `C:\tze_hud\logs\hud.stdout.log` and `hud.stderr.log`
- Verify MCP HTTP endpoint config in runtime config file
- Verify firewall allows HTTP (port 9090 by default) from Linux host

**Symptom**: MCP publish request rejected with 401/403
- Verify `MCP_TEST_PSK` environment variable is set
- Verify PSK matches value in Windows runtime config
- Check MCP authentication enforcement in runtime logs

### Debugging Tips

**Tail launcher logs on Windows:**

```bash
ssh -i ~/.ssh/hud-ssh-key hud-user@windows-host.example \
  "powershell -Command \"Get-Content -Path 'C:\\tze_hud\\logs\\hud.launcher.log' -Tail 50 -Wait\""
```

**Stop running runtime and check process state:**

```bash
ssh -i ~/.ssh/hud-ssh-key hud-user@windows-host.example \
  "powershell -Command \"Get-Process tze_hud -ErrorAction SilentlyContinue | Stop-Process -Force\""
```

**Verify artifact was copied:**

```bash
ssh -i ~/.ssh/hud-ssh-key hud-user@windows-host.example \
  "powershell -Command \"Get-Item 'C:\\tze_hud\\tze_hud.exe' | Select-Object FullName, Length, LastWriteTime\""
```
