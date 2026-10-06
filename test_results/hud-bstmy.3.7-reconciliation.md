# T8.1b generation 1 reconciliation — hud-bstmy.3.7

Audited snapshot: `17de3331b11a1ee55fa22197fd13b99b26dac267` (2026-10-06).
Worker branch: `agent/hud-bstmy.3.7`. Production code was read only.
Verdict: **partial; coverage gaps remain**. The six closed implementation
siblings removed their named orphan paths, but closing the parent is premature.
Two existing follow-ups need to be attached to this epic, and two additional
bounded follow-ups are proposed below. Run generation 2 after those land.
Current report test delta: **+0 ~0 -0**. No production build or test gate was run
for this evidence-only change; verification is the source/reference inventory,
deletion scans and inspection of the implementing code and existing gates.

All `file:line` citations below refer to the audited snapshot, not a moving main.
The old doctrine/tag is history, not a source of requirements. Sources are
`docs/scope.md:37` (T8), `docs/scope.md:60` (Working rules), `docs/vision.md:85`
(deferred means deleted), `docs/invariants.md:3`, the `hud-bstmy.3` description,
and every sibling's description, acceptance criteria and Files/Tests lists.
No escalation sibling exists under this epic. No changes were made to the
per-monitor branch or PR #1336, T7 boundary files, or Beads lifecycle state.

## Requirement checklist

