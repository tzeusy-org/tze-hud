# Scope and implementation status

The project was cut back from a general "agent presence engine" to an
MCP/gRPC layer for the HUD real-estate lifecycle (see [vision.md](vision.md)).
T0–T4 removed what doesn't serve that; T5 redesigned the API that remains;
T6–T8 prove the result on Windows and shrink the code to match; T9–T14 are
owner-directed additions. Each tranche must still build, pass tests, and boot
the overlay.

This file states the present. Per-change evidence (runs, line inventories,
audits) lives in PR descriptions and beads; update the status cell and the
owner-check list here when a change moves them. Source implementation,
automated evidence and owner acceptance are distinct: green CI or a mechanical
Windows run never closes an owner check.

## Keep

| Area | Where |
|---|---|
| Overlay window, compositor, text/markdown, images | `tze_hud_runtime` (windowed), `tze_hud_compositor` |
| Scene graph: tabs, tiles, nodes, zones | `tze_hud_scene` |
| Session portal + projection | `tze_hud_portal`, `tze_hud_runtime` portal modules, `tze_hud_input` |
| MCP tools (zones, widgets, portal) | `tze_hud_mcp` |
| gRPC resident session (zones, widgets, uploads, lifecycle notices) | `tze_hud_protocol` |
| SVG widgets + asset store | `tze_hud_widget`, `tze_hud_resource` |
| Config, flat design tokens, built-in themes | `tze_hud_config` |
| Frame/idle telemetry | `tze_hud_telemetry` |
| App binary | `app/tze_hud_app` |

## Tranches

