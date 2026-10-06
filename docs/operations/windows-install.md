# Installing tze_hud on Windows

CI builds `tze_hud.exe` (`x86_64-pc-windows-msvc`, static CRT, one file) and
publishes it from `.github/workflows/windows.yml`:

- **`dev`**: a rolling prerelease rebuilt from every merge to `main`.
- **`v*`**: a release per tag.

Each release carries `tze_hud.exe`, `tze_hud.exe.sha256`, `tze_hud.exe.minisig`,
and `tze_hud.pdb` (symbols for WPA, PIX, and crash dumps) when emitted.

## Download and verify

Download `tze_hud.exe` from the release page in a browser, or with the GitHub
CLI:

```powershell
gh release download dev -R tzeusy-org/tze-hud -p "tze_hud.exe*"
minisign -Vm tze_hud.exe -P RWTpCWtkNWD3YEwR2XCS2cwutGmd/fJCVuq9a99frgpLfTinnjWPsuvE
```

The public key is also committed at `app/tze_hud_app/minisign.pub`. The
signing key lives only in the `MINISIGN_SECRET_KEY` repository secret; the
release job refuses to publish without it and checks every signature against
the committed public key.

## Install

Double-click `tze_hud.exe` (or run it with no arguments). The exe is not
Authenticode-signed, so Windows SmartScreen may show "Windows protected your
PC": click **More info**, then **Run anyway**. (The minisign check above is
the integrity check.) From outside the
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

With no agents paired, the HUD shows a 6-digit code and its Tailscale address
on a card. The Tailscale address is one in `100.64.0.0/10` or `fd7a:115c:a1e0::/48` on
an interface named `Tailscale*`; such an address on any other adapter (ISP or
other-VPN carrier-grade NAT) is ignored. Give the code to the agent, which trades it for its key:

```sh
curl -s http://<tailscale-ip>:9090/pair -d '{"agent":"claude","code":"482913"}'
```

or, from this repo, which saves the PSK to `~/.config/tze-hud/<host>.psk`
(mode 0600, never printed) for the other skill scripts:

```sh
HUD_HOST=<tailscale-ip> python3 .claude/skills/user-test/scripts/hud_pair.py --code 482913 --agent claude --admin
```

The reply carries the PSK (once), the MCP URL, and the gRPC address. The HUD
stores only the PSK's SHA-256 in `%APPDATA%\tze_hud\agents.toml`. Add
`"admin": true` to the request to let that agent use `/admin/*`. To pair another
agent, or re-pair one (which rotates its key), run `tze_hud.exe --pair` or press
Ctrl+Shift+P with the HUD focused. A code works once and expires after 5
minutes; five wrong codes replace it, and repeated failures pause pairing for
60 seconds.

The card states a fixed 5-minute validity rather than a countdown, and it
clears when the code expires. The code is on screen, so an agent holding an
`admin` PSK can read it with `GET /admin/screenshot`; grant `admin` sparingly.

`app/tze_hud_app/config/production.toml` is the reference (and default) config.

## Remote agents

The HUD listens on loopback and on each Tailscale address of the machine
(`100.64.0.0/10`, `fd7a:115c:a1e0::/48`), on the MCP/pairing port (9090) and the
gRPC port (50051). It never binds a wildcard address. Agents on other tailnet
machines reach it only if Windows Firewall lets inbound TCP to those ports in.

The HUD reads the firewall policy (no admin needed) when the pairing card opens
and when `/admin/status` is requested, never per frame, and reports
`tailnet_inbound`:

| State | Meaning |
|---|---|
| `allowed` | an enabled inbound allow rule covers `tze_hud.exe` or its ports for the tailnet, or the firewall is off for the active profile |
| `blocked` | an enabled inbound block rule matches (`reason: block_rule`, `rule` names it), or no allow rule exists and the default inbound action is Block (`no_allow_rule`) |
| `unknown` | the policy could not be read (`error`), for example a third-party firewall |
| `not_applicable` | no Tailscale address is bound, or not Windows |

When `blocked`, the pairing card (shown for a tailnet address) adds a line,
"Windows Firewall blocks remote agents". A block rule wins over any allow rule;
only rules on a currently active profile count. To fix it, add an inbound allow
rule for `tze_hud.exe` (an elevated PowerShell):

