"""Assemble the source-backed audit packet; no production/test edits."""
from pathlib import Path
import json
import re
import subprocess

OUT = Path('test_results/reconciliation/hud-bstmy.2.12')
HEAD = json.loads((OUT/'audit-context.json').read_text())['source_head']
source_delta = subprocess.check_output(['git','diff','--name-only',HEAD,'--','.',':!test_results/reconciliation/hud-bstmy.2.12'],text=True)
assert not source_delta, 'Do not reuse source-pinned evidence across changed non-evidence blobs: '+source_delta
authority = 'docs/scope.md:37,62-65; docs/vision.md:85-99,107; docs/invariants.md:132-149'
protected = 'Preserve all 72 named invariant definitions and the full protected T7 file/import list in audit-context.json. No auth/allow/trust/schema/persistence/policy expansion; no new mirror/source-scan/test species.'

rows = [
('C01','Delete six unreachable resource modules and exports','implemented','resource/src/lib.rs:30-64; deletion-checks.json shows all six absent and the exact resource grep empty.'),
('C02','Remove duplicate validation and ResourceRecord refcount; classify R6','implemented','resource/src/upload.rs:469-568 is the retained complete_upload; validation.rs now only owns individual checks. Current R6 references are in current-diagnostic-reference-inventory.json. Further residuals are C12/R2, not an assertion that the six prior groups persist.'),
('C03','Keep upload, resident ledger, font and runtime-widget semantics and eight store gates','implemented','resource/tests/store_behavior.rs:84-309; resource/upload.rs:241 retains ResourceRecord.resource_type as a live inline dedup admission reader; protocol/session_server/upload.rs:239,433; resource/resident_ledger.rs:156-206; runtime/headless.rs:294-301 and windowed/mod.rs:2830-2838. Physical resident and logical leased-texture budgets are different owners. Concurrent count admission remains a qualified source risk below.'),
('C04','Delete invariants checker module; retain layer-0 property behavior','implemented','scene/src/lib.rs:19,57; scene/tests/proptest_invariants.rs:156,173,223,303,348,485; test_scenes.rs owns the retained checker. Exact invariants:: scan is empty. Broader checker changes are already owned by hud-369u8.'),
('C05','Delete five prost/literal suites, retain fuzz and agent-bound single-use tokens','implemented','deletion-checks.json records five absent paths; protocol/token.rs:108-142,249-318; protocol/tests/fuzz_protocol_boundary.rs:377. No new boundary species introduced.'),
('C06','Delete registry protobuf models/converters and regenerated stub references','implemented','No match in code/protos/examples/app/stubs. Exact unbounded grep at the source head matches historical test_results/reconciliation/hud-bstmy.2.6/deletion-checks.json:33, exit0. Thus the literal prints-nothing AC deviates as an evidence-contaminated instrument; production deletion is implemented.'),
('C07','Narrow/delete protocol helpers and classify P20','implemented','protocol/src/lib.rs:8-16; convert.rs:11,26,36,225,455,464,493,590,707; session.rs:246; service.rs:213,320; token.rs:153. Five portal outbound converters remain explicitly T7-excluded. Additional grouped-reexport/write-only misses are C12/P2.'),
('C08','Delete six scene types, diff, SimulatedClock; retain local style','implemented','Exact deleted-type scan is empty; timing/mod.rs:6-8 keeps WallUs/MonoUs; clock.rs:148 keeps TestClock. compositor/renderer/tile_render.rs:2528,2534 really reads local_style. No similarly named protocol type is a scene caller.'),
('C09','Remove dead scene timing model and classify S23','implemented','scene/timing/hints.rs absent; lib.rs:50-60 retains real BatchTimingHints, WallUs, MonoUs. Complete 23-group matrix below includes owner-qualified BudgetExceeded/get_occupancy/to_json distinctions. budget_warning has production producers and named fixture readers, no production reader.'),
('C10','Keep each current named invariant definition exactly once','implemented','invariant-test-inventory.json independently parses current docs and attributed definitions: exactly72. Current moved widget gate is graph/widget_tests.rs:7; current graph::limits_tests and degradation::tests qualified names are included. Presence is distinct from execution.'),
('C11','Resource behavior gates, local llvmpipe, strip historical references','partial','Eight store_behavior definitions remain; justfile:74-100 pins/requires real llvmpipe in GPU gates. Old RFC/spec comments remain, e.g. resource/validation.rs:30 and scene/types.rs:1000; exclusively duplicate existing open hud-bstmy.8.3.'),
('C12','Delete/narrow every unprotected item without a current production or necessary fixture user','partial','R2: three fresh dedup diagnostic groups plus suppressed construction-only InflightUpload fields. Record.resource_type remains live through resource/upload.rs:241, unlike unread Record dimensions; earlier draft claim corrected explicitly. P2: write-only heartbeat timestamp and outbound classification helper used only by five classification fixtures, plus two mechanical fixture literal edits. See complete gap packets. Source-name counts are not deletion authority.'),
('I01','Arrival versus presentation; TTL/expiry without agent help','implemented','scene/mutation.rs BatchTimingHints and deferred apply; protocol/verbs.rs:571,619,748 use scene time; graph/zone_ops.rs expiry and compositor frame sweep. Current named MCP/gRPC/integration presentation and TTL gates are inventoried. Horizon injection gap is I09.'),
('I02','Three delivery classes remain distinct','partial','Actual protocol input/response channels and zone contention remain. protocol/traffic.rs:28 classify_server_payload has no production caller; tests/backpressure.rs:16-68 merely classify literals. The two designated input coalescing tests target the unwired batching/coalescing island, already owned by open hud-bstmy.4.5. No new caller is invented.'),
('I03','Runtime/human override wins; no agent chrome/portal disclosure','partial','Real runtime/windowed/safe_mode_toggle and shell/safe_mode paths, viewer-dismiss/reclaimed and hung-agent POC gates remain. FreezeState and SharedState.freeze_active have no human production activation; exact existing decision owner is hud-bstmy.5.6. Safe mode and dismiss are not claimed to prove freeze.'),
('I04','Disconnect orphans; resume restores identity; reclaim is lease-scoped','partial','protocol/mod.rs:482-528 cleanup uses scene.now_millis for lease and token grace, then aborts namespace uploads/removes enforcer before test-only completion. handshake.rs:302,331 restores same leases. Scene lease reclamation and T\'s three repaired actual-drop invariants are implemented. The resource upload worker/namespace seam is a separate unexecuted R3 isolation candidate; no live two-session upload outcome is claimed.'),
('I05','Lease TTL pauses during safe-mode suspension','implemented','scene/lease types and graph/leases.rs:206-260 preserve ttl_remaining_at_suspend_ms and identity; runtime/shell/safe_mode updates shared state; named suspend/resume and MCP gates remain.'),
('I06','Degradation changes drawing only','implemented','runtime/degradation.rs:152 controller owns telemetry/envelope/virtual time, no SceneGraph; compositor/renderer/mod.rs:36 policy has level/texture settings only, no suppression set. tile_render.rs:3158 pins hidden/visible opacity. record_frame_at/record_quiescent_at accept authoritative time.'),
('I07','Hard caps and whole atomic rejection','partial','scene/graph/budget.rs:25 distinguishes logical leased usage from physical ledger; mutation.rs rolls back whole failed batches; runtime/mutation_budget_bridge owns atomic admission per session. Resource bytes have an atomic production ledger. Store count check-then-insert and same-agent multi-session aggregate-cap coverage remain qualified source/proof limits; no live over-admission is claimed and no auth/budget redesign is proposed.'),
('I08','Every rejection has stable code and actionable next step','partial','All current handwritten AuthRejection/SessionError constructors now have nonempty recovery hints, including E\'s actual channel tables. Live RequestResult validation-error strings and malformed lease detail, plus the generic ResourceErrorResponse flow hint, still omit category-specific next actions. E2 qualifies these actual dynamic paths.'),
('I09','Inject deadline time and never use sleep as semantic completion','partial','Scene TTL/grace/degradation and limiter arithmetic take injected/explicit time. T\'s three exact invariant fixtures comply. validate_timing_hints internally samples SystemTime; replay TTL internally samples Instant; five other resume fixtures assume cleanup after100/150ms. Graceful-close fixture accepts timeout as observed termination. T2 packets preserve watchdogs as failure, not semantic evidence.'),
('G01','Deletion scans and narrowed-lib advisory instrument','partial','Exact commands/raw exits in deletion-checks.json. Fresh scans compile, but warnings are not a universal dead-code inventory. scripts/dead_code.py:24,53-62,65-81 omits app, includes inline tests/comments/strings, conflates names, retains grouped/wildcard exports and misses inferred externally returned types. RUSTFLAGS caps warnings. Allow(dead_code) hides private residual fields.'),
('G02','Closed error-code and hint coverage instrument','partial','protocol/verbs.rs:820 includes only verbs.rs/mutations.rs and scans uppercase quoted literals; it excludes handshake/auth/resource files and dynamic producers and never asserts recovery guidance. E tables exercise actual registration/initial-read failures; widget/zone fixtures still assert codes or offending names, not next actions. Stable SessionError/resource enum categories are separate unchanged wire envelopes.'),
]
assert len(rows) == 23