| Tranche | What | Status |
|---|---|---|
| T0 | Doctrine, RFCs, OpenSpec, curriculum, evidence/report docs, doctrine and OpenSpec agent skills, vocabulary lint | done |
| T1 | Unused crates: `tze_hud_a11y`, `tze_hud_media_apple`, `tze_hud_media_android`, `tze_hud_policy`; Android/iOS/Safari CI workflows | done |
| T2 | Media and cloud relay: GStreamer/`v2_preview` features, media ingress/admission, video surface, media signaling protobuf messages (field numbers reserved), media config and capability, real-decode and v2-preview CI, Python media exemplars | done |
| T3 | Governance: attention budget, quiet hours, privacy redaction and viewer classes, `[privacy]`/`[degradation]`/`[chrome]` config, admission controller, budget ladder (now plain hard caps), unwired lease state machine and suspension manager, degradation ladder down to one fallback (Normal ↔ Simplified). **Kept** the lease lifecycle (request, TTL, renew, release, revoke, disconnect grace). Capability-scope shrink moved to T5: it changes the session-init and lease wire contract | done |
| T4 | Scaffolding: `tze_hud_validation`, replay/trace recording, v1-thesis/Layer-4 artifact harness and their CI jobs; component profiles (flat `[design_tokens]` stay; removed profile sections are rejected); sync groups and clock-skew estimation (`compositor_timestamp_wall_us` stays for `present_at`/`expires_at`; wire fields reserved); hardware calibration (tests use a fixed `test_budget` slack; the benchmark keeps its CI factors); test scenes for removed features; reserved mobile display profile; unwired tab-switch trigger; redundant tests | done |
| T5 | API redesign ([api.md](api.md)): one verb set per lifecycle stage across MCP and gRPC (five MCP tools replace 22; gRPC `Publish`/`Clear`/`Hold`/`RequestResult`); per-agent PSK identity with an `allow` list replaces the capability vocabulary, resident principal, and portal owner token; one shared error-code set with hints; geometry remains runtime-owned; token budgets enforced in CI; dead wire removed and reserved; session-server constructors collapsed; invariant 1 and 4 breaks on the gRPC path fixed; owner-directed public tile removal on 2026-10-14 (deferred spec hud-x1wuq) | done |
| T6 | Windows delivery without SSH, then prove the POC on it. (1) CI builds `x86_64-pc-windows-msvc` natively on a Windows runner, boots it under WARP with an MCP smoke, and publishes minisign-signed releases (rolling `dev` from main, `v*` tags). (2) The exe self-installs per user (`%LOCALAPPDATA%`, `HKCU` Run autostart, no admin). (3) It listens on loopback and the Tailscale address only (owner reaffirmed 2026-10-06: no loopback-only default, no relay). The HUD detects when Windows Firewall blocks tailnet inbound and says so on the pairing card and in `/admin/status`; an opt-in elevated helper adds one program-scoped rule (bead `hud-lgb6c`). (4) First-run pairing: the HUD shows a one-time code, an agent host trades it for a per-agent PSK (`POST /pair`; only a hash is stored); delete `--psk`, the default PSK, and `--bind-all-interfaces`. (5) `/admin/*` operator endpoints (status, logs, screenshot of the HUD's own frame, restart, signed pull-only self-update with health handoff), off the MCP surface. (6) Rewrite `user-test`/`hud-projection` around the paired PSK; delete SSH deploy tooling. Then run POC acceptance on the owner's host; close stale draft PRs | source complete: GEN6 audit (`hud-i2e10.28`) found 37 of 37 requirements implemented; owner checks open (below) |
| T7 | Portal as a runtime feature: projection state lives in the runtime keyed by PSK identity. Delete the external-authority design: owner tokens, audit log, provider-neutral contract, `projection_authority` binary, `resident_grpc` adapter. Fold the `PROJECTION_*` error codes into the shared set. Internal interfaces may break; the MCP surface and token footprint must not regress. Owner-amended target: at most 15,000 physical Rust lines (raised from 14,000 on 2026-11-09 for portal links, selection and copy; further growth needs a new owner decision), including tests/comments/blanks, in the same four groups/eight files (portal crate, driver, windowed portal, composer), from ~45k | source complete: `tze_hud_portal`, shared `PortalHub`, direct scene renderer; portal wire fields 13–17 reserved; 15,000-line denominator measured 14,602 after `hud-ohgc9.4`; owner check `hud-lirh2.12` and reconciliation `hud-lirh2.13` open |
| T8 | Code and test diet, crate by crate (compositor, runtime, scene, protocol, input first): delete code without a current user, replace oversized test files with behavior tests tied to `invariants.md`, add `tze_hud_resource` tests, a local llvmpipe recipe for compositor tests, and strip pre-reset references (openspec, RFCs, doctrine) from comments and protos | in progress: line target not met ([`size_report.py`](../scripts/ci/size_report.py), printed by `just ci`); residual citations removed, the citation guard hardened and unjustified `allow(dead_code)` entries cleaned up; the greenfield-parity work continues under `hud-3n24e` |
| T9 | Owner-directed 2026-10-05, alongside T6 rather than after the POC. Per-monitor overlays: one window and surface per monitor sharing one scene, the default with no flag; delete `--monitor` and monitor cycling; logical (dp) token sizes scaled per monitor (bead `hud-1pt5h`). Owner-directed 2026-10-06: `[displays.<key>]` is keyed by EDID model and serial, plus the GPU connector path when two monitors are ambiguous; `/admin/status` and the log show each key (bead `hud-avr7s`). One global `ui.scale` token multiplies every logical size on top of each monitor's Windows scale factor; there are no per-monitor overrides (bead `hud-h51u7.3`) | partial: per-monitor windows, dp sizing and `ui.scale` (`hud-h51u7.3`), EDID display keys with legacy-key reporting (`hud-avr7s.2`, `.3`) and sleep/wake reassertion (`hud-jm8nq.25`) are in source; owner checks open |
| T10 | Owner-directed 2026-10-05. Swappable visual design: themes as a token layer between canonical defaults and `[design_tokens]` overrides, with Tonal Glass as the default; semantic token families (surfaces, roles, state, motion, shape, spacing, type); configurable fonts (bundled IBM Plex, fonts beside the config, Windows fonts by name); live theme and font switching from the config file and from an in-HUD picker (now the T11 control panel's Appearance section), which writes the choice back to the config (owner-directed 2026-10-06; agents never set themes); every element restyled to use tokens only (bead `hud-h51u7`). This reverses T4's removal of component profiles for themes only: themes hold token values, never layout | partial: themes, fonts and live switching (`hud-h51u7.5`) exist; zone elements and gauge/progress/status restyled in source; other restyling and owner signoffs (`hud-h51u7.4.6`, `hud-h51u7.8.6`) open |
| T11 | Owner-directed 2026-10-15. In-HUD control panel: one keyboard-first, full-width, Spotlight-style pane (search, quick actions such as Hide all, section cards that scroll vertically) of every owner-tunable mechanism (agents and trust including a copy-pastable connect prompt, visibility, appearance, layout, chords, network, diagnostics, system, every design token), each row showing its value source and whether it applies live or after a restart. The safe-mode chord opens it with agents paused; a second chord opens it without pausing; closing resumes. Hiding a surface class is session-only and silent to agents (no draw, input or raster work; state unchanged). Agents never see the panel; no MCP or gRPC change. Supersedes the T10 settings card (epic `hud-vniri`) | source complete (`hud-vniri`): shell, Visibility, Agents, Network/Diagnostics/System, Layout, chords, Appearance, tokens and Zoom; chords stay `panel_hotkey` Ctrl+Shift+F11 and `pairing_hotkey` Ctrl+Shift+F10 (owner decision 2026-11-01); answer-window row (`hud-bz9bv.5`); Windows interaction and focus checks open |
| T12 | Owner-directed 2026-11-02. Multiple-choice prompts as a choice card: notification actions up to 4, stacked labels, keyboard, chosen state; an owner-set answer window (default 1 min, control panel); Claude Code AskUserQuestion answered from the card by a PreToolUse hook. No new widget, surface or MCP field. Epic hud-bz9bv. | source complete (`hud-bz9bv.2`, `.5`); the owner accepted the measured 114/120-token baseline on 2026-11-08 (`hud-j7kxn`); hook and combined live Claude acceptance open |
| T13 | Owner-directed 2026-11-08. Autonomous mechanical validation: agents drive the real HUD binary with real OS input on displays the owner never sees, so owner checks are reserved for design, experience and owner-hardware questions. Two isolated tiers share one owned-mode scenario set in `scripts/ci/hud_gestures.py`: a local Xvfb tier (real Linux build, XTEST input, `just gesture`) and a hosted-Windows tier (a step in the existing `windows.yml` job, WARP, `SendInput`; scenarios graduated into its `GESTURES_REQUIRED` list fail `windows-build` and so withhold the `dev` prerelease, the rest stay informational). Observation is read-only, admin-only `portals`, `notifications` and `widgets` blocks, `link_launches` and `idle_since_ms` in `/admin/status` (never MCP). Assertions use observed state and readiness. Each scenario gets at most one attempt per sweep. The hosted card-link fixture can reacquire the owned click target within the original press deadline. Each intended link press allows at most one native button submission. Injection is refused outside a harness-owned display or CI VM; there is no borrowed-HUD mode. No host identifiers in tracked files. Required merge checks unchanged: a scenario graduates only through a PR citing 10 consecutive main PASS runs and leaves at once on a flake (`hud-ljhku.16`). Target: owner acceptance narrows to visual preference, owner hardware, network/install and time-based behaviour once closeout evidence shows it. Beads `hud-1dnk5.11`–`.16` under `hud-1dnk5` | tiers L and W exist; gesture scenarios run in `just gesture` and `windows.yml`; graduation to required checks needs 10 consecutive main passes (`hud-ljhku.16`); harness-found defects `hud-l82wy`, `hud-65gpz`, `hud-be6lt`. Mechanical passes are not owner acceptance |
| T14 | Owner-directed 2026-11-09 (vision pursuit; dossier https://claude.ai/artifact/YGF1BfiKtjfri3FxmQB68a). Windows experience: focus etiquette (no-activate clicks, focus return, an answer chord), portal links/selection/copy, one-call ask-and-wait, per-session input scope and card labels, outcome receipts only when non-default (clipped, displaced, unread), a lighter client layer (skill diet gated in CI, scoped waiter, user-scope install via the managed dotfiles pointing at the main checkout), discovery trims (default reply fields kept), card formulae (progress, copy, full-text reveal), widget primitives (countdown, KPI tile, stepper; elapsed timer and whole-number counts in `hud-ohgc9.7`), a per-session widget dock, a short typed reply on cards, Windows desktop presence (tray, active-monitor panel, lock-aware cards; focused-window question placement has its own owner gate; `hud-ljhku.12.2` implements it in source with per-display stacks, the full-screen exception, and the answer chord and Tab across displays, owner three-monitor, chord/Tab and full-screen checks remaining), Windows text polish, card/link/widget gesture scenarios, and an owner-pressed "Go to session" window activation (vision.md amendment in this change). Each move goes through the feature-request funnel with owner spec sign-off before code; agent-visible API moves land one after another. Epic `hud-ljhku` (`.1`–`.15`) | partial: ask-and-wait (`hud-ljhku.3`; owner accepted the 77/90-token baseline 2026-11-12), widget audiences (`.7`), HTTPS links (`.17`), outcome receipts and native unread (`.5`, R5.4 = B), gesture diagnostics (`.14`), scoped background ask (`.6.1`, [#1696](https://github.com/tzeusy-org/tze-hud/pull/1696)), tray and pause controls (`.12.1`, [#1605](https://github.com/tzeusy-org/tze-hud/pull/1605), with [Tier W registration, tooltip and measured idle evidence](https://github.com/tzeusy-org/tze-hud/actions/runs/42757137507)), hide chord and capture exclusion (`hud-rf026.5`), text polish (`.13`) and text size/caret (`hud-ohgc9.12`) and card Go to session (`.15`) are in source. The hosted `go-to-session` gesture [passed](https://github.com/tzeusy-org/tze-hud/actions/runs/45109896776). Owner checks of Windows Terminal, VS Code, a classic console, a minimized window, background tabs and tmux remain. The rest of `hud-ljhku`, `hud-ohgc9` and `hud-66nb0` is open |

## Open owner checks

Each runs on the owner's Windows host and is never a merge gate; automated and
tier-W results narrow them but do not close them.

- **Acceptance session and daily use.** `hud-aovax` (2026-11-14 packet,
  plan A): the owner-only list stays owner-run; generic daily use with 4–12
  main sessions (`hud-b5zr9`). A live Claude Code turn checks hook-driven
  session mirroring and the existing writing cue settling when activity ends
  (`hud-x38zn.1`).
- **Delivery, network and install (T6).** Tailnet firewall UX (`hud-lgb6c.3`);
  overnight overlay visibility after monitor sleep/resume (`hud-jm8nq.26`,
  repair `hud-jm8nq.25`); live smoke against the paired HUD (`hud-51ujj.7`);
  pairing, autostart, update, UAC and Explorer behaviour; the late-bind pairing
  card transition.
- **Portal (T7).** The eleven-check rebuilt-portal check (`hud-lirh2.12`, partial)
  and final reconciliation (`hud-lirh2.13`); physical typing, local echo, drag
  and held-release; the last original zone-grip clause (`hud-3to0a`); portal
  OUTPUT reading: links, selection/copy, code ranges, cursors (`hud-ljhku.2`).
- **Monitors (T9).** Display keys survive replug, cable swap and reboot
  (`hud-avr7s.4`); mixed-DPI and `WM_DPICHANGED` on the owner rig (`hud-670ds`);
  three 4K monitors at 150%, NVIDIA and DWM transparency; Windows scanout and
  multi-monitor presentation of the frame turn (`hud-ximtf.10`).
- **Visuals (T10).** Restyled zone elements, subtitle, status bar and move
  handles (`hud-h51u7.4.6`); restyled widgets and runtime chrome
  (`hud-h51u7.8.6`); the BEFORE capture for the pairing-card restyle (#1630,
  `hud-h51u7.8.3` AC3); theme, density, motion, reply cue, markdown, timestamps.
- **Control panel (T11, T14).** Windows interaction and focus, opening the
  panel on each monitor by chord and opening it from the tray, and restart
  timing. The active-monitor placement is implemented by `hud-ljhku.12.3`.
- **Choice cards (T12).** Hook and combined live Claude AskUserQuestion
  acceptance (`hud-bz9bv`).
- **Windows experience (T14).**
  - Ask-and-wait on Windows (`hud-ljhku.3`); real multi-app focus with a
    browser and the feel of focus return (`hud-ljhku.1`).
  - Tray: a real tray click focuses the newest question and Esc returns to the
    previous app,
    Win+B Enter matches the click, menu chord labels on light and dark
    taskbars, a counted-but-undrawn card opens the panel (`hud-ohgc9.5`,
    `hud-66nb0.3`, #1684).
  - Hide chord and capture exclusion with Teams, OBS, Snipping Tool,
    multi-monitor and Remote Desktop (`hud-rf026.5`).
  - IME, scroll feel, motion and Japanese system fonts (`hud-ljhku.13`); text
    size at 125% (`hud-ohgc9.12`).
  - Kept-draft composer look and Esc focus return (`hud-ohgc9.4`, #1669); hand
    cursor over each control, panel and secondary monitor (`hud-ohgc9.3`,
    #1674); link feedback wording on cards and portals (#1678); 600 ms
    destructive hold feel incl. an urgency-3 card (`hud-ohgc9.6`, #1676);
    fresh-card press guard feel (`hud-ohgc9.1`, #1685).
  - Owner merge of the press-in-progress card hold with its signed
    invariants §1 text (`hud-ohgc9.13`, #1686).
  - HTTPS link default-browser foreground and card stays (`hud-bz9bv.8`).
  - Five-hour usage gauge: genuine Claude recordings and Windows
    fill/readout/hover/caution/clear and status-line latency (`hud-o4ixb.3`).
  - Overlay shell hygiene: virtual desktops, Alt+Tab, Alt+F4 (`hud-66nb0.1`).
  - Client layer (`hud-ljhku.6`): skill diet, the `install-tze-hud.sh` run from
    ai-bootstrap (PR63 merged), the fresh five-tool cross-repo discovery check,
    and MCP adapter acceptance.
- **Time-based.** Sleep/wake, hot-plug and overnight leases.
- **Measurement holds.** Publish-to-present on the Windows host
  (`hud-ddm2g`); merge-group 6-minute target misses (`hud-4k3b2`) and the
  owner merge of the #1648 merge-budget proposal (`hud-rnad7`); the
  delayed-subtitle witness (`hud-rnq52`, `hud-q26ya`).

## Where the rest lives

| Project question | Current home |
|---|---|
| Why, boundaries and principles | [vision.md](vision.md) |
| API and design contracts | [api.md](api.md), with wire definitions in [protocol](../crates/tze_hud_protocol/proto/) |
| Required behavior and regression evidence | [invariants.md](invariants.md) |
| Components, ownership and data flow | [architecture.md](architecture.md) |
| How to change and verify it | [README](../README.md), [AGENTS.md](../AGENTS.md), [justfile](../justfile), [Windows development](development/windows.md) |

Archived doctrine, RFCs and OpenSpec are history at tag `pre-reset-2026-10-02`,
not requirements; a th-projects shape scan reports `UNSHAPED` for that reason.

## POC acceptance

The POC is done when this passes on the Windows overlay, each stage in one or
two calls within its [api.md](api.md) token budget:

- **Portal:** a session attaches, streams output, receives a typed reply
  (local echo, no round trip), and detaches; an abandoned portal is reclaimed.
- **Zones:** a notification shows for its TTL and disappears unattended;
  `delay_ms` content appears on schedule; a notification action reaches `hud_input`.
  The [latest-wins count guard](../crates/tze_hud_compositor/src/renderer/tests/zone_stack.rs)
  checks warm/unchanged/one-zone-update/unchanged shaping work (2/0/1/0).
  These are frame-path shaping counts, not all CPU layout or presentation costs;
  zero widget raster/upload work still leaves full-frame rendering and GPU work.
- **Widgets:** a typed parameter update re-rasterizes only that widget's SVG
  (the frame is still re-presented in full; no damage tracking).
- **Override:** safe mode and dismiss win with a hung agent.
- **Idle:** an idle HUD with content on screen uses ~0% CPU and redraws nothing.

After the POC passes, use it before planning more.

Adopt windowed damage tracking only if `hud-w2t3x` / `hud-ddm2g`
publish-to-present measurements identify full re-present as the bottleneck;
`hud-ko552`'s historical ~5 µs per `collect_text_items` call measures isolated
text collection, not a frame or present cost.

Public agent-owned tiles were removed on 2026-10-14 for lack of a user.
The deferred spec and entry criterion remain in hud-x1wuq; internal scene
tiles, leases and native portal rendering remain.

## Merge gate and testability

The merge queue is the development loop, so it is held to targets, not just
kept green. Changing a required check updates the `main merge queue` ruleset,
the [`ci.yml`](../.github/workflows/ci.yml) header and this section together.

Linux compile jobs install a SHA256-pinned mold linker through
[`install_mold.py`](../scripts/ci/install_mold.py).

- **Fast.** A merge-group run's required checks finish within 6 minutes of the
  run starting. Suites that cannot fit run in the weekly lanes
  (`perf-budget.yml`, `perf-assert.yml`) or in `windows.yml`, never in the
  required set.
- **Not queued.** Wall time is run time plus the wait for a hosted runner, and
  every job, PR push, merge-group build and main push draws on one shared
  pool. Required checks are three jobs (`static checks`, `cargo clippy`,
  `cargo test`); a new gate is a step in one of them, not a new job.
  Superseded PR runs are cancelled, and `windows.yml` keeps its
  informational idle measurement off PRs and runs its theme captures only
  when a PR touches rendered paths.
- **Repeatable.** A correct change passes on the first attempt. Nextest runs
  with `retries = 0`; a per-test retry override in
  [`.config/nextest.toml`](../.config/nextest.toml) names its reason and is
  tracked debt, not a fix.
- **Unattended.** No required check waits on a person, the owner's hardware or
  a live HUD. Pixels are checked by machine on llvmpipe (`just test-gpu`,
  `just render-scene`); the Windows build and WARP smoke in `windows.yml` are
  informational. Owner-host checks, including POC acceptance above, are never
  a merge gate.
- **Mirrored locally.** `just ci` runs the required gates in CI order;
  `just pre-push` runs the host-identifier check, fmt and the exact CI clippy command.

The size gate compares tracked physical lines against the branch merge base
locally and the tested merge's base in PR and merge-queue CI. It fails when a
mapped surface and metric grow and finish above their target plus signed feature
allowances. An over-target count may stay flat or shrink. The size report shows
physical totals for all surfaces and distance to target where a ceiling applies.
The [target table](../scripts/ci/size_targets.toml) maps the greenfield sketch's
logical-line estimates to current report surfaces. Each allowance cites the
owner-signed feature spec that granted it.

Open work toward these targets: `hud-1dnk5` (`just dev-run`, Windows-first
quickstart, capture diff) and `hud-51ujj.7` (live smoke against the paired
HUD).

## Working rules

- Prefer deleting over generalizing. New abstractions need a current user.
- Tests guard behavior a user would notice, not internal structure.
- Removals and redesigns must keep every contract in `invariants.md`; changing
  one is a deliberate decision recorded there, with its tests.
- Keep `docs/` compact: vision, current scope, architecture, invariants, API,
  quickstart, development guidance and operational runbooks. Put investigation
  logs in PR descriptions; retain only reusable operational decisions in runbooks.
- Behavior changes update the affected API/invariant/operational docs in the same
  change. Status changes update this file with source evidence and any remaining
  owner acceptance; do not restore the retired doctrine/spec trees.
