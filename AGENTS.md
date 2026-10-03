# Agent Instructions

This project uses **bd** (beads) for issue tracking. Run `bd onboard` to get started.

## Local Dev Harness

The repo ships a `justfile` that reproduces CI gates locally. Requires [just](https://github.com/casey/just).

```bash
just check           # cargo check (fast compilation gate)
just fmt             # cargo fmt --check
just fmt-fix         # cargo fmt (apply formatting)
just clippy          # cargo clippy --workspace --all-targets -D warnings
just test            # cargo test --workspace --all-targets --exclude integration (incl. GPU + pixel_readback)
just test-gpu        # GPU subset only (compositor + pixel_readback), llvmpipe-pinned
just test-integration # integration headless suites
just test-python     # pure-Python suites (pytest + scripts/ci unittest)
just token-footprint # deterministic LLM-facing token-footprint gate
just idle-efficiency-checker # fail-closed idle artifact contract tests
just production-boot # vertical_slice production config boot
just canonical-app-boot # canonical app production config boot
just deps-unused     # cargo machete: unused dependencies
just dead-code <crate> # advisory dead pub-item list for one crate
just dev-mode-guard  # verify dev-mode is not enabled in any package's default-build dependency closure
just clippy-windows-gnu # clippy on x86_64-pc-windows-gnu (prints SKIPPED and passes if the target/mingw is missing)
just cargo-deny      # advisories/licenses/bans/sources (SKIPPED if cargo-deny is missing)
just overlay-harness-contract # pwsh overlay-harness contract test (SKIPPED if pwsh is missing)
just ci              # full local CI sweep (all blocking gates except test-gpu, which `test` already covers; tool-gated gates SKIP loudly when their tool is absent)
```

The toolchain is pinned in `rust-toolchain.toml` (Rust 1.88, matching CI and the
`glyphon 0.8.x` / `wgpu 24.x` co-pin). Workspace lint policy is declared in
`[workspace.lints]` in the root `Cargo.toml` and inherited via `lints.workspace = true`
in every member crate.

GPU tests: run `just test-gpu` (compositor render tests plus runtime `pixel_readback`,
pinned to Mesa llvmpipe via `VK_ICD_FILENAMES`). Device creation is serialized by a
process-wide mutex in the compositor `tests/common` helper, the runtime `pixel_helpers`
helper, and the runtime lib-test `test_support`; the integration, `vertical_slice`, and
boot suites are not serialized, so every `just` recipe that builds a GPU device pins
llvmpipe when the ICD is installed. Don't run bare `cargo test -p tze_hud_compositor` on a host with a hardware
Vulkan ICD (recorded hangs were NVIDIA driver threads). Building needs protoc >= 3.15; set
`PROTOC=/path/to/protoc` if `/usr/bin/protoc` is older.

On Windows, `just` needs Git for Windows' `sh` and a real `python3`; setup, the
per-recipe status, and runtime differences are in `docs/development/windows.md`.

## Quick Reference

```bash
bd ready              # Find available work
bd show <id>          # View issue details
bd update <id> --claim  # Claim work atomically
bd close <id>         # Complete work
```

## LLM Self-Projection

If you are an LLM session that wants to project itself onto the HUD — to show your output, status, or live transcript on screen — use the **`hud-projection`** skill (`.claude/skills/hud-projection/SKILL.md` / `.codex/skills/hud-projection/SKILL.md`).

Trigger phrases: "project this session to the HUD", "attach this agent to HUD", "show this LLM session in a text-stream portal", "check HUD input", "publish status to screen".

This is cooperative opt-in projection, not PTY capture or terminal scraping. Portal state lives in the runtime's `PortalHub`, keyed by agent and portal id; you reach it with the MCP verbs on the surface `portal:<id>` (`hud_publish` attaches and publishes, `hud_input` collects replies, `hud_clear` detaches; see `docs/api.md`). For one-shot zone publishing (no session lifecycle), use the **`th-hud-publish`** skill instead.

## Non-Interactive Shell Commands

**ALWAYS use non-interactive flags** with file operations to avoid hanging on confirmation prompts.

Shell commands like `cp`, `mv`, and `rm` may be aliased to include `-i` (interactive) mode on some systems, causing the agent to hang indefinitely waiting for y/n input.

**Use these forms instead:**
```bash
# Force overwrite without prompting
cp -f source dest           # NOT: cp source dest
mv -f source dest           # NOT: mv source dest
rm -f file                  # NOT: rm file

# For recursive operations
rm -rf directory            # NOT: rm -r directory
cp -rf source dest          # NOT: cp -r source dest
```

**Other commands that may prompt:**
- `apt-get` - use `-y` flag
- `brew` - use `HOMEBREW_NO_AUTO_UPDATE=1` env var

## Landing the Plane (Session Completion)

**When ending a work session**, you MUST complete ALL steps below. Work is NOT complete until `git push` succeeds.

**MANDATORY WORKFLOW:**

1. **File issues for remaining work** - Create issues for anything that needs follow-up
2. **Run quality gates** (if code changed) - Tests, linters, builds
3. **Update issue status** - Close finished work, update in-progress items
4. **PUSH TO REMOTE** - This is MANDATORY:
   ```bash
   git pull --rebase
   git push
   git status  # MUST show "up to date with origin"
   ```
5. **Clean up** - Clear stashes, prune remote branches
6. **Verify** - All changes committed AND pushed
7. **Hand off** - Provide context for next session

**CRITICAL RULES:**
- Work is NOT complete until `git push` succeeds
- NEVER stop before pushing - that leaves work stranded locally
- NEVER say "ready to push when you are" - YOU must push
- If push fails, resolve and retry until it succeeds
- `git pull --rebase` can stall ~4-5 min: the post-checkout hook runs `bd import` of the full issues JSONL. This is normal — wait it out; do not abort the rebase or restart Dolt.

## Worker Isolation for Rust Code Changes

Use a worktree of this repo for isolated workers; don't switch branches in the main checkout:

```bash
scripts/worktree-add.sh .worktrees/<name> -b <branch>
```

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
- Epic report scaffolding lives at `scripts/epic-report-scaffold.sh`; it must normalize both object-root and array-root payloads from `bd show --json`.

## Git / GitHub Workflow

- `gh pr merge <N> --squash --delete-branch` fails with "already checked out" when a worktree has the base branch checked out. The merge still succeeds via the API; the error is only about the local git cleanup. Verify with `gh pr view <N> --json state,mergedAt`.
- `beads-pr-reviewer-worker`: automated reviewer threads (Copilot, Gemini) are left by bots and count toward `UNRESOLVED_COUNT`; they must be replied to and resolved before merge just like human threads.
- `beads-pr-reviewer-worker`: the `list_review_threads.py` helper may return 0 threads even when threads exist; always verify with `evaluate_merge_readiness.py` or `gh api repos/.../pulls/<N>/comments` before treating unresolved count as zero.
- Current `main` branch protection requires status checks but not pull-request reviews. Empty GitHub `reviewDecision` is not itself a blocker for Windows soak PR lanes; verify required checks and merge state before creating approval blocker beads.
- `git pull --rebase` in this repo can stall ~4-5 min in a detached-HEAD state: the post-checkout hook runs `bd import` of the full 1,745-issue `.beads/issues.jsonl` under `timeout 300`. This is normal, not a hung rebase — wait it out; do not abort the rebase or restart Dolt. Light bd commands (`bd ready`) stay sub-second throughout.
- `.codex/skills` should remain a symlink to `../.claude/skills`; `.claude/skills` is the canonical tracked tree and currently strictly supersets the Codex mirror.
- `test_results/` is gitignored; evidence transcripts that must be committed are selectively force-added with `git add -f`, matching prior tracked files in that directory.
- Merged `agent/*` branches are deleted automatically by `.github/workflows/delete-merged-branches.yml` (daily cron, 03:00 UTC). The workflow runs `scripts/prune-merged-branches.sh --execute` which: (a) fetches remote state, (b) lists all remote `agent/*` branches reachable from `origin/main`, (c) skips any branch checked out in a local worktree, and (d) deletes the rest with `git push origin --delete` (never force). To preview deletions without executing: `bash scripts/prune-merged-branches.sh --dry-run`. To trigger an immediate cleanup via CI: `gh workflow run delete-merged-branches.yml`. Stale merged branches that accumulated before this automation was added (hud-3qpgv.6) were cleaned up in that same PR.

## CI / Build

- `main` merges go through the GitHub merge queue (ruleset `main merge queue`, squash). Enqueue with `gh pr merge <N> --squash --auto`. Every required check in `ci.yml` must also trigger on `merge_group`; a required check name that no job emits blocks the queue forever, so update the ruleset whenever a required job is renamed or removed.
- Merge-gate CI jobs must stay under ~8 minutes; slower suites go to the weekly lanes (`perf-budget.yml`, `perf-assert.yml`). Every job has a job-level `timeout-minutes` and every apt step has its own (plus `$APT_OPTS` retries), so a stalled runner fails fast instead of timing out the merge queue.

- `tze_hud_protocol` requires `protoc` (protobuf-compiler) as a build dependency; GitHub-hosted runners don't include it by default. All CI jobs that compile Rust must install `protobuf-compiler` via apt before running cargo commands.
- Quiescent-efficiency proof is emitted by the real constrained windowed runtime via `--quiescent-efficiency-emit`; keep its sampler deadline independent of `scheduled_main_deadline` so a normal winit resume cannot be counted as an excluded measurement wake. The canonical Windows CI harness is `scripts/ci/windows/run-quiescent-efficiency.ps1`: it enforces process affinity and validates an actual `overlay` artifact through `check_idle_efficiency.py` rather than accepting a host-side fixture.
- Timing assertions in tests use `tze_hud_scene::perf_budget::test_budget(base)`: the reference budget times a fixed slack (default 20×, override with `TZE_HUD_TEST_BUDGET_SLACK`). p99 hard asserts stay gated behind `TZE_HUD_PERF_ASSERT=1` (`perf-assert.yml`). The headless `benchmark` example keeps its own CPU/GPU/upload factor vector because the weekly Windows and constrained-envelope gates (`perf-budget.yml`) normalize locked ceilings by it.
- CI's clippy gate runs `cargo clippy --workspace --all-targets -- -D warnings`, so test, example, and bench code must be clippy-clean too. The `integration` package's headless test targets are gated by the `test-integration` CI job.
- FOOTGUN (bit 3 PRs in the 2026-07-04 coordinator session): the repo-root `tests/integration/` is a SEPARATE cargo package that crate-scoped runs do NOT compile. `cargo test -p tze_hud_scene`, `cargo clippy -p <crate>`, or a crate's own `cargo test` all PASS while a struct/field/signature change you made (e.g. adding a field to `ElementStoreEntry` or `WindowedRuntimeState`, changing `MarkdownCache::get`'s arity) leaves stale struct-literals/call-sites in `tests/integration/*.rs` — which then fail CI's `cargo clippy --workspace --all-targets` and `test-integration` with `E0063`/`E0061`. ALWAYS validate a crate-level type change with `cargo clippy --workspace --all-targets -- -D warnings` (or `just clippy`) + `cargo check --workspace` before pushing, and grep `tests/integration/` for literals/calls of the symbol you changed. Note `--all-features` is NOT what CI runs (it pulls glib-sys/GStreamer sys deps the runners lack) — use `--workspace --all-targets` exactly. Related: on a repo where `main` advances under parallel branches, `git fetch && git rebase origin/main` + `cargo check --workspace` right before finalizing catches the same drift when a concurrent PR adds a field to a struct your branch constructs.
- GitHub-hosted `ubuntu-latest` runners can enter a TOTAL queue-stall (observed 2026-06-14 ~03:14–05:47 UTC, ~2.5h): every workflow created after the cutoff sits `queued` with 0 jobs picked up — including `main` post-merge runs. Diagnose it's GitHub-side (not the repo/your infra): `gh run view <id> --json status,jobs` shows `status:queued`, all jobs `queued`, empty `runner_name`; `gh api repos/<o>/<r>/actions/runners` is empty (no self-hosted) and the queued jobs are labeled `ubuntu-latest`. Likely cause: Actions spending-limit hit or a GitHub incident (check Settings→Billing→Actions and githubstatus.com). Not fixable from the repo — surface to the owner.
- WORKAROUND during a confirmed runner-stall, for VERIFIED BYTE-IDENTICAL MOVE-ONLY refactors ONLY: merge on local-green via `gh pr merge <N> --squash --admin` (the `--admin` flag bypasses the required-status-check branch protection; needs admin perms, which the repo owner's `gh` has). Authorize it only AFTER the full local suite passes including `RUSTFLAGS="-D warnings" cargo check -p <crate>` (PRODUCTION config, no `--tests` — this is the key CI-substitute; plain `--tests`/`cargo test --lib` MISS cfg(test)-gated unused-import failures that the production/dev-mode-guard/feature CI lanes catch) plus downstream + `--features ... --bins` guards, and you re-confirm the diff-stat is a pure move. Merge parallel sibling PRs one-at-a-time; a later branch may need a rebase to resolve the shared module-declaration list (non-adjacent `pub mod` lines usually auto-merge server-side). All 18 such local-green merges in the hud-luovo god-module-split session retroactively passed full real CI once runners recovered — zero regressions.
- GUARDRAIL on the above: local-green `--admin` merge is STRICTLY for verified move-only changes. NEVER bypass CI for behavior changes — let those wait for real CI (e.g. the hud-bsr7u portal-freeze fix PR #869 was held for full CI even though runners were flaky, then merged with a normal squash once green). When runners recover, spot-check post-merge CI conclusions on the local-green commits (`gh run list --branch main --json conclusion,headSha`) as a backstop.
- Phase-1 portal review pattern (seen twice, 2026-06-11): perf/wiring beads got closed on PR merge while a named vector or scope item remained undelivered (hud-xq0uo's backtick flood still O(n^2) because the test had no timing assertion; hud-2ps6p's pointer-affordance leg never wired). **These patterns are now encoded as formal review standards in `about/craft-and-care/engineering-bar.md §4` (items 9–10, Merge Mechanics).** Summary: (a) production call-site coverage — every caller of a changed shared symbol must be updated and build-clean, not just the changed definition; (b) adversarial re-review by bead type: perf beads require empirical re-run of the exact named payloads in release mode with timing assertions; wiring beads require grep-verified production call-sites before merge. Reviewers should consult engineering-bar §4 directly rather than this bullet.
- Host disk imbalance (observed 2026-07-18 on this rig's build host): `/` is the main OS disk (1.4T) and hosts both `.worktrees/` and their `target/` build dirs — it was at 84% used (221G free) while `/data` (1.8T, separate disk) sat at 13% used (~1.5T free). `.worktrees/` alone measured 118G; `target/` dirs are the bulk of that (255G observed in a fuller swarm run) and are fully regenerable. If `/` fills again, the durable fix is structural, not a one-off prune: point cargo at the empty disk via `CARGO_TARGET_DIR` (or relocate `.worktrees/` there), since idle->6h `target/` pruning only buys time and recurs. Neither fix is applied yet as of this note.

## Scene / Runtime

- Production pointer-free command input enters at `windowed/keyboard.rs`: default keyboard bindings produce `RawCommandEvent`, `CommandProcessor` applies focus/local feedback and builds the owner-scoped dispatch, and `input_dispatch::dispatch_command_event` serializes the existing protobuf `CommandInputEvent`. Keep portal/composer precedence ahead of the generic adapter, and use `windowed/event_loop_harness.rs` to prove the real pending-keyboard drain rather than a reconstructed closure.
- Shell-reserved keyboard handling is a global `windowed/keyboard.rs` precedence seam: execute the shell action before portal/composer/agent routing, update the authoritative `SceneGraph` plus `active_tab_mirror` for tab switches, and track physical key identity so the matching release remains agent-inaccessible even if modifiers are released first. The earlier winit F8/F9/safe-mode stage must record the same consumed identity.
- `size_of::<Node>()` is tested against a 150-byte limit (scene-graph/spec.md line 302). Adding heap-allocated fields to `HitRegionNode` (which is a variant of `NodeData`) inflates `Node` inline. Box large optional structs (`AccessibilityMeta`, `LocalStyle`) to stay under budget.
- When `HitRegionNode` gains new fields with `#[serde(default)]`, all existing struct literal constructions in tests and production code must add `..Default::default()`. Grep for `HitRegion(HitRegionNode {` across the workspace to find them all.
- `update_hover_state` should use `entry().or_insert_with()` rather than `get_mut` so that HitRegionNodes inserted directly into `self.nodes` (bypassing `set_tile_root`/`add_node_to_tile`) still get local state initialized on first hit.
- Zone ontology (rig-ar23): `ZoneDefinition` needs `#[serde(default)]` on `layer_attachment` and a `Default` impl returning `Content` so older serialized defs without the field still deserialize correctly.
- When the scene's `publish_to_zone` signature grows (e.g., adding `expires_at_wall_us`, `content_classification`), any main-branch wrapper that calls it (e.g., `publish_to_zone_with_lease`) will fail to compile — grep for all call sites.
- When proto messages gain new fields (for example `Publish` or `ClaimTile`), `prost` struct literals in both `src/session_server.rs` tests and `crates/tze_hud_protocol/tests/*.rs` must add explicit defaults (typically `element_id: Vec::new()`), or test-target compilation fails even if library code builds.
- `RenderingPolicy::from_zone_policy()` margin fallback: `margin_horizontal`/`margin_vertical` must fall back to `margin_px` (via `.or(policy.margin_px)`), not hardcoded 8.0 — per spec §Extended RenderingPolicy "when None, falls back to margin_px". Missing this fallback is a spec compliance bug.
- `update_zone_animations` (PR #260) only iterates `current_active` (zones in `active_publishes`). Zones removed from registry entirely (unregistered) disappear from that map, so fade-out on unregistration must be handled by pruning stale animation states for absent zones after the main loop.
- Runtime hover/tooltip behavior is now widget-definition-driven: `WidgetDefinition.hover_behavior` declares trigger rect + delay + target f32 param, `windowed.rs` builds generic trackers via `widget_hover.rs`, and local hover writes MUST use `SceneGraph::set_widget_param_local` (not `publish_to_widget`) to avoid polluting contention/publication state.
- Retained widget SVG text has a simple sans-serif fast path in `crates/tze_hud_compositor/src/widget.rs` using `ab_glyph`; unsupported font families or dominant-baseline values intentionally fall back to the cropped `resvg` text-mask path.
- Font-residency budgeting must cover two different compositor owners: `TextRasterizer` initializes a bundled-only font system, while the widget renderer's `shared_widget_fontdb()` currently calls `load_system_fonts()` and is host-dependent. A deterministic profile font ceiling must either converge the widget path on bundled fonts or measure and preflight supported-host system-font residency before enforcement.
- Pixel-readback tests for display-relative canonical scenes must sample display-relative coordinates too; stale fixed 800x600-era samples can hit the clear background (`[63, 63, 89, 255]`) on the 1920x1080 runtime and look like z-order/contention failures.
- `cargo test -p tze_hud_runtime --lib` should be reliable in headless Linux after GPU-backed runtime-lib tests serialize real headless compositor/runtime construction with the test-only `HEADLESS_RUNTIME_MUTEX`; before that guard, the default parallel harness could wedge under llvmpipe/wgpu while `-- --test-threads=1` passed. Use `timeout 180s cargo test -p tze_hud_runtime --lib` as the bounded focused runtime-lib gate; a warmed run should complete in seconds, while a fresh worktree may spend ~1-2 minutes compiling before tests start.
- Runtime widget cleanup contract: clearing/TTL expiry removes active publications, refreshes `WidgetInstance.current_params` from remaining publications/defaults, and the compositor only draws widgets with active publications; default params alone should not leave a visible stale widget.
- Text-layout logic in `tze_hud_compositor` is CPU-testable WITHOUT a GPU (dodging the llvmpipe deadlock): cosmic-text/glyphon shaping (`FontSystem::new()` → `Buffer::set_text`/`shape_until_scroll` with `Shaping::Advanced`) is pure-CPU; only the wgpu render pass needs a GPU. So functions that consume shaped `Buffer`s — e.g. `compute_inline_backdrop_quads` (selection/inline-code backdrop geometry), and the composer wrap/caret measurements — can be unit-tested by shaping a `Buffer` in the test and asserting the returned geometry (`InlineBackdropQuad`s, line/caret byte↔x). Prefer this over the GPU `require_gpu!`/`render_frame_headless` path, which is CI-only. Note: with `Shaping::Advanced`, `LayoutGlyph::start`/`end` are byte offsets within their `BufferLine`; add the `BufferLine` base offset (`sum(line.text().len()+1)`) for full-text byte ranges when a draft has hard `\n`.
- Cargo can report a STALE test binary right after a `git rebase` (the rebase rewrites file mtimes, confusing cargo's fingerprint): `cargo test` runs an old build and shows a wrong test count / "0 tests" for freshly-added tests that are provably in the source. Fix: `cargo clean -p <crate>` then re-run. CI always builds fresh, so this is a LOCAL-only artifact — don't trust a post-rebase local count without a clean rebuild. (Also: multi-positional `cargo test A B C` filters are flaky about matching; a single-substring filter that forces a recompile is more reliable.)

## Policy / Lease Governance

- Policy-planning and reconciliation must account for three policy surfaces, not two: direct runtime enforcement (`crates/tze_hud_runtime`), pure evaluators in `crates/tze_hud_policy`), and the separate scene-side contract in `crates/tze_hud_scene/src/policy/`. Ownership matrices that omit the scene-side layer are incomplete.
- Identity and permissions (T5 S2): the handshake PSK identifies the agent (`AgentDirectory::resolve` in `tze_hud_scene::config::agents` hashes it and compares SHA-256 digests from `agents.toml`; the directory is `SharedAgents`, read per MCP request and per gRPC handshake; `AgentDirectory::unrestricted(psk)` is the dev/test PSK that claims any id); `StreamSession.capabilities` is the agent's `allow` list expanded to permission strings, fixed for the session. There is no capability negotiation. The session server checks permissions at the boundary (publish, mutation batch, upload, subscription); scene leases carry no capabilities or priority.
- `docs/reconciliations/policy_wiring_seam_contract.md` is the canonical PW-02 seam artifact; policy-wiring implementation/reconciliation beads should use it as the source of truth for level input provenance, ownership boundaries, and PolicyContext/ArbitrationOutcome contracts.
- `crates/tze_hud_protocol::session_server` tests run unrestricted (`HudSessionImpl::new` → `AgentDirectory::unrestricted`); tests that need narrower permissions build a directory with explicit `permissions`.
- Mutation-path pilot latency conformance now lives in `crates/tze_hud_telemetry/src/validation.rs` under `evaluate_policy_mutation_latency_conformance` with budget constant `POLICY_MUTATION_EVAL_BUDGET_US = 50`; `session_server` policy-admission logs emit the structured conformance payload, and future policy telemetry work should extend that harness instead of inventing a parallel metric.

## Windows / User-Test

- `tze_hud.exe` takes no PSK (`--psk` is an unknown flag, `TZE_HUD_PSK` is ignored by the runtime). Agents authenticate against `agents.toml` beside the config, which holds only each PSK's SHA-256 and `allow` list; an invalid file fails strict startup, a missing one means every request is rejected. Pair agents with `POST /pair` (`docs/operations/windows-install.md`).
- Notification styling (urgency tints, card radius, type scale) comes from `[design_tokens]` in the deployed `tze_hud.toml`; copy the block from `app/tze_hud_app/config/production.toml`. Component profiles and the `profiles/` directory are gone; old `[component_profile_bundles]`/`[component_profiles]` sections are ignored.
- `user-test` notification coverage now has a dedicated full-gamut batch file at `.claude/skills/user-test/scripts/notification-full-gamut.json` (urgency 0-3, two-line title/body, long-body containment, and action-button rows); `SKILL.md` references it under "Notification Full-Gamut Pass".
- The gauge, progress-bar and status-indicator widget bundles are embedded in the exe (`crates/tze_hud_runtime/src/widget_startup.rs`); no `widget_bundles\` directory needs deploying. An optional `[widget_bundles].paths` root overrides a built-in by type name.
- `.claude/skills/user-test/scripts/publish_widget_batch.py` can exit `0` even when a `published[*].response.error` payload is present (e.g., `WIDGET_PARAMETER_INVALID_VALUE` validation fixture); treat response-body errors as test outcomes, not process-exit outcomes.
- `/user-test` widget runs can clear durable stale UI with `.claude/skills/user-test/scripts/widget-cleanup.json`; `publish_widget_batch.py --cleanup-on-exit` clears touched widget instances on normal, error, and KeyboardInterrupt paths.
- `.claude/skills/user-test/scripts/hud_grpc_client.py` now exposes resident-flow helpers for avatar PNG creation + content-addressed ResourceIds, Presence Card tile sequencing, and explicit graceful vs hard disconnect paths; the avatar hash helper falls back to a cached cargo-built BLAKE3 binary when Python `blake3` is unavailable.
- Only agents paired in `agents.toml` can connect; Presence Card `/user-test` needs its agent paired there with `allow = ["tiles"]`.
- Overlay `/user-test` sizing must keep runtime config, compositor surface, and `SceneGraph.display_area` in sync. Resident exemplar scripts should prefer the `SceneSnapshot.display_area` dimensions over hardcoded 1920x1080 drag/placement bounds.
- The current Presence Card exemplar contract is the expanded interactive glass variant: 320x112 tile, 24px left/bottom margins, 12px vertical gaps, `InputMode::Capture`, and a 13-node flat stack (background root plus sheen, accent rail, avatar plate, avatar, eyebrow, name, status, chip background, chip text, dismiss background, dismiss label, dismiss hit region). Any remaining 200x80, 3-node, or `Passthrough` assumptions are stale.
- Isolated Windows HUD validation scripts that force-stop `tze_hud.exe` must wait for the stopped PID to disappear before starting the next HUD. The HUD holds a per-user named mutex (`Local\tze_hud`); a second instance exits 0 immediately, so a still-running predecessor makes the next HUD vanish before binding its ports.
- The runtime MCP server (`http://<host>:9090/mcp`) is standard MCP over JSON-RPC: `initialize`, `notifications/initialized`, `tools/list`, and `tools/call` for the five `hud_*` tools (`docs/api.md`). Bare tool-name methods return `-32601`. Tool failures are results with `isError: true` and `{"code","hint"}`. `tools/list` is the cheapest reachability+auth probe. Bearer = the agent's PSK (its SHA-256 is `[agents.<id>] psk_sha256` in `agents.toml`).
- Windows diagnostic Unicode injection via `SendInput` must marshal a full native-shaped `INPUT` union (`MOUSEINPUT`, `KEYBDINPUT`, and `HARDWAREINPUT`); a keyboard-only union can shrink `cbSize` and make `SendInput` fail before HUD keyboard evidence is produced.
- `crates/tze_hud_protocol/proto/session.proto` currently has no resident `UploadResource`/`ResourceUploadStart` client message; raw-tile `/user-test` Python scenarios can drive tiles and node mutations over `HudSession`, but `StaticImageNode` upload still needs a separate helper or transport.
- RFC 0011 already defines `ResourceErrorResponse` and says chunked uploads get a runtime-assigned `upload_id`, but it still lacks a clean start-ack message that returns that `upload_id`; resident scene-resource upload is therefore a spec-contract seam first, not just an unimplemented handler.

## Performance / Benchmarks

- `.claude/skills/user-test-performance/scripts/perf_common.py` owns the `results.csv` schema and now auto-migrates existing CSV headers during append; add new audit fields there first, then wire both MCP/gRPC scripts. `grpc_widget_publish_perf.py` now imports vendored stubs from `user-test-performance/scripts/proto_gen/` (self-contained, no `/user-test` path dependency).
- Durable gRPC widget `Publish` replies are `RequestResult` correlated by `seq`. The Rust publish-load harness (`examples/widget_publish_load_harness/`) is the canonical gRPC widget benchmark path; `/user-test-performance` routes gRPC widget benchmarks through it.
- Windows perf baseline `hud-1753c`: `docs/reports/windows_perf_baseline_2026-05.md` records the first reference-hardware pass. The default config is not benchmark-ready for live widget publishing because benchmark agents lack `publish_widget:*` caps; `examples/benchmark` also does not sample `input_to_local_ack` because it injects no input events.
- Scene-lock double-buffer work (`hud-ibzl4`) remains unwarranted after `hud-pio04`: its paced model deliberately synchronizes 18 single-mutation holds across 180 frames and treats `1..=20` misses as healthy, while non-contended sessions remain 0. It does not exercise the 10x shape or establish a latency breach. Do not add clone-on-dirty / clone-on-writer full-scene snapshots (they violate `efficiency.md` work-proportional-to-change and risk Stage 4/commit budgets); reopen only with live miss-to-staleness/latency correlation, a failing 30-agent/240-tile gate, or a spec-defined change-proportional front/back mutation model.
- Windows live widget benchmarks use `app/tze_hud_app/config/benchmark.toml` (`tze_hud.exe --config <path>`); three-agent soak artifacts are emitted by `.claude/skills/user-test-performance/scripts/widget_soak_runner.py`.
- Long `widget_publish_load_harness` paced runs can hang without per-agent artifacts because responses are drained only after sending; a valid 60-minute Windows soak needs concurrent result draining or an overall diagnostic drain deadline.
- `hud-nfl7n` release soak resource sampling should pass `--windows-process-command-match` set to the benchmark config path so samples target the benchmark-config HUD process rather than any unrelated `tze_hud*` process.
- `widget_publish_load_harness` paced/burst runs must drain `HudSession` responses concurrently while sending; missing `RequestResult` acks should still produce a diagnostic artifact, then exit nonzero so `widget_soak_runner.py` can report per-agent failures.
- Current `widget_publish_load_harness` artifacts preserve aggregate RTT percentiles/max only (`histogram_path: null`), so max-tail outlier localization needs a future bounded top-N RTT tail with request sequence plus send/ack timestamps.
- Production degradation wiring is contract-blocked beyond the fixed 60 Hz case: RFC 0002 defines 14/12 ms and 10/30 presented-frame windows, while the windowed idle gate emits no frame telemetry. Before wiring it, decide cadence-derived thresholds/time windows, quiescent recovery, and the six-level runtime to seven-value `DegradationNotice` mapping. Use `tze_hud_runtime::DegradationController` as the sole transition authority; the independent scene-side `DegradationTracker` must not also run. Level 1 belongs to outbound state-stream fan-out, Levels 2-5 are compositor policy, and the current protocol `broadcast` path can lag/drop despite the transactional contract. See `docs/reconciliations/degradation_cadence_production_wiring_seam_20260716.md`.
- Headless `max_agent_update_hz` is 60 Hz (RFC 0006 §3.4 and `DisplayProfile::headless()`), not 30. It is a per-agent state-stream admission ceiling consumed by configuration validation; neither headless nor windowed runtime uses it for compositor cadence, so it must not be used to justify idle frame submission. The stale canonical-spec 30 was corrected by hud-1utwb.

## Text-Stream Portals

- Text-stream-portals phase-0 intentionally avoids new portal-specific proto RPCs; transport-agnostic adapter proof currently lives in integration tests (`text_stream_portal_adapter.rs`) over the existing primary `HudSession` stream.
- Since T5 S4b, `CreateTile` and the portal mutations (`MutationProto` 13–17) are runtime-internal: only the in-process portal driver applies them, and the session server rejects them from agents. The portal is driven through the MCP `portal:` surface (`hud_publish`/`hud_input`/`hud_hold`/`hud_clear`); agents never draw portal tiles.
- Text-stream portal composer text currently uses a full-span `TextMarkdownNode.color_runs` marker (same color as base text) to request literal monospace rendering through proto conversion; normal printable input should come from runtime character events, with key-down fallback only for Space on the Windows path.
- Text Stream Portal minimized-icon restore should be pointer-down driven; current gRPC hit-region input can drop `pointer_up` for icon gestures, leaving click-vs-drag state stuck until an idle watchdog fires.
- If a long-lived text-stream portal appears frozen while stdout repeatedly prints `Composer text/caret updated`, suspect a composer render/mutation storm rather than a dead gRPC connection. Stop the bridge, inspect the transcript for cleanup errors, and restart `TzeHudOverlay` if lease release times out and leaves orphaned tiles.
- Composer local echo is runtime-owned (`LocalComposerState`) and should render in the focused composer `HitRegionNode` bounds, not as a bottom strip of the whole tile.
- Text-stream portal input-history redesign (submitted inputs as upward-bubbling sections with a bottom-pinned growing composer) reverses the prior `docs/reports/text-stream-refinement.md` "No bottom-chat-style input" decision, so it needs a scoped OpenSpec change before implementation. Keep submitted history bounded in adapter/projection state and materialize only visible cards plus the live draft in the scene graph.

## Cooperative HUD Projection

- Cooperative projection defines `/hud-projection` as cooperative opt-in for already-running LLM sessions: no PTY attachment, no terminal capture, daemon owns durable transcript/inbox/HUD state outside token context, and the LLM session publishes/polls/acks through a provider-neutral contract.
- Cooperative HUD projection portal rendering code lives behind the `tze_hud_projection` `resident-grpc` feature (`crates/tze_hud_projection/src/resident_grpc.rs`). The in-process portal driver uses `render_batch_with_surface` to build portal mutations it applies directly to the scene; the resident gRPC bridge that sent them over a session stream was removed in T5 S4b, because agents can no longer send those mutations.
- The MCP `tools/list` schemas are hand-written in `crates/tze_hud_mcp/src/schema.rs`; every byte is paid by every attached session. `schema::tests` pins them to the `*Params` structs (`deny_unknown_fields`) and to the byte budget, and `scripts/ci/check_token_footprint.py` gates model-visible tokens per flow against `docs/api.md`.
- Cooperative HUD projection gen-2 reconciliation lives at `docs/reports/cooperative_hud_projection_gen2_reconciliation_20260510.md`; it records the accepted runtime-native readback substitution for unavailable SSH desktop screenshot capture.
- `bd create --graph <plan.json>` (bd v1.x, this repo) has two traps: (1) `--dry-run` is IGNORED for `--graph` — it CREATES real beads (delete junk with `bd delete <id>... -f`); (2) per-node `deps` entries (e.g. `"deps":["blocks:otherKey"]`) are SILENTLY DROPPED — only `parent` links and the nodes themselves are created, blocking edges are NOT. Wire edges separately afterward via `bd dep add --file edges.jsonl` where each line is `{"from":"<blocked/dependent-id>","to":"<blocker/prereq-id>"}` (from depends on to; default type `blocks`). Then verify with `bd dep cycles` and `bd ready`. Graph-plan schema: top-level `{"nodes":[...]}`, each node keyed by `key` (not `id`), with `title`/`type`/`priority`/`description`/`parent`.
- cargo-deny `[advisories].ignore` entries are per-advisory-ID and cannot be scoped to one crate instance: if an advisory hits both a direct dep and a build-time transitive (e.g. quick-xml RUSTSEC-2026-0194/0195 in our workspace AND inside winit's wayland-scanner), first bump the direct workspace dep to the patched version, then add the ID waiver documenting that the only remaining instance is the pinned transitive. Follow the existing documented-waiver style in deny.toml (id/reason/action). Verify with `cargo deny check advisories licenses` (fast, local, safe).
- `gh pr merge --delete-branch` fails when the PR branch is checked out in a `.worktrees/` worktree ("cannot delete branch used by worktree") — but the MERGE still succeeds. Sequence: `gh pr merge <n> --squash`, then `git worktree remove .worktrees/<dir> --force`, `git branch -D <branch>`, `git push origin --delete <branch>`.
- Review trap (learned from PR #982→#988): when a PR claims an existing mechanism provides behavior "for free" (e.g. "new focusable nodes inherit the token-driven focus ring"), grep for actual CONSUMERS of that mechanism before accepting — `FocusRingUpdate`/`compute_ring` had zero consumers outside `tze_hud_input`, so the focus ring had never rendered anywhere and the claim was unverifiable-by-construction. Draw-list/CI green proves the tested seam only; a mechanism with no consumer renders nothing.
- A PR with `mergeable: CONFLICTING` gets NO pull_request CI runs at all — `gh pr checks` says "no checks reported", and close/reopen + empty-commit pushes do NOT help (GitHub cannot build the merge ref, so the event produces no run, silently). When a PR shows zero checks, run `gh pr view <n> --json mergeable` FIRST; if CONFLICTING, rebase onto main and push — CI registers on the next buildable head. Common cause: parallel portal workers branching before sibling PRs merge (compositor/scene files are high-collision).
- Coordinator shell hygiene: after `cd`-ing into a worker's `.worktrees/<dir>` for a one-off git operation, cd back IMMEDIATELY — the Bash working directory persists across calls, and later `git add/commit/status` will silently operate on the WORKER's in-progress index (this session: an AGENTS.md commit landed in a worker's mid-rebase staging area and had to be unwound). Prefer `git -C <path>` for one-offs instead of cd.
- Compositor overlay-alpha tests: `render_frame_headless` ALWAYS uses the blending pipeline — the overlay REPLACE `clear_pipeline` path is hardcoded off in headless, so pixel readback CANNOT represent live Windows overlay alpha (overlapping quads blend instead of last-write-wins). Assert on the generated draw list (RectVertex colors / TexturedDrawCmd tint) for overlay-alpha behavior, not readback.
- Whole-tile fade convention: every node FILL must scale its alpha by `Compositor::tile_effective_opacity` (drag boost × §6.3 portal fade) — established #985/#1002 across flat backdrop, TextMarkdown bg, SolidColor, rounded SDF, StaticImage tint+placeholders. New fill code that omits it re-introduces the see-through/non-uniform-fade class. Deliberately NOT applied to HitRegion hover/press tints or the focus ring (interaction feedback, not tile body).
- Fresh scrollable/portal tiles are mid §6.3 fade-IN on frame 0 (fills translucent at t=0). Tests asserting opaque portal fills must settle first: warm one frame then `compositor.portal_tile_anim_states.clear()`, or pin with a `duration_ms:0` ZoneAnimationState.
- Markdown parse cache is keyed on BLAKE3(content) ONLY (no token discriminator; link/code styling baked into the cached parse) and there is ONE global `markdown_tokens`. The portal.transcript.* token preference (#1005) is therefore GLOBAL — safe only while portals are the sole governed markdown surface. Tripwire bead: hud-hjckr (per-tile scoping required before any second markdown surface ships).
- Desktop-headroom audit `docs/reports/hud-48s45_desktop_headroom_assumption_audit_20260716.md` found three portability seams: `RuntimeContext.profile` reaches truncation bounds and config-time agent-ceiling validation but does not govern windowed cadence or one unified runtime budget/degradation envelope; `DegradationController` and `CommandProcessor` still have no production runtime consumers. Treat these as configuration/runtime/input reconciliation work, not as glasses/VR implementation.
- `bd create --json` occasionally returns EMPTY output while the create actually succeeded or silently no-opped — if a --json create looks like it did nothing, verify with `bd show` / retry WITHOUT --json before assuming either outcome.
- Runtime-produced input must use `tze_hud_protocol::session_server::InputEventSender`, not a raw Tokio broadcast channel: it fans transactional variants (down/up/key/focus/capture/command and submit/cancel) into ordered per-session durable queues while keeping pointer move/hover, scroll-offset, and composer draft state on the bounded droppable lane.
- Portals (`PortalHub`, `crates/tze_hud_projection/src/hub.rs`, keyed by agent and portal id) degrade 30 s after the agent's last publish, poll, or hold (the driver orphans the portal's scene lease: badge, content kept) and are reclaimed 30 s later, taking the transcript and pending input with them. `hud_hold {surface: "portal:<id>", ttl_ms}` suspends that until the hold lapses (0 = until `hud_clear`); a long-poll `hud_input` also keeps it live. After a reap, the next `hud_publish` re-attaches with an empty transcript.
- Mutation-batch fan-out from user-test clients must call `HudClient.configure_batch_pacing(0.035)` (or stay under 30Hz) — the profile envelope hard-rejects faster with RESOURCE_BUDGET_EXCEEDED/UpdateRateExceeded (hud-2j9as).