surface_notes = {
'R': [
'Refcount increment/decrement/read island removed; SceneGraph resource_ref_count is a distinct fixture owner.',
'RuntimeWidgetStore put_svg, asset_count, contains and PutOutcome are private cfg(test) persisted-startup/budget fixtures; total_bytes_used removed. open/reindex/enforce_budgets remain production.',
'write_atomic is private cfg(test), called by retained persisted-asset fixtures only.',
'sync_parent_dir is private cfg(test), called by those same persisted fixtures only.',
'ResourceStore dedup_index/abort_upload/in_flight_count use existing test-support/self-dev convention; eight external store_behavior gates are actual users. Production abort_all_uploads remains.',
'validate_upload removed; actual complete_upload admission/decode/storage remains.'
],
'P': [
'Obsolete authenticate_session_init removed; identify_session/evaluate_auth_credential remain live.',
'T7 exception, no production-caller claim: peer-class outbound portal converter.',
'T7 exception, no production-caller claim: lifecycle outbound portal converter.',
'T7 exception, no production-caller claim: display-state outbound portal converter.',
'T7 exception, no production-caller claim: part-kind outbound portal converter.',
'T7 exception, no production-caller claim: complete outbound portal converter and its fixture.',
'cfg(test) node-layout fixture converter; live inbound node-layout decoding retained.',
'cfg(test) ResourceId outbound fixture converter; actual resource replies construct raw IDs directly.',
'existing dev-mode external fuzz/convert fixture, different owner from scene ResourceId and live image decoding.',
'cfg(test) text-overflow outbound fixture; real inbound decoding retained.',
'cfg(test) color-run outbound fixture; real inbound decoding retained.',
'cfg(test) input-mode outbound fixture; real inbound decoding retained.',
'cfg(test) scene_node_to_proto used by existing wire/session fixtures, never claimed a production serializer.',
'cfg(test) nested outbound node-tree fixture; live inbound subtree decoder retained.',
'cfg(test) scene ID outbound fixture; live scene_id_to_bytes is a different helper.',
'get_session/get_session_mut removed; session_count is existing dev-mode/test fixture only.',
'Unused allows_mutations removed; live lifecycle/permission checks retained.',
'inject_input_event cfg(test) actual channel fixtures; broadcast_frame_presented removed; emit_drag_repositioned_event existing dev-mode separate-integration fixture.',
'Unused subscription build_event_batch_message removed; live filtering retained.',
'evict_expired private cfg(test); actual consume validates/removes expiry with explicit now_ms.'
],
'S': [
'MAX_TILES_PER_LEASE removed. suspend/resume test-support real MCP/T7/lease fixtures; expire_lease cfg(test); real runtime safe-mode uses bulk suspend/resume operations.',
'clear_tile_font_scale cfg(test) existing round-trip fixture; live set/query retained.',
'Hover/pressed/focused/drag helper methods are existing test-support hit/render fixtures; live production local feedback writes remain different methods.',
'SceneGraph.resource_ref_count cfg(test) scene ownership/eviction fixtures, distinct from removed ResourceRecord counter.',
'SceneGraph.from_json test-support lifecycle/snapshot fixtures; live serialized snapshot shape unchanged.',
'Lease-wrapped tab fixture methods cfg(test); live manage_tabs boundary remains.',
'create_tile_checked test-support lease fixtures; update_node_content test-support pixel/T7 fixtures; live mutation path retained.',
'unregister_zone cfg(test); breakpoints and no-lease publish wrappers test-support; production lease-aware publish paths retained.',
'MutationResult.budget_warning retains current return/fixture contract. Production producers call is_lease_budget_warning; only graph/tests.rs:3026,3132 read the field. No production reader is asserted.',
'TRANSACTION_VALIDATION_BUDGET_US cfg(test)/test-support for actual vertical_slice budget_assertions.rs; constant declaration attributes are manually qualified.',
'DurationUs and after_wall/after_mono island removed; real WallUs/MonoUs remain.',
'Dead Schedule helpers/model removed; live present_at scheduling remains BatchTimingHints/protobuf fields.',
'Dead scene TimingHints helpers/model removed; generated protocol TimingHints remains a different owner.',
'HitResult.node_hit_ids test-support existing hit-test fixture.',
'Removed only unconstructed CapsError MaxTilesPerLeaseExceeded/MaxNodesPerTileExceeded variants.',
'Removed only LeaseError LeaseNotActive/BudgetExceeded variants. Live ValidationError, ResourceError and BudgetError same-spelling variants remain.',
'geometry_policy_to_absolute_rect test-support separate movable-elements integration fixture; live geometry_policy_to_proto retained in protocol events.',
'Unused unenforced MAX_ACTION_LABEL_LEN removed; renderer/schema behavior unchanged.',
'WidgetRegistry.get_instance/active_for_widget test-support widget/cleanup fixtures. Out-of-line graph test modules are cfg(test), including new widget_tests/test_helpers owners.',
'SceneGraphSnapshot.verify_checksum/from_json test-support fixtures. to_json remains live via actual graph_snap.to_json at protocol/mod.rs:280; unrelated JSON methods are not evidence.',
'ZoneRegistry zones_accepting/all_zones/get_occupancy test-support ontology fixtures. WidgetRegistry.get_occupancy is a different live graph/zone_ops.rs reader.',
'PortalPartKind.is_text_bearing private cfg(test); actual T7 types/variants remain.',
'BatchRejected.primary_code test-support existing atomicity/mutation fixtures; real rejection structure unchanged.'
]
}

