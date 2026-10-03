# Developing on Windows

How to work on tze_hud from a Windows machine, usually the same machine that
runs the HUD. Windows is the only deployment target, so the runtime builds and
runs natively. What needs care is the tooling around it: `just`, the shell
scripts, Beads, and some Linux-only test lanes. For installing a released
binary rather than building one, see
[`../operations/windows-install.md`](../operations/windows-install.md).

Linux CI (`ci.yml`) is still the merge gate. The Windows CI job (`windows.yml`)
only builds the release exe and boot-smokes it. It does not run `cargo test`,
so a green Windows test run proves less than a green CI.

## One-time setup

Install these and make sure each one is on `PATH`:

| Tool | Why | Notes |
|---|---|---|
| Visual Studio Build Tools, "Desktop development with C++" | MSVC linker, plus `rc.exe` from the Windows SDK | `app/tze_hud_app/build.rs` embeds the DPI manifest with `embed-resource`, which needs the SDK resource compiler |
| rustup | Toolchain | `rust-toolchain.toml` pins 1.88 with rustfmt and clippy. The default host triple must be `x86_64-pc-windows-msvc` |
| `protoc` | `tze_hud_protocol` build | CI uses protobuf v29.3 `win64.zip`. Unzip it and add `bin\` to `PATH`, or set `PROTOC` |
| Git for Windows | Git, plus the `sh` that runs `just` recipes, git hooks, and Claude Code's Bash tool | See [Git settings](#git-settings) |
| `just` | `just ci` and other recipes | `winget install Casey.Just` |
| Python 3 | `scripts/ci/*.py`, skill scripts | See [`python3`](#python3) |
| `gh` | PRs, releases | |
| Tailscale | Reaching the Beads Dolt server | See [Beads](#beads) |
| `bd` | Issue tracking | |

### Git settings

Set these before cloning, or the clone will contain broken files:

```powershell
git config --global core.autocrlf false   # shell scripts with CRLF fail under sh ("$'\r': command not found")
git config --global core.longpaths true   # deep .worktrees\...\target\... paths pass 260 chars
```

The repo has no `.gitattributes`, so line endings depend entirely on
`core.autocrlf`.

`.codex/skills` is a symlink to `../.claude/skills`, and it is the only tracked
symlink. Git checks it out as a symlink only when Windows Developer Mode is on
and `core.symlinks=true` is set. Otherwise it becomes a one-line text file.
That only matters to Codex, and `.claude/skills` is the canonical tree. Don't
commit a change that turns it into a regular file.

Also enable Win32 long paths, which needs admin:

```powershell
New-ItemProperty -Path HKLM:\SYSTEM\CurrentControlSet\Control\FileSystem -Name LongPathsEnabled -Value 1 -PropertyType DWORD -Force
```

### `python3`

The `justfile` and several scripts call `python3`. The python.org installer
provides `python` and `py`, but not `python3`. On a clean machine, `python3`
resolves to the Microsoft Store "App execution alias" stub, which opens the
Store or exits without doing anything. Fix it one of these ways:

- Turn off the `python3.exe` alias (Settings → Apps → Advanced app settings →
  App execution aliases), then make a real `python3` available. For example,
  copy `python.exe` to `python3.exe` in the install directory.
- Or install Python with `winget install Python.Python.3.12`, then check that
  `python3 --version` prints a version.

For `just test-python`, run `pip install grpcio protobuf pillow blake3 pytest`.

### Build speed

Windows Defender real-time scanning slows `cargo` builds a lot. Put the
checkout, or at least `target\` and `%USERPROFILE%\.cargo`, on a Dev Drive.
Alternatively, add Defender exclusions for those paths.

## Building and running the HUD

```powershell
cargo build --release -p tze_hud_app --bin tze_hud
```

This is the same build CI ships. `.cargo/config.toml` links the CRT statically
for `x86_64-pc-windows-msvc`, so the exe needs no VC++ redistributable. A
debug build (`cargo build -p tze_hud_app --bin tze_hud`) is fine for iteration.

To run it, give it a config and an `agents.toml`:

- Copy `app/tze_hud_app/config/production.toml` as the config.
- `[widget_bundles]` paths resolve relative to the config file, so either keep
  the config somewhere that path still points at the repo's bundles, or
  pass `--config` with the repo path.
- `agents.toml` sits beside the config and holds only PSK SHA-256 digests. The
  PowerShell snippet in [`windows-install.md`](../operations/windows-install.md#run)
  generates one.

```powershell
.\target\release\tze_hud.exe --config app\tze_hud_app\config\production.toml --window-mode overlay
.\target\release\tze_hud.exe --print-attach-info   # MCP URL + client config, no window
```

If there is no `--config`, the config is resolved in this order:
`TZE_HUD_CONFIG`, then `.\tze_hud.toml`, then
`%APPDATA%\tze_hud\config.toml`. Agents live in `%APPDATA%\tze_hud\agents.toml`
when no config file resolves. `TZE_HUD_DEV_ALLOW_INSECURE_STARTUP=1` allows a
debug run with no config.

`scripts/quickstart.sh` runs under Git Bash but was written for Linux. On
Windows, the binary's own `--print-attach-info` together with the
`windows-install.md` snippet is the more direct path.

### Runtime behavior specific to Windows

- **Interactive desktop only.** Launch the HUD from a terminal in your own
  desktop session. A process started over SSH, or by a service, cannot reach
  the desktop GPU, and it draws a grey opaque window instead of a transparent
  overlay. That is why the remote-deploy tooling launches through the
  `TzeHudOverlay` scheduled task (`scripts/windows/run_hud.ps1`). Local
  development doesn't need that.
- **Ctrl+C does not stop it.** `tze_hud.exe` is a GUI-subsystem binary. It
  attaches to the parent console for log output, then ignores console
  Ctrl+C (`app/tze_hud_app/src/main.rs`, `ignore_console_ctrl_c`). Stop it with
  `Stop-Process -Name tze_hud`. In fullscreen mode, closing the window also
  works.
- **One HUD at a time.** On startup the runtime takes
  `C:\ProgramData\tze_hud\gpu.lock` (`crates/tze_hud_runtime/src/gpu_lock.rs`).
  It refuses to start while another live process holds the lock, and it
  reclaims locks left by dead PIDs.
- **GPU backend.** The windowed runtime asks for D3D12 or Vulkan.
  `HEADLESS_FORCE_SOFTWARE=1` makes headless adapters fall back to WARP, the
  Windows counterpart of llvmpipe.
- **Shell hotkeys.** Ctrl+Shift+F8 and Ctrl+Shift+F9 cycle monitors.
  Ctrl+Shift+Esc is the safe-mode exit chord, but Windows reserves it for
  Task Manager, so expect Task Manager to open. Whether the HUD also sees the
  chord has not been checked.
- **Frame pacing** raises the timer resolution to 1 ms
  (`timeBeginPeriod`) while the compositor runs. This is expected.
- **SIGHUP config reload is Unix-only**
  (`crates/tze_hud_runtime/src/reload_triggers.rs`). Restart the process to
  pick up config changes. `agents.toml` changes still apply live.

### Publishing to your local HUD

The skill scripts are Python and take the MCP URL as input, so they run
unchanged against `http://127.0.0.1:9090/mcp`:

- `th-hud-publish`: `python .claude/skills/th-hud-publish/scripts/publish.py --url http://127.0.0.1:9090/mcp ...`
- `hud-projection`: set `HUD_MCP_URL=http://127.0.0.1:9090/mcp` and the agent PSK.
- `user-test`: its `*.sh` deploy scripts (`deploy_windows_hud.sh`,
  `tzehouse_env.sh`) SSH from a Linux host into Windows, and you don't need
  them locally. Its `publish_zone_batch.py` and `publish_widget_batch.py` still
  work when given a local `--url`.

## Running the gates

`just` runs recipes with `sh` on every platform, including Windows. The recipes
use POSIX syntax, such as `VAR=1 cmd`, `mkdir -p`, `cmp`, and `python3`. Run
`just` either from Git Bash, or from PowerShell with Git's `usr\bin` on `PATH`.
In plain PowerShell without that, recipes fail with "could not find `sh`".

| Recipe | On Windows |
|---|---|
| `check`, `fmt`, `clippy`, `dev-mode-guard` | Work. Clippy lints both the `cfg(windows)` and non-Windows code that is compiled for the host, so also let Linux CI check `cfg(not(windows))` code |
| `test`, `test-integration`, `production-boot`, `canonical-app-boot`, `token-footprint` | Use WARP through `HEADLESS_FORCE_SOFTWARE=1`. CI never runs these on Windows, so treat a failure as a possible Windows-only gap rather than a regression, and check it against Linux CI |
| `idle-efficiency-checker`, `test-python` | Work once `python3` is fixed |
| GPU pixel-readback (`test-gpu-pixel-readback` in CI) | Linux/Mesa-only and informational. Leave it to CI |

All workspace targets, tests included, compiled for Windows as of 2026-10-03.
This was checked with
`cargo check --workspace --all-targets --target x86_64-pc-windows-gnu` from
Linux. Whether they pass on Windows has not been checked.

Tests marked `#[cfg(unix)]` (for example in
`crates/tze_hud_projection/src/tests/mod.rs`) don't run on Windows. Linux CI
covers them.

`cargo test -p tze_hud_compositor` without `--test` is still forbidden. That
deadlock is specific to headless Linux, but the rule keeps both platforms
consistent.

To reproduce the Windows CI job locally:

```powershell
cargo build --release --locked -p tze_hud_app --bin tze_hud
python scripts/ci/windows_smoke.py --exe target/release/tze_hud.exe --config app/tze_hud_app/config/production.toml
```

The smoke test opens a real overlay for a few seconds and drives the MCP
lifecycle.

## Beads

`.beads/config.yaml` points `bd` at the Dolt server
`dolt.parrot-hen.ts.net:3307`, so you need to be on the tailnet. The Beads git
hooks are shell scripts, and Git for Windows runs them through its bundled
`sh`. The ~4–5 minute `bd import` stall on `git pull --rebase` described in
`AGENTS.md` applies on Windows too.

## Worktrees and agent sessions

- `scripts/worktree-add.sh` relocates `target/` to `/data` on the Linux build
  host. On Windows, use plain
  `git worktree add .worktrees/<name> -b <branch>`. Each worktree builds its
  own `target\`, which takes several GB, so keep `.worktrees\` on the same Dev
  Drive.
- Claude Code on native Windows runs its Bash tool through Git Bash. If it
  can't find Git Bash, set `CLAUDE_CODE_GIT_BASH_PATH` to the full path of
  `bash.exe`.
- Machine-specific rules in `~/.claude/CLAUDE.md` on the Linux host, such as
  the `orca` user's Kubernetes and secret-handling rules, don't carry over to
  Windows. Repo rules in `CLAUDE.md` and `AGENTS.md` still apply.
- Use `-o BatchMode=yes` for `ssh`/`scp`, and non-interactive `cp`/`mv`/`rm`
  flags, the same as on Linux (`AGENTS.md`).
