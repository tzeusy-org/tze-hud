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
| Git for Windows | Git, plus `sh`, `bash`, `env` and `cygpath` for `just` recipes and git hooks | See [Git settings](#git-settings); Bash shebang recipes use their declared interpreter |
| `just` | `just ci` and other recipes | `winget install Casey.Just` |
| `cargo-nextest`0.9.114 | Required by `just test` | `cargo install --locked --version 0.9.114 cargo-nextest`; verify `cargo nextest --version` |
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

For `just test-python`, run `pip install -r scripts/requirements-dev.txt`.

### Build speed

Windows Defender real-time scanning slows `cargo` builds a lot. Put the
checkout, or at least `target\` and `%USERPROFILE%\.cargo`, on a Dev Drive.
Alternatively, add Defender exclusions for those paths.

## Building and running the HUD

```powershell
cargo build --profile release-dev -p tze_hud_app --bin tze_hud
```

PR, merge-queue, main, and manual Windows builds use `release-dev`: optimized
code with thin LTO and 16 codegen units. Version tags keep `--release` with
full LTO and one codegen unit; `perf-budget.yml` also keeps `--release`.
`perf-assert.yml` keeps its existing default test profile.
`.cargo/config.toml` links the CRT statically for `x86_64-pc-windows-msvc`,
so the exe needs no VC++ redistributable. A debug build
(`cargo build -p tze_hud_app --bin tze_hud`) is fine for iteration.

### Always pass arguments to a dev build

A bare launch, with no arguments and from outside the install directory,
installs the exe for the current user. It then registers autostart and
relaunches the installed copy (see
[`windows-install.md`](../operations/windows-install.md#install)). Running a
freshly built `target\release-dev\tze_hud.exe` with no arguments therefore replaces
your installed HUD with the dev build. Any argument at all makes it run in
place instead:

```powershell
.\target\release-dev\tze_hud.exe --config app\tze_hud_app\config\production.toml --window-mode overlay
.\target\release-dev\tze_hud.exe --print-attach-info   # MCP URL + client config, no window
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
  desktop session. A process started by a service or a remote shell cannot
  reach the desktop GPU, and it draws a grey opaque window instead of a
  transparent overlay. The installed HUD autostarts at logon through the
  `HKCU` Run key (see [windows-install.md](../operations/windows-install.md)),
  which is an interactive session.
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
- **Every monitor is overlaid.** Overlay mode opens one transparent,
  click-through window per connected monitor at its native resolution, all
  showing the same scene. Zones stay on the primary monitor unless the config
  places them elsewhere by OS display name (`/admin/status` `displays` lists
  the names):

  ```toml
  [displays.DISPLAY6]
  zones = ["notification-area"]
  ```

  A zone placed on a monitor that is not connected falls back to the primary;
  the log warns and `/admin/status` `unplaced_zones` lists it. So does a zone on
  a monitor whose overlay surface failed 3 times in a row (reason
  `overlay_failed`; unplug and replug, or restart, to retry). A monitor that is
  asleep or occluded is retried on a backoff (100 ms doubling to 2 s), so it
  does not keep the HUD rendering.
  Tiles, portals and widgets stay on the primary. Plugging, unplugging or
  rescaling a monitor opens or closes its window; no restart needed (changing
  `[displays]` itself does need one). The overlays are not resizable or
  maximizable, so Windows cannot re-maximize them; if it still moves or
  resizes one during the change, each one, the primary included, is pinned
  back to its monitor's bounds afterwards (at most 5 times per 10 s if Windows keeps
  fighting it, logged as a warning). Explicit `--width`/`--height` gives a
  single window on the primary. Screenshot one monitor with
  `hud_admin.py screenshot --display N` (or `--all`).
- **Window hotkeys.** With the HUD focused, Ctrl+Shift+P shows a pairing code.
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
- `user-test`: `publish_zone_batch.py` and `publish_widget_batch.py` work when
  given a local `--url` (or `HUD_HOST=127.0.0.1`).

## Running the gates

Ordinary linewise `just` recipes use `sh`, including on Windows. Bash shebang
recipes, including `test`, use their declared `#!/usr/bin/env bash` interpreter.
The recipes also use POSIX utilities and `python3`. Run `just` from Git Bash,
or from PowerShell with Git's `usr\bin` on `PATH` so `sh`, `bash`, `env` and
`cygpath` are available. In plain PowerShell without that, shell recipes fail.
`just test` requires exactly `cargo-nextest`0.9.114 and uses the same arguments
as Linux CI: `cargo nextest run --workspace --all-targets --exclude integration --features tze_hud_runtime/dev-mode`.

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
Vulkan driver may still be loaded alongside WARP. Nextest's `gpu` group limits
constructor-reaching test processes to one, conservatively including CPU cases
in those packages; other CPU cases retain normal global parallelism. For a
separate diagnostic Nextest command, global concurrency is `--test-threads=1`,
without libtest's `--` separator. Retained focused libtest
commands use `cargo test -p <crate> -- --test-threads=1`. Record the exact
command/features/adapter and result; this diagnostic is not a substitute for
the normal parallel merge gate or proof that the full Windows suite passes.