def gap(key,title,priority,observed,files,tests,delta,design):
    return {'key':key,'title':title,'type':'task','priority':priority,'parent':'hud-bstmy.2','dedupe_key':'T8.1a-gen2:'+key,
      'source_head':HEAD,'authority':authority,'observed_behavior':observed,'owned_files':files,'nearest_existing_gates':tests,'proposed_test_delta':delta,
      'description': 'Outcome: '+title+'. '+observed+' Maintenance authority: '+authority+'. Non-goals: '+protected,
      'design':design+' Documentation impact: update only affected source comments, keep authoritative specs unchanged. Validate current app/crates/examples/tests/Windows/default consumers; workspace check and all-target clippy exactly as CI for any public/signature changes; jobs1 and repo llvmpipe recipes. No broad abstractions or policy redesign.',
      'acceptance_criteria':['Prove each actual owner/caller/cfg/reexport before the scoped change; preserve production upload/auth/error/token/lease semantics and T7 exclusions.', 'Exercise the listed existing behavior gates with their complete original assertions; proposed Tests: '+delta+'. No new parallel test species.', 'Run current named72 inventory, cargo fmt, workspace check, workspace all-target clippy and required repository gates; commit/push/review via coordinator lifecycle.'],
      'dedupe':{'same_parent_repairs_closed':['hud-bstmy.2.7','hud-bstmy.2.8','hud-bstmy.2.9','hud-bstmy.2.10','hud-bstmy.2.11'],'status':'Full active/closed tracker snapshot cross-checked; this concrete residual is outside the repaired subjects. Existing external owners are listed separately below.'}}

gaps=[]
gaps.append(gap('R2','Finish resource dedup/inspection narrowing and delete construction-only fields',2,
 'Fresh narrowed resource scan reports three remaining groups. InflightUpload.upload_id/started_at are assigned only and suppressed. DedupIndex.is_empty has no caller; contains is an actual store_behavior fixture; remove plus duplicated ledger/font handles are used only by the font-GC self-test. ResourceRecord.resource_type is LIVE at upload.rs:241 in inline dedup type-admission, so retain the field and its constructor parameter. Only ResourceRecord width/height fields have no reader. ResourceStore.font_bytes has no current caller. FontBytesStore try_insert remains a live upload writer; its inspection/legacy convenience methods have fixture-only callers.',
 ['crates/tze_hud_resource/src/upload.rs','crates/tze_hud_resource/src/dedup.rs','crates/tze_hud_resource/src/font_bytes_store.rs','crates/tze_hud_resource/src/lib.rs'],
 ['agent_texture_budget_rejects_whole_upload_and_stores_nothing','runtime_wide_texture_cap_is_shared_across_agents','resource_count_cap_rejects_the_extra_resource','per_resource_size_cap_rejects_chunked_upload_at_start','upload_slot_cap_is_per_agent_and_freed_by_abort','chunked_upload_rejected_at_complete_frees_its_slot_and_stores_nothing','rejections_carry_stable_wire_code_and_actionable_detail','chunk_protocol_errors_have_stable_codes','mandatory_decoded_resource_debits_shared_resident_ledger_atomically','font_source_admission_uses_font_class_and_is_atomic'],
 '+0 ~0 -2',
 'Delete the two construction-only upload fields and their initializers, unused DedupIndex.is_empty, uncalled ResourceStore.font_bytes getter and unread Record dimensions. Retain ResourceRecord.resource_type, ResourceRecord::new resource_type parameter and upload.rs:241 fast-path type comparison unchanged. Delete the unreachable dedup/font GC removal island and its exact two helper-only definitions font_gc_releases_the_retained_source_copy_and_ledger_charge/remove_evicts_entry; simplify only now-dead duplicate handles/constructors. Narrow contains and remaining FontBytesStore inspection/convenience fixtures under the established nondefault test-support/cfg(test) convention, retaining actual atomic font admission and raw stored bytes. Preserve ResourceId/DecodedMeta type-name distinctions, complete_upload admission/hash/dedup, resident ledger, RuntimeWidgetStore persisted behavior and the real transport byte limiter. No GC replacement, font policy change or resource-count concurrency fix.'))
