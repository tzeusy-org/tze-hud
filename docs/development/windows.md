# Developing on Windows

How to work on tze_hud from a Windows machine, usually the same machine that
runs the HUD. Windows is the only deployment target, so the runtime builds and
runs natively. What needs care is the tooling around it: `just`, the shell
scripts, Beads, and some Linux-only test lanes. For installing a released
binary rather than building one, see
[`../operations/windows-install.md`](../operations/windows-install.md).

There are two ways to work on this machine:

- **Native.** Edit and build on Windows. Most of this doc covers that.
- **WSL2.** Keep the repo and agents in Linux, and run only the HUD as a
  Windows process. See [Developing from WSL2](#developing-from-wsl2).

Linux CI (`ci.yml`) is still the merge gate. The Windows CI job (`windows.yml`)
builds the release exe, boot-smokes it, and runs an install and uninstall
smoke test. Linux CI also runs clippy for the `windows-gnu` target. Neither
runs `cargo test` on Windows, so a green Windows test run proves less than a
green CI.

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

### Always pass arguments to a dev build

A bare launch, with no arguments and from outside the install directory,
installs the exe for the current user. It then registers autostart and
relaunches the installed copy (see
[`windows-install.md`](../operations/windows-install.md#install)). Running a
freshly built `target\release\tze_hud.exe` with no arguments therefore replaces
your installed HUD with the dev build. Any argument at all makes it run in
place instead:

```powershell
.\target\release\tze_hud.exe --config app\tze_hud_app\config\production.toml --window-mode overlay
.\target\release\tze_hud.exe --print-attach-info   # MCP URL + client config, no window
```

### One instance per user

Only one HUD runs per Windows user, enforced by the named mutex
`Local\tze_hud` (`crates/tze_hud_runtime/src/operator/install.rs`). If the
installed copy is already running (it autostarts at logon), a dev build
started with arguments prints "already running" and exits 2. Stop the
installed one first:

- `Stop-Process -Name tze_hud`, which works for any instance.
- Or `tze_hud.exe --uninstall`, which also removes autostart and keeps
  `%APPDATA%\tze_hud`.

### Config and agents

If there is no `--config`, the config is resolved in this order:
`TZE_HUD_CONFIG`, then `.\tze_hud.toml`, then
`%APPDATA%\tze_hud\config.toml`. The gauge, progress-bar, and
status-indicator widget bundles are built into the exe, so a config file is
the only other file it needs. `TZE_HUD_DEV_ALLOW_INSECURE_STARTUP=1` allows a
debug run with no config.

Agents are paired into `agents.toml` beside the resolved config, which holds
only PSK SHA-256 digests. With the repo config, that file is
`app\tze_hud_app\config\agents.toml`. Git does not ignore it, so add it
to `.git/info/exclude` to keep it from being committed. To pair an agent:

- With no agents paired, the HUD shows a code. Otherwise press Ctrl+Shift+P on
  the HUD, or run `tze_hud.exe --pair`.
- The agent trades the code for its PSK with `POST /pair`. See
  [Pair an agent](../operations/windows-install.md#pair-an-agent).

`scripts/quickstart.sh` runs under Git Bash but was written for Linux. On
Windows, `--print-attach-info` plus pairing is the more direct path.

### Runtime behavior specific to Windows

- **Interactive desktop only.** Launch the HUD from a terminal in your own
  desktop session. A process started over SSH, or by a service, cannot reach
  the desktop GPU, and it draws a grey opaque window instead of a transparent
  overlay. That is why the remote-deploy tooling launches through the
  `TzeHudOverlay` scheduled task (`scripts/windows/run_hud.ps1`). Local
  development doesn't need that.
- **Ctrl+C does not stop it.** `tze_hud.exe` is a GUI-subsystem binary. It
  attaches to the parent console for output, then ignores console Ctrl+C
  (`app/tze_hud_app/src/main.rs`). Stop it with `Stop-Process -Name tze_hud`.
  In fullscreen mode, closing the window also works.
- **Logs** go to `%LOCALAPPDATA%\tze_hud\logs\tze_hud.log` (override with
  `TZE_HUD_LOG_DIR`), because the overlay has no console of its own. Panics go
  to `hud-diag.log` in the same directory. An agent with `admin` can also read
  them with `GET /admin/logs`.
- **GPU backend.** The windowed runtime asks for D3D12 or Vulkan.
  `HEADLESS_FORCE_SOFTWARE=1` makes headless adapters fall back to WARP, the
  Windows counterpart of llvmpipe.
- **Global safe-mode hotkey.** It is registered with `RegisterHotKey`. The
  default is Ctrl+Shift+F12, and `[runtime].safe_mode_hotkey` changes it. If
  another program owns the chord, `/admin/status` reports
  `registered: false`. Ctrl+Shift+Esc is unusable because Windows reserves it
  for Task Manager.
- **Window hotkeys.** With the HUD focused, Ctrl+Shift+F8 and Ctrl+Shift+F9
  cycle monitors, and Ctrl+Shift+P shows a pairing code.
- **Frame pacing** raises the timer resolution to 1 ms
  (`timeBeginPeriod`) while the compositor runs. This is expected.
- **Config changes need a restart.** Restart the process, or use
  `POST /admin/restart` (see
  [Restart and handoff](../operations/windows-install.md#restart-and-handoff)).

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
use POSIX syntax, such as `VAR=1 cmd`, `[ -f ... ]`, `mkdir -p`, `cmp`, and
`python3`. Run `just` either from Git Bash, or from PowerShell with Git's
`usr\bin` on `PATH`. In plain PowerShell without that, recipes fail with
"could not find `sh`".

| Recipe | On Windows |
|---|---|
| `check`, `fmt`, `clippy`, `dev-mode-guard`, `dead-code` | Work. On Windows, clippy checks the `cfg(windows)` code, so leave the `cfg(not(windows))` code to Linux CI |
| `test`, `test-integration`, `production-boot`, `canonical-app-boot`, `token-footprint` | Run on WARP through `HEADLESS_FORCE_SOFTWARE=1`. The llvmpipe pin (`VK_ICD_FILENAMES`) applies only when the Linux ICD file exists, so it does nothing here. CI never runs these on Windows, so treat a failure as a possible Windows-only gap and check it against Linux CI. Running them with a hardware GPU present has not been tested (see the note below) |
| `test-gpu` | Always fails: it requires the Linux Mesa llvmpipe ICD. Leave it to CI |
| `clippy-windows-gnu` | Prints `SKIPPED` unless the `x86_64-pc-windows-gnu` target and MinGW are installed. On an MSVC host, plain `clippy` already covers the same Windows code paths |
| `overlay-harness-contract` | Runs when `pwsh` (PowerShell 7) is on `PATH`, otherwise prints `SKIPPED` |
| `cargo-deny`, `deps-unused` | Need `cargo install --locked cargo-deny cargo-machete` |
| `idle-efficiency-checker`, `test-python` | Work once `python3` is fixed |

The hardware-GPU note: on Linux, concurrent device creation with a hardware
Vulkan ICD (NVIDIA) has hung tests, which is why the recipes pin llvmpipe
there. Headless test adapters use `Backends::all()`, so on Windows the NVIDIA
Vulkan driver may still be loaded alongside WARP. If `just test` hangs, rerun
the stuck crate with `-- --test-threads=1` and record the result in this doc.

All workspace targets, tests included, compiled for Windows as of 2026-10-04.
This was checked with
`cargo check --workspace --all-targets --target x86_64-pc-windows-gnu` from
Linux. Whether they pass on Windows has not been checked. Tests marked
`#[cfg(unix)]` (for example in `crates/tze_hud_projection/src/tests/mod.rs`)
don't run on Windows. Linux CI covers them.

To reproduce the Windows CI job (`windows.yml`) locally, run the commands
below. Stop your installed HUD first. The install smoke test points
`LOCALAPPDATA` and `APPDATA` at temp dirs, but it writes and then deletes the
real `HKCU\...\Run\tze_hud` autostart value. Afterwards, reinstall your HUD
(bare launch, or `--install`) to get autostart back.

```powershell
cargo build --release --locked -p tze_hud_app --bin tze_hud
python scripts/ci/windows_smoke.py --exe target/release/tze_hud.exe --config app/tze_hud_app/config/production.toml
python scripts/ci/windows_install_smoke.py --exe target/release/tze_hud.exe
```

## Developing from WSL2

The HUD has to be a native Windows process: a Linux build inside WSL (WSLg)
cannot draw a transparent always-on-top overlay over the Windows desktop.
Everything else can stay in WSL, including the repo, `just ci`, Beads, and
Claude Code. That makes WSL a Linux dev box that happens to share the screen,
so the rest of this doc's Windows tooling setup isn't needed. No code changes
are needed for this setup. The one hard requirement is networking.

### Networking: use mirrored mode

The HUD listens only on `127.0.0.1` and the host's Tailscale addresses
(`crates/tze_hud_runtime/src/net_addrs.rs`). There is no bind-all switch, and
none should be added. With WSL2's default NAT networking, `127.0.0.1` inside
WSL is the VM's own loopback, not Windows', so agents in WSL can't reach the
HUD. Turn on mirrored networking (Windows 11 22H2 or later) in
`%USERPROFILE%\.wslconfig`:

```ini
[wsl2]
networkingMode=mirrored
```

Then run `wsl --shutdown` and reopen WSL. After that,
`http://127.0.0.1:9090/mcp` from WSL reaches the HUD's MCP port, and gRPC is
reachable on `127.0.0.1:50051`, so:

- Skill scripts and Claude Code MCP configs use `127.0.0.1` as they would on
  Windows: `HUD_MCP_URL=http://127.0.0.1:9090/mcp`, with the paired PSK as the
  bearer.
- Pairing works from WSL:
  `curl -s http://127.0.0.1:9090/pair -d '{"agent":"claude","code":"<code>"}'`.

If mirrored mode isn't available, the fallback is the tailnet. Run Tailscale
inside WSL as its own node, and point agents at the Windows host's Tailscale IP
(the HUD shows it on the pairing card).

### Getting a Windows build of the HUD

Choose one of these:

- **Cross-compile in WSL.** This is the fastest loop:

  ```sh
  rustup target add x86_64-pc-windows-gnu
  sudo apt install mingw-w64
  cargo build --release --target x86_64-pc-windows-gnu -p tze_hud_app --bin tze_hud
  ```

  - The output is `target/x86_64-pc-windows-gnu/release/tze_hud.exe`.
  - On 2026-10-04, a Linux build imported only Windows system DLLs and
    embedded the DPI manifest (`windres` comes from mingw-w64).
  - It has not been run on Windows yet.
  - It is the GNU flavor, not the MSVC build CI ships, so confirm anything
    toolchain-sensitive against a CI build.
- **Use CI's build.** `gh release download dev -R tzeusy-org/tze-hud -p "tze_hud.exe*"`
  gets the rolling build of `main`. For a PR, download the `tze_hud-windows-msvc`
  artifact from its `windows` workflow run.
- **Build natively** on the Windows side, with a separate checkout and the
  toolchain from [One-time setup](#one-time-setup).

### Running it from WSL

WSL interop can start `.exe` files directly. A process started that way runs
as your Windows user, in your desktop session, so it should get a real
overlay. This hasn't been confirmed on this machine. If the window comes up
grey and opaque, start it from a Windows terminal instead.

- Copy the exe and its config to a Windows directory, and run it from there.
  The exe and the config are the only files it needs, because widget bundles
  are built in. Running it from the Linux filesystem would put the paired
  `agents.toml` beside the config, over a `\\wsl.localhost\...` path that
  hasn't been tested.
- Paths passed to the exe must be Windows paths. Convert them with
  `wslpath -w`.
- Stop the HUD from WSL with `taskkill.exe /IM tze_hud.exe /F`. Ctrl+C in the
  WSL terminal is ignored, the same as natively.

```sh
dst=/mnt/c/Users/<you>/tze_hud-dev
mkdir -p "$dst" && cp -f target/x86_64-pc-windows-gnu/release/tze_hud.exe "$dst/"
cp -f app/tze_hud_app/config/production.toml "$dst/tze_hud.toml"
"$dst/tze_hud.exe" --config "$(wslpath -w "$dst/tze_hud.toml")" --window-mode overlay &
```

The [one-instance](#one-instance-per-user) and
[always-pass-arguments](#always-pass-arguments-to-a-dev-build) rules still
apply. Stop the installed HUD before starting a dev build, and always pass
arguments.

### Gates in WSL

`just ci` runs as it does on any Linux host. For GPU tests, install
`mesa-vulkan-drivers` and `libvulkan1`, so the recipes pin llvmpipe instead of
the WSL GPU driver. Also install `protoc` 3.15 or later, and `mingw-w64` for
`clippy-windows-gnu`. Keep the checkout on the WSL filesystem (`~/...`), not
`/mnt/c`, because cargo and git over the Windows mount are many times slower.

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