```powershell
New-NetFirewallRule -DisplayName "tze_hud" -Direction Inbound -Action Allow `
  -Program "$env:LOCALAPPDATA\Programs\tze_hud\tze_hud.exe" -Protocol TCP `
  -RemoteAddress 100.64.0.0/10,fd7a:115c:a1e0::/48
```

Adjust `-Program` to where `tze_hud.exe` is installed; remove a conflicting block
rule if `rule` names one. Loopback agents are never affected.

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
| `GET /admin/status` | JSON: `version`, `sha`, `channel`, `pid`, `uptime_s`, `binds`, `tailnet_inbound` (`state`: `allowed`, `blocked`, `unknown` or `not_applicable`; when `blocked`, also `reason` (`block_rule` or `no_allow_rule`), `rule` (the blocking rule's name, or `null`) and `fix`; `unknown` carries `error`; cached for 5 s), `agents` (`id`, `admin`), `safe_mode`, `safe_mode_hotkey` (`chord`, `registered`, `error`; `null` when no hotkey is active (non-Windows, or no network runtime); `registered: null` means registration is still pending; `registered: false` means another program owns the chord and there is no human override, also shown in the startup banner and logged at error level), `frames_presented`, `cpu_pct_2s` (sampled over 2 s, so the call takes about 2 s), `cpu_pct_avg` (percent of one core), `last_update` (null until updates land), `last_restart` (`null`, or `{ok, pid, error}` for the last restart that did not hand over) |
| `GET /admin/logs?tail=N` | `text/plain`, the last N lines (default 100, max 2000) across the rotation |
| `GET /admin/screenshot` | `image/png` of the HUD's own frame at the window size (what the compositor draws, not an OS capture); rendered once per request, so idle cost is unchanged. One at a time (429), 503 if the compositor does not answer within 3 s |
| `POST /admin/restart` | 202 `{"restarting":true}`, then the HUD relaunches itself (see below). POST only; the request body is ignored. 429 `BUSY` while one is in progress |
| `POST /admin/update` | `{"channel":"dev"}`, `"stable"` (latest release) or a tag such as `"v1.2.3"`. Downloads, verifies and installs a signed release (see Update below). 200 `{"up_to_date":true}`, 202 `{"updating":true,"sha":...}`, 400 `UPDATE_FAILED` for a bad body or channel, 409 `NOT_INSTALLED`, 429 `BUSY`, 502 `UPDATE_FAILED`, 503 `UNAVAILABLE` |

The same calls are wrapped by `python3 .claude/skills/user-test/scripts/hud_admin.py`
(`status`, `logs --tail N`, `screenshot -o FILE`, `update --channel dev`,
`restart`).

Without a valid PSK the answer is 401; with one lacking `admin`, 403
`{"code":"NOT_ADMIN","hint":...}`.

### Operator error codes

`/pair` and `/admin/*` errors are JSON `{"code","hint"}`; the `hint` says what
to do.

| Code | HTTP | Meaning |
|---|---|---|
| `UNAUTHENTICATED` | 401 | no valid PSK bearer on an `/admin/*` call |
| `NOT_ADMIN` | 403 | the agent's `allow` in `agents.toml` lacks `admin` (`*` does not grant it) |
| `PAIRING_CLOSED` | 403 | no open code: press Ctrl+Shift+P on the HUD or run `tze_hud.exe --pair`, then retry |
| `PAIR_CODE_INVALID` | 403 | wrong code; read the current one off the HUD |
| `BAD_REQUEST` | 400 | `/pair` body is not `{"agent","code"[,"admin"]}` JSON, or the agent id is not 1-32 characters of `a-z`, `0-9`, `-` |
| `NOT_INSTALLED` | 409 | `/admin/update` on a copy that is not the installed one |
| `UPDATE_FAILED` | 400, 502 | bad channel, or download, signature, channel or handoff failure (cause in the log) |
| `BUSY` | 429 | a screenshot, restart or update is already running |
| `UNAVAILABLE` | 503 | no display, compositor silent for 3 s, capture failed, restart or update could not start, or `/pair` could not save `agents.toml` (a new code is shown) |
| `TOO_LARGE` | 422 | the frame exceeds the screenshot size limit |
| `NO_SUCH_DISPLAY` | 404 | `/admin/screenshot?display=<i>` names no display in `/admin/status`, or one whose overlay closed since (indexes shift on hot-plug; re-read status) |

## Update

`POST /admin/update` is pull-only: the HUD fetches `tze_hud.exe` and
`tze_hud.exe.minisig` for the channel from
`https://github.com/tzeusy-org/tze-hud/releases` with the system `curl.exe`
(`TZE_HUD_RELEASES_URL` overrides the base, for testing). Nothing in the request
picks a URL, path or argument. It only works on the installed copy
(`%LOCALAPPDATA%\Programs\tze_hud\tze_hud.exe`); anything else gets
`NOT_INSTALLED`.

Before any file is replaced, the exe is checked against the minisign public key
compiled into the running HUD (`app/tze_hud_app/minisign.pub`), and the signed
trusted comment `tze_hud <ref> <sha>` must name the requested channel (`dev`
for `dev`, the tag for a tag, any `v*` tag for `stable`). A tampered exe,
another key, or a build signed for another channel is refused. A release whose
sha is the running sha answers `up_to_date`. The release workflow signs
`tze_hud <channel> <sha>`, so dev builds say `dev`, not `main`.

A verified exe is staged beside the installed one, the running exe is renamed
to `tze_hud.old.exe`, the new one takes its name, and the restart handoff below
runs against it with `--updated-from <old sha>`. If the new build does not
report ready within 30 s, it is killed, the failed exe is renamed aside to
`tze_hud.failed.exe` (never deleted in place, since a locked exe cannot be
deleted; cleaned up at the next start like `tze_hud.old.exe`), `tze_hud.exe` is
put back, and the old HUD keeps running. If even the put-back fails,
`last_update.error` and the toast say to reinstall (the HTTP answer stays
`UPDATE_FAILED`). Update and restart share one in-flight flag: while either
runs, the other answers 429 `BUSY`. The new HUD toasts `Updated to dev-<sha7>`; a failure toasts
`Update failed; still on dev-<sha7>`, `last_update` in `/admin/status` is
`{ok, sha, error}`, and the cause (download, signature, channel, handoff) is in
the log. Over HTTP every failure is the same `UPDATE_FAILED`. Downloads are
capped (256 MiB, 120 s per file, 5 redirects) and curl runs with `-q`, so no
`.curlrc` can add options.

Accepted limits: `TZE_HUD_RELEASES_URL` may be plain `http` (it exists for
testing); safety then rests on the signature alone. Any validly signed build
of the same channel is accepted, including an older one, so there is no
downgrade or replay protection. The window between verifying the staged exe
and renaming it into place is open to another process running as the same
user, who could already replace the installed exe directly.

## Restart and handoff

`POST /admin/restart` starts the same exe with the same arguments plus an
internal `--handoff 127.0.0.1:<port>:<nonce>`; nothing from the request is
used. The old instance keeps serving while the new one starts its window and
GPU. When the new one has submitted its first frame it reports `READY` over the
loopback socket (the nonce proves it is the child that was started), and the
old instance shuts down cleanly (exit 0), releasing its ports and the
single-instance mutex. The new instance then takes the mutex (waiting up to
35 s) and binds the gRPC and MCP ports (retrying up to 10 s). Poll
`GET /admin/status` until `pid` changes; the same PSK keeps working. The
overlay is briefly doubled and the ports are briefly unreachable during the
handover.

If the new instance exits or has not reported ready within 30 s, it is killed
and the old instance keeps running untouched; `/admin/status` shows
`last_restart` `{ok:false, error}`. Other local connections to the one-shot
handoff port (wrong nonce, junk) are ignored. The nonce is 16 random bytes.

Residual failure mode: the old instance exits as soon as the new one reports
ready, before the new one has bound the ports (the two cannot hold the same
port). If the new instance then fails to bind within 10 s, it logs the error
and exits, and no instance is running until the next autostart or a manual
launch. The listening sockets are created non-inheritable so the child cannot
keep the old instance's port alive.