gaps.append(gap('P2','Remove write-only protocol heartbeat timestamp and unused outbound classifier',2,
 'StreamSession.last_heartbeat_ms has initializers and writes but no reader; the real heartbeat timeout is a separate Tokio receive timeout. classify_server_payload has no production caller; only four literal backpressure fixtures and test_degradation_notice_is_transactional call it. Grouped pub reexport keeps it invisible to the name-only advisory scan.',
 ['crates/tze_hud_protocol/src/session_server/stream_session.rs','crates/tze_hud_protocol/src/session_server/mod.rs','crates/tze_hud_protocol/src/session_server/handshake.rs','crates/tze_hud_protocol/src/session_server/traffic.rs','crates/tze_hud_protocol/tests/backpressure.rs','crates/tze_hud_protocol/src/session_server/tests/events.rs','crates/tze_hud_protocol/src/session_server/tests/safe_mode.rs','crates/tze_hud_protocol/src/session_server/tests/render_wake.rs'],
 ['test_heartbeat_echo','transactional_command_input_does_not_lag_under_receiver_backpressure','test_degradation_notice_broadcast_to_active_session'],
 '+0 ~2 -5 (core helper-only removal +0 ~0 -5; two mechanical fixture literal edits)',
 'Delete the unused field/writes/fixture initializers and the outbound classifier/reexport with its exact five helper-only classification tests. Preserve TrafficClass and classify_inbound_batch where the existing freeze queue actually references them; do not decide freeze activation/removal owned by hud-bstmy.5.6. Preserve heartbeat echoes, actual timeout/order/sends, degradation delivery and all real backpressure/event gates. Update fixture struct literals in safe_mode.rs/render_wake.rs without creating tests; preserve real T7 MutationTrafficClass as a distinct runtime owner.'))
gaps.append(gap('E2','Give live request/resource rejections category-specific next actions',2,
 'All SessionError/AuthRejection hints are now nonempty. Live RequestResult producers still use descriptions without a next action: malformed lease bytes at mutations.rs:657, generic conversion/admission/cache error messages, root-node missing data and dynamic ValidationError display text at verbs.rs:282,602,630,715,783. Widget-not-found and unknown-param fixtures assert only codes. mod.rs:1208 emits the same upload-flow hint even for capability/budget/decode/hash failures; expected_flow alone does not describe those recovery actions. Complete producer classifications are in rejection-path-catalog.json.',
 ['crates/tze_hud_protocol/src/session_server/mutations.rs','crates/tze_hud_protocol/src/session_server/verbs.rs','crates/tze_hud_protocol/src/session_server/mod.rs','crates/tze_hud_protocol/src/session_server/tests'],
 ['test_mutation_result_echoes_client_batch_id','publish_to_unknown_zone_names_the_zone','test_widget_publish_not_found','test_widget_publish_unknown_parameter','test_resource_upload_start_requires_upload_resource_capability','test_resource_upload_chunk_error_aborts_inflight_tracking'],
 '+0 ~6 -0',
 'Complete the actual handwritten rejection catalog, following dynamic validation/cached-result/resource-error producers. Add deterministic next-call/retry/repair guidance only inside existing protobuf hint fields (including the existing opaque ResourceErrorResponse.hint JSON); no proto/wire/schema expansion. Preserve exact current code fields, message fields and all acceptance/cache/replay/token/budget/trust/close/send order; description-only text currently occupying RequestResult.hint may be augmented, not silently relabeled as a message-field change. Expose no credential values. Extend the six nearest existing tests through actual outbound channels, including malformed lease correlation and replay guidance. The five T7 converters remain excluded; inactive freeze-policy decisions stay with hud-bstmy.5.6. This is affordance completion beyond E\'s seven AuthRejection/SessionError fixes, not their reopening.'))
gaps.append(gap('T2','Finish protocol deadline injection and observe remaining cleanup tests',2,
 'validate_timing_hints reads host SystemTime internally and too-future test uses a1-second jitter margin. DedupWindow samples Instant internally for its60-second expiry; its two expiry tests instead set TTL0. Five resume fixtures rely on100/150ms cleanup sleeps. test_graceful_disconnect_session_close marks timeout Err(_) as successful stream termination, although no None was observed.',
 ['crates/tze_hud_protocol/src/dedup.rs','crates/tze_hud_protocol/src/session_server/mod.rs','crates/tze_hud_protocol/src/session_server/mutations.rs','crates/tze_hud_protocol/src/session_server/handshake.rs','crates/tze_hud_protocol/src/session_server/tests/timing.rs','crates/tze_hud_protocol/src/session_server/tests/resume.rs','crates/tze_hud_protocol/src/session_server/tests/session_handshake.rs'],
 ['test_timing_hints_too_old','test_timing_hints_too_future','test_timing_hints_expiry_before_present','test_timing_hints_valid_future','test_timing_hints_zero_bypasses_validation','test_ttl_expiry','test_re_insert_after_expiry_treated_as_new','test_resume_with_token','test_reconnect_within_grace_accepted','test_resume_token_single_use','test_resume_auth_required','test_resume_result_carries_subscription_state','test_graceful_disconnect_session_close'],
 '+0 ~13 -0',
 'Use the existing injectable scene wall/monotonic time or narrowly explicit-now operations for scheduling validation and replay expiry, retaining SystemClock default behavior, horizon/TTL/FIFO/capacity/code/cache semantics. Drive positive boundary cases with TestClock/explicit now instead of TTL0 or jitter margins, and update all actual lookup/insert/handshake callers. Reuse the existing test-only SessionRegistry completion witness registered before real transport drop for the five retained resume fixtures. Graceful close must distinguish actual stream end from watchdog failure; never count timeout as completion, and test the existing seam counterfactually without a parallel source guard. Keep bounded watchdogs, hold no locks across await, preserve T\'s three already repaired named invariants and concurrent observer isolation. No new production clock/event architecture, auth posture, lease/grace state machine, or operational transport-timeout policy.'))