| Requirement / implementing sibling | Classification | Code and acceptance evidence |
|---|---|---|
| Delete `font_loader.rs`, its module/reexports, and old font-loader tests (.3.1, PR #1272, `61279c66`) | implemented | File is untracked/absent; runtime module list starts at `crates/tze_hud_runtime/src/lib.rs:39`; the exact deletion regex over all four mandated roots returns zero. The resource accessor's old font-loader caller was not restored. New T10 font loading is a different live path. |
| Cut the channel zoo to frame-ready watch and OS queue payload (.3.1) | implemented within explicit T7 exception | `crates/tze_hud_runtime/src/channels.rs:9`, `:22`, `:32`, `:41`; frame-ready watch is used by `crates/tze_hud_runtime/src/windowed/lifecycle.rs:2250`; queue producer/consumer wiring is at `crates/tze_hud_runtime/src/windowed/lifecycle.rs:1617` and `crates/tze_hud_runtime/src/windowed/input_dispatch.rs:742`. The queue has no drain and carries `allow(dead_code)`, expressly retained until the portal path stops constructing it (`channels.rs:27`). Do not count that protected retention as a new deletion bead. |
| Delete fake stage1–8 methods, `run_frame`, `run_scene_frame`, and nine unused budgets; retain slim pipeline and hit-test snapshot (.3.1) | implemented | `crates/tze_hud_runtime/src/pipeline.rs:22` retains exactly STAGE3/4/5 and INPUT_TO_NEXT_PRESENT; `:157` is a single snapshot owner; `:179` is the later shared deadline sweep. No old stage-method definitions remain. `:201` retains the hit-test behavior test. |
| Delete `TileBoundsEntry` named in the original Files list (.3.1) | deviates, justified live dependency | `crates/tze_hud_runtime/src/pipeline.rs:49` remains because `HitTestSnapshot.tiles` (`:42`), construction (`:84`) and hit testing (`:117`) require it. Its close-hover fields now implement viewer dismiss (`:127`). Deleting this live data type would contradict the same bead's snapshot keep-list. No follow-up proposed. |
| Delete telemetry thread, generic graceful shutdown/config/role/handle helpers and macOS elevation (.3.1) | implemented | Definitions are absent from `crates/tze_hud_runtime/src/threads.rs`. Remaining shutdown token, compositor spawn and network runtime have live consumers in `crates/tze_hud_runtime/src/windowed/mod.rs:112`, `:1452`, `:2636`, `:2763`. Windows elevation (`threads.rs:135`) calls the Windows thread API directly and remains called at `windowed/mod.rs:1372`; removed macOS/adapter policy was inspected in the deletion diff. |
| Delete overlay support/fallback probe, tab-name collector, media regression file and unused runtime dev-deps; shrink exports (.3.1) | implemented for enumerated items, partial for parent-wide no-caller claim | `crates/tze_hud_runtime/src/window.rs:107` retains the live pointer-capture decision; dead probes/collector/regression file are absent. `crates/tze_hud_runtime/Cargo.toml` no longer has the `blake3`/`proptest` dev-deps. `crates/tze_hud_runtime/src/lib.rs:39` narrows internal modules and `:67` begins the reduced reexports. Additional caller-free runtime methods remain in G3 below; shell leftovers already belong to `hud-6vuue`. |
| Delete SIGHUP/ReloadConfig/hot state/RuntimeService, preserve startup validation (.3.2, PR #1280, `cfb38d00`) | implemented | Exact regex has zero references. `crates/tze_hud_protocol/proto/session.proto:21` defines the sole remaining service, HudSession. `crates/tze_hud_config/src/validate.rs:12` parses/validates without runtime writes; `app/tze_hud_app/src/main.rs:333` converts errors for strict startup and `:1060` rejects invalid startup config. |
| One TOML→RuntimeContext implementation for both runtimes (.3.2) | implemented | The sole parse/freeze/fallback builder is `crates/tze_hud_runtime/src/runtime_context.rs:153`, forwarding to `:136`; windowed delegates at `crates/tze_hud_runtime/src/windowed/network.rs:19`, headless delegates at `crates/tze_hud_runtime/src/headless.rs:143`. Headless's explicit dev-mode check for absent TOML is an admission guard, not another config builder. Gate: `runtime_context.rs:396`, `from_toml_loads_valid_config_and_falls_back_on_bad_input`; app startup validation tests remain at `main.rs:1690`, `:1703`, `:1713`. |
| Delete dead directories, scene JSON fixtures, gauge scenario, orphan input source/script, CHANGELOG and tracked bytecode (.3.3, PR #1254, `734a1722`) | implemented; literal acceptance scan is overbroad | All exact Files-list paths are absent from the tracked inventory, including `tests/golden`, `tests/v1_proof`, `tests/vertical_slice/mod.rs`, `tests/scenes/*.json`, `tests/user-test/gauge-5step-scenario.json`, `crates/tze_hud_input/src/scene_local_patch.rs`, `scripts/mcp_reachability_check.py`, `CHANGELOG.md`; `git ls-files '*.pyc'` is empty. `.gitignore:6` ignores `__pycache__/`. Root regex has two legitimate `scene_local_patch_*` test names in `crates/tze_hud_input/src/events.rs:312` and `:318`, not orphan-file imports. |
| Delete compositor adapter and named dead methods, vertical-flow wrapper, easing helper; rewrite retained tests to production path (.3.4, PR #1282, `b21992c3`) | implemented for enumerated removed APIs | Exact deletion regex returns zero. Adapter file is absent; `mark_dirty`, `update_format`, `has_active_animations`, `StyleAttr::is_plain`, `loaded_font_count`, `with_tau_ms`, test-only TextItem constructors and `VerticalFlowLayout` definitions are absent. `crates/tze_hud_compositor/src/vertical_flow.rs:337` retains live tile-flow resolution. `crates/tze_hud_compositor/src/renderer/hit_regions.rs:37` retains the precomputed production population path, and `renderer/tests/hit_drag.rs:483` uses that path rather than the deleted duplicate wrapper. |
| Delete no-op compositor/runtime/render_artifacts `headless` feature and dependency forwards (.3.4) | implemented | No feature table exists in `crates/tze_hud_compositor/Cargo.toml`; runtime feature table at `crates/tze_hud_runtime/Cargo.toml:8` contains dev-mode/test-harness only; `examples/render_artifacts/Cargo.toml:18` and `tests/integration/Cargo.toml:116` use compositor without the feature. `examples/benchmark/Cargo.toml:18` still declares its own `headless` feature, which controls benchmark mode and is outside the deleted compositor feature. Runtime lib-doc feature table is stale prose (`runtime/src/lib.rs:29`), tracked by T8.4's reference cleanup rather than a resurrected Cargo feature. |
| Narrow remaining no-external-production-caller compositor items; move pixel helpers out of compiled API (.3.4) | partial — G1, existing `hud-rk7q5` | `crates/tze_hud_compositor/src/lib.rs:8` still exports every module publicly; `crates/tze_hud_compositor/src/surface.rs:935` / `:950` compile pixel extraction/assertion helpers used only in test trees. Additional examples include `renderer/mod.rs:1273` (unused recovery accessor) and `widget.rs:2266` (test cache clearing). Closing .3.4 documented this remainder; the parent must retain the dependency. |
| Delete duplicate input events::InputEnvelope/EventBatch, scroll unregister, batching accessor and unused drag accessors (.3.5, PR #1258, `6f7220f0`) | implemented for enumerated removed items; partial no-caller claim | `crates/tze_hud_input/src/events.rs:20`, `:55`, `:202` are the live hit result/route/patch definitions; no duplicate envelope/batch definitions remain there. `unregister_tile`, `pending_agent_count`, old drag accessors and old focus revoke/disconnect hooks are absent. Portal directly reads retained `InputProcessor.drag_states` at `crates/tze_hud_runtime/src/windowed/portal.rs:441`. G3 lists other unprotected input convenience APIs still without production callers. |
| Focus is cleared after lease removal and retained during disconnect grace (.3.5) | code implemented; behavior gate partial — G4 | `crates/tze_hud_input/src/focus.rs:514` clears missing owners, skipping missing fallback tiles. Windowed calls it at `crates/tze_hud_runtime/src/windowed/lifecycle.rs:1341`, from the real settle sequence at `windowed/mod.rs:811`, before ring publication and pending keyboard drain (`:825`). All terminal paths remove tiles: `crates/tze_hud_scene/src/graph/leases.rs:113` (revoke) and `:315` (TTL/grace reaper); `:236` (disconnect) keeps the tile and only badges it, as invariant 4 requires. The sole gate `focus.rs:1307` manually calls `scene.tiles.remove`, then the helper; it never invokes revoke, clock-driven expiry, or runtime settle. |
| Delete budget-ladder telemetry records, sender/channel/drain and calibration accessor (.3.5) | implemented for named records, partial parent no-caller claim | No definitions/references to BudgetTier/BudgetViolationKind/BudgetViolationEvent/FrameTimeShedEvent/TelemetrySender/telemetry_channel/drain_channel/is_fully_calibrated remain. `crates/tze_hud_telemetry/src/lib.rs:15` exports FrameRecorder/TelemetryCollector without the old sender. Live frame recorder remains at `collector.rs:77`. G3 lists unused compiled collector convenience APIs. |
| Delete schema emitter, validate-only runtime/tab/layout fields and event-tab-switch state (.3.6, PR #1269, `d61e7bf3`) | implemented deletion; unknown-key behavior deviates — G2 | Schema/profile source files and schemars derives/dependency are absent; exact specified config regex returns zero across config/scene/app. `crates/tze_hud_config/src/raw.rs:26` / `:37` have no removed fields. But these structs and RawConfig (`:179`) deserialize permissively, and `crates/tze_hud_config/src/loader.rs:48` directly uses `toml::from_str`; removed/unknown keys are ignored before validation. The promised hinted error is absent. |
| Built-in profiles only; reject custom `[display_profile]` and `auto` with hint (.3.6) | implemented | `crates/tze_hud_scene/src/config/mod.rs:130` is fixed lookup; `crates/tze_hud_config/src/loader.rs:250` uses it after exhaustive validation (`:321`). `raw.rs:195` is a presence-only AnyValue marker; `loader.rs:169` always rejects it with `DisplayProfileNotSupported` and a removal hint. Gate `crates/tze_hud_config/src/tests.rs:379` parses a real table and checks the error/hint. The marker preserves refusal behavior; it is not a profile override. |
| Keep every named invariant test or update invariant document in same change (all six siblings) | implemented preservation inventory | All **78 mentions / 72 unique named tests** resolve to live `#[test]` / `#[tokio::test]` declarations; exact index appended below. This verifies preservation, not that a declaration alone proves production wiring. G4 is the independent gate-coverage gap. |
| T8 broad behavior test diet/resource tests/llvmpipe/reference cleanup | mapped, not this parent's closure claim | Test diets belong to `hud-bstmy.6` / `.7`; resource API gates live in `crates/tze_hud_resource/tests/store_behavior.rs:84` onward (`hud-bstmy.7.1`). llvmpipe recipe is `justfile:86` (`hud-bstmy.1.1`), compiler/CI parity `justfile:232` (`hud-bstmy.1.2` / `.1.9`), and pre-reset references remain deliberately tracked by open `hud-bstmy.8.3`. Do not create duplicate cross-epic follow-ups. |

## Universal/negative path enumeration

The exact sibling acceptance scans were executed against the audited HEAD.
They are acceptance commands, not persistent fail-closed CI guards.

| Scan | Mandated roots | Result at audited HEAD / instrument limitation |
|---|---|---|
| .3.1: `font_loader\|spawn_telemetry_thread\|graceful_shutdown\|check_overlay_support\|run_scene_frame` | `crates app examples tests` | 0 hits. Extended symbol/definition inventory also checked ShutdownConfig, ThreadRole, CompositorThreadHandle, elevate_macos, collect_tab_name_to_id, old stage methods and removed Cargo dev-deps. |
| .3.2: `reload_hot_config\|HotReloadableConfig\|RuntimeService\|reload_triggers\|SIGHUP` | `crates app examples tests` | 0 hits. Sole service and all RuntimeContext builder call sites read; both runtime entrypoints delegate, app validates before starting. No platform-specific SIGHUP path survives. |
| .3.3: `CHANGELOG\|scene_local_patch\|mcp_reachability_check\|v1_proof\|tests/golden\|gauge-5step` | tracked repo except `docs/archive` | 2 hits, both retained events.rs test names. Exact tracked path inventory is empty. This report necessarily names deleted paths; future scans must use the pinned snapshot or exclude investigation evidence, and qualify actual module/file references. |
| .3.4: `select_gpu_adapter\|resolve_vertical_flow\|populate_drag_handle_hit_regions\b\|features = \[.*"headless"` | `crates tests examples` | 0 hits. The symbol-boundary on the drag wrapper correctly leaves `_from` production method. Feature regex catches same-line forwards only; Cargo manifests were separately read so multiline tables/arrays cannot escape the audit. This regex does not cover bulk API narrowing or surface test helpers. |
| .3.5: `InputEnvelope\|BudgetViolationEvent\|FrameTimeShedEvent\|TelemetrySender\|is_fully_calibrated\|pending_agent_count` | `crates` | 167 hits, all the retained InputEnvelope contract, its imports/comments/tests. File inventory: input batching/coalescing/envelope/event_queue/lib; protocol events/session protos, subscriptions and tests; runtime input_dispatch. Qualified events.rs definitions have 0 duplicate envelope/batch hits. The other five terms have 0 hits. Input batching/coalescing deletion is independently tracked by `hud-bstmy.4.5`; protobuf/runtime envelopes must remain. |
| .3.6: `schemars\|emit_schema\|max_media_streams\|tab_switch_on_event\|default_layout\|extends` | config crate, scene crate, app | 0 hits. Fields/schema/profile files and tab-switch methods checked directly; permissive unknown-key deserialization is a separate behavior gap, undetectable with this scan. |
| Bytecode | `git ls-files '*.pyc'` | 0 tracked files; `.gitignore:6` prevents recurrence for normal add operations. |

All deadline frame drivers were enumerated: the same
`crates/tze_hud_runtime/src/pipeline.rs:179` sweep is called from
`windowed/mod.rs:1685`, `headless.rs:387`, and
`windowed/event_loop_harness.rs:488`. It applies due batches, drains tile/zone/
widget expiry and expires leases. Disconnect is not tile removal; revoke and
terminal reaping are. Focus clears only after actual removal. Nothing in the
shared builder mutates hot config, and the separate strict app validator remains.
The preserved degradation implementation has no scene owner
(`crates/tze_hud_runtime/src/degradation.rs:157`) and the compositor policy is
only level/texture parameters (`crates/tze_hud_compositor/src/renderer/mod.rs:36`).
Thus the named deletion work did not introduce state suppression through drawing.

Whole-tree caller searches include **app, crates, examples, benches and tests**,
not just sibling diffs. The remaining precise zero/test-only caller examples
are in G1/G3. These falsify the universal no-production-caller claim without
claiming the rest of each public API is proven dead. Gen-2 must re-enumerate it
after narrowing, including Windows cfg code and protected T7 imports.

## Guard and gate audit

- `scripts/dead_code.py:28` matches public declarations lexically; `:60` matches
  multiline reexports and conservatively retains every name when one is kept.
  `:47` / `:53` scans `crates/*/src/**/*.rs` and `examples/*/src/**/*.rs`, omitting
  `app/**`, benches and external integration tests. `is_test_path` (`:46`)
  excludes explicit tests directories/files but does not remove inline
  `#[cfg(test)]` modules or comments. Unrelated identifier collisions keep
  otherwise-dead methods public; app-only consumers can be mistakenly narrowed.
  It compiles only the target library (`:123`) and returns 0 on successful
  advisory scans, regardless of dead-item count (`:137`). It is not the gate
  for a universal deletion claim. No new scan-test species is proposed; the
  follow-up uses direct call-site inventory plus workspace/Windows compilation.
- `cargo machete` checks dependency usage, not function visibility/callers.
  `justfile:208` expressly calls dead-code advisory. Clippy does not report
  externally public unused API; `allow(dead_code)` masks private examples
  (`surface recovery`, `winit_logical_to_str`). Green gates cannot close G1/G3.
- Existing config rejection gate (`config/src/tests.rs:379`) executes parsing
  and validation, so it correctly pins `[display_profile]` rejection. It says
  nothing about RawRuntime/RawTab unknown keys. G2 extends this nearest table
  rather than adding a second scenario/gate species.
- Existing focus gate (`input/src/focus.rs:1307`) executes the cleanup helper
  after directly deleting a map entry. The production call exists, but removing
  `windowed/mod.rs:811` would leave this test green. G4 moves/extends this same
  gate to the real event-loop harness and lease APIs; no second test is needed.
- `justfile:86` explicitly fails without the llvmpipe ICD, builds first, pins
  VK_ICD_FILENAMES/HEADLESS_FORCE_SOFTWARE and bounds test execution. GPU creation
  helper mutex lives at `crates/tze_hud_compositor/tests/common/mod.rs:18`;
  `:20` guards creation. This is the existing GPU gate species; no additional
  GPU or scan gate is proposed for unchanged rendering.
- Invariant error-code guard (`crates/tze_hud_mcp/src/error.rs:130`) iterates
  ERROR_CODES and uses an exact backtick match against `docs/api.md` (`:135`).
  `server/tests.rs:338` exercises a bounded sample of returned errors, not an
  exhaustive scan of every rejection branch. This audit verifies these gates
  were preserved; wider protocol/error closure belongs to T8.1a/T5 reconciliation.

Historical GitHub rollups were inspected for all six implementation PRs
(#1254, #1258, #1269, #1272, #1280, #1282): all are MERGED with no failing
rollup conclusion. For #1272 specifically, `windows build + boot smoke` and
`cargo clippy (windows-gnu cross-target)` both completed SUCCESS. This is
historical validation of sibling delivery, not a claim that the current
audited snapshot was rebuilt. T8.1b had no owner-authorized contract change
to `docs/invariants.md`; all named gates remain.

No new mirrored implementation tests, sleep-based deadlines, structural unit
tests or duplicate behavioral gate species are requested. Existing POC/invariant
behavior tests are retained. Source declaration counts across the six merge
commits are .3.1 **-44**, .3.2 **-20**, .3.3 **-11**, .3.4 **-15**, .3.5 **-3**,
.3.6 **-54**, total **-147**. This is a historical text count of Rust
`#[test]`/`#[tokio::test]` function declarations, not an executed-test count;
.3.3 includes uncompiled orphan-file tests. It is distinct from this report's
**+0 ~0 -0** delta.

## Dedupe-ready gaps

### G1 — attach existing hud-rk7q5; compositor no-caller cleanup

Spec: `docs/scope.md:37`, `:63`; .3.4 Files/Approach, parent description.
Existing open bead exactly covers the incomplete bulk public narrowing and
moving HeadlessSurface pixel helpers. Evidence: `compositor/src/lib.rs:8`,
`surface.rs:935`, `:950`, `renderer/mod.rs:1273`, `widget.rs:2266`.
Read all consumers including benches and Windows paths before narrowing;
keep T7 imports and rewrite test-only pixel access to the nearest existing
test helper. Validate workspace all-targets and existing llvmpipe recipe.
Expected new test delta **+0 ~0 -0**; moving helper code is not a new test.
Gate species: qualified source scan plus existing behavior/compile gates.

### G2 — attach existing hud-qudoh; reject removed/unknown config keys

Spec: .3.6 Approach explicitly promises hinted unknown-key rejection;
`docs/scope.md:64` preserves contracts, `docs/invariants.md:132` errors help users.
Evidence: RawRuntime/RawTab/RawConfig at `config/src/raw.rs:26`, `:37`, `:179`,
direct permissive parse at `loader.rs:48`, validator only checks known fields.
Extend the existing rejection test at `tests.rs:379` into one table that
invokes public `validate_config` with `emit_schema`, `headless_width`/height,
`max_media_streams`, `default_layout`, `tab_switch_on_event`, nested tabs.layout,
unknown top-level/runtime/tab keys, and `[display_profile]`. Each must return
a parse/ConfigError with a usable hint. Shipped TOMLs still parse/freeze.
Expected test delta **+0 ~1 -0**; gate species: public config behavior table.

### G3 — new bounded child; finish caller-free runtime/config/input/telemetry helpers

Spec: parent description "Delete production code with no production caller",
`docs/scope.md:37`, `:63`, `docs/vision.md:85`; .3.1/.3.5/.3.6 deletion method.
Confirmed shortlist, with entire-repo searches performed:

- Runtime: `DegradationLevel::is_normal` at `runtime/src/degradation.rs:104`
  has no caller; `winit_logical_to_str` at `windowed/input_dispatch.rs:784`
  has only its two unit-test callers (`:897`, `:904`) and is dead-code-allowed.
- Config: `parse_token_value` at `config/src/tokens.rs:197` has only its four
  unit-test callers (`:1296`, `:1302`, `:1309`, `:1315`);
  `custom_zone_type_names` at `config/src/zones.rs:145` has no caller.
- Input events: `HitTestResult::requires_agent_dispatch` (`events.rs:40`,
  only the unit test at `:288`); `LocalStateUpdate::{with_focused,has_changes}`
  (`:125`, `:137`, neither has a caller); `ScrollOffsetUpdate::{from_user,
  from_agent}` (`:165`, `:175`, neither has a caller); `SceneLocalPatch::push_scroll`
  (`:223`, no caller), `update_node` (`:228`, only the unit test at `:318`).
  Keep the live patch types and `push_state`/`merge_from` used by local input.
- Telemetry: `TelemetryCollector::emit_json` (`collector.rs:46`) has **no**
  caller; `summary_mut` (`:36`) is used only in `tests/integration/multi_agent.rs:474`
  and five `examples/vertical_slice/tests/budget_assertions.rs` sites (`:214`,
  `:219`, `:306`, `:394`, `:471`). Rework those fixtures through existing public
  record/summary behavior or confine the helper to explicit test support.

Acceptance: remove this shortlist from compiled production, narrow the remaining
unprotected surface and inspect any new dead warnings rather than asserting
all unreferenced names are automatically safe to delete. Qualify calls by owner
so unrelated `update_node`/`from_user` symbols do not create false positives.
Keep every invariant declaration and live behavior consumer; run existing
workspace fmt/clippy/check, input/config/telemetry tests and existing integration
gates. No new test is warranted for APIs that have no observable production
behavior. Remove the 2 runtime, 4 token-convenience and 2 event-convenience
implementation tests along with their dead helpers. Expected delta
**+0 ~0 -8** (telemetry fixture edits do not add/remove tests).
Gate species: qualified structural deletion scan plus existing behavior gates.
Exclusions: T7 files, input batching/coalescing (`hud-bstmy.4.5`), shell/http
(`hud-6vuue`), HeadlessRuntime scroll wrapper (`hud-jx1fk`), compositor (`hud-rk7q5`).

### G4 — new bounded child; make the existing focus gate exercise removal lifecycle

Spec: .3.5 acceptance 4/Approach, `docs/scope.md:64`,
`docs/invariants.md:73` (disconnect grace), `:96` (TTL).
Code is wired; this is a coverage gap, not a claim that immediate disconnect
must erase focus. Relocate/extend `focus_cleared_when_lease_revoked` from
`input/src/focus.rs:1307` into the existing runtime event-loop harness,
using `install_button` (`runtime/src/windowed/event_loop_harness.rs:1022`),
the real `settle_scene_work`, SceneGraph::revoke_lease and a TestClock-driven
orphan grace expiry. Focus/ring must clear after revoke/terminal grace removal;
disconnect within grace must preserve the same focused surface. Include a
missing fallback tile in the same scenario table, and prove the runtime
cleanup call is necessary. Keep one gate, not both the manual-map test and
a new duplicate runtime test. Expected delta **+0 ~1 -0**, one test moved and
extended, gate species: GPU-free runtime lifecycle behavior.

### Generation 2

Create one generation-2 reconciliation under `hud-bstmy.3` after G1–G4 are
materialized/deduped. Depend on existing `hud-rk7q5`, existing `hud-qudoh`,
the new helper cleanup and focus coverage child, and the evidence PR reaching
main. Re-read current T8/invariants plus implementation siblings, re-run
qualified deletion/manifest scans, rebuild the live caller inventory including
Windows/T7 consumers, and verify the moved/extended gates. Artifact-only
test delta **+0 ~0 -0**. This original reconciliation remains blocked until
coverage gaps are resolved and the next generation confirms closure.

## Preserved invariant test index

The following index is generated from the audited tracked Rust sources.
Each name resolves to a live test attribute and function; repeated mentions
across invariants are deduplicated. This is preservation evidence only.

| Named gate | Test attribute location |
|---|---|
| `test_hud_publish_zone_ttl_sets_content_expiry_and_is_swept` | `crates/tze_hud_mcp/src/server/tests.rs:460` |
| `delay_ms_holds_content_until_due` | `crates/tze_hud_mcp/src/server/tests.rs:528` |
| `notification_ttl_zero_is_held_and_hold_retimes_it` | `crates/tze_hud_mcp/src/server/tests.rs:750` |
| `widget_ttl_only_expired_publication_removed_when_mixed` | `crates/tze_hud_scene/src/graph/spec_scenarios.rs:2108` |
| `test_publication_ttl_ms_uses_expires_at_wall_us` | `crates/tze_hud_compositor/src/renderer/tests/zone_layers.rs:524` |
| `hold_moves_the_fade_deadline_and_ttl_zero_never_fades` | `crates/tze_hud_compositor/src/renderer/tests/idle_gate.rs:430` |
| `grpc_zone_publish_ttl_sets_expiry_and_is_swept` | `crates/tze_hud_protocol/src/session_server/tests/invariants.rs:259` |
| `grpc_zone_publish_expires_at_is_swept` | `crates/tze_hud_protocol/src/session_server/tests/invariants.rs:288` |
| `grpc_zone_publish_present_at_is_held_until_due` | `crates/tze_hud_protocol/src/session_server/tests/invariants.rs:319` |
| `grpc_batch_present_at_holds_content_until_due` | `crates/tze_hud_protocol/src/session_server/tests/invariants.rs:437` |
| `grpc_batch_expires_at_sweeps_tile` | `crates/tze_hud_protocol/src/session_server/tests/invariants.rs:481` |
| `poc_zone_notification_ttl_disappears_unattended` | `tests/integration/poc_acceptance.rs:737` |
| `poc_zone_delay_ms_appears_on_schedule` | `tests/integration/poc_acceptance.rs:769` |
| `test_hud_publish_zone_contention_policy_latest_wins` | `crates/tze_hud_mcp/src/server/tests.rs:489` |
| `test_latest_wins_zone_renders_only_latest_publication` | `crates/tze_hud_compositor/src/renderer/tests/zone_stack.rs:893` |
| `test_pointer_move_coalesced_in_batch` | `crates/tze_hud_input/src/batching.rs:278` |
| `test_coalesce_scroll_latest_wins` | `crates/tze_hud_input/src/coalescing.rs:347` |
| `test_enter_safe_mode_suspends_active_leases` | `crates/tze_hud_runtime/src/shell/safe_mode.rs:267` |
| `test_mutations_rejected_via_shared_state_flag` | `crates/tze_hud_runtime/src/shell/safe_mode.rs:353` |
| `hotkey_event_enters_safe_mode_and_suspends_leases` | `crates/tze_hud_runtime/src/windowed/safe_mode_toggle.rs:112` |
| `safe_mode_hotkey_defaults_overrides_and_rejects_garbage` | `crates/tze_hud_config/src/tests.rs:674` |
| `shell_dismiss_override_removes_portal_tile` | `tests/integration/text_stream_portal_governance.rs:336` |
| `shell_status_snapshot_exposes_no_portal_identity_or_transcript` | `tests/integration/text_stream_portal_governance.rs:302` |
| `viewer_dismiss_tile_revokes_lease_in_any_live_state` | `crates/tze_hud_scene/src/graph/tests.rs:4822` |
| `viewer_dismiss_tile_pushes_reclaimed_override` | `crates/tze_hud_protocol/src/session_server/tests/events.rs:173` |
| `viewer_dismiss_portal_detaches_and_next_publish_reattaches` | `crates/tze_hud_runtime/src/portal_projection_driver.rs:1537` |
| `viewer_close_button_dismisses_hovered_tile_and_notifies_owner` | `crates/tze_hud_runtime/src/windowed/event_loop_harness.rs:940` |
| `poc_portal_viewer_dismiss_then_mcp_verbs` | `tests/integration/poc_acceptance.rs:672` |
| `tile_close_button_draw_and_hit_region_share_token_geometry` | `crates/tze_hud_compositor/src/renderer/tests/hit_drag.rs:1005` |
| `poc_override_safe_mode_wins_with_hung_grpc_agent` | `tests/integration/poc_acceptance.rs:994` |
| `poc_override_exit_safe_mode_with_agent_hung_during_it` | `tests/integration/poc_acceptance.rs:1041` |
| `poc_override_notifies_the_agent` | `tests/integration/poc_acceptance.rs:1070` |
| `hud_publish_in_safe_mode_returns_safe_mode_active` | `crates/tze_hud_mcp/src/server/tests.rs:1122` |
| `safe_mode_does_not_regrant_suspended_mcp_lease` | `crates/tze_hud_mcp/src/server/tests.rs:1181` |
| `resume_restores_mcp_publishing` | `crates/tze_hud_mcp/src/server/tests.rs:1213` |
| `widget_publish_with_suspended_lease_is_safe_mode_active` | `crates/tze_hud_scene/src/graph/spec_scenarios.rs:852` |
| `test_chrome_always_above_max_zorder_tile` | `crates/tze_hud_compositor/src/renderer/tests/surface.rs:32` |
| `windowed_frame_draws_chrome_overlay_in_safe_mode` | `crates/tze_hud_compositor/src/renderer/safe_mode_overlay.rs:88` |
| `disconnect_transitions_to_orphaned_and_sets_disconnection_badge` | `crates/tze_hud_protocol/tests/lease_governance.rs:83` |
| `grace_period_expiry_removes_tile_and_nodes` | `crates/tze_hud_protocol/tests/lease_governance.rs:264` |
| `disconnect_then_reconnect_within_grace_resumes_same_surface_without_duplication` | `crates/tze_hud_runtime/src/portal_projection_driver.rs:1766` |
| `resumed_session_restores_usage_before_accepting_new_mutations` | `crates/tze_hud_runtime/src/mutation_budget_bridge.rs:459` |
| `grpc_disconnect_orphans_leases_and_badges_tiles` | `crates/tze_hud_protocol/src/session_server/tests/invariants.rs:143` |
| `grpc_resume_within_grace_restores_same_lease_and_tile` | `crates/tze_hud_protocol/src/session_server/tests/invariants.rs:164` |
| `grpc_grace_expiry_reclaims_orphaned_lease_and_rejects_resume` | `crates/tze_hud_protocol/src/session_server/tests/invariants.rs:192` |
| `render_frame_reclaims_orphaned_lease_after_grace` | `crates/tze_hud_runtime/src/headless.rs:1296` |
| `orphaned_tile_emits_disconnection_badge_draw_cmd` | `crates/tze_hud_compositor/src/renderer/tests/focus_chrome.rs:247` |
| `poc_portal_abandoned_is_reclaimed` | `tests/integration/poc_acceptance.rs:710` |
| `poc_tile_claim_with_placement_two_round_trips` | `tests/integration/poc_acceptance.rs:837` |
| `poc_tile_update_then_orphan_then_reclaim_after_grace` | `tests/integration/poc_acceptance.rs:897` |
| `tile_lease_reap_keeps_same_namespace_mcp_publications` | `crates/tze_hud_scene/src/graph/spec_scenarios.rs:2765` |
| `revoked_lease_clears_only_its_publications` | `crates/tze_hud_scene/src/graph/spec_scenarios.rs:2775` |
| `test_ttl_excluded_during_suspension` | `crates/tze_hud_runtime/src/shell/safe_mode.rs:328` |
| `test_lease_identity_preserved_across_suspend_resume` | `crates/tze_hud_runtime/src/shell/safe_mode.rs:306` |
| `test_lease_suspend_from_active` | `crates/tze_hud_scene/src/graph/tests.rs:2857` |
| `test_lease_resume_from_suspended` | `crates/tze_hud_scene/src/graph/tests.rs:2892` |
| `significant_degradation_preserves_hidden_tiles_and_opaques_visible_tiles` | `crates/tze_hud_compositor/src/renderer/tile_render.rs:3157` |
| `sustained_overload_does_not_escalate_past_simplified` | `crates/tze_hud_runtime/src/degradation.rs:721` |
| `registered_budget_rejects_mutation_above_tile_limit` | `crates/tze_hud_runtime/src/mutation_budget_bridge.rs:331` |
| `aggregate_limits_are_atomic_across_agents` | `crates/tze_hud_runtime/src/mutation_budget_bridge.rs:362` |
| `agent_texture_budget_rejects_whole_upload_and_stores_nothing` | `crates/tze_hud_resource/tests/store_behavior.rs:83` |
| `runtime_wide_texture_cap_is_shared_across_agents` | `crates/tze_hud_resource/tests/store_behavior.rs:108` |
| `resource_count_cap_rejects_the_extra_resource` | `crates/tze_hud_resource/tests/store_behavior.rs:128` |
| `per_resource_size_cap_rejects_chunked_upload_at_start` | `crates/tze_hud_resource/tests/store_behavior.rs:150` |
| `upload_slot_cap_is_per_agent_and_freed_by_abort` | `crates/tze_hud_resource/tests/store_behavior.rs:171` |
| `chunked_upload_rejected_at_complete_frees_its_slot_and_stores_nothing` | `crates/tze_hud_resource/tests/store_behavior.rs:203` |
| `mutation_batch_oversized_rejected_with_structured_error` | `crates/tze_hud_protocol/tests/fuzz_protocol_boundary.rs:376` |
| `test_structured_error_has_hint_field` | `crates/tze_hud_mcp/src/server/tests.rs:256` |
| `error_codes_are_unique_and_documented` | `crates/tze_hud_mcp/src/error.rs:129` |
| `every_returned_code_is_in_the_closed_set` | `crates/tze_hud_mcp/src/server/tests.rs:337` |
| `rejections_carry_stable_wire_code_and_actionable_detail` | `crates/tze_hud_resource/tests/store_behavior.rs:235` |
| `chunk_protocol_errors_have_stable_codes` | `crates/tze_hud_resource/tests/store_behavior.rs:279` |
