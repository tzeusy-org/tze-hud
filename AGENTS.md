# Agent Instructions

This project uses **bd** (beads) for issue tracking. Run `bd onboard` to get started.

## Current project documentation

Read `docs/vision.md`, `docs/scope.md` (status and open owner checks),
`docs/architecture.md`, `docs/api.md` and `docs/invariants.md`. The retired
`about/`, OpenSpec and doctrine trees stay retired; update current docs with
behavior changes. This file holds recipes and gotchas only.

## Local Dev Harness

The repo ships a `justfile` that reproduces CI gates locally. Requires [just](https://github.com/casey/just).

```bash
just bootstrap       # install/report dev-host deps (idempotent; --check reports only)
just build-windows   # cross-build tze_hud.exe (x86_64-pc-windows-gnu) from Linux/WSL
just dev-run <dir> [host] # rebuild and hot-swap the running Windows dev HUD (docs/development/windows.md)
just gesture [scenario...] # tier-L gesture checks: real Linux build, private Xvfb, XTEST input (local only, not in `ci`)
just check           # cargo check (fast compilation gate)
just fmt             # cargo fmt --check
just fmt-fix         # cargo fmt (apply formatting)
just clippy          # cargo clippy --workspace --all-targets -D warnings
just pre-push        # current-HEAD Rust/Cargo diff: fmt --all + workspace all-target Clippy
just test            # cargo nextest run --workspace --all-targets --exclude integration --features tze_hud_runtime/dev-mode (GPU + pixel_readback)
just test-gpu        # GPU subset only (compositor + pixel_readback), llvmpipe-pinned
just test-integration # integration headless suites
just test-python     # pure-Python suites (pytest + scripts/ci unittest)
just token-footprint # deterministic LLM-facing token-footprint gate
just current-references # reject retired design citations in current source
just size-report     # lines per surface, production and test (report only)
just size-check      # check surface growth against origin/main's merge base
just idle-efficiency-checker # fail-closed idle artifact contract tests
just production-boot # vertical_slice production config boot
just canonical-app-boot # canonical app production config boot
just deps-unused     # cargo machete: unused dependencies
just dead-code <crate> # advisory dead pub-item list for one crate
just dev-mode-guard  # verify dev-mode is not enabled in any package's default-build dependency closure
just clippy-windows-gnu # clippy on x86_64-pc-windows-gnu (prints SKIPPED and passes if the Rust target is missing)
just cargo-deny      # advisories/licenses/bans/sources (SKIPPED if cargo-deny is missing)
just ci              # full local CI sweep (all blocking gates except test-gpu, which `test` already covers; tool-gated gates SKIP loudly when their tool is absent)
```

The toolchain is pinned in `rust-toolchain.toml` (Rust 1.88, matching CI and the
`glyphon 0.8.x` / `wgpu 24.x` co-pin). Workspace lint policy is declared in
`[workspace.lints]` in the root `Cargo.toml` and inherited via `lints.workspace = true`
in every member crate.

GPU tests: run `just test-gpu` (compositor render tests plus runtime
`pixel_readback`, pinned to Mesa llvmpipe via `VK_ICD_FILENAMES`). GPU
initialization is serialized per test process by test-only gates; separate
processes are not coordinated. Every `just` recipe that builds a GPU device pins
llvmpipe when its ICD is installed (`lvp_icd.x86_64.json` or Mesa 25's
`lvp_icd.json`). Don't run bare `cargo test -p tze_hud_compositor` on a host
with a hardware Vulkan ICD (recorded hangs were NVIDIA driver threads).

On Windows, `just` needs Git for Windows' `sh` and a real `python3`; setup, the
per-recipe status, and runtime differences are in `docs/development/windows.md`.

## Quick Reference

```bash
bd ready              # Find available work
bd show <id>          # View issue details
bd update <id> --claim  # Claim work atomically
bd close <id>         # Complete work
```

## HUD dogfooding: use what we build

The owner wants sessions working in this repository to actively use the HUD
for concise status, attention and quick decisions. Treat this as standing
opt-in for deliberate HUD publication using the existing paired connection.
Prefer a useful HUD element over another chat progress paragraph when the HUD
is available; keep the final outcome and durable evidence in chat or the PR.
The main session owns this presence. Subagents report through their parent
unless explicitly delegated an existing element; they do not each create a
status entry, notification stream or portal.

- **Status:** prefer one compact status element with a unique session/work
  key and a recognizable label. Update it on meaningful changes such as
  working, testing, blocked and done; coalesce updates under that key. Discover
  surfaces first and verify their actual presentation. The current status bar
  rendered as an intrusive slab in owner dogfooding (2026-11-04; tracked under
  `hud-h51u7.4.4`); do not use it by default until its compact presentation is
  accepted. Use brief milestone notifications meanwhile. Use an existing widget
  only when its typed params fit and it is allocated to this work; do not
  commandeer a shared widget.
- **Attention and decisions:** prefer a short notification for an actionable
  blocker, meaningful completion, or an actual owner choice. Use a concise
  question with explicit action labels, such as Yes / No / Later, within the
  running build's supported action limit. Give callback ids a unique session
  and question identity. A label-only action (its answer is its label) is fine
  only with a session-unique `key` (it comes back on the item), or with an
  armed `wait_ms` whose `ttl_ms` is at most the wait (the card leaves when the
  wait ends); otherwise keep unique callback ids. Publish only when ready to
  consume the answer. For a question you can leave pending, run
  `portal_client.py ask` as a background job and read its one JSON result on
  exit. The job owns its notification and consumes its answer within its own
  MCP session. See [Background HUD asks](.claude/skills/hud-projection/references/background-ask.md).
- **Replies:** use a session-named portal when a short typed answer or compact
  ongoing exchange is useful and its presentation is accepted. The current
  default portal is owner-rejected (2026-11-04): drag makes it transparent
  (`hud-1wizi`). The latest owner size direction (2026-11-06) is 2× the width
  and 3× the height at the existing base font: 1440×1080 before display fitting.
  Keep ordinary exchanges in chat until those defects are resolved;
  use the portal only for an explicit validation step meanwhile. Do not mirror
  every chat message or open a portal for each subagent. The owner's normal
  coding workload is 4–12 main sessions, excluding subagents; keep attention
  and screen space proportionate to that workload.
- **Keep it glanceable:** status is a few words; a notification carries one
  question and at most a couple of short lines. Keep long explanations,
  logs, diffs and reports in chat, a PR or another reading surface; name or
  link that destination. Do not invent link-action support on an older build.
- **Own the interaction:** identify the originating work in each prompt.
  Route input by surface and the current question's callback identity, handle
  it once, then ACK the items handled. Each MCP session (one stdio adapter per
  Claude session) owns its own cards, portals and input under the shared
  pairing; the adapter releases them when the session exits cleanly. Python
  scripts without a session share the agent's one unscoped owner: they don't
  see a session's input or portals.
  Do not offer action buttons without safe answer routing; use chat instead.
- **Bound the footprint:** use finite, appropriate TTLs, update existing
  status instead of stacking repeats, and remove or let expire owned content
  on completion/cancellation. Clear only an exclusively owned surface; for a
  shared status bar, remove only this session's entry. No heartbeat, progress
  spam or repeated model polling while waiting for the owner.
- **Fail gracefully:** if the HUD is unavailable or rejects a call, continue
  in chat and report the limitation once. Timeout, silence and dismiss are
  not consent. HUD answers carry the same authority as a clear owner answer
  in chat; destructive or otherwise unauthorized actions retain their normal
  approval requirements. This opt-in does not install client hooks, change
  pairing/permissions or authorize publishing secrets.

Use **`th-hud-publish`** for zones/widgets and **`hud-projection`** for portal
interactions. Their scripts work without a client-specific hook.
`TZE_HUD_MIRROR=1` opts a Claude workspace into its automatic portal mirror.
Follow `docs/api.md` and the running build, rather than assuming a planned feature
has shipped. Record dogfooding defects against existing beads where possible;
an accepted publish or runtime screenshot is not owner visual acceptance.

## LLM Self-Projection

Use the **`hud-projection`** skill to project a session onto the HUD, and
**`th-hud-publish`** for one-shot zone publishing; CLAUDE.md has pairing details.

## Non-Interactive Shell Commands

`cp`, `mv` and `rm` may be aliased to `-i`; use `cp -f`, `mv -f`, `rm -f`
(`-rf` for directories), `apt-get -y` and `HOMEBREW_NO_AUTO_UPDATE=1`.

## Landing the Plane (Session Completion)

Work is not complete until it is pushed: file follow-up beads, run the quality
gates for what changed, update bead status, then `git pull --rebase`,
`git push`, and confirm `git status` is up to date with origin. Never stop at
"ready to push". `git pull --rebase` can stall 4–5 min while the
post-checkout hook runs `bd import`; wait it out, do not abort or restart Dolt.

## Worker Isolation for Rust Code Changes

Use a worktree of this repo for isolated workers; don't switch branches in the main checkout:

```bash
scripts/worktree-add.sh .worktrees/<name> -b <branch>
```

Run `just pre-push` before pushing Rust/Cargo changes. `just bootstrap`
(`--hooks-only` for just the hook, `--check` to report without writes) installs
the guarded `.githooks/pre-push`: it checks each pushed ref against
`origin/main`, skips proven docs-only pushes and refuses Rust heads that are not
checked out. Its lock serializes hooks, not other Cargo processes.
`git push --no-verify` needs the coordinator's explicit authorization.

## Beads / Issue Tracking

- Local `bd` CLI in this repo currently does not support `bd sync` (returns `unknown command "sync"`). End-of-session sync should use explicit git pull/rebase + push flow instead.
- `bd ready --json --limit 0` can omit open `feature` beads in this repo's default view; use explicit type filters (for example `bd ready --type feature --json --limit 0`) during coordinator dispatch so ready feature work isn't skipped.
- `bd dep add <epic> <task> --type blocks` fails (`epics can only block other epics`). For epic PR review flow, create the `pr-review-task` bead independently (no `blocks` dep from epic), keep the epic blocked with `external_ref=gh-pr:<N>`, and resolve epic closure from PR merge state + dependent child status.
- Worktree `.beads/dolt-server.port` can get corrupted (e.g., concatenated to `428893307` instead of `3307`) when a worktree is created while the port file is written. Symptom: `bd` commands fail with "database not found on Dolt server at 127.0.0.1:42889". Fix: `printf '3307' > <worktree>/.beads/dolt-server.port`.
- If `bd` reports `database "hud" not found` after Dolt recovery, `bd bootstrap` may import into the SQL server's exposed `dolt` database rather than creating `hud`. Temporary recovery is `bd dolt set database dolt` plus `bd import .beads/issues.jsonl`; restore `.beads/metadata.json` before committing.
- The local Beads Dolt store currently has no configured `origin`; `bd dolt push` fails with `remote 'origin' not found`. Session closeout should still run normal `git pull --rebase && git push`, but Beads DB pushes require configuring a Dolt remote first.
- `bd backup status` can report recent auto-backups while `bd backup sync` still fails with no destination configured; `.beads/backup/` is local-only and ignored by git, so treat this as local recovery state unless a backup destination is explicitly configured.
- Beads coordination backup setup is documented in `docs/operations/beads-coordination-backup.md`; a real fix requires an operator-owned DoltHub/NAS/synced-repo destination, not a local `.beads/backup/` path.
- `bd stats --json` hangs indefinitely (>5 min, observed 2026-06-12) while the Dolt server reports healthy and other bd queries return in <1s. Avoid it in scripts; derive counts from `bd list --json` instead, and kill any leaked `bd stats` process.
- `bd create --graph <plan.json>` (bd v1.x, this repo) has two traps: (1) `--dry-run` is IGNORED for `--graph` — it CREATES real beads (delete junk with `bd delete <id>... -f`); (2) per-node `deps` entries (e.g. `"deps":["blocks:otherKey"]`) are SILENTLY DROPPED — only `parent` links and the nodes themselves are created, blocking edges are NOT. Wire edges separately afterward via `bd dep add --file edges.jsonl` where each line is `{"from":"<blocked/dependent-id>","to":"<blocker/prereq-id>"}` (from depends on to; default type `blocks`). Then verify with `bd dep cycles` and `bd ready`. Graph-plan schema: top-level `{"nodes":[...]}`, each node keyed by `key` (not `id`), with `title`/`type`/`priority`/`description`/`parent`.
- `bd create --json` occasionally returns EMPTY output while the create actually succeeded or silently no-opped — if a --json create looks like it did nothing, verify with `bd show` / retry WITHOUT --json before assuming either outcome.

## Git / GitHub Workflow

- Size policy changes in [`scripts/ci/size_targets.toml`](scripts/ci/size_targets.toml)
  require a citation to the owner's signed approval in the PR. Apply the
  `owner-merge` hold for every change to ceilings, mappings or allowances,
  including later amendments. For a feature allowance, cite the signed
  feature spec in both the PR and the table. The size check validates that the
  reference is present. The owner verifies that it grants the recorded change.
- `main` takes no direct merges: the `main merge queue` ruleset requires every PR to land through the queue, so the older `gh pr merge <N> --squash [--delete-branch]` recipes no longer apply. Enqueue instead (CI / Build below) and confirm the landing with `gh pr view <N> --json state,mergedAt`.
- `beads-pr-reviewer-worker`: automated reviewer threads (Copilot, Gemini) are left by bots and count toward `UNRESOLVED_COUNT`; they must be replied to and resolved before merge just like human threads.
- `beads-pr-reviewer-worker`: the `list_review_threads.py` helper may return 0 threads even when threads exist; always verify with `evaluate_merge_readiness.py` or `gh api repos/.../pulls/<N>/comments` before treating unresolved count as zero.
- A PR labelled `owner-merge` waits for the owner. Sessions and the fleet must
  never enqueue or merge it. From this checkout, run
  `python3 .claude/skills/hud-projection/scripts/hud_pr.py hold <N>` to hand it
  to the owner, or run `release <N>` through the same script to return it to
  the fleet. See [Held pull requests](.claude/skills/hud-projection/references/claude-code-hooks.md#held-pull-requests).
- `.codex/skills` should remain a symlink to `../.claude/skills`; `.claude/skills` is the canonical tracked tree and currently strictly supersets the Codex mirror.
- Run mechanical UI checks yourself before asking the owner (T13): `just gesture` (scenario names: `python3 scripts/ci/hud_gestures.py --list`), then the Windows tier: `gh workflow run windows.yml --ref <branch>` runs the same scenarios plus no-focus-steal and the safe-mode chord with real `SendInput` on a hosted VM (fullscreen, WARP; results in the step summary, evidence in the `gestures` artifact; it also runs on PRs touching rendered, input or portal paths). Scenarios in that step's `GESTURES_REQUIRED` fail `windows-build` (withholding `dev`, not merges); the rest are informational. Graduate a scenario only by citing 10 consecutive main runs where it PASSed; demote a flaking one at once, never retry it to green. Tier L launches its own HUD on a private Xvfb display, refuses to inject anywhere else, and writes `timeline.json`, status snapshots and frames to `test_results/gestures/<sha>/<time>-<pid>/`; parallel runs take distinct displays and ports. Ask the owner only about the T13 residual list in `docs/scope.md` (visual preference, owner hardware, network/install, time-based behaviour), with a link to the evidence. A scenario FAIL names the failing step and the last observed portal state; read that before the HUD log.
- `test_results/` and `.handoff/` hold local execution receipts and are gitignored. Put durable evidence in PR descriptions and CI artifacts; never force-add files from either root. The same Git-index guard runs early in CI and `just ci`.
- Tracked files never carry real host identifiers: tailnet addresses or MagicDNS names, private-key filenames, or the owner's host names (owner rule, 2026-11-08). Keep them in local config outside the repository, such as the per-host endpoint record under `~/.config/tze-hud/`. Fixtures use the synthetic CGNAT values listed in `scripts/ci/check_host_identifiers.py`, documentation-only IPv4 addresses or `example` names. That check (`just host-identifiers`, in `static checks`, `just ci` and pre-push) fails on any other CGNAT literal. Locally it also applies a literal denylist from `~/.config/tze-hud/host-identifiers.deny`, which CI never sees. It reports `path:line` only, never the value.
- Merged `agent/*` branches are deleted daily by `.github/workflows/delete-merged-branches.yml` (`scripts/prune-merged-branches.sh --execute`; it skips branches checked out in a local worktree and never force-deletes). Preview with `bash scripts/prune-merged-branches.sh --dry-run`.

## CI / Build

- If an Actions workflow or check suite fails while all emitted jobs succeed, reconcile the exact attempt's jobs, suite check runs and annotations with its head-SHA workflow before changing gates. Keep required checks, aggregate conclusion and actual merge state separate; missing jobs are incomplete provider evidence, not a proven repository cause. Preserve the original failure and put the diagnosis and evidence limits in the PR description (observed in CI run 37655059228, attempt 1).
- `main` merges go through the GitHub merge queue (ruleset `main merge queue`, squash). `gh pr merge <N> --squash --auto` fails here; enqueue with the GraphQL mutation once required checks are green: `PR_ID=$(gh pr view <N> --json id -q .id)`, then `gh api graphql -f query='mutation($id:ID!){enqueuePullRequest(input:{pullRequestId:$id}){mergeQueueEntry{position state}}}' -f id="$PR_ID"`. No approving review is required (empty `reviewDecision` is not a blocker); PRs land through the merge queue. A non-null `mergeQueueEntry` means it is queued. `gh pr view --json` has no `mergeQueueEntry` field; to re-check later, query `gh api graphql -f query='query{repository(owner:"tzeusy-org",name:"tze-hud"){pullRequest(number:<N>){state mergeQueueEntry{position state}}}}'` (entry `null` with state `MERGED` means it landed; `null` with state `OPEN` means it was dequeued, so check the merge-group run for the reason). Every required check in `ci.yml` must also trigger on `merge_group`; a required check name that no job emits blocks the queue forever, so update the ruleset whenever a required job is renamed or removed.
- CI has three required checks (owner-approved 2026-11-01): `static checks` (no compilation), `cargo clippy` and `cargo test`. Add a step to an existing job before adding a job; every job draws on one shared hosted-runner pool. Targets are in `docs/scope.md` "Merge gate and testability".
- Timing acceptance is nearest-rank p95 ≤360 seconds over the last 20
  chronological natural first-attempt `main` merge-group runs, including
  hosted-runner wait. The 360-second per-run target and first-attempt success
  still apply. Follow `docs/scope.md` "Merge gate and testability" for cohort
  selection, completeness and reporting.
- Every job has a job-level `timeout-minutes` and every apt step its own (plus `$APT_OPTS` retries), so a stalled runner fails fast instead of timing out the merge queue.

- `tze_hud_protocol/build.rs` selects bundled protoc 31.1 through pinned `protoc-bin-vendored =3.2.0` and `prost-build =0.13.5`. Cargo fetches the tool; no apt/zip/choco protoc prerequisite or `PROTOC` override is needed. Native and cross-target builds choose the build-host binary, and unsupported hosts fail rather than falling back. The two retained protos regenerate into `OUT_DIR` during the ordinary build.
- Quiescent-efficiency proof is emitted by the real constrained windowed runtime via `--quiescent-efficiency-emit`; keep its sampler deadline independent of `scheduled_main_deadline` so a normal winit resume cannot be counted as an excluded measurement wake. The canonical Windows CI harness is `scripts/ci/windows/run-quiescent-efficiency.ps1`: it enforces process affinity and validates an actual `overlay` artifact through `check_idle_efficiency.py` rather than accepting a host-side fixture.
- Timing assertions in tests use `tze_hud_scene::perf_budget::test_budget(base)`: the reference budget times a fixed slack (default 20×, override with `TZE_HUD_TEST_BUDGET_SLACK`). p99 hard asserts stay gated behind `TZE_HUD_PERF_ASSERT=1` in the Linux timing job of `perf-budget.yml`. The headless `benchmark` example keeps its own CPU/GPU/upload factor vector because the weekly Windows and constrained-envelope gates (`perf-budget.yml`) normalize locked ceilings by it.
- CI's clippy gate runs `cargo clippy --workspace --all-targets -- -D warnings`, so test, example, and bench code must be clippy-clean too. The `integration` package's headless test targets are gated by the integration step of the `cargo test` CI job.
- FOOTGUN (bit 3 PRs in the 2026-07-04 coordinator session): the repo-root `tests/integration/` is a SEPARATE cargo package that crate-scoped runs do NOT compile. `cargo test -p tze_hud_scene`, `cargo clippy -p <crate>`, or a crate's own `cargo test` all PASS while a struct/field/signature change you made (e.g. adding a field to `ElementStoreEntry` or `WindowedRuntimeState`, changing `MarkdownCache::get`'s arity) leaves stale struct-literals/call-sites in `tests/integration/*.rs` — which then fail CI's `cargo clippy --workspace --all-targets` and integration step with `E0063`/`E0061`. ALWAYS validate a crate-level type change with `cargo clippy --workspace --all-targets -- -D warnings` (or `just clippy`) + `cargo check --workspace` before pushing, and grep `tests/integration/` for literals/calls of the symbol you changed. Note `--all-features` is NOT what CI runs (it pulls glib-sys/GStreamer sys deps the runners lack) — use `--workspace --all-targets` exactly. Related: on a repo where `main` advances under parallel branches, `git fetch && git rebase origin/main` + `cargo check --workspace` right before finalizing catches the same drift when a concurrent PR adds a field to a struct your branch constructs.
- A total GitHub-hosted runner stall (every new run `queued`, jobs with empty `runner_name`, no self-hosted runners) is GitHub-side: an incident or the Actions spending limit. Surface it to the owner; it is not fixable from the repo.
- Agents cannot bypass the `main merge queue` ruleset (`current_user_can_bypass: never`), so `gh pr merge --admin` is never an agent action. During a confirmed runner stall, ask the owner.
- Review rule: perf beads need an empirical re-run of the named payloads in release mode with timing assertions; wiring beads need grep-verified production call sites. Every caller of a changed shared symbol must build clean.

## Scene / Runtime

- Production pointer-free command input enters at `windowed/keyboard.rs`: default keyboard bindings produce `RawCommandEvent`, `CommandProcessor` applies local focus/feedback, and `InputProcessor` applies focused keyboard scroll. Keep portal/composer precedence ahead of generic local command handling, and use `windowed/event_loop_harness.rs` to prove the real pending-keyboard drain rather than a reconstructed closure.
- Shell-reserved keyboard handling is a global `windowed/keyboard.rs` precedence seam: execute the shell action before portal/composer/local command routing, update the authoritative `SceneGraph` plus `active_tab_mirror` for tab switches, and track physical key identity so later local routing cannot consume the matching release even if modifiers are released first. The control panel's key routing (`windowed/panel_input.rs`), which runs first while the panel is open, records the same consumed identity.
- `size_of::<Node>()` must stay below 160 bytes (`graph/limits_tests.rs::node_struct_size_under_160_bytes`). The existing hud-yfj8u amendment raised the budget from 150 to 160 for the additive `NodeLayout` field. Adding heap-allocated fields to `HitRegionNode` (which is a variant of `NodeData`) inflates `Node` inline. Box large optional structs (`AccessibilityMeta`, `LocalStyle`) to stay under budget.
- When `HitRegionNode` gains new fields with `#[serde(default)]`, all existing struct literal constructions in tests and production code must add `..Default::default()`. Grep for `HitRegion(HitRegionNode {` across the workspace to find them all.
- `update_hover_state` should use `entry().or_insert_with()` rather than `get_mut` so that HitRegionNodes inserted directly into `self.nodes` (bypassing `set_tile_root`/`add_node_to_tile`) still get local state initialized on first hit.
- Zone ontology (rig-ar23): `ZoneDefinition` needs `#[serde(default)]` on `layer_attachment` and a `Default` impl returning `Content` so older serialized defs without the field still deserialize correctly.
- When proto messages gain new fields (for example `Publish`), `prost` struct literals in both `crates/tze_hud_protocol/src/session_server/tests/` and `crates/tze_hud_protocol/tests/*.rs` must add explicit defaults (typically `element_id: Vec::new()`), or test-target compilation fails even if library code builds.
- `RenderingPolicy::from_zone_policy()` margin fallback: `margin_horizontal`/`margin_vertical` must fall back to `margin_px` (via `.or(policy.margin_px)`), not hardcoded 8.0. Preserve that fallback when changing zone policies.
- `update_zone_animations` (PR #260) only iterates `current_active` (zones in `active_publishes`). Zones removed from registry entirely (unregistered) disappear from that map, so fade-out on unregistration must be handled by pruning stale animation states for absent zones after the main loop.
- Runtime hover/tooltip behavior is now widget-definition-driven: `WidgetDefinition.hover_behavior` declares trigger rect + delay + target f32 param, `windowed.rs` builds generic trackers via `widget_hover.rs`, and local hover writes MUST use `SceneGraph::set_widget_param_local` (not `publish_to_widget`) to avoid polluting contention/publication state.
- Retained widget SVG text has a simple sans-serif fast path in `crates/tze_hud_compositor/src/widget.rs` using `ab_glyph`; unsupported font families or dominant-baseline values intentionally fall back to the cropped `resvg` text-mask path.
- Pixel-readback tests for display-relative canonical scenes must sample display-relative coordinates too; stale fixed 800x600-era samples can hit the clear background (`[63, 63, 89, 255]`) on the 1920x1080 runtime and look like z-order/contention failures.
- Bound the focused runtime-lib gate: `timeout 180s cargo test -p tze_hud_runtime --lib`. A warm run takes seconds; a fresh worktree may compile for 1–2 minutes first.
- Runtime widget cleanup contract: clearing/TTL expiry removes active publications, refreshes `WidgetInstance.current_params` from remaining publications/defaults, and the compositor only draws widgets with active publications; default params alone should not leave a visible stale widget.
- Text-layout logic in `tze_hud_compositor` is CPU-testable WITHOUT a GPU (dodging the llvmpipe deadlock): cosmic-text/glyphon shaping (`FontSystem::new()` → `Buffer::set_text`/`shape_until_scroll` with `Shaping::Advanced`) is pure-CPU; only the wgpu render pass needs a GPU. So functions that consume shaped `Buffer`s — e.g. `compute_inline_backdrop_quads` (selection/inline-code backdrop geometry), and the composer wrap/caret measurements — can be unit-tested by shaping a `Buffer` in the test and asserting the returned geometry (`InlineBackdropQuad`s, line/caret byte↔x). Prefer this over the GPU `require_gpu!`/`render_frame_headless` path, which is CI-only. Note: with `Shaping::Advanced`, `LayoutGlyph::start`/`end` are byte offsets within their `BufferLine`; add the `BufferLine` base offset (`sum(line.text().len()+1)`) for full-text byte ranges when a draft has hard `\n`.
- Shared local Cargo targets can supply another isolated checkout's types or test binaries; a passing count alone does not prove current-source execution. Use the opt-in [source-binding policy](docs/development/source-freshness.md) for every writer of a shared target under the coordinator's exclusive lease. Each checkout gets one HOME target, reused warm across its commits and refused to other checkouts; the coordinator deletes it when the bead's PR merges or closes, keeping receipts, per the [retention rule](docs/development/source-freshness.md#retention). Never touch source mtimes. The guard covers canonical `just` Cargo gates and their effective pre-push descendant when enabled; ordinary unset defaults and direct Cargo are outside that cooperative policy. The exact historical Cargo-internal cause remains unproved.

## Policy / Lease Governance

- Policy-planning and reconciliation must account for configuration (`crates/tze_hud_config/src/policy_builder.rs`), runtime enforcement (`crates/tze_hud_runtime`), and scene-side lease contracts (`crates/tze_hud_scene/src/lease/`). Verify the current production consumers when assigning ownership across these surfaces.
- Identity and permissions (T5 S2): the handshake PSK identifies the agent (`AgentDirectory::resolve` in `tze_hud_scene::config::agents` hashes it and compares SHA-256 digests from `agents.toml`; the directory is `SharedAgents`, read per MCP request and per gRPC handshake; `AgentDirectory::unrestricted(psk)` is the dev/test PSK that claims any id); `StreamSession.capabilities` is the agent's `allow` list expanded to permission strings, fixed for the session. There is no capability negotiation. The resident session server checks publication and upload permissions at the boundary; scene leases carry no capabilities or priority.
- `crates/tze_hud_protocol::session_server` tests run unrestricted (`HudSessionImpl::new` → `AgentDirectory::unrestricted`); tests that need narrower permissions build a directory with explicit `permissions`.
- Preserve the 50 µs policy-mutation evaluation latency target when extending telemetry. Verify the actual production admission caller and its measurements; do not assume a historical pilot harness is still wired or invent a parallel metric.

## Windows / User-Test

- `tze_hud.exe` takes no PSK (`--psk` is an unknown flag, `TZE_HUD_PSK` is ignored by the runtime). Agents authenticate against `agents.toml` beside the config, which holds only each PSK's SHA-256 and `allow` list; an invalid file fails strict startup, a missing one means every request is rejected. Pair agents with `POST /pair` (`docs/operations/windows-install.md`).
- Notification styling (urgency tints, card radius, type scale) comes from `[design_tokens]` in the deployed `tze_hud.toml`; copy the block from `app/tze_hud_app/config/production.toml`. Component profiles and the `profiles/` directory are gone; old `[component_profile_bundles]`/`[component_profiles]` sections are rejected at startup and must be removed.
- Windows smoke captures the notification gamut fixture at `.claude/skills/user-test/scripts/notification-full-gamut.json` (urgencies 0–3, long body, and action buttons).
- The gauge, progress-bar and status-indicator widget bundles are embedded in the exe (`crates/tze_hud_runtime/src/widget_startup.rs`); no `widget_bundles\` directory needs deploying. An optional `[widget_bundles].paths` root overrides a built-in by type name.
- `user-test` uses `.claude/skills/th-hud-publish/scripts/publish.py` for both zones and widgets. Check each batch receipt. The command exits nonzero on a failed mutation.
- For a dedicated idle test pairing, `--cleanup-on-exit` attempts to clear confirmed or uncertain widget publishes after completion, a transport error, or interruption. For screenshots, use finite TTLs that cover the capture.
- Isolated Windows HUD validation scripts that force-stop `tze_hud.exe` must wait for the stopped PID to disappear before starting the next HUD. The HUD holds a per-user named mutex (`Local\tze_hud`); a second instance exits 0 immediately, so a still-running predecessor makes the next HUD vanish before binding its ports.
- The runtime MCP server (`http://<host>:9090/mcp`) is standard MCP over JSON-RPC: `initialize`, `notifications/initialized`, `tools/list`, and `tools/call` for the five `hud_*` tools (`docs/api.md`). Bare tool-name methods return `-32601`. Tool failures are results with `isError: true` and `{"code","hint"}`. `tools/list` is the cheapest reachability+auth probe. Bearer = the agent's PSK (its SHA-256 is `[agents.<id>] psk_sha256` in `agents.toml`).
- Windows diagnostic Unicode injection via `SendInput` must marshal a full native-shaped `INPUT` union (`MOUSEINPUT`, `KEYBDINPUT`, and `HARDWAREINPUT`); a keyboard-only union can shrink `cbSize` and make `SendInput` fail before HUD keyboard evidence is produced.

## Performance / Benchmarks

- `.claude/skills/user-test-performance/scripts/perf_common.py` owns the `results.csv` schema and now auto-migrates existing CSV headers during append; add new audit fields there first, then wire both MCP/gRPC scripts. `grpc_widget_publish_perf.py` now imports vendored stubs from `user-test-performance/scripts/proto_gen/` (self-contained, no `/user-test` path dependency).
- Durable gRPC widget `Publish` replies are `RequestResult` correlated by `seq`. The Rust publish-load harness (`examples/widget_publish_load_harness/`) is the canonical gRPC widget benchmark path; `/user-test-performance` routes gRPC widget benchmarks through it.
- `examples/benchmark` times what agents send: `notification_publish`, `widget_param_update` and `portal_append` each make one `hud_publish` per frame over the real MCP listener (`input_to_scene_commit` is that call's round trip; only `notification_publish` samples `input_to_local_ack`, by hovering its action). The publish-call budgets in `scripts/ci/check_windows_perf_budgets.py` are absolute, not scaled by the CPU factor, so a fixed admission delay fails them on any host. Calibrations are publish-composed too (CPU: zone/widget publish commits; GPU: zone-fill and SVG-gauge overlay; upload: gauge re-rasterizations/s), with references rescaled so a host keeps its factor (vector `tze_hud.cpu-gpu-upload.v2`).
- Do not add clone-on-dirty or clone-on-writer full-scene snapshots for scene-lock contention (`hud-ibzl4`); they break work-proportional-to-change. Reopen only with a measured latency breach.
- Windows live widget benchmarks use `app/tze_hud_app/config/benchmark.toml` (`tze_hud.exe --config <path>`); three-agent soak artifacts are emitted by `.claude/skills/user-test-performance/scripts/widget_soak_runner.py`.
- Long `widget_publish_load_harness` paced runs can hang without per-agent artifacts because responses are drained only after sending; a valid 60-minute Windows soak needs concurrent result draining or an overall diagnostic drain deadline.
- `hud-nfl7n` release soak resource sampling should pass `--windows-process-command-match` set to the benchmark config path so samples target the benchmark-config HUD process rather than any unrelated `tze_hud*` process.
- `widget_publish_load_harness` paced/burst runs must drain `HudSession` responses concurrently while sending; missing `RequestResult` acks should still produce a diagnostic artifact, then exit nonzero so `widget_soak_runner.py` can report per-agent failures.
- Current `widget_publish_load_harness` artifacts preserve aggregate RTT percentiles/max only (`histogram_path: null`), so max-tail outlier localization needs a future bounded top-N RTT tail with request sequence plus send/ack timestamps.
- Production degradation wiring is contract-blocked beyond the fixed 60 Hz case: decide cadence-derived thresholds, quiescent recovery and the level-to-`DegradationNotice` mapping first. `tze_hud_runtime::DegradationController` is the sole transition authority.
- Headless `max_agent_update_hz` is 60 Hz (`DisplayProfile::headless()`), not 30. It is a per-agent state-stream admission ceiling consumed by configuration validation; neither headless nor windowed runtime uses it for compositor cadence, so it must not be used to justify idle frame submission.

## Text-Stream Portals

- Tile creation and portal mutations are runtime-internal scene calls: only the in-process portal driver applies them, and the protocol has no node or mutation message. The portal is driven through the MCP `portal:` surface (`hud_publish`/`hud_input`/`hud_hold`/`hud_clear`); agents never draw portal tiles.
- Text-stream portal composer text currently uses a full-span `TextMarkdownNode.color_runs` marker (same color as base text) to request literal monospace rendering; normal printable input should come from runtime character events, with key-down fallback only for Space on the Windows path.
- Composer local echo is runtime-owned (`LocalComposerState`) and should render in the focused composer `HitRegionNode` bounds, not as a bottom strip of the whole tile.
- Keep text-stream portal submitted history bounded in adapter/projection state and materialize only visible cards plus the live draft in the scene graph.

## Cooperative HUD Projection

- `/hud-projection` is cooperative opt-in for an already-running LLM session, with no PTY attachment or terminal capture. The session owns durable history and uses the standard MCP publish/input/clear verbs; the runtime owns ephemeral portal presentation and queued replies.
- `tze_hud_portal` owns `PortalHub`, resolved tokens and the pure scene-node renderer. MCP and `crates/tze_hud_runtime/src/portal.rs` share `PortalHandle`; `PortalDriver` applies native scene data directly. No resident adapter, external authority or portal protobuf round trip remains.
- The MCP `tools/list` schemas are hand-written in `crates/tze_hud_mcp/src/schema.rs`; every byte is paid by every attached session. `schema::tests` pins them to the `*Params` structs (`deny_unknown_fields`) and to the byte budget, and `scripts/ci/check_token_footprint.py` gates model-visible tokens per flow against `docs/api.md`.
- cargo-deny `[advisories].ignore` entries are per-advisory-ID and cannot be scoped to one crate instance: if an advisory hits both a direct dep and a build-time transitive (e.g. quick-xml RUSTSEC-2026-0194/0195 in our workspace AND inside winit's wayland-scanner), first bump the direct workspace dep to the patched version, then add the ID waiver documenting that the only remaining instance is the pinned transitive. Follow the existing documented-waiver style in deny.toml (id/reason/action). Verify with `cargo deny check advisories licenses` (fast, local, safe).
- After the merge queue lands a PR whose branch is checked out in a `.worktrees/` worktree, clean up with `git worktree remove .worktrees/<dir> --force`, `git branch -D <branch>`, `git push origin --delete <branch>` (or leave `agent/*` branches to `delete-merged-branches.yml`).
- Review trap (learned from PR #982→#988): when a PR claims an existing mechanism provides behavior "for free" (e.g. "new focusable nodes inherit the token-driven focus ring"), grep for actual CONSUMERS of that mechanism before accepting — `FocusRingUpdate`/`compute_ring` had zero consumers outside `tze_hud_input`, so the focus ring had never rendered anywhere and the claim was unverifiable-by-construction. Draw-list/CI green proves the tested seam only; a mechanism with no consumer renders nothing.
- A PR with `mergeable: CONFLICTING` gets NO pull_request CI runs at all — `gh pr checks` says "no checks reported", and close/reopen + empty-commit pushes do NOT help (GitHub cannot build the merge ref, so the event produces no run, silently). When a PR shows zero checks, run `gh pr view <n> --json mergeable` FIRST; if CONFLICTING, rebase onto main and push — CI registers on the next buildable head. Common cause: parallel portal workers branching before sibling PRs merge (compositor/scene files are high-collision).
- Coordinator shell hygiene: after `cd`-ing into a worker's `.worktrees/<dir>` for a one-off git operation, cd back IMMEDIATELY — the Bash working directory persists across calls, and later `git add/commit/status` will silently operate on the WORKER's in-progress index (this session: an AGENTS.md commit landed in a worker's mid-rebase staging area and had to be unwound). Prefer `git -C <path>` for one-offs instead of cd.
- Compositor overlay-alpha tests: `render_frame_headless` ALWAYS uses the blending pipeline — the overlay REPLACE `clear_pipeline` path is hardcoded off in headless, so pixel readback CANNOT represent live Windows overlay alpha (overlapping quads blend instead of last-write-wins). Assert on the generated draw list (RectVertex colors / TexturedDrawCmd tint) for overlay-alpha behavior, not readback.
- Whole-tile fade convention: every node FILL must scale its alpha by `Compositor::tile_effective_opacity` (drag boost × portal fade) — established #985/#1002 across flat backdrop, TextMarkdown bg, SolidColor, rounded SDF, StaticImage tint+placeholders. New fill code that omits it re-introduces the see-through/non-uniform-fade class. Deliberately NOT applied to HitRegion hover/press tints or the focus ring (interaction feedback, not tile body).
- Fresh scrollable/portal tiles are mid fade-IN on frame 0 (fills translucent at t=0). Tests asserting opaque portal fills must settle first: warm one frame then `compositor.portal_tile_anim_states.clear()`, or pin with a `duration_ms:0` ZoneAnimationState.
- Markdown parse-cache identity includes content and resolved token values. Resolve `MarkdownScope::Generic` for non-portal surfaces so portal token overrides do not leak; keep commit-time node keys and fallback lookup under the same token scope.
- `RuntimeContext.profile` reaches truncation bounds and config-time agent-ceiling validation; it does not alone prove one unified runtime cadence/budget envelope. `DegradationController` has headless/windowed consumers and `CommandProcessor` feeds the windowed keyboard adapter; verify these callers when reconciling configuration/runtime/input behavior. These paths are not glasses/VR implementation.
- Runtime-produced input must use `tze_hud_protocol::session_server::InputEventSender`, not a raw Tokio broadcast channel: it fans transactional variants (down/up/key/focus/capture/command and submit/cancel) into ordered per-session durable queues while keeping pointer move/hover, scroll-offset, and composer draft state on the bounded droppable lane.
- Portals (`PortalHub`, `crates/tze_hud_portal/src/hub.rs`, keyed by agent and portal id) degrade 30 s after the agent's last publish, poll, or hold (the driver orphans the portal's scene lease: badge, content kept) and are reclaimed 30 s later, taking the transcript and pending input with them. `hud_hold {surface: "portal:<id>", ttl_ms}` suspends that until the hold lapses (0 = until `hud_clear`); a long-poll `hud_input` also keeps it live. After a reap, the next `hud_publish` re-attaches with an empty transcript.