verification_candidate = gap('R3','Verify owning-session upload cleanup and worker termination isolation',2,
 'Unexecuted source candidate: multiple connections may register the same authenticated agent namespace. Each has a detached upload worker with its own upload-ID map, while normal session cleanup calls ResourceStore.abort_all_uploads(namespace), deleting every pending upload for that namespace. A worker can still drain queued starts/chunks/completions after its session begins cleanup. The existing one-session success gate does not establish that a live B upload survives A\'s actual disconnect; no runtime failure is asserted.',
 ['crates/tze_hud_protocol/src/session_server/mod.rs','crates/tze_hud_protocol/src/session_server/upload.rs','crates/tze_hud_resource/src/upload.rs','crates/tze_hud_protocol/src/session_server/tests/resource_upload.rs'],
 ['test_resource_upload_chunked_success_correlates_by_request_sequence','test_resource_upload_chunk_error_aborts_inflight_tracking'],
 '+0 ~2 -0',
 'Verification first: extend these two existing channel-driven upload scenarios to observe B\'s acknowledged start, register A\'s existing cleanup witness, drop A\'s actual transport, await actual cleanup and complete B\'s upload. Use authoritative cleanup observation and bounded failure watchdogs, not sleeps. Resolve the candidate explicitly if it is not reproducible. Only if confirmed, stop/join the owning session worker and abort only its actual owned pending upload IDs through existing resource operations, including queued late starts and failed chunks. Joining must not hang on a full event channel after the main receive loop exits; keep completion/order observations at the real channel seam. Preserve agent namespace as authorization/dedup/budget owner, existing per-agent slot and byte ceilings, immutable completed resources, request-sequence/upload-ID correlations, codes/replay/close order and T\'s three exact repaired lease/observer gates. No new trust model, cross-session budget policy, schema, public types/features or T7 change. Any existing abort-method gate/visibility change needs an actual retained cleanup caller and default/workspace/all-target proof; no parallel test species.')
verification_candidate['classification'] = 'unexecuted verification candidate; not an observed runtime defect'
verification_candidate['exact_source_evidence'] = ['crates/tze_hud_protocol/src/session.rs:201-206', 'crates/tze_hud_protocol/src/session_server/mod.rs:322-335,516-519', 'crates/tze_hud_protocol/src/session_server/upload.rs:190-477', 'crates/tze_hud_resource/src/upload.rs:454-456']
verification_candidate['behavior_matrix'] = [
 'Two live sessions, same authenticated agent: B start acknowledged, A actual disconnect completes, B chunk/complete remains correlated and admitted.',
 'Different agents: A cleanup cannot affect B uploads or namespace authorization.',
 'A queued late Start/Chunk/Complete: shutdown joins or cancels owned work, then removes only A pending IDs; no orphaned late upload.',
 'Repeated close, replayed command, bad/unknown upload ID and failed chunk: bounded idempotent cleanup, unchanged result codes and peer state.',
 'Completed immutable resource/dedup record retained; slot/byte/count admission and exact request sequence unchanged.'
]

matrices = {
 'R2': ['Normal/corrupt/hash/size/type/budget upload: all eight retained store_behavior results unchanged.', 'Atomic font admission and rejected charge rollback retained; remove only two GC-helper definitions.', 'Repeated/dedup upload and resident ledger charges unchanged; no count-concurrency redesign.', 'Default app, test-support fixtures and similarly named scene/proto metadata remain build-clean.'],
 'P2': ['Heartbeat echo/real receive timeout and close order unchanged after unused timestamp deletion.', 'Real transactional command channel under receiver backpressure and degradation broadcast unchanged.', 'Inbound freeze classification retained as current caller; human freeze decision remains hud-bstmy.5.6.', 'Replay/cache/token/default/wire/T7 converters unchanged; all affected struct literals and grouped exports resolved.'],
 'E2': ['Malformed lease and dynamic widget/zone validation: same code/message/correlation plus deterministic next action.', 'Capability/hash/decode/resource-budget failures: category-specific repair/retry affordance, same acceptance and sends.', 'Cached rejection replay retains identical guidance and no second mutation/upload.', 'No paired identity, credential, token, host path or permission material in guidance.'],
 'T2': ['Positive before/at/after wall horizon and monotonic replay TTL boundaries; same default clock and expiry rules.', 'Matching/expired/wrong-agent/single-use resume and subscription restoration after observed actual cleanup.', 'Fast cleanup/concurrent observer registration/repeated close: session-ID witness isolation, no lock held across await.', 'Actual stream None passes; watchdog timeout fails; original three T invariants and operational receive timeouts unchanged.']
}
for g in gaps:
    g['classification'] = 'source-confirmed residual; passing existing gates do not cover its universal obligation'
    g['surface_trust_map'] = {'owners':g['owned_files'], 'callers':'All tracked app/crates/examples/tests/benches and Windows cfg paths; distinguish declaration, producer and reader; inventory and surface-matrix are evidence.', 'protected':protected}
    g['behavior_matrix'] = matrices[g['key']]
    g['behavior_verification'] = 'Use the nearest named existing scenarios at real handler/store/time seams; keep all original assertions, and run current named72/default/workspace/all-target/full repository gates. Source audit is evidence, not a new test species.'
    g['governing_spec'] = {'R2':'docs/scope.md:37,62-65; docs/vision.md:85-99,107', 'P2':'docs/scope.md:37,62-65; docs/vision.md:85-99,107', 'E2':'docs/invariants.md:132-136', 'T2':'docs/invariants.md:142-149'}[g['key']]
    if g['key']=='P2':
        g['mechanical_fixture_delta'] = {'modified_definitions':['test_fifo_preserved_when_mutation_arrives_during_drain_window','test_freeze_retransmit_deduped_applied_exactly_once'], 'reason':'Remove last_heartbeat_ms from their StreamSession literals; keep every behavioral assertion. direct_handler_test_session is a shared helper, not a test definition.'}
    if g['key']=='P2':
        # Resolve exact nearest gate names from current source below, rather than stale guesses.
        g['nearest_existing_gates']=['test_heartbeat_echo','transactional_command_input_does_not_lag_under_receiver_backpressure','test_degradation_notice_broadcast_to_active_session']