All workspace targets, tests included, compiled for Windows as of 2026-10-04.
This was checked with
`cargo check --workspace --all-targets --target x86_64-pc-windows-gnu` from
Linux. Whether they pass on Windows has not been checked. Tests marked
`#[cfg(unix)]` (for example in `crates/tze_hud_projection/src/tests/mod.rs`)
don't run on Windows. Linux CI covers them.

For the non-tag Windows CI build and smoke (`windows.yml`), run the commands
below. CI also runs the full runtime library and the app parser tests in
`release-dev`, with a fresh `LOCALAPPDATA` directory under `RUNNER_TEMP`
scoped to that test step. Version tags retain the remote helper subset and
parser tests with `--release` and artifacts under `target/release`. Stop your installed HUD first. The install smoke test points
`LOCALAPPDATA` and `APPDATA` at temp dirs, but it writes and then deletes the
real `HKCU\...\Run\tze_hud` autostart value. Afterwards, reinstall your HUD
(bare launch, or `--install`) to get autostart back.

```powershell
cargo build --profile release-dev --locked -p tze_hud_app --bin tze_hud
cargo build --profile release-dev --locked -p poc_demo --bin poc_demo
python scripts/ci/windows_smoke.py --exe target/release-dev/tze_hud.exe --poc-demo target/release-dev/poc_demo.exe --config app/tze_hud_app/config/production.toml
python scripts/ci/windows_install_smoke.py --exe target/release-dev/tze_hud.exe
```

## Developing from WSL2

The HUD has to be a native Windows process: a Linux build inside WSL (WSLg)
cannot draw a transparent always-on-top overlay over the Windows desktop.
Everything else can stay in WSL, including the repo, `just ci`, Beads, and
Claude Code. That makes WSL a Linux dev box that happens to share the screen,
so the rest of this doc's Windows tooling setup isn't needed. No code changes
are needed for this setup. The one hard requirement is networking.

### Networking: NAT plus the tailnet

The HUD listens only on `127.0.0.1` and the host's Tailscale addresses
(`crates/tze_hud_runtime/src/net_addrs.rs`). There is no bind-all switch, and
none should be added. So an agent in WSL reaches the HUD one of two ways:
over Windows loopback (mirrored mode), or over the tailnet.

Use the tailnet. Run Tailscale inside WSL as its own node (`sudo tailscale up`),
keep WSL on its default NAT networking, and point agents at the Windows host's
Tailscale name or IP (the HUD shows it on the pairing card):

- `HUD_HOST=<windows-tailscale-name>` for the skill scripts, and
  `curl -s http://<windows-tailscale-ip>:9090/pair -d '{"agent":"claude","code":"<code>"}'`
  to pair by hand.
