# Scope and implementation status

The project is being cut back from a general "agent presence engine" to an
MCP/gRPC layer for the HUD real-estate lifecycle (see [vision.md](vision.md)).
Tranches T0–T4 removed what doesn't serve that; T5 redesigned the API that
remains. T6–T8 prove the result on Windows and shrink the code to match.
T9–T14 are owner-directed additions: per-monitor overlays, a swappable
visual design, an in-HUD control panel, choice cards, autonomous
mechanical validation, and the Windows experience pursuit.
Each tranche must still build, pass tests, and boot the overlay.

Source snapshot: `855f8397` (latest `origin/main` fetched on 2026-10-13).
The tranche descriptions below retain the approved direction; the status and
[evidence](#current-implementation-evidence) distinguish implemented pieces
from outstanding work. [Observed] means inspected source or tracker state,
[Inferred] means a conclusion from it, and [Unknown] means unverified here.
This refresh checked documentation against source; it did not run Windows
acceptance or certify the performance targets. Update this snapshot and its
source links when implementation changes the status.

## Keep

| Area | Where |
|---|---|
| Overlay window, compositor, text/markdown, images | `tze_hud_runtime` (windowed), `tze_hud_compositor` |
| Scene graph: tabs, tiles, nodes, zones | `tze_hud_scene` |
| Session portal + projection | `tze_hud_portal`, `tze_hud_runtime` portal modules, `tze_hud_input` |
| MCP tools (zones, widgets, portal) | `tze_hud_mcp` |
| gRPC resident session (zones, widgets, uploads, events) | `tze_hud_protocol` |
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
| T6 | Windows delivery without SSH, then prove the POC on it. (1) CI builds `x86_64-pc-windows-msvc` natively on a Windows runner, boots it under WARP with an MCP smoke, and publishes minisign-signed releases (rolling `dev` from main, `v*` tags). (2) The exe self-installs per user (`%LOCALAPPDATA%`, `HKCU` Run autostart, no admin). (3) It listens on loopback and the Tailscale address only (owner reaffirmed 2026-10-06: no loopback-only default, no relay). The HUD detects when Windows Firewall blocks tailnet inbound and says so on the pairing card and in `/admin/status`; an opt-in elevated helper adds one program-scoped rule (bead `hud-lgb6c`). (4) First-run pairing: the HUD shows a one-time code, an agent host trades it for a per-agent PSK (`POST /pair`; only a hash is stored); delete `--psk`, the default PSK, and `--bind-all-interfaces`. (5) `/admin/*` operator endpoints (status, logs, screenshot of the HUD's own frame, restart, signed pull-only self-update with health handoff), off the MCP surface. (6) Rewrite `user-test`/`hud-projection` around the paired PSK; delete SSH deploy tooling. Then run POC acceptance on the owner's host; close stale draft PRs | delivery paths implemented; GEN6: 36 implementation rows + 1 historical owner record; diagnostic, logging and mutex repairs merged; current acceptance holds retained |
| T7 | Portal as a runtime feature: projection state lives in the runtime keyed by PSK identity. Delete the external-authority design: owner tokens, audit log, provider-neutral contract, `projection_authority` binary, `resident_grpc` adapter. Fold the `PROJECTION_*` error codes into the shared set. Internal interfaces may break; the MCP surface and token footprint must not regress. Owner-amended target: at most 15,000 physical Rust lines (raised from 14,000 on 2026-11-09 for portal links, selection and copy; further growth needs a new owner decision), including tests/comments/blanks, in the same four groups/eight files (portal crate, driver, windowed portal, composer), from ~45k | source implemented: `tze_hud_portal`, shared `PortalHub` and direct scene renderer; portal-only wire fields 13–17 are reserved; final owner acceptance and cross-child reconciliation remain |
| T8 | Code and test diet, crate by crate (compositor, runtime, scene, protocol, input first): delete code without a current user, replace oversized test files with behavior tests tied to `invariants.md`, add `tze_hud_resource` tests, a local llvmpipe recipe for compositor tests, and strip pre-reset references (openspec, RFCs, doctrine) from comments and protos | in progress: resource behavior tests and llvmpipe recipes exist; diet and reconciliation continue |
| T9 | Owner-directed 2026-10-05, alongside T6 rather than after the POC. Per-monitor overlays: one window and surface per monitor sharing one scene, the default with no flag; delete `--monitor` and monitor cycling; logical (dp) token sizes scaled per monitor (bead `hud-1pt5h`). Owner-directed 2026-10-06: `[displays.<key>]` is keyed by EDID model and serial, plus the GPU connector path when two monitors are ambiguous; `/admin/status` and the log show each key (bead `hud-avr7s`). One global `ui.scale` token multiplies every logical size on top of each monitor's Windows scale factor; there are no per-monitor overrides (bead `hud-h51u7.3`) | partial: overlay windows exist; logical (dp) sizing and `ui.scale` landed (`hud-h51u7.3`); stable keys remain |
| T10 | Owner-directed 2026-10-05. Swappable visual design: themes as a token layer between canonical defaults and `[design_tokens]` overrides, with Tonal Glass as the default; semantic token families (surfaces, roles, state, motion, shape, spacing, type); configurable fonts (bundled IBM Plex, fonts beside the config, Windows fonts by name); live theme and font switching from the config file and from an in-HUD picker (now the T11 control panel's Appearance section), which writes the choice back to the config (owner-directed 2026-10-06; agents never set themes); every element restyled to use tokens only (bead `hud-h51u7`). This reverses T4's removal of component profiles for themes only: themes hold token values, never layout | partial: startup themes/fonts and live switching (config file and control panel; `hud-h51u7.5`) exist; `ui.scale` landed (`hud-h51u7.3`); restyling remains |
| T11 | Owner-directed 2026-10-15. In-HUD control panel: one keyboard-first, full-width, Spotlight-style pane (search, quick actions such as Hide all, section cards that scroll vertically) of every owner-tunable mechanism (agents and trust including a copy-pastable connect prompt, visibility, appearance, layout, chords, network, diagnostics, system, every design token), each row showing its value source and whether it applies live or after a restart. The safe-mode chord opens it with agents paused; a second chord opens it without pausing; closing resumes. Hiding a surface class is session-only and silent to agents (no draw, input or raster work; state unchanged). Agents never see the panel; no MCP or gRPC change. Supersedes the T10 settings card (epic `hud-vniri`) | partial: shell landed (two chords, search, quick tiles, keyboard and pointer, Agents/Input/System rows; `hud-vniri.2`); Visibility rows, Hide all and the hidden-content pill landed (`hud-vniri.4`); the agent roster with Clear, Unpair and allow-list grants, and Connect an agent with its copyable prompt, landed (`hud-vniri.6`); Network, Diagnostics and System rows (ports staged behind Restart now, live log level, zone outlines and frame cap, firewall, update, autostart; value sources from flags, env and `[runtime]`) landed (`hud-vniri.7`, part A); Layout (widget box and clears-after, live and saved; display zones read-only) and live chord rebinding landed (`hud-vniri.7`, part B); Appearance (theme, fonts) and Advanced tokens (every token, Reset all overrides), live and saved, landed (`hud-vniri.5`); Zoom (`ui.scale`, live and saved, widgets included) landed (`hud-h51u7.3`); the display profile and window show read-only with their source; the chords keep F-key defaults under `panel_hotkey` (Ctrl+Shift+F11) and `pairing_hotkey` (Ctrl+Shift+F10), not the epic's Ctrl+Shift+P under `settings_hotkey` (owner decision 2026-11-01: a global letter chord steals editors' command palette); the header state line counts agents and holdings and flags a pending restart, whose Restart now stays in System (one place to restart; the mockup's header has none), and the panel keeps its scrim whether or not agents are paused (the pane always owns the pointer over it); a changed MCP port asks first; changed rows offer Reset (Delete or the row's link) where the panel can restore the default live (Visibility, Appearance, tokens, chords, Diagnostics), while ports and widget boxes take a typed value (a port Reset would skip the MCP confirmation; a widget's default box is its asset's); the theme previews on Left/Right and Alt+T, Enter keeps it, leaving restores it, and a click keeps at once (the spec's "preview on focus, commit on Left/Right or Enter" assumed an option list; the row is a ‹ › choice, so the keyboard step is the focus and Enter the commit); chord rows name their source; the Connect firewall line links to Network › Remote agents; restart timing, UAC, autostart and Explorer checks are Windows owner checks; Agents "Questions wait for an answer" row and shared live saved `[runtime] answer_window_ms` source implemented (`hud-bz9bv.5`), with config, panel and both-plane lifetime test coverage; Windows interaction acceptance outstanding; Windows focus check outstanding |
| T12 | Owner-directed 2026-11-02. Multiple-choice prompts as a choice card: notification actions up to 4, stacked labels, keyboard, chosen state; an owner-set answer window (default 1 min, control panel); Claude Code AskUserQuestion answered from the card by a PreToolUse hook. No new widget, surface or MCP field. Epic hud-bz9bv. | partial source: Agents "Questions wait for an answer" row, 1 min default, live saved 1–300000 ms setting and both-plane omission defaults implemented (`hud-bz9bv.5`); config, panel and both-plane lifetime tests cover the source contract. Four-action admission, retained shaped layout, keyboard traversal and chosen exit are implemented in the current-main source port (`hud-bz9bv.2`), with focused choice and precedence parents plus canonical FIFO/ACK integration passing; two matching complete-flow calibrations measure 114/120 tokens, and the owner accepted the existing measured baseline on 2026-11-08 (`hud-j7kxn`) without changing any ceiling; all nine `hud-bz9bv.2` owner acceptance criteria are complete; hook and combined live Claude acceptance remain outstanding |
| T13 | Owner-directed 2026-11-08. Autonomous mechanical validation: agents drive the real HUD binary with real OS input on displays the owner never sees, so owner checks are reserved for design, experience and owner-hardware questions. Two isolated tiers share one owned-mode scenario set in `scripts/ci/hud_gestures.py`: a local Xvfb tier (real Linux build, XTEST input, `just gesture`) and a hosted-Windows tier (an informational step in the existing `windows.yml` job, WARP, `SendInput`). Observation is read-only, admin-only `portals`, `notifications` and `widgets` blocks, `link_launches` and `idle_since_ms` in `/admin/status` (never MCP); assertions are state-first, readiness is observed rather than slept, and nothing retries. Injection is refused outside a harness-owned display or CI VM; there is no borrowed-HUD mode. No host identifiers in tracked files. Required checks unchanged. Target: owner acceptance narrows to visual preference, owner hardware, network/install and time-based behaviour once closeout evidence shows it. Beads `hud-1dnk5.11`–`.16` under `hud-1dnk5` | partial: tiers L and W exist and pass on `f4480c11` (2026-11-08). The admin-only `/admin/status` portal state and idle clock (`hud-1dnk5.12`) back portal assertions. `hud-ljhku.14` ([PR1600](https://github.com/tzeusy-org/tze-hud/pull/1600)) adds last-frame card and widget geometry, a link-handoff counter, and shared click/callback, composer-to-card keyboard, dismiss, link/rate-limit and widget-hover scenarios. At `d9a81c1e`, tier L passes 20/20 in 133.0 s; the matching-tree [Windows run42675340749](https://github.com/tzeusy-org/tze-hud/actions/runs/42675340749) passes 22/22 fullscreen scenarios in 203.81 s and 1/1 overlay scenario in 5.05 s, with no harness failures or short input sends. These are mechanical results, not owner visual or hardware acceptance. Tier L, `just gesture` (`hud-1dnk5.14`): 13/13 scenarios PASS in 34 s on a release-dev build, also as two parallel runs on distinct displays and ports. Tier W (`hud-1dnk5.15`), an informational `windows.yml` step with real `SendInput` on a hosted VM (WARP, fullscreen): 15/15 PASS in 36 s on the main push run, adding no-focus-steal (10), the safe-mode chord (12) and Unicode typing. Closeout (`hud-1dnk5.16`) pre-verified the mechanical parts of `hud-lirh2.15` AC2 (anchor through append and head-trim, unread, jump-to-latest, follow-tail, opaque badge) and `hud-lirh2.12` items 3 and 5 plus parts of 7, 8 and 10, and handed the evidence to the main looper. That safe-mode run toggled the real chord and checked refusal/recovery, but paused only about 16 ms with a fresh hold and input poll; it did not prove expiry suspension or no reap on exit. The shared harness in `hud-1dnk5.21` adds stream cue settlement and real close/reattach scenarios, strengthens idle stale/dim/recovery/reap, pointer and Ctrl+Shift+Arrow resize/republish, and measures a 65 s unheld pause plus 5 s after resume before any refresh. Actual tier results belong in that PR's evidence; source coverage alone is not a Windows pass. A real Linux run exposed OUTPUT clicks leaving composer focus active; the input path now dispatches tile-background clicks through the existing focus transition and preserves the unsent draft. Overlay scenario 11 (`hud-1dnk5.20`) adds a hash-pinned lavapipe setup after the fullscreen WARP run, relative pointer entry and click-through into owned Notepad; the matching-tree hosted run above passes that overlay mechanical check. Visible desktop composition and owner hardware acceptance remain separate. Remaining owner checks otherwise: visual and experience (theme, density, motion, the reply cue, markdown and timestamps), owner hardware (three 4K monitors at 150%, NVIDIA and DWM transparency, per-monitor DPI, long-uptime overlay visibility), network and install (pairing, tailnet, firewall, autostart, updates), time (sleep/wake, hot-plug, overnight leases), real multi-app focus with a browser, the feel of focus return (`hud-ljhku.1`, epic closeout item 3), and the client-side T12 and MCP adapter acceptance. Defects the harness found: `hud-l82wy`, `hud-65gpz`, `hud-be6lt` |
| T14 | Owner-directed 2026-11-09 (vision pursuit; dossier https://claude.ai/artifact/YGF1BfiKtjfri3FxmQB68a). Windows experience: focus etiquette (no-activate clicks, focus return, an answer chord), portal links/selection/copy, one-call ask-and-wait, per-session input scope and card labels, outcome receipts only when non-default (clipped, displaced, unread), a lighter client layer (skill diet gated in CI, scoped waiter, user-scope install via the managed dotfiles pointing at the main checkout), discovery trims (default reply fields kept), card formulae (progress, copy, full-text reveal), widget primitives (countdown, KPI tile, stepper), a per-session widget dock, a short typed reply on cards, Windows desktop presence (tray, active-monitor panel, lock-aware cards; focused-window question placement has its own owner gate), Windows text polish, card/link/widget gesture scenarios, and an owner-pressed "Go to session" window activation (vision.md amendment in this change). Each move goes through the feature-request funnel with owner spec sign-off before code; agent-visible API moves land one after another. Epic `hud-ljhku` (`.1`–`.15`) | partial: model/owner/runtime widget parameter audiences and model-only discovery landed (`hud-ljhku.7`); HTTPS link guidance landed (`hud-ljhku.17`). Card/link/widget diagnostics and gesture scenarios are implemented and mechanically verified (`hud-ljhku.14`, PR1600; tier L and actual tier W results above); required-CI graduation (`hud-ljhku.16`), other moves and whole-outcome acceptance remain |

## Current implementation evidence

- **Client teaching checks (`hud-ljhku.6`, PR1) [Source].** The two HUD
  skills' Markdown JSON examples require adjacent tool or exemption annotations;
  a Cargo test applies the production MCP argument parser and reports file:line
  on failures. The token artifact and checker report complete skill-file o200k
  costs and model tool calls per canonical flow without changing existing
  payloads or budgets. Skill ceilings remain unarmed until PR2; the skill diet,
  scoped background ask (after `.3` and `.4` land), separate managed-dotfiles PR
  and live cross-repo discovery evidence remain. This is partial member scope,
  not whole-client acceptance or proof that earlier PRs ran the new check.

- **T8 — input pipeline diet [Source].** `hud-bstmy.4.5` removes the unused
  native batching/coalescing/queue/envelope modules after the final T7 source
  landing (`hud-lirh2.11`). Invariant 2 now names the real resident input bus:
  transactional input retains order; ephemeral samples can be dropped on lag.
  Local scroll and the existing portal scroll fixture use live scroll state.
  Production dispatch and delivery semantics are unchanged. T7 Windows owner
  acceptance (`.12`) and final cross-child reconciliation (`.13`) remain separate.

- **Zone grips — session placement [Source].** `hud-3to0a` routes existing
  notification-zone grips through scene-owned transient placement and shared
  effective geometry. The existing human Reset restores defaults; clear,
  expiry and republish retain placement, while restart restores defaults.
  Primary/unassigned/disconnected fallback grips are retained; connected
  secondary content/actions keep their existing no-grip behavior. Display
  assignment remains read-only. No saved-zone feature, new agent event,
  secondary/widget grip or restyle is introduced. Six existing parents cover
  the source contract, including actual-render/input red→green; the original
  Windows acceptance now covers movement, drop/action, cancellation and
  session/restart lifetimes on the repaired build (14/15 original parent
  clauses); the remaining parent clause is not inferred from source tests.
  At that repair the unchanged T7 four-group/eight-file denominator measured
  13,995 physical Rust lines including tests/comments/blanks (was 13,918; +77).
  The final D5 inventory below includes subsequent portal repairs and records
  the owner-approved 14,000 cap. Owner .12 acceptance and .13 reconciliation
  remain distinct from those source changes.

- **F7 — HTTPS link actions [Source].** `hud-bz9bv.8` adds shared strict
  target validation, indexed local opening, token-derived marks and unlocked
  worker handoff. Windows default-browser foreground/card-stays acceptance
  remains unexecuted.

- **T6 — delivery [Observed].** The [Windows workflow](../.github/workflows/windows.yml)
  builds, tests, smoke-checks, and publishes signed releases. The runtime has
  [self-install](../crates/tze_hud_runtime/src/operator/install.rs),
  [pairing](../crates/tze_hud_runtime/src/pairing.rs),
  [operator routes](../crates/tze_hud_runtime/src/http.rs) (`route`),
  [restart handoff](../crates/tze_hud_runtime/src/operator/handoff.rs),
  [signed update](../crates/tze_hud_runtime/src/operator/update.rs), and
  [firewall diagnostics/helper](../crates/tze_hud_runtime/src/firewall.rs).
  Both transports use the shared [listener manager](../crates/tze_hud_runtime/src/net_addrs.rs)
  to follow every later Tailscale address via OS notifications and retry only
  missing binds, including failed restart/update handoff binds. The deterministic
  `listener_lifecycle_follows_changes_and_retries_only_missing_binds` gate covers
  address events, partial failures, deduplication and absence of idle polling;
  static address selection remains a separate gate. Existing MCP fan-out and
  gRPC handoff fixtures additionally inject a late v6 address and failed-then-
  successful bind, then connect to the real transport over IPv6 loopback;
  MCP also verifies that status lists only successful binds. Pairing uses each
  transport's successful bind list independently, with `grpc: null` before a
  gRPC listener is adopted; the pairing fixture covers unavailable and later
  successful gRPC addresses. A successful late MCP bind refreshes an eligible
  open startup/`--pair` system card with its original code and deadlines; the
  pairing and MCP fixtures cover that update and stale/expired-card suppression.
  This source evidence does not establish the displayed transition on Windows.
  The real [runtime capture gate](../crates/tze_hud_runtime/src/windowed/event_loop_harness.rs),
  `building_an_idle_frame_leaves_the_idle_gate_inputs_unchanged`, checks that
  capture leaves the primary scene/geometry and presentation bookkeeping unchanged,
  then an ordinary scene change still presents. The [operational retirement guard](../.claude/skills/user-test/tests/test_skills_docs.py),
  `test_operational_sources_do_not_use_retired_transports_or_doc_psks`, covers
  supported skill mirrors, scripts, workflows, `justfile` and `.mcp.json`.
  T6-only independent GEN4 audit (`hud-i2e10.24`, 2026-11-03) examined actual
  merge `9fc3114976b9e8fd2fc8c7b19e8895880fe83555`, tree
  `6a7bcebf6eb43173aad14f3ef6d11137171214aa`, against all 37 original
  requirements and current source bodies, callers and contracts. Its qualified
  matrix is 36 implemented, 0 partial, 0 missing and 1 deviation
  (36 I / 0 P / 0 M / 1 D). That qualified handoff actually merged in
  [PR #1548](https://github.com/tzeusy-org/tze-hud/pull/1548) as `313d0e28`,
  tree `613ea2bf19c25d4c7fc78642f880162aeaed09a5`.
  The mutex-error producer and every startup, install
  and handoff consumer are repaired by `hud-i2e10.23`
  ([PR #1547](https://github.com/tzeusy-org/tze-hud/pull/1547)), actually merged
  at that commit. Native Windows `41651272937` attempt 1 exercised a real kernel
  object-name collision: startup exited 1 and installation refused staging and
  registration. Normal install, bare duplicate exit 0, explicit duplicate exit 2,
  restart and uninstall also passed. Required CI `41651273086` attempt 1 passed
  on the same effective tree, including the actual acquisition-error consumer
  parent and 2,838 workspace tests with 0 skips.

  The log-location repair `hud-i2e10.25` actually merged in
  [PR #1549](https://github.com/tzeusy-org/tze-hud/pull/1549) as
  `fcf228afdd66b4e42a3241a703fc1d28870627c6`, tree
  `efc8e5c41f81c62daa12637b2f55145db5a8c028`. It fulfills the original
  `hud-i2e10.7` in-place config-adjacent log condition: one normal startup config
  selection is reused by loading and logging; actual installed-executable
  identity selects installed defaults, while in-place runs use the selected
  config's parent. Relative paths resolve once, nonempty ordinary overrides win,
  empty overrides are unset, and the diagnostic override remains independent.
  Tracing, default panic diagnostics, admin log tails and panel Open logs share
  that process choice. Helper successes and refusals stay ahead of ordinary
  logging; rotation, the 2,000-line tail cap, live file level, stdout, install,
  duplicate/mutex and handoff behavior remain unchanged.

  Local receipts at `e657c190`, tree
  `62c665ce1b0f55a74e4429e664a36fda8e3fed08`, passed 17 app tests, 4 rotation/tail
  tests and 1 admin-tail test, plus source-bound workspace check, pre-push and the
  ordinary push hook. Native Windows `41663759685` attempt 1 actually checked out
  `4992fe0e`, with that same entire tree, and passed the existing MSVC runtime/app
  parents, real log-directory output, admin/restart/install/duplicate and mutex
  refusal checks. Required CI `41663759868` passed separately. The landed tree
  differs only in `docs/scope.md`; those results retain their tested-tree
  attribution and are not executions of the GEN5 documentation checkout.

  Historical GEN5 (`hud-i2e10.26`) found **36 implemented, 1 partial,
  0 missing and 0 deviations**. Its T6-36 finding was an unmet original
  `hud-lgb6c.1` condition: COM/enumeration failures could become an incomplete
  successful diagnostic snapshot. Disclosure in the helper record did not waive
  that condition. Repair `hud-i2e10.27` landed in
  [PR #1551](https://github.com/tzeusy-org/tze-hud/pull/1551), commit `25372a24`.
  The diagnostic now accepts normal enumeration exhaustion only for
  `S_FALSE` with zero fetched items, requires `S_OK` with exactly one item,
  and preserves required getter, variant, null-interface and cast failures as
  `Unknown`. TCP/UDP require LocalPorts; protocol-inapplicable LocalPorts remain
  valid. The explicit protected and reversible helper retains its separate full
  rule-image reader and mutation boundary.

  Fresh independent GEN6 (`hud-i2e10.28`) read the original epic and all
  governing contracts, all 37 original requirements and their production callers,
  the full 3,334-record namespace, and the finite operational/auth/bind/admin/
  logging/firewall/capture/update/helper/CI universe at `25372a24`, tree
  `6ae341393beef9b52767c951a43ccc3f8db69b07`. Its qualified source result is
  **37 implemented, 0 partial, 0 missing and 0 deviations**. T6-37 is historical
  owner-evidence bookkeeping only. The operational census covers 154 logical
  paths backed by 96 physical files, with 47 exclusions and no retired transport,
  documented PSK or removed-tool matches. This source result does not close the
  separate current owner, witness or provider holds below.

  The existing `com_error_is_unknown` parent was amended, with no new test
  species. Saved local gates at reviewed `27007d44`, tree
  `2389a948745894882cfe49df7f4c50e0df8fab09`, and native Windows
  `41680170937` attempt 1 preserve their actual tested-tree identities. Native
  checkout was synthetic `1de703e6` with that same tree: the MSVC build, amended
  diagnostic parent, unchanged standalone COM helper parent, 575 runtime tests
  and two parser tests passed. Its later subtitle smoke failed as retained
  below; later install/theme/staging steps were skipped. Required CI
  `41680170408` passed separately. Only `docs/scope.md` differs between the
  reviewed/tested and landed trees. Semantic source/fixture/workflow equality
  does not make the current checkout, build SHA or all-tracked source-guard
  identity equal. GEN6 ran no product gate in its read-only audit.

  The GEN2 read-only audit (`hud-i2e10.18`, at `af06ca8a`) found absolute
  handoff-deadline and ordinary paired-client IPv6 HTTP-origin gaps. Their repairs
  landed as `hud-i2e10.19` (`7d30e984`,
  [PR #1544](https://github.com/tzeusy-org/tze-hud/pull/1544)) and
  `hud-i2e10.20` (`315a4149`,
  [PR #1543](https://github.com/tzeusy-org/tze-hud/pull/1543)); GEN6 reread both
  repaired bodies and their actual restart/update and paired-client callers.
  The accepted `.18` selected receipts ran at `099f28a2`, the same tree
  `87bcf10db4ed7d463c82dc58b91ed3167d9bde6a`: 9 native tests, 11 Python tests
  and 8 soak unittests passed. Original `.18` AC2/3 were fulfilled, not transferred.

  The two existing docs guards separately passed at `05dfc54c`, tree
  `7dfa72651bc334ebded6b6c008eadc00d3647583`; the preceding bare-Python launcher
  failure exited 1, executed zero test bodies and produced no JUnit. That old
  success is distinct from the current guard inputs: the corpus includes changed
  `scripts/ci/windows_install_smoke.py` and skill prose/mirrors, covered by the
  actual current required `static checks`. GEN5 added and ran no tests; the
  initial GEN6 audit likewise ran none. This scope-only evidence update lies
  outside the guards' input roots and changes no implementing, test, Cargo,
  protobuf, schema, CI or Nextest blob; no duplicate gate run is
  required. None of those old runs executed this different documentation tree.
  The soak unittests do not establish an overnight physical run. Other repair receipts
  retain their own tested heads; inspection, merged repairs and green checks
  alone do not establish full coverage.

  The provider record retains the initial 460 s miss in run `41523755581`.
  Required checks all succeeded on attempt 1, but the later `.19` merge group took
  630 s and the `.18` docs merge group took 411 s, exceeding the 6-minute
  requirement. Later PR #1546 merge group `41650500131` attempt 1 completed
  its required checks in 196 s, and PR #1547 merge group `41654124481` attempt 1
  in 197 s. PR #1548 group `41667340421` attempt 1 completed all three required
  checks in 218 s, and PR #1549 group `41668443416` attempt 1 in 233 s.
  PR #1550 group `41683139231` and PR #1551 group `41683496773`, both
  attempt 1, completed all three required checks in 197 s and 218 s respectively.
  Existing `hud-4k3b2` retains the failures and provider evidence hold against
  the unchanged 360 s requirement. The later successes establish neither a
  provider cause nor a durable scheduling fix, and no waiver is claimed.

  At `05dfc54c`, required CI `41591610223` attempt 1 reported all three checks
  successful, while optional Windows `41591610053` attempt 1 failed the delayed
  subtitle witness (192 pixels versus 200). Later docs-only effective checkout
  `008d49d7`, tree `389322eff456dd36e11146a4fac2986443b2ba7e`, passed the
  unchanged witness with 9,009 pixels in optional Windows `41598409928` attempt 1;
  required CI `41598409915` also passed. No runtime or harness correction is
  inferred. The later success does not erase the original failure, diagnose its
  cause, waive criteria or establish physical acceptance. Existing
  `hud-rnq52`/`hud-q26ya` retain those follow-ups.

  The later PR #1551 optional Windows run `41680170937` attempt 1 also
  failed the original first delayed-subtitle witness at 192 pixels versus 200,
  on actual checkout `1de703e6`, tree `2389a948745894882cfe49df7f4c50e0df8fab09`.
  Its original PNGs and timing artifacts are retained. The trace supports a
  first capture during an incomplete fade; a complete cause and production
  correction remain unproved. The aggregate failure, positive native parents,
  separate required checks and actual PR merge remain distinct. On 2026-11-04
  the owner answered `hud-q26ya` FORM C (canonical O2): only the no-extra-waits
  restriction may change to a bounded condition wait tied to this publication's
  existing fade before the one original late PNG. The >=200 assertion, one
  publication, original delay/TTL, current-alpha capture and user fade remain.
  This changes proof from near-due to settled; the source now checks numeric identity and readiness before that PNG;
  source checks, the first automatic native result and physical acceptance remain separate. It does not
  explain the production cause, authorize replay or establish physical acceptance.

  PR #1553 optional Windows run `41767996217` attempt 1 likewise failed the
  original delayed-subtitle witness at 192 pixels versus 200. ROOT bound reported
  head `d7edbc8e` to actual checkout `c68dfb56c941d717997966d8dc0b0b06a179bad5`
  (parents `90b5cbf6` and `d7edbc8e`), tree
  `a6d31afba904cf610787d4a0bfa38c5be312a0f5`, equal to reviewed and merged
  `375fa96d`. Native runtime 576 and parser 2 passed before Boot smoke failed;
  later quiescent/install/theme/artifact/release steps were skipped. Required
  merge-group `41769390774` attempt 1 passed all three checks in 183 seconds.
  These remain separate outcomes; no production-cause finding, native witness
  pass or physical acceptance is inferred. The existing `hud-rnq52` watch remains.

  PR #1555's first natural Windows run `41775443601` attempt 1 failed before
  its original late PNG or 200-pixel comparison. Actual checkout `9127c4a8`, tree
  `d82e065f7a2dda6d3586891cfb5fcc72e1ee2724`, matches reviewed `77335c5d`.
  Build/runtime/parser passed; boot smoke rejected the runtime due time as
  173.25 microseconds beyond an extrapolated client wall-clock upper bound.
  Later validation/release steps were skipped; the original warm/baseline/early
  PNGs and numeric trace are retained. That guard assumed an unsupported affine
  relation between a prior UTC observation and later `perf_counter` readings.
  The source correction validates delay/TTL from the producer's exact scheduling
  sample, with client deadlines remaining monotonic. The first native guard
  failure remains FAIL, not a 192-pixel result or an O2 native pass. Its OS clock
  mechanism and the historical production undercount remain unproved; required
  source checks and physical owner acceptance remain separate.

  This source/automated evidence does not establish current Windows owner acceptance.
  `hud-i2e10.13` is closed with six historical owner passes and two decisions
  recorded; those results do not accept the current host or a new build. Separate firewall
  owner checks (`hud-lgb6c.3`) remain open, and overnight POC validation
  (`hud-jm8nq.26`) remains blocked. The current owner host is hibernating;
  no new physical acceptance, owner obligation or host probe is claimed here.
  Existing owner decisions and activation holds remain separate and unchanged.
- **Five-hour usage gauge [Source; native acceptance pending].** The optional
  [status-line helper](../.claude/skills/hud-projection/references/claude-code-usage.md)
  publishes typed gauge parameters directly with zero model tokens and a silent,
  detached one-second total work budget. It keeps bounded desired/attempt/ACK
  generations, one live delivery owner and no same-generation uncertain retry.
  Owner-approved option C permits a later distinct percentage/window/clear
  transition after ownership releases. Old server work can still overwrite a
  newer window, resurrect cleared content, or remove newer content indefinitely;
  unchanged input does not repair it and no finite convergence is promised.
  Real-helper/fake-HTTP and Bash fixtures are synthetic source evidence; genuine
  Claude recordings/cancellation/reset and Windows fill/readout/hover/caution/
  clear/status-line latency acceptance remain unexecuted (`hud-o4ixb.3`).
- **T7 — portal [Observed].** MCP tools now share
  [`PortalHandle`](../crates/tze_hud_mcp/src/server.rs) directly with the
  runtime's [portal driver](../crates/tze_hud_runtime/src/portal.rs).
  [`PortalHub`](../crates/tze_hud_portal/src/hub.rs) owns transcripts,
  replies, liveness, pacing, cue deadlines and retained unread state under
  `(agent, portal id)`. The [pure renderer](../crates/tze_hud_portal/src/render.rs)
  builds scene nodes from `PortalView` and resolved tokens; the driver applies
  them directly. The resident adapter and protobuf render round trip are gone.
  The [projection crate](../crates/tze_hud_portal/src/lib.rs) no longer exports
  the external authority, contract or coalescer. Portal-only `MutationProto`
  fields 13–17 and their names are reserved; their wire messages, converters and
  scene mutation variants are removed. Native rendering retains the checked
  direct scene methods. This is source evidence, not Windows owner acceptance.
  Follow-up source regressions cover cumulative unread/read-through state,
  an opaque foreground jump badge and the header drag band yielding to its own
  close button (`hud-lirh2.15` / `.16`). The unchanged eight-file physical
  denominator is 14,000 lines (13,956 before this follow-up); no source/tests
  moved out of it. Owner83eb retained-line/jump checks passed, but unread and
  close failed on that build. Native d6ad close-button dispatch, reclaim and same-id
  reattachment later passed with independently audited runtime evidence; this
  records functional acceptance without inventing an owner visual verdict.
  Owner d6ad later accepted retained Line 50, cumulative two-unread and jumping
  to both new markers, but found an empty badge background after read-through
  and backscroll. The follow-up gates portal fill, label and hit target together
  at zero unread; its source regression checks zero/one/two and retained generic
  jump controls. On native bfd6895b the zero/one/two-unread captures retained
  Line 50 and showed readable one/two-unread labels. After jumping to both new
  markers and scrolling back, the owner confirmed “Yes, badge stays completely
  gone”; the final capture also has no empty gray box. `hud-lirh2.15` and `.16`
  are closed; the original eleven-check `.12` acceptance remains partial.
  The runtime module/type is now `portal.rs` / `PortalDriver`, with one binding
  map and immediate scene-owned lease revocation. Scene, MCP and pause control
  share the injected clock; healthy portal idle/hold allowances survive pause
  while input expiry, prior orphan grace and maximum suspension continue.
  Scene bounds now supply the live fractional render/follow-tail viewport;
  the obsolete retained geometry cache is gone. Windowed submission routes text
  through the complete handle and resets history once in the direct native
  submit route. Legacy raw-tile draft transport, history seeding and renderer
  storage are removed; resident wire fields remain reserved.
  Resize broadcast/persistence completion and detached store persistence each
  have one post-lock owner.

  Physical line inventory (comments/blanks included; production-prefix / test
  module / whole file) against `6ef4bf1a`, after the windowed deduplication:

  | File | Before | After |
  |---|---:|---:|
  | runtime/windowed/portal.rs | 2207 / 6734 / 8941 | 2171 / 4094 / 6265 |
  | runtime/windowed/keyboard.rs | 2072 / 319 / 2391 | 2044 / 1706 / 3750 |
  | runtime/windowed/lifecycle.rs | 3087 / 501 / 3588 | 3052 / 1336 / 4388 |
  | runtime/windowed/test_support.rs | 216 / 1046 / 1262 | 530 / 1046 / 1576 |
  | runtime/portal.rs | 986 / 1511 / 2497 | 949 / 1521 / 2470 |
  | projection/hub.rs | 1178 / 681 / 1859 | 1096 / 565 / 1661 |
  | input/portal_resize.rs (comment only) | 1067 / 1375 / 2442 | 1064 / 1375 / 2439 |
  | projection/Cargo.toml | 16 total | 14 total |

  The finite combined surface includes both move destinations and all of
  `test_support.rs`: 22,996 → 22,563 lines (433 fewer). Docs and the single
  removed lockfile dependency edge are outside that aggregate. These are
  physical module boundaries, not semantic production LOC: `test_support.rs`
  is entirely test-only, driver prefix retains a test-only hub accessor, and
  lifecycle prefix retains test-only imports. The feature-gated event-loop
  harness is unchanged and is not counted as production reduction.
  Thirteen keyboard and fourteen lifecycle test parents moved unchanged in
  name; shared factories moved once into test support. Five dimension parents
  became one five-row table and three pruning parents became one table at the
  real app method, including busy locks and idempotence. Five cache-only
  parents disappeared with their obsolete types. Tests: +2 ~3 -13 (net -11);
  moves are zero delta. Driver's 21 unique parents remain.
  Approximate windowed targets of 1700 / 2600 / 4300 are missed by
  471 / 1494 / 1965 lines; group resize/drag, then-present raw-history seeding and every
  unique retained regression remain in scope. Driver's approximate 750 / 1100
  targets are also missed (949 / 1521). Moving tests is ownership cleanup,
  not combined deletion. This source inventory does not establish executed
  verification or physical Windows acceptance. The final D5 inventory below
  records the later mechanical crate rename and size reconciliation; those
  source changes cannot close the remaining Windows owner checks.
  The composer cleanup (`hud-lirh2.10`) removes the scheduler, raw draft wire
  messages, raw input-history queue/store and their obsolete assertions. Native
  local editing, accepted history and complete-handle routing remain. It keeps
  one keyed unsent draft alongside the active draft; an empty peer preserves
  the slot and a nonempty peer replaces it. Two mixed GPU parents move into the
  existing composer test module, retaining live font/wrap and first-frame
  fill/text/clip assertions; raw-history anchor/divider assertions retire with
  that renderer. All nine integration composer parents remain. Automated
  verification and exact source measurements are recorded with the change;
  physical Windows typing/echo/drag/held-release acceptance is UNPERFORMED.

  Physical cleanup counts against `28471250` (prefix / test-module suffix /
  total; comments and blanks included):

  | Group | Before | After |
  |---|---:|---:|
  | Composer core | 4194 / 4804 / 8998 | 3595 / 4503 / 8098 |
  | Windowed and fixtures | 13208 / 11400 / 24608 | 12421 / 10416 / 22837 |
  | Compositor mapped renderer files | 14418 / 1097 / 15515 | 12603 / 959 / 13562 |
  | Protocol | 1921 / 859 / 2780 | 1861 / 798 / 2659 |
  | Integration composer | 168 / 0 / 168 | 160 / 0 / 160 |

  The entire finite packet surface plus the compositor `lib.rs` reexports,
  `text.rs` orphan-method cleanup and scene type flag documentation measures
  75,497 → 70,763 lines (4,734 fewer). These three supplemental
  files are included so no changed file is omitted.
  Test-only renderer files, shared support and the feature-gated harness count
  in physical prefixes; these are not semantic production LOC. Core prefix/test
  reductions miss their estimated 600–900 / 350–600 lines; unique coverage takes priority
  over estimates. Tests: +0 ~54 -59 (net -59); two moves have zero add/remove
  delta. Native projection/hub/goldens and driver source remain unchanged.


  **D5 — final crate rename [Source].** `tze_hud_portal` now names the
  in-process hub, tokens and pure renderer. Workspace/dependency paths,
  imports and current contributor/skill references follow that name; the
  resident feature and old protocol/crypto dependencies were already gone.
  MCP identity, shared errors and reserved wire fields 13–17 are unchanged.
  The owner explicitly amended the earlier ~12k target to **at most 14,000
  physical Rust lines** in the same four groups/eight files, including all
  tests, comments and blanks. The historical pre-repair baseline is 13,918;
  this rename starts at 14,000. On 2026-11-09 the owner raised the cap to
  **15,000** total (decision `pursuit-2026-11-09-portal-cap`, bead
  `hud-ljhku.2`) so portal links, selection and copy can land; code still goes
  where it belongs, and spending headroom beyond that move needs a new owner
  decision. The final measured inventory at the rename is:

  | Group | Before rename | After rename |
  |---|---:|---:|
  | Portal crate (all five Rust files) | 3,214 | 3,214 |
  | runtime/src/portal.rs | 2,511 | 2,511 |
  | runtime/src/windowed/portal.rs | 5,027 | 5,027 |
  | input/src/composer_draft.rs | 3,248 | 3,248 |
  | Total | 14,000 | 14,000 |

  Reproduce with `wc -l crates/tze_hud_portal/src/*.rs
  crates/tze_hud_runtime/src/portal.rs
  crates/tze_hud_runtime/src/windowed/portal.rs
  crates/tze_hud_input/src/composer_draft.rs` (one command). No source or
  tests moved outside the denominator. This is source completion of D5;
  `hud-lirh2.12` owner acceptance and `.13` final reconciliation remain open.

- **T8 — diet [Observed].** The [local harness](../justfile) includes
  llvmpipe GPU recipes and Nextest; resource acceptance has a compact
  [`store_behavior`](../crates/tze_hud_resource/tests/store_behavior.rs) suite.
  Obsolete overlay/benchmark harnesses were removed in PR #1457. Unsupported
  root/tab zone configuration and the unused sealed-image upload workflow are
  removed; built-in zones, display assignments and offline render-scene and
  render-widget authoring remain supported. No Criterion benchmark targets
  are currently declared. The orphaned windowed benchmark mode and unbounded
  frame history are removed; telemetry keeps cumulative session totals and
  bounded recent latency samples. Headless benchmarks and the windowed
  quiescent-efficiency artifact remain. Pre-reset references remain in the projection and
  protocol crate docs. `hud-bstmy` and its reconciliation work remain open;
  a passing test inventory alone does not establish that the diet is complete.
- **T8 — frame correlation KEEP [Observed].** `hud-uehz7` retains the existing
  optional [`FramePresented`](api.md#grpc-resident-sessions) contract after the
  relevant T7 source ownership release: D5 merged in PR #1577, and its source
  and review readers released before reinventory at `84be1d31`. This is a
  narrow source checkpoint; `hud-lirh2.12` Windows owner acceptance and `.13`
  final reconciliation remain outstanding, as do other whole-T7 blockers.
  The declared server field 52, fields 1/2/3, permitted opt-in session gate,
  windowed producer, optional headless producer and existing correlation tests
  support KEEP. The event correlates internal batches with local GPU work,
  without observing main-thread surface present or physical display completion.
  No external opt-in adoption was observed. The retired Python user-test
  client/parser is already absent and is not recreated. This clarification
  changes no runtime statements, wire declarations, grants, defaults, test
  assertions or budgets; the broader gRPC product decision remains separate.

- **T9 — monitors [Observed].**
  [`windowed/displays.rs`](../crates/tze_hud_runtime/src/windowed/displays.rs)
  creates per-monitor overlay windows sharing a scene in physical desktop
  pixels. Config still assigns zones by OS name such as `DISPLAY6`
  ([`zone_display_assignments`](../crates/tze_hud_config/src/displays.rs)).
  [EDID identity discovery](../crates/tze_hud_runtime/src/windowed/monitor_identity.rs)
  logs identity data; it does not yet replace those config keys.
  Token sizes are dp at each monitor's scale × `ui.scale`
  ([`set_px_scale`](../crates/tze_hud_config/src/tokens.rs), `hud-h51u7.3`).
  Stable-key integration (`hud-avr7s.2`) and mixed-DPI window sizing
  (`hud-670ds`) remain open. The vision describes their
  intended end state, not current deployment guarantees.
  Overlay sleep/wake recovery (`hud-jm8nq.25`) reports native visibility, iconic,
  topmost and DWM cloaking state per display, and requests no-activate reassertion
  on display/resume/power-on events or a status observation of drift. The decision
  has deterministic unit coverage. The owner's overnight monitor sleep/wake check
  remains pending; source checks do not establish that the original invisible
  overlay failure was repaired.
- **T10 — visuals [Observed].**
  [`resolve_config_tokens`](../crates/tze_hud_config/src/themes.rs) selects
  Tonal Glass, Blueprint or Classic at startup; [font loading](api.md#fonts)
  supports bundled, config-adjacent and named Windows fonts.
  Live switching (`hud-h51u7.5`) applies `[design_tokens]` edits to the
  config file and the control panel's Appearance and Advanced tokens choices
  without a restart ([`appearance.rs`](../crates/tze_hud_runtime/src/appearance.rs));
  the panel saves them with
  [`set_design_tokens`](../crates/tze_hud_config/src/config_write.rs). Widget
  text still uses the bundled fonts. The remaining restyle/owner-signoff work
  in `hud-h51u7` is outstanding. The Windows workflow captures theme fixtures and reports
  pixel differences; that evidence is not an owner visual signoff.

**Performance qualification [Observed].** The
[demand-layout investigation](operations/demand-layout-capability.md) records
NO-GO for complete-word deferral as a solution to the named giant-word
workload. Its prototype parity/performance gates were not executed, and the
prior measured Stage 4 failures remain failures. This documentation landing
must not be read as closing the performance outcome.

## Documentation shape after the reset

[Observed] T0 deliberately retired `about/`, OpenSpec and their navigation
skills. The generic th-projects shape scanner therefore reports
`UNSHAPED`, `DOCTRINE_LAYOUT=NONE`, and a failed formal traceability gate.
That measures the absent standard layout, not the absence of project guidance.
Use the compact equivalents below; archived specs are history, not current
requirements ([reset commit](https://github.com/tzeusy-org/tze-hud/commit/e32c619458f7ee527d738d17215286a632b32322)).

| Project question | Current home |
|---|---|
| Why, boundaries and principles | [vision.md](vision.md) |
| API and design contracts | [api.md](api.md), with wire definitions in [protocol](../crates/tze_hud_protocol/proto/) |
| Required behavior and regression evidence | [invariants.md](invariants.md); approved tranche direction above |
| Components, ownership and data flow | [architecture.md](architecture.md) |
| How to change and verify it | [README](../README.md), [AGENTS.md](../AGENTS.md), [justfile](../justfile), [Windows development](development/windows.md) |

[Inferred] The main maintenance risk is calling a tranche done because one
implementation or test landed. Keep source implementation, automated evidence
and owner acceptance distinct. [Unknown] This refresh is not an exhaustive
requirement-by-requirement audit of every public interface. Existing T6–T10
reconciliation and owner-check beads remain the follow-up record; this refresh
does not close them or authorize new product scope.

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

Linux compile jobs install their pinned mold linker through
[`install_mold.py`](../scripts/ci/install_mold.py): one bounded official release
asset API download, a repository-pinned SHA256 check before extraction, and an
exact version check before adding its temporary bin directory to `GITHUB_PATH`.
The existing read-only GitHub token supplies public-asset access.

- **Fast.** A merge-group run's required checks finish within 6 minutes of the
  run starting. Suites that cannot fit run in the weekly lanes
  (`perf-budget.yml`, `perf-assert.yml`) or in `windows.yml`, never in the
  required set. [Observed 2026-10-14] p50 4.1 min, max 5.2 min.
- **Not queued.** Wall time is run time plus the wait for a hosted runner, and
  every job, PR push, merge-group build and main push draws on one shared
  pool. Required checks are three jobs (`static checks`, `cargo clippy`,
  `cargo test`); a new gate is a step in one of them, not a new job.
  Superseded PR runs are cancelled, and `windows.yml` keeps its
  informational idle measurement off PRs and runs its theme captures only
  when a PR touches rendered paths. [Observed 2026-11-01, nine jobs per run]
  jobs ran ≤3.3 min but waited 4–15 min for a runner; PR and merge-group wall
  time reached 13–18 min.
- **Repeatable.** A correct change passes on the first attempt. Nextest runs
  with `retries = 0`; a per-test retry override in
  [`.config/nextest.toml`](../.config/nextest.toml) names its reason and is
  tracked debt, not a fix. [Observed 2026-10-14] 25 of 25 merge-group runs
  green on the first attempt.
- **Unattended.** No required check waits on a person, the owner's hardware or
  a live HUD. Pixels are checked by machine on llvmpipe (`just test-gpu`,
  `just render-scene`); the Windows build and WARP smoke in `windows.yml` are
  informational. Owner-host checks, including POC acceptance above, are never
  a merge gate.
- **Mirrored locally.** `just ci` runs the required gates in CI order;
  `just pre-push` runs the host-identifier check, fmt and the exact CI clippy command.

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