generation3={'title':'Terminal generation3 reconciliation of T8.1a residual repairs','type':'task','priority':2,'parent':'hud-bstmy.2','generation':3,
 'materialization_required':True,'blocking_gap_keys':[g['key'] for g in gaps],
 'depends_on':'Coordinator must replace gap keys with actual new candidate IDs and require this GEN2 evidence PR merged; never depend on .2.6 or .2.12 closure.',
 'scope':'One terminal same-parent reconciliation, complete23 outcomes/current72/T7/current scanners and real behavior after all concrete residual repairs merge. Explicitly resolve the separately unexecuted R3 upload-isolation candidate through the existing scenario before a universal clean result; do not silently promote source traces to executed defects. Generation4 is forbidden; no second shaping or parallel subsystem recon.',
 'proposed_test_delta':'+0 ~0 -0'}
generation3.update({
 'dedupe_key':'T8.1a-terminal-gen3',
 'description':'Outcome: one terminal current-main T8.1a reconciliation after concrete R2/P2/E2/T2 repairs and this GEN2 evidence PR reach main. Reconcile all23 current requirements/instruments, every current named invariant and protected T7 consumer. Resolve the explicitly unexecuted upload cleanup/count/same-agent-cap proof limits before a universal clean claim. Non-goals: no generation4, no parallel subsystem reconciliation/second shaping, code/test/spec/trust/policy redesign, new test species or automatic original.2.6 closure.',
 'design':'Maintenance authority is current docs/scope.md T8/Working rules, docs/vision.md and docs/invariants.md. Surface/trust map remains the live AgentDirectory/Init/Resume/capability/proto-to-scene/lease/budget/resource/runtime chain across actual app/crates/examples/tests/Windows/default callers, plus protectedT7 paths. Confirm all concrete candidate IDs and their actual merged commits, plus this evidence PR MERGED/reachable; never depend on .2.6/.2.12 closure or a self edge. Re-enumerate owners/readers/cfg/reexports and every rejection emitter; re-run the exact inventories/narrowed scans and existing behavior/default/workspace/all-target/full repository gates at a new recorded main source SHA. Preserve code/message/close/cache/idempotence, grace/time authority, serialization/default and physical versus logical budget contracts. Classify source-only risks honestly and reconcile already-owned8.3/4.5/5.6/369u8 outcomes rather than duplicate them. Docs impact: durable evidence-only report under a generation3 directory, no production/spec amendment. Rollback is removal of report-only evidence.',
 'acceptance_criteria':[
   'Coordinator replaces R2/P2/E2/T2 keys with real materialized IDs and requires their reviewed outcomes plus GEN2 evidence PR actually MERGED/reachable on audited main; no invented/self/cyclic dependency.',
   'Produce complete23-group file:line checklist, current full named-invariant definitions/executions and T7/current default/caller matrix; retain scanner/clock/timeout/wire-category limitations and every skip/failure/raw command source SHA.',
   'Resolve R3 through the existing two-session upload scenarios before claiming cleanup isolation; same-agent admission/count interleavings need actual authority/behavior evidence, not pass counts. Reconcile separately owned input/freeze/history/layer-0 decisions without reopening or inventing production callers.',
   'Run meaningful current existing seam gates and one required normal full sweep under allocated warmed target/jobs1/protoc/actual required llvmpipe. No code/test changes or new mirror scenario species in the reconciliation; Tests:+0~0-0.',
   'Commit/push an evidence PR, return real follow-up/blocker arrays, and let coordinator confirm main merge and actual clean outcomes. Remaining gaps keep outcome partial/blocked with concrete ownership; generation4 is forbidden.'
 ],
 'existing_owner_reconciliation':['hud-bstmy.8.3','hud-bstmy.4.5','hud-bstmy.5.6','hud-369u8'],
 'verification_prerequisites':['R3 unexecuted owning-session upload cleanup candidate','resource-count interleaving proof limit','same-agent aggregate-cap contract/coverage limit']
})
duplicates=[{'id':'hud-bstmy.8.3','status':'open','scope':'pre-reset source/proto references and guard'}, {'id':'hud-bstmy.4.5','status':'open','scope':'unwired input batching/coalescing and real invariant2 gates'}, {'id':'hud-bstmy.5.6','status':'open','scope':'owner decision on never-activated freeze'}, {'id':'hud-369u8','status':'open','scope':'widening retained layer-0 checker'}]
blockers=[{'title':'Materialize and merge the concrete GEN2 residual repairs before terminal generation3','type':'task','priority':2,'rationale':'Evidence-only GEN2 does not implement R2/P2/E2/T2; current negative/universal requirements remain partial.','unblock_condition':'Coordinator materializes exact candidate IDs, merges their independently reviewed outcomes and this evidence PR, then runs only the generation3 terminal reconciliation. No lifecycle changes or generation4.'}]

def full_paths(value):
    """Expand report shorthand to real repository paths, never source text."""
    mappings = {
      'protocol/session_server/':'crates/tze_hud_protocol/src/session_server/',
      'protocol/src/':'crates/tze_hud_protocol/src/',
      'protocol/tests/':'crates/tze_hud_protocol/tests/',
      'resource/src/':'crates/tze_hud_resource/src/',
      'resource/tests/':'crates/tze_hud_resource/tests/',
      'scene/src/':'crates/tze_hud_scene/src/',
      'scene/tests/':'crates/tze_hud_scene/tests/',
    }
    for short,long in mappings.items():
        value=re.sub(r'(?<![\w/])'+re.escape(short),long,value)
    for owner in ('resource','scene','runtime','compositor'):
        value=re.sub(r'(?<![\w/])'+owner+r'/(?!src/|tests/)', 'crates/tze_hud_'+owner+'/src/',value)
    for stem in ('mod','verbs','mutations','handshake','traffic','service'):
        value=re.sub(r'(?<![\w/])protocol/'+stem+r'\.rs', 'crates/tze_hud_protocol/src/session_server/'+stem+'.rs',value)
    for stem in ('token','convert','session'):
        value=re.sub(r'(?<![\w/])protocol/'+stem+r'\.rs', 'crates/tze_hud_protocol/src/'+stem+'.rs',value)
    return value

