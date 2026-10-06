# T8.1a generation-1 reconciliation — hud-bstmy.2.6

Audited base: `17de3331b11a1ee55fa22197fd13b99b26dac267` (2026-10-06).
Worker: `agent/hud-bstmy.2.6`, isolated worktree verified by `assert_worker_context.py`.
This is an evidence-only change. Production code and test definitions are unchanged.

Outcome: **coverage gaps remain**. The named file/type deletions landed, and all
72 unique tests named by the pinned `docs/invariants.md` exist exactly once
(78 mentions, including module-qualified references). However,
all three crates retain production items with no production caller, and seven
production error construction sites supply an empty hint. The focused suites
passed (901 passed, 1 ignored); that does not establish the missing contracts.

## Governing requirements and ownership

Read the parent `hud-bstmy.2`, all five implementation sibling descriptions,
acceptance criteria and designs, and the current vision, scope, invariants and
API. The implementation commits are `.2.1` = `8755604a` / PR #1274; `.2.2` =
`e31cae9c` / #1256; `.2.3` = `96c0c12e` / #1263; `.2.4` = `8d955d27` / #1281;
`.2.5` = `7728c7b8` / #1268. They are ancestors of the audited base.

| Requirement | Implementation bead | Classification | Read code / checked evidence |
|---|---|---|---|
| Delete the six resource sharing/GC/refcount/budget/font-cache/store modules and their exports | hud-bstmy.2.1 | implemented | All six paths absent; `src/lib.rs:35` exports only retained modules. Exact sibling deletion grep returns no matches; see `deletion-checks.json`. |
| Remove the redundant `validate_upload` pipeline and narrow the retained resource surface | hud-bstmy.2.1 | partial | `resource/src/validation.rs:481` remains definition-only. `upload.rs:469` implements the live pipeline separately. `dedup.rs:51,71,83,111` retains a scene refcount with no production increment/decrement/read. Six narrowed-lib diagnostic groups remain. Gap R. |
| Preserve upload, resident-memory accounting, runtime widget store, wire codes and resource invariant gates | hud-bstmy.2.1; hud-bstmy.7.1 | implemented | `upload.rs:223,400,469`; `resident_ledger.rs:156`; `types.rs:307`; all eight `store_behavior` gates read and executed. RuntimeWidgetStore is explicitly retained by the sibling non-goal; diagnostics there are follow-up narrowing inputs, not authorization to discard the store wholesale. |
| Delete scene `invariants.rs` and keep a property over the existing layer-0 checker | hud-bstmy.2.2 | implemented | Old module absent and no `invariants::` references. `test_scenes.rs:87,1982` owns the violation type and invokes 13 checks. `tests/proptest_invariants.rs:156` executes the checker; `:485` deliberately violates active-tab identity and detects it (hud-gvqzf / #1350). |
| Delete the five prost/literal protocol suites, retain boundary fuzzing and single-use / agent-bound tokens | hud-bstmy.2.3 | implemented | All five paths absent. `token.rs:108` consumes only a valid matching token; wrong agent leaves it intact. Tests `:249,294,310` execute this behavior. `tests/fuzz_protocol_boundary.rs:377` still executes whole-batch rejection. |
| Delete unused registry snapshot/rendering-policy protos, conversions and generated stub references | hud-bstmy.2.4 | implemented | Exact whole-tracked-tree sibling grep has zero matches. `convert.rs:541` keeps geometry event conversion; `session_server/mod.rs:325` takes the live scene snapshot and sends JSON. Mutation proto-to-scene conversion and portal conversion remain. |
| Narrow dedup/lease/subscriptions and remove the enumerated dead protocol accessors/converters | hud-bstmy.2.4 | partial | `lib.rs:8,9` makes dedup/lease crate-private, but `:12` still publicly exposes subscriptions. `session.rs:138,214,227`, `subscriptions.rs:240`, `auth.rs:116`, `convert.rs:581,688` retain named zero-production-caller items. See Gap P and all reference lists. |
| Delete CursorStyle/EventMask/AccessibilityMeta/TransportConstraint/SceneDiff/SimulatedClock and stale initializers | hud-bstmy.2.5 | implemented | Exact sibling grep across crates/examples/tests is empty; `diff.rs` absent. `clock.rs:148` has the unified TestClock. Node hit-region `local_style` is retained and read by `compositor/src/renderer/tile_render.rs:2528,2534`, as required by the non-goal. |
| Delete scene timing MessageClass/DeliveryPolicy and remaining no-caller scene helpers | hud-bstmy.2.5 | partial | `timing/hints.rs:36,56,143` and root `lib.rs:59` retain the complete unused scene timing-hints model. Its four classes still include `ClockedMediaCue` (`:46`). Production uses protobuf TimingHints and `mutation::BatchTimingHints`, not this type. Other planned helpers remain; 23 narrowed-lib diagnostic groups. Gap S. |
| Every named invariant gate survives or its reference is updated in the same change | all five siblings | implemented | `invariant-test-inventory.json`: 78 mentions / 72 unique attributed test definitions, no missing or duplicate definitions. This inventory establishes existence; implementation and gate adequacy are assessed separately below. |
| Add resource behavior tests; provide llvmpipe local verification; strip old comment/proto references | sibling T8 sub-epics | partial, already owned | Resource tests: hud-bstmy.7.1, closed. llvmpipe: `justfile:86` / T8.0. Pre-reset-reference guard: hud-bstmy.8.3, open. Existing RFC/OpenSpec comments, including `scene/src/timing/hints.rs:5` and `resource/src/lib.rs:6`, belong to `.8.3`; no duplicate gap is proposed. |
| Production/test abstractions need a current user; test user-visible behavior | docs/scope.md:37,62–65; docs/vision.md:85–99 | partial | Named structural removals are complete, but the residual compiled islands falsify the parent universal deletion claim. Unused APIs serving retained invariant fixtures must be gated or replaced with appropriate fixtures, not removed in a way that loses a named contract. |

## Invariant coverage and operation enumeration

The inventory contains a file and line for **every named gate**, including ones
outside these three crates. This audit reads production seams affected by
T8.1a; runtime/compositor/MCP-only behavior is identified as retained coverage,
not claimed as independently re-proven by the 901-test command.

The inventory includes the qualified references at `docs/invariants.md:115,123`:
`degradation::tests::sustained_overload_does_not_escalate_past_simplified`
(`crates/tze_hud_runtime/src/degradation.rs:722`) and
`mutation_budget_bridge::tests::registered_budget_rejects_mutation_above_tile_limit`
(`crates/tze_hud_runtime/src/mutation_budget_bridge.rs:332`). Both declarations carry test
attributes at the audited base; these runtime gates are inventoried as retained
coverage and were not executed by the focused scene/resource/protocol command.

`operation-path-inventory.json` records the actual search patterns and roots.
`remaining-symbol-references.json` enumerates every Rust reference to the named
residual deletion candidates over `crates`, `examples`, `tests`, **and `app`**.
Raw inventories deliberately retain comments and inline test matches, so a
textual hit is never silently equated to a production call.

| Invariant | Classification for the affected seams | Code paths read and designated gate species |
|---|---|---|
| 1. Arrival/presentation/expiry are distinct (`invariants.md:8`) | implemented | Protocol `verbs.rs:636` computes expiry from presentation, `mutations.rs:739,982` schedules against scene time; scene `graph/timed.rs:26,88,127` queues/applies/sweeps tiles; `graph/zone_ops.rs:902,960` sweeps zone/widget publications. Both publish/batch paths are pinned by the five named `grpc_*` timing tests at `session_server/tests/invariants.rs:260,289,320,438,482`; mixed-widget expiry is a separate surface gate at `graph/spec_scenarios.rs:2109`. No new timing gate proposed. |
| 2. Three message classes (`:26`) | implemented | Live `session_server/traffic.rs:29` exhaustively classifies outbound payloads, `:70` classifies batches; transactional replies and lease replay use `verbs.rs:79,109` and `mod.rs:735`. The dead scene MessageClass does not drive these paths. Retained input batching/latest-wins gates listed in the inventory are the designated behavior gates. |
| 3. Runtime/human override wins; agents cannot address chrome (`:37`) | implemented at retained seams | Shared safe-mode check is `verbs.rs:189`; claim checks it at `:274`; scene active-lease checks reject suspended mutation/publication. All terminal human-dismiss states use `graph/leases.rs:149` and common cleanup. `viewer_dismiss_tile_revokes_lease_in_any_live_state` (`graph/tests.rs:4823`) and protocol viewer-dismiss push gate execute these seams. Runtime hung-agent/chrome gates remain; no T8.1a deletion removed their definitions. |
| 4. Disconnect is orphaning; reclaim is lease-scoped (`:71`) | implemented | All scene disconnect/reconnect transitions are `graph/leases.rs:236,257`; expiry paths `:315,378` share `lease_terminal_state` and `reap_lease(:418)`; explicit revoke `:113` also calls `clear_publications_for_lease`. That helper (`graph/zone_ops.rs:810`) filters BOTH zone and widget records by lease id, not namespace. Protocol cleanup retains surfaces, issues resume token; resume `handshake.rs:259,289` uses scene-clock grace and restores owned leases. Named disconnect/grace/resume and two namespace-isolation gates already pin these paths; no new cleanup gate proposed. |
| 5. Lease TTL pauses during suspension (`:95`) | implemented | Individual and bulk suspend/resume `graph/leases.rs:208,219,275,291` pass injected times through Lease methods. Expiry excludes suspended leases from TTL and handles the explicit suspension safety bound. Existing named suspend/resume gates cover this; wall-time advancement is not used for lease TTL tests. |
| 6. Degradation changes drawing, never scene (`:107`) | implemented structurally, retained external gate | `runtime/src/degradation.rs:157` has controller-owned time/config/frame state and no scene owner; `compositor/src/renderer/mod.rs:36` has the draw policy, no tile-suppression set. The named rendering/overload behavior gates still exist. No degradation implementation was changed by these siblings. |
| 7. Hard caps and whole rejection (`:117`) | implemented at read seams | Scene `graph/budget.rs:170` computes projected tile/texture/node usage before mutation; `mutation.rs:460,472` rejects then snapshots for atomic rollback. Resource inline and chunk completion converge on `upload.rs:469`; capability/hash/size/decode/budget checks precede insertion `:533`, and chunk completion removes the in-flight slot before validation. All eight resource public-API gates execute the budget/error scenarios. Protocol upload transport backpressure (`session_server/upload.rs:501`) is a live throughput mechanism, not an unused resource-budget helper; its heartbeat responsiveness gate is retained. This audit does not claim a new concurrent-upload admission proof. |
| 8. Every rejection has stable code + actionable hint (`:132`) | partial | RequestResult `verbs.rs:47` and resource error envelope `mod.rs:1235` provide hints; **seven other production construction sites do not** (listed below). The existing error-code scan excludes those sites, and affected behavior tests omit hint assertions. Gap E. |
| 9. Injectable deadlines and tests never sleep (`:144`) | partial | Content/lease/resume grace paths above read scene Clock/TestClock. A named invariant transport helper still sleeps 5ms in a real-clock polling loop at `session_server/tests/invariants.rs:67`, bounded by `Instant::now()` at `:54`. It waits for async cleanup rather than advancing grace; it nevertheless violates the literal no-sleep testing contract. Gap T extends the existing scenarios, with no new scenario/gate. Wire timestamps/transport timeouts are separately real-time; this audit makes no claim that all process/transport clocks have been injected. |

## Residual deletion and instrument audit

The exact four sibling deletion scans pass, and all 13 named deleted file
paths are absent. Those scans have important limits: `.2.1` only matches module
names such as `refcount::`, so it misses `ResourceRecord.refcount`; `.2.5`
names six removed types, so it says nothing about MessageClass/DeliveryPolicy;
`.2.4` names four registry identifiers, so it misses unused node conversion
functions. They are specific deletion checks, not universal dead-code guards.

The advisory tool (`scripts/dead_code.py`) was read, its two existing unit
tests passed, and its actual narrowed `cargo check --lib` was run for all
three crates. Logs report scene **23**, resource **6**, protocol **20** diagnostic
groups (groups can name several items, not counts of individual methods).

The tool's `external_names` (`:50–56`) scans only `crates/*/src/**/*.rs` and
`examples/*/src/**/*.rs`; it omits `app/*/src/**/*.rs`. It includes comments,
string literals, and inline `#[cfg(test)]` modules because `is_test_path(:46)`
only examines paths. Its name-only matching conflates unrelated identifiers:
the scene's dead `TimingHints` is kept because protocol code mentions its
different protobuf TimingHints. Grouped reexports (`:63–75`) keep every name
in the group when any one name is used. `scan-instrument-probe.json` records
the actual keep-set results and confirms an app-only synthetic reference is
missed. `just dead-code` is explicitly advisory (`justfile:204`), always
succeeds after a successful check, and is not in the `just ci` dependency
list (`:232`). Thus a green tool result is not deletion coverage.

The separate `grpc_codes_are_in_the_shared_set` test (`verbs.rs:820`) scans
only `verbs.rs` and `mutations.rs` via `include_str!`. Its quoted-literal
heuristic (`:835`) recognizes uppercase underscore strings with selected
suffixes. It cannot inspect dynamic enforcer codes, `SessionError` paths in
`handshake.rs`/`mod.rs`, or `AuthRejection` in `auth.rs`, and it makes no hint
assertion. Its name/comment scopes it to RequestResult; do not treat it as a
gate for the universal rejection-affordance requirement.

Empty production hint sites, fully enumerated by the source search:

- `protocol/src/auth.rs:162`: non-PSK credential rejection (flows through both Init/Resume).
- `protocol/src/session_server/handshake.rs:141,338`: budget registration refusal on Init and Resume.
- `protocol/src/session_server/mod.rs:221,235`: inbound stream closes or fails before handshake.
- `protocol/src/session_server/mod.rs:282`: first message is not Init/Resume.
- `protocol/src/session_server/mod.rs:770`: sequence gap or regression.

Other `String::new()` hits in `lease.rs` are inline **successful-response test
fixtures**, not production rejections. `verbs::ok` also represents success.
Named wrong-local-peer Init/Resume tests (`session_handshake.rs:470,532`) and
sequence tests (`:129,187`) execute four categories but assert only code or
message. Extend them rather than add duplicate scenarios. Budget-registration
rejection and initial malformed/closed handshake categories have no equivalent
hint gate in the read protocol tests.

T7 exclusions are mandatory: the five unused scene-portal-to-proto converters
flagged at `convert.rs:1029,1073,1121,1160,1232` remain for hud-lirh2, as the
siblings explicitly require. Keep `geometry_policy_to_proto` (production
runtime/service call sites), live proto-to-scene MutationBatch conversion,
`WallUs`, `MonoUs`, local_style, and the named invariant tests. The source
inventories include these keep decisions so a cold-start deletion worker does
not blindly apply every compiler diagnostic.

## Follow-up candidates and next reconciliation

Machine-readable candidates are in `follow-ups.json`; blocker disposition is
in `blockers.json`. Keys R/P/S/E/T correspond to the evidence above. Candidates
are scoped to this parent, identify existing implementation dependencies, name
the exact spec/behavior and gate species, and avoid adding tests for existing
scenarios. Additional compiler-only fixture/current-user decisions must be
resolved before deleting any retained interface. The next-generation candidate
depends on the materialized fixes through its candidate dependency keys.

Do not close generation 1 on green tests or on merging this evidence alone.
Coverage gaps remain; the coordinator owns deduplication, materialization,
dependencies and lifecycle. Generation 2 must re-run the exact deletion scans,
the complete named-gate inventory, the actual narrowed-lib checks with manual
app/T7/cfg review, and the affected behavioral gates after gap fixes land.

## Verification

- `assert_worker_context.py`: pass; correct cwd, branch and common Git directory; no inherited GIT overrides.
- Four exact deletion greps: zero matches (expected grep exit 1); 13 removed paths absent.
- Named invariant inventory: 78 mentions / 72 unique attributed test definitions; no missing or duplicate definitions. Module-qualified references are normalized by their final `::` component, preserving all original 70 entries and adding the two previously omitted declarations.
- `PROTOC=/usr/bin/protoc CARGO_TARGET_DIR="$PWD/target" cargo test -p tze_hud_scene -p tze_hud_resource -p tze_hud_protocol --quiet`: pass, 901 passed / 1 ignored / 0 failed. System protoc is 3.21.12; the historical ~/.local path is absent.
- `python3 -m unittest scripts/tests/test_dead_code.py -q`: 2 passed.
- `TMPDIR="$PWD/target" CARGO_TARGET_DIR="$PWD/target" PROTOC=/usr/bin/protoc python3 scripts/dead_code.py <crate>` for each of the three crates: compiler check passes; residual diagnostics preserved in the logs.
- Artifact JSON and paths validated before commit. `git diff --check` passes.
- No full workspace/Windows/GPU/POC rerun: this changes only evidence; those external gate definitions were inventoried, not re-executed. No production behavior or test was changed. **Tests: +0 ~0 -0.**
