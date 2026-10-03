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

## Run

Until first-run install and pairing land (T6 in `docs/scope.md`), run it with
a config and an `agents.toml` beside it. The HUD stores only each agent's PSK
SHA-256; the agent keeps the PSK and sends it as its bearer:

```powershell
$psk = -join ((1..32) | ForEach-Object { '{0:x2}' -f (Get-Random -Maximum 256) })
$sha = [System.Security.Cryptography.SHA256]::Create()
$hash = -join ($sha.ComputeHash([Text.Encoding]::UTF8.GetBytes($psk)) | ForEach-Object { $_.ToString('x2') })
Set-Content agents.toml "[agents.claude]`npsk_sha256 = `"$hash`"`nallow = [`"*`"]" -Encoding ascii
.\tze_hud.exe --config tze_hud.toml --window-mode overlay
```

Give `$psk` to the agent host and do not store it on the HUD side.
`app/tze_hud_app/config/production.toml` is the reference config.

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
| `GET /admin/status` | JSON: `version`, `sha`, `channel`, `pid`, `uptime_s`, `binds`, `agents` (`id`, `admin`), `safe_mode`, `frames_presented`, `cpu_pct_2s` (sampled over 2 s, so the call takes about 2 s), `cpu_pct_avg` (percent of one core), `last_update` (null until updates land) |
| `GET /admin/logs?tail=N` | `text/plain`, the last N lines (default 100, max 2000) across the rotation |

Without a valid PSK the answer is 401; with one lacking `admin`, 403
`{"code":"NOT_ADMIN","hint":...}`.