rows=[(key,requirement,classification,full_paths(evidence)) for key,requirement,classification,evidence in rows]
row_paths={
 'C02':{'validation.rs':'crates/tze_hud_resource/src/validation.rs'},
 'C03':{'windowed/mod.rs':'crates/tze_hud_runtime/src/windowed/mod.rs'},
 'C04':{'test_scenes.rs':'crates/tze_hud_scene/src/test_scenes.rs'},
 'C07':{**{name+'.rs':'crates/tze_hud_protocol/src/'+name+'.rs' for name in ('convert','session','token')},'service.rs':'crates/tze_hud_protocol/src/session_server/service.rs'},
 'C08':{'timing/mod.rs':'crates/tze_hud_scene/src/timing/mod.rs','clock.rs:148':'crates/tze_hud_scene/src/clock.rs:136'},
 'C09':{'lib.rs':'crates/tze_hud_scene/src/lib.rs'},
 'C10':{'graph/widget_tests.rs':'crates/tze_hud_scene/src/graph/widget_tests.rs'},
 'I04':{'handshake.rs':'crates/tze_hud_protocol/src/session_server/handshake.rs','zone_ops.rs':'crates/tze_hud_scene/src/graph/zone_ops.rs'},
 'I07':{'mutation.rs':'crates/tze_hud_scene/src/mutation.rs'},
}
for i,(key,requirement,classification,evidence) in enumerate(rows):
    for short,long in row_paths.get(key,{}).items():
        evidence=re.sub(r'(?<![\w/])'+re.escape(short),long,evidence)
    rows[i]=(key,requirement,classification,evidence)

(OUT/'coverage-checklist.json').write_text(json.dumps({'source_head':HEAD,'groups':[dict(zip(['id','requirement','classification','evidence'],r)) for r in rows]},indent=2)+'\n')
(OUT/'gap-packets.json').write_text(json.dumps({'source_head':HEAD,'candidates':gaps,'verification_candidates':[verification_candidate],'existing_owners':duplicates,'proposed_generation3':generation3},indent=2)+'\n')
(OUT/'upload-isolation-verification-packet.json').write_text(json.dumps(verification_candidate,indent=2)+'\n')
(OUT/'follow-ups.json').write_text(json.dumps(gaps+[generation3],indent=2)+'\n')
(OUT/'blockers.json').write_text(json.dumps(blockers,indent=2)+'\n')

inventory=json.loads((OUT/'current-diagnostic-reference-inventory.json').read_text())
matrix=['# Owner-qualified repair surface reconciliation','',f'Source main `{HEAD}`. Raw current references accompany this table; name-only hits are not caller proof. Out-of-line cfg(test) modules and declaration attributes are manually resolved.','']
for label in 'RPS':
    matrix += [f'## {label}: {len(surface_notes[label])} original diagnostic groups','', '| Group | Current disposition / actual ownership |','|---|---|']
    for i,note in enumerate(surface_notes[label],1):
        refs=inventory['groups'][label][i-1]['current_references']
        locations=[]
        for sym,xs in refs.items():
            owned=[x for x in xs if x['path'].startswith({'R':'crates/tze_hud_resource/','P':'crates/tze_hud_protocol/','S':'crates/tze_hud_scene/'}[label]) and (x['owner'].endswith('::'+sym) or x['owner']==sym)]
            if owned: locations.append(f"{sym}: {owned[0]['path']}:{owned[0]['line']}")
        matrix.append('| '+str(i)+' | '+full_paths(note)+(' Current declaration: '+'; '.join(locations) if locations else '')+' |')
    matrix.append('')
(OUT/'surface-matrix.md').write_text('\n'.join(matrix)+'\n')

