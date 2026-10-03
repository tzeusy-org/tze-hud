# Installing tze_hud on Windows

CI builds `tze_hud.exe` (`x86_64-pc-windows-msvc`, static CRT, one file) and
publishes it from `.github/workflows/windows.yml`:

- **`dev`**: a rolling prerelease rebuilt from every merge to `main`.
- **`v*`**: a release per tag.

Each release carries `tze_hud.exe`, `tze_hud.exe.sha256`, `tze_hud.exe.minisig`,
and `tze_hud.pdb` (symbols for WPA, PIX, and crash dumps) when emitted.

## Download and verify

```powershell
gh release download dev -R tzeusy-org/tze-hud -p "tze_hud.exe*"
minisign -Vm tze_hud.exe -P RWTpCWtkNWD3YEwR2XCS2cwutGmd/fJCVuq9a99frgpLfTinnjWPsuvE
```

The public key is also committed at `app/tze_hud_app/minisign.pub`. The
signing key lives only in the `MINISIGN_SECRET_KEY` repository secret; the
release job refuses to publish without it and checks every signature against
the committed public key.

## Install

Double-click `tze_hud.exe` (or run it with no arguments). From outside the
install dir it installs for the current user, no admin rights:

- copies itself to `%LOCALAPPDATA%\Programs\tze_hud\tze_hud.exe` (a previous
  copy is parked as `tze_hud.old.exe` and removed on the next start);
- writes `%APPDATA%\tze_hud\config.toml` from the built-in default if absent
  (an existing config is never overwritten);
- registers `HKCU\...\Run\tze_hud` (autostart at logon, overlay mode) and
  `HKCU\...\App Paths\tze_hud.exe`;
- relaunches the installed copy detached and exits.

Running a newer download the same way upgrades in place: the running instance
is asked to quit, then the new one starts. Any command-line argument runs the
exe where it is instead; `--install` forces the install. One instance runs per
user: a second bare launch (or the autostart command) logs "already running"
and exits 0, while a second launch with any other arguments (benchmark, CI)
prints an error and exits 2. `--handoff` waits up to 35 s for the previous one
to exit instead. Install and uninstall stop the running instance through a
named event that only the windowed runtime listens on; if it has not exited
within 15 s, install fails without replacing anything.

`tze_hud.exe --uninstall` removes both registry entries, stops the running
instance, and deletes the install dir once it has exited. It keeps
`%APPDATA%\tze_hud` (config, `agents.toml`). Add `--purge` to delete that and
`%LOCALAPPDATA%\tze_hud` (logs) too. Nothing outside `tze_hud`-named
directories and the two registry entries is touched.

## Pair an agent

Until first-run pairing lands (T6 in `docs/scope.md`), add an `agents.toml`
beside the config. The HUD stores only each agent's PSK SHA-256; the agent keeps
the PSK and sends it as its bearer:

```powershell
$psk = -join ((1..32) | ForEach-Object { '{0:x2}' -f (Get-Random -Maximum 256) })
$sha = [System.Security.Cryptography.SHA256]::Create()
$hash = -join ($sha.ComputeHash([Text.Encoding]::UTF8.GetBytes($psk)) | ForEach-Object { $_.ToString('x2') })
Set-Content "$env:APPDATA\tze_hud\agents.toml" "[agents.claude]`npsk_sha256 = `"$hash`"`nallow = [`"*`"]" -Encoding ascii
```

Give `$psk` to the agent host and do not store it on the HUD side.
`app/tze_hud_app/config/production.toml` is the reference (and default) config.

## Logs and operator endpoints

The overlay has no console, so tracing also goes to `tze_hud.log` in
`%LOCALAPPDATA%\tze_hud\logs` (override with `TZE_HUD_LOG_DIR`; elsewhere
`<temp>/tze_hud/logs`). It rotates to `tze_hud.log.1` at 10 MB. File level is
`info`; set `TZE_HUD_FILE_LOG` to change it. Panics go to `hud-diag.log` in the
same directory.

An agent whose `allow` includes `admin` (`*` does not grant it) can read, on
the MCP port with its PSK as the bearer:

| Request | Response |
|---|---|
| `GET /admin/status` | JSON: `version`, `sha`, `channel`, `pid`, `uptime_s`, `binds`, `agents` (`id`, `admin`), `safe_mode`, `safe_mode_hotkey` (`chord`, `registered`, `error`; `null` when no hotkey is active (non-Windows, or no network runtime); `registered: null` means registration is still pending; `registered: false` means another program owns the chord and there is no human override, also shown in the startup banner and logged at error level), `frames_presented`, `cpu_pct_2s` (sampled over 2 s, so the call takes about 2 s), `cpu_pct_avg` (percent of one core), `last_update` (null until updates land) |
| `GET /admin/logs?tail=N` | `text/plain`, the last N lines (default 100, max 2000) across the rotation |

Without a valid PSK the answer is 401; with one lacking `admin`, 403
`{"code":"NOT_ADMIN","hint":...}`.