- Inbound tailnet traffic crosses Windows Firewall. Run the intended dev executable
  with `--allow-remote` (and the same listen-port overrides as startup), then use
  that executable's `--disallow-remote` to restore its changes before removing it.
  See [Remote agents](../operations/windows-install.md#remote-agents) for UAC,
  same-program BLOCK cleanup, full undo and policy limits. Ordinary startup does
  not modify firewall policy.

Mirrored mode (`networkingMode=mirrored`) looks like the simpler option, but it
failed on the reference machine (WSL 3.0.1.0, observed 2026-10-04):

- Loopback to Windows listeners started after WSL booted hangs. Windows
  shows `SYN_RECEIVED`, while WSL stays in `SYN-SENT`. This matches
  [microsoft/WSL#40343](https://github.com/microsoft/WSL/issues/40343).
  VS Code's forwarded ports kept working; the cause of the difference is
  unknown.
- Mirrored mode also copies the Windows Tailscale addresses onto WSL's `eth0`.
  Linux then treats them as local, so the tailnet route to the HUD is
  unreachable too.
- VS Code on Windows can hold common localhost ports (9090, 3307) for its port
  forwarding, which in mirrored mode WSL shares. If the HUD logs
  `failed to bind MCP HTTP server`, pass `--mcp-port <free port>`.

### Getting a Windows build of the HUD

Choose one of these:

- **Cross-compile in WSL.** This is the fastest loop:

  ```sh
  just bootstrap        # scripts/dev-bootstrap.sh: mingw-w64, the Rust target, .venv, ...
  just build-windows    # add `-j 8` if the link runs out of memory
  ```

  - The output is `target/x86_64-pc-windows-gnu/release/tze_hud.exe`.
  - On 2026-10-04, a Linux build imported only Windows system DLLs and
    embedded the DPI manifest (`windres` comes from mingw-w64).
  - On 2026-10-05, it ran on Windows as an overlay. It paired from WSL over
    the tailnet and rendered zones and widgets published over MCP.
  - It is the GNU flavor, not the MSVC build CI ships, so confirm anything
    toolchain-sensitive against a CI build.
- **Use CI's build.** `gh release download dev -R tzeusy-org/tze-hud -p "tze_hud.exe*"`
  gets the rolling build of `main`. For a PR, download the `tze_hud-windows-msvc`
  artifact from its `windows` workflow run.
- **Build natively** on the Windows side, with a separate checkout and the
  toolchain from [One-time setup](#one-time-setup).

### Running it from WSL

WSL interop can start `.exe` files directly. A process started that way runs
as your Windows user, in your desktop session, and gets a real overlay
(confirmed 2026-10-05). If the window comes up grey and opaque, start it from
a Windows terminal instead.

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

`just ci` runs as it does on any Linux host. Run `just bootstrap` first, and
again whenever a dependency is added. It is idempotent, and it installs or
reports everything the gates need. That includes `mesa-vulkan-drivers` and
`libvulkan1`, so GPU recipes pin llvmpipe instead of the WSL GPU driver,
`protoc` 3.15 or later, `mingw-w64` for `clippy-windows-gnu`, required pinned
`cargo-nextest`0.9.114 for `just test`, and the Python venv for `just test-python`. Without passwordless sudo, it prints the
`apt-get install` line to run, and `just bootstrap --check` reports without
installing. It also checks WSL interop, networking mode, and Tailscale. New
dependencies go in its manifest at the top of `scripts/dev-bootstrap.sh`, and
Python packages go in `scripts/requirements-dev.txt`, which CI also uses.

Keep the checkout on the WSL filesystem (`~/...`), not
`/mnt/c`, because cargo and git over the Windows mount are many times slower.

### Offline scene and widget PNGs in WSL

`just render-scene` applies a user-test JSON batch to an isolated scene through
the normal typed MCP handlers, then uses the compositor's **windowed frame
build/capture** seam. It creates no HUD window, listener, paired credential or
desktop screenshot. The GPU recipe requires Mesa llvmpipe; adapter failures
are errors. The default 1920×1080 layout uses the canonical production scene.

```bash
mkdir -p test_results/render-scene
just render-scene --fixture .claude/skills/user-test/scripts/all-zones-test.json \
  --theme tonal-glass --output test_results/render-scene/zones.png \
  > test_results/render-scene/zones.json
just render-widget --widget assets/widget_bundles/status-indicator \
  --params '{"status":"online","theme":"friendly","label":"Butler"}' \
  --width 252 --height 96 --output test_results/render-scene/widget.png \
  > test_results/render-scene/widget.json
```

Both recipes forward arguments to the single `render-scene` binary. Widget
mode validates the bundle and typed parameters, then uses the existing retained
`WidgetRenderPlan` primitive/resvg CPU rasterizer. `--theme` selects `tonal-glass`,
`classic` or `blueprint`; `--tokens FILE` reads a flat `[design_tokens]` TOML
table, with an explicit `--theme` taking precedence over its selector. Positive
`--width`/`--height` are bounded to 8192 per axis and 16M pixels total.

The batch accepts ordered zone/content or widget/params publishes, clears and
holds. TTL, merge keys and supported delayed zone publication go through their
existing owners; errors in response bodies fail the command. `--capture-at-ms`
selects a checkpoint after the last message, from 0 to 5000ms. The status fixture
`status-indicator-theme-status-matrix-test.json` updates `main-status` twelve
times: one final image shows **friendly/offline**, not all twelve states.
Use separate checkpoint fixtures/images to make intermediate-state claims.

The PNG is straight-alpha sRGB RGBA: GPU sRGB framebuffer bytes are decoded,
unpremultiplied in linear RGB and re-encoded; tiny-skia widget bytes are
unpremultiplied in their byte domain. Zero alpha becomes transparent black.
Output publication is atomic and refuses an existing destination. The JSON
manifest on stdout records hashes, dimensions, checkpoint, adapter and elapsed
scope. Its checkout HEAD is observed at invocation, while its compiled-example
source hash identifies embedded source; the surrounding build receipt binds
the complete tested source and executable. Separate initial compilation from
warm **full-command wall time**, which must be under 10 seconds per matrix cell.

The authoring matrix is three themes × `all-zones-test.json`,
`notification-full-gamut.json`, `subtitle-multiline.json` and the exact status
fixture above. Deliver all twelve WSL-produced PNGs and their manifests through
PR-linked CI artifacts with byte-for-byte upload/download verification; local
paths or separately regenerated CI images do not establish that attachment.
These are informational authoring images, not required pixel goldens or owner
visual sign-off: Mesa/WARP antialiasing, fonts and desktop/DPI remain distinct.

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