report=['# T8.1a generation2 reconciliation','',f'Audited fetched main: `{HEAD}`. This is source/evidence only, Tests: **+0 ~0 -0**. Production/spec/API/test sources and all protectedT7 files are unchanged. Original `.2.6` remains partial; evidence merge alone cannot close it.','',
'## Authority and provenance','',
 'Current docs/scope.md T8 and Working rules, docs/vision.md, docs/invariants.md govern. Thirteen full projected parent/sibling/repair packets are preserved in canonical-requirements.json. merge-reachability.json proves the antecedent/repair/evidence PRs MERGED and their squash commits reachable on the recorded audit main, including actual scene PR1368; PR1354 is an external merge gate, never a dependency on .2.6 closure. audit-context.json records tool versions and22 protectedT7 blobs.','',
'Initial b5 source evidence is preserved separately under prepin-b5c8852b. Fetched main advanced through scene PR1368 to398c6f91 before execution. That real diff moved scene tests/helpers and two invariant citations; all current source and72-name inventories were regenerated. No old command is represented as current-head execution.','',
'## Complete 23-group coverage checklist','', '| Group | Requirement | Class | Current file:line evidence / limit |','|---|---|---|---|']
report += ['| '+' | '.join(r)+' |' for r in rows]
report += ['', '## Live boundary, ownership and trust','',
'AgentDirectory::resolve (scene/config/agents.rs:164-215) hashes paired PSKs and fixes the allow-derived permissions. Non-PSK identify_session rejection and resolve_local are distinct; production local credentials identify no paired agent. Init/Resume handlers preserve codes, close order, subscriptions and agent-bound single-use token consumption. Capability gates precede conversion/mutation/upload. Live proto-to-scene decoders and geometry_policy_to_proto remain actual production paths; outbound fixture converters and the five T7 exceptions are separately classified.','',
'The removed tze_hud_policy crate and scene/src/policy directory are absent on current main, not hidden live authority layers. Their retained contracts live in scene graph/lease/budget/validation, runtime MutationBudgetBridge and draw-only degradation. No claim relies on obsolete AGENTS topology. Scene methods return JSON snapshots to protocol/mod.rs:280; SceneGraphSnapshot.to_json is an inferred-return-type scanner false positive. Generated protobuf TimingHints, live BatchTimingHints, WallUs/MonoUs and injected TestClock remain distinct from the deleted scene timing model.','',
'ResourceStore normal/chunked completion shares capability/hash/size/type/decode/budget admission; rejected chunk completion frees its slot before validation. Dedup and physical resident ledger are distinct owners. Production headless/windowed resource-byte ceilings match the atomic resident ledger. The upload worker passes unlimited AgentBudget because uploaded physical residency is separate from leased active-tile texture budget (ResourceBudget.max_texture_bytes at scene/types.rs:600 and graph/budget.rs:25); the public AgentBudget fixture does not prove production leased admission. RuntimeWidgetStore open/reindex owns actual startup/persisted behavior; its writers/inspection remain retained fixture contracts.','',
'T normal cleanup removes the session, orphans only its active leases and inserts token at the same scene-clock instant, then aborts namespace uploads/removes its enforcer, and only then signals test-only observers. RegistryGuard fallback deliberately does not signal full cleanup. Observer registration precedes actual drop, no shared lock is held across await, oneshots retain fast completion, and concurrent same-namespace session IDs remain separate. Resume consumes matching valid tokens only; wrong-agent attempts do not consume. Existing claim/hold/clear replies and MutationBatch results replay cached acceptance/rejection without double mutation.','',
'## Remaining concrete gaps and dedupe','',
 'gap-packets.json provides complete same-parent R2/P2/E2/T2 scope, authority, owner/trust map, behavior matrix, nearest existing gates and exact proposed test delta. These are current-source residuals, not reopened claims that gen1 repairs failed their scoped outcomes. R3 is separately labeled an unexecuted verification candidate, with no runtime failure asserted. The full active/closed tracker snapshot was searched; existing8.3/4.5/5.6/369u8 work is linked as duplicate ownership and not recreated. A generation3 proposal requires actual materialized gap IDs and evidence-PR merge; it has no .2.6/.2.12 closure dependency and cannot create a cycle. Generation4 is forbidden.','',
'Graceful-close instrument truth table: Ok(None) observes end; Ok(Some(_)) observes a remaining message; Err(timeout) observes no completion. Current session_handshake.rs:476 groups Ok(None)|Err(_) and sets got_stream_end=true, so a nonclosing/hung stream can satisfy this gate after100ms. This is a source-confirmed false-positive instrument, not a claim that current production close fails. Five resume definitions still use100/150ms as assumed cleanup; T only repaired its exact three named invariants and is not described as fixing these.','',
 'Resource concurrency qualification: resource_count_cap_rejects_the_extra_resource and runtime_wide_texture_cap_is_shared_across_agents are sequential. complete_upload reads DedupIndex totals/count before an atomic per-key insert, with no global count transaction; distinct sessions have separate workers. A possible count interleaving remains source-visible, but this audit has not observed a runtime over-admission or added a synthetic concurrent probe. The real production byte ceiling is separately serialized by ResidentLedger. No concurrency-policy repair is silently included in R2.','',
 'Same-agent cap qualification: docs/invariants.md:119 requires per-agent and runtime-wide hard caps, and scene/types.rs:601 calls max_tiles an agent limit. RuntimeMutationBudgetEnforcer::register_session at runtime/mutation_budget_bridge.rs:172-175 ignores namespace and stores allowances/usage by session ID. Its removing_one_of_two_same_namespace_sessions_preserves_the_other fixture:492 proves removal isolation, not combined per-agent admission. Scene check_budget is also lease-scoped. This is an unexecuted contract/coverage ambiguity; no live aggregate breach, owner decision or budget redesign is asserted. Generation3 must not call this universally clean without resolving the actual per-agent versus per-session contract.','',
 'R3 upload cleanup qualification: protocol/session.rs:201-206 permits separate session UUIDs for one namespace; protocol/mod.rs:322-335 spawns an unjoined worker per session, while cleanup:516-519 calls namespace-wide resource abort. ResourceStore.abort_all_uploads at resource/upload.rs:454-456 removes all pending IDs for that namespace. The worker drains queued commands through protocol/session_server/upload.rs:477 without a shutdown/join cleanup barrier. A B-start/A-real-drop/B-complete sequence is a candidate deterministic mismatch, but this exact two-session outcome was not executed in this read-only audit. upload-isolation-verification-packet.json scopes the existing two upload scenarios, failures/replay/late-start/queued-chunk/ID cleanup and peer controls. Agent namespace remains the authorization/dedup/budget owner; source trace is not authorization to change that contract.','',
'## Verification and instrument limits','',
'Gate commands, exact source SHA, terminal exits, chosen target/environment and SHA256 logs are in *-receipt.json and raw *.log. Reused prior repair reports are orientation only. The fresh source parser covers327 tracked app/crates/examples/tests Rust files with no parse errors and all72 attributed names exactly once; parser and name/cfg limitations are explicit in each inventory. No AST evidence is a repository test or a universal deletion oracle.','',
'Fresh narrowed-lib scans: resource3 groups (real residuals), protocol5 (explicitT7 exceptions), scene2 (budget_warning approved return/fixture contract with no production reader, and live JSON to_json false positive). Exit0 means the copied narrowed lib compiled with capped lints, not absence of dead code. The exact four sibling commands retain raw exits: three empty code inventories exit1; the unbounded registry scan exits0 on historical evidence. Evidence-only commits add further historical matches and do not resurrect production symbols.','',
'Latency gates are repository behavior budgets with test_budget default20x slack, not new reference-hardware claims. Existing budget/assert/presentation/input/runtime seams are preserved; no timing benchmark, lock/contention policy or production abstraction is added. Repository GPU recipes must actually require llvmpipe, never skip. Tool-gated omissions and every failed attempt are retained honestly.','']
receipts=[]
for p in sorted(OUT.glob('*-receipt.json')):
    x=json.loads(p.read_text()); receipts.append(x)
    report.append(f"- `{x['label']}`: exit{x['exit_code']}, source `{x['source_head'][:8]}`, log SHA256 `{x['log_sha256']}`.")
report += ['', 'Final behavior summaries and named72 execution cross-check are in verification-summary.json once all planned gates terminate. No blanket pass-count closure is claimed.','',
'## Terminal handoff','',
'Evidence-only PR must reach main under independent review. Nonempty residual packets keep this reconciliation blocked awaiting coordinator materialization and reviewed repair outcomes. This worker never mutates Beads. Discovered-Follow-Ups-JSON is follow-ups.json; Blockers-JSON is blockers.json. Source/test delta is exactly+0~0-0; authoritative docs impact is evidence only.','']
(OUT/'report.md').write_text(full_paths('\n'.join(report))+'\n')
print(json.dumps({'source_head':HEAD,'coverage_groups':len(rows),'gap_candidates':len(gaps),'generation':3,'diagnostic_groups':{k:len(v) for k,v in surface_notes.items()}}))
