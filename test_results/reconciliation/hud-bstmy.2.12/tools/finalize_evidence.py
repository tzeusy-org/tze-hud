"""Finish bounded evidence only; never compiles, mutates source or updates Beads."""
import hashlib
import json
import re
import runpy
import subprocess
from pathlib import Path

OUT = Path('test_results/reconciliation/hud-bstmy.2.12')
HERE = Path(__file__).resolve().parent
HEAD = subprocess.check_output(['git', 'rev-parse', 'HEAD'], text=True).strip()

def write(name, value):
    (OUT / name).write_text(json.dumps(value, indent=2) + '\n')

audit = runpy.run_path(str(HERE / 'source_audit.py'))
runpy.run_path(str(HERE / 'build_report.py'))
gaps = json.loads((OUT / 'gap-packets.json').read_text())

symbols = ['InflightUpload', 'upload_id', 'started_at', 'width_px', 'height_px',
           'font_bytes', 'resident_ledger', 'resource_type', 'FontBytesStore',
           'DedupIndex', 'last_heartbeat_ms', 'classify_server_payload',
           'classify_inbound_batch', 'abort_all_uploads', 'abort_upload',
           'run_upload_worker', 'validate_timing_hints', 'DedupWindow',
           'budget_warning', 'is_lease_budget_warning', 'to_json']
write('residual-owner-reference-inventory.json', {
    'source_head': HEAD,
    'limits': audit['matrices'] and ['Raw identifier hits require defining-owner/call/reader/cfg qualification; out-of-line cfg and same-name collisions are resolved in the report. No comment or string is a caller.'],
    'symbols': {s: audit['references'].get(s, []) for s in symbols},
})

remove_names = ['font_gc_releases_the_retained_source_copy_and_ledger_charge',
                'remove_evicts_entry', 'transactional_messages_never_dropped',
                'heartbeat_is_ephemeral_and_droppable',
                'state_stream_messages_are_coalesced_class',
                'transactional_not_droppable_different_from_ephemeral',
                'test_degradation_notice_is_transactional']
gate_names = sorted(set(remove_names + [n for g in gaps['candidates'] + gaps['verification_candidates']
                                      for n in g['nearest_existing_gates']
                                      if not n.startswith('store_behavior')]))
gate_defs = {name: audit['functions'].get(name, []) for name in gate_names}
assert all(len(v) == 1 for v in gate_defs.values()), {k:v for k,v in gate_defs.items() if len(v) != 1}
write('candidate-gate-definition-inventory.json', {'source_head': HEAD, 'definitions': gate_defs,
      'core_removed_definitions': remove_names, 'limit': 'Definitions are nearest existing scenario owners; proposed extensions are not implemented or executed by this audit.'})

log = (OUT / 'full-ci.log').read_text()
results = re.findall(r'test result: ok\. (\d+) passed; (\d+) failed; (\d+) ignored', log)
passed_names = re.findall(r'^test ([^ ]+) \.\.\. ok$', log, re.M)
named = json.loads((OUT / 'invariant-test-inventory.json').read_text())
executions = [{**x, 'passed_log_names': [n for n in passed_names if n.split('::')[-1] == x['test']]}
              for x in named['tests']]
assert len(executions) == 72 and all(x['passed_log_names'] for x in executions)
write('named-invariant-execution-inventory.json', {'source_head': HEAD, 'count': 72,
      'log': 'full-ci.log', 'tests': executions, 'limit': 'Exact 72 definitions and passing execution verified; universal production wiring still requires source/contract qualification.'})
receipts = [json.loads(p.read_text()) for p in sorted(OUT.glob('*-receipt.json'))]
assert all(r['source_head'] == HEAD == r['head_after'] and r['exit_code'] == 0 for r in receipts)
assert all(hashlib.sha256((OUT / (r['label'] + '.log')).read_bytes()).hexdigest() == r['log_sha256'] for r in receipts)
summary = {
    'source_head': HEAD, 'full_ci_command': ['just', 'ci'],
    'full_ci_terminal_exit': 0,
    'rust_passed': sum(int(a) for a,_,_ in results), 'rust_summaries': len(results),
    'rust_failed': sum(int(b) for _,b,_ in results), 'rust_ignored': sum(int(c) for _,_,c in results),
    'named_invariants_defined_once': 72, 'named_invariants_observed_passing': 72,
    'python_summary_lines': [line for line in log.splitlines() if not line.startswith('test result:') and re.search(r'\d+ passed|Ran \d+ tests',line)],
    'tool_and_checker_skip_lines': [line for line in log.splitlines() if re.search(r'\bSKIP(?:PED)?\b', line)],
    'gpu_skip_lines': [line for line in log.splitlines() if re.search(r'skip.*GPU|GPU.*skip', line, re.I)],
    'gpu_execution_provenance': 'Repository recipes executed with actual /usr/share/vulkan/icd.d/lvp_icd.json, HEADLESS_FORCE_SOFTWARE=1, TZE_HUD_REQUIRE_GPU=1. Missing GPU must fail the existing helpers; no GPU skip was accepted.',
    'attempts': 'One normal full sweep at source398, terminal0; no retry or failed Rust/CI attempt in GEN2. Prepin source-only b5 inventories retained separately.',
    'tests_delta': {'added': 0, 'modified': 0, 'removed': 0},
    'gate_receipts': receipts,
    'performance_limit': 'Behavioral test_budget slack/default debug gates are not new reference-hardware latency measurements.',
}
assert summary['rust_passed'] == 3250 and summary['rust_summaries'] == 71
assert summary['rust_failed'] == summary['rust_ignored'] == 0 and not summary['gpu_skip_lines']
write('verification-summary.json', summary)

# Classify every handwritten envelope/call in the current inventory, including
# successful constructors and deferred freeze queue branches. Dynamic values
# are traced to their current producer, not inferred from a literal-only guard.
catalog = []
for x in audit['envelopes']:
    y = dict(x)
    p, line, owner = x['path'], x['line'], x['owner']
    if x['source_class'] != 'default-eligible' or '/tests/' in p:
        disposition = 'fixture/success syntax only; not a production rejection'
    elif x['symbol'] in ['SessionResumeResult', 'RequestResult'] and owner in ['ok','handle_claim_tile','handle_hold','handle_session_resume']:
        disposition = 'success constructor; no rejection hint obligation'
    elif x['symbol'] == 'AuthRejection' or (x['symbol'] == 'SessionError' and '/handshake.rs' in p):
        disposition = 'live identity/initial-read/init/resume rejection; nonempty paired credential/reopen/capacity guidance, structured protocol version range or TokenError guidance retained; no auth/code/close change'
    elif x['symbol'] == 'SessionError':
        disposition = 'live handshake timeout or sequence rejection; next-stream/increasing-sequence guidance retained'
    elif x['symbol'] == 'ResourceErrorResponse':
        disposition = 'live unified resource error constructor; generic flow guidance is partial for capability/hash/decode/budget failures (E2)'
    elif '/mutations.rs' in p and 385 <= line <= 611:
        disposition = 'freeze queue/cache/pressure branch; decision owner hud-bstmy.5.6. accepted=true notifications distinguished from actual rejected=false results; no claimed production freeze activation'
    elif '/mutations.rs' in p:
        disposition = 'live mutation admission/conversion/dedup/atomic-result producer; trace cached.error_message to malformed lease, conversion, validator or BudgetBridge text. Replays preserve the same result; description-only hints are E2 residuals'
    elif '/verbs.rs' in p and owner in ['fail','batch_result']:
        disposition = 'live common result wrapper; dynamic code/hint supplied by the catalogued owners, not universal hint coverage'
    elif '/verbs.rs' in p and owner in ['not_allowed','safe_mode','not_held','allowed_surface']:
        disposition = 'live grammar/allow/resume/claim affordance; explicit expected surface/allow/next operation retained'
    elif '/verbs.rs' in p:
        disposition = 'live claim/publish/clear/hold producer; validation detail/root data/internal-not-applied hints have E2 actionability limits; successful constructors classified separately'
    else:
        disposition = 'manual review required'
    y['classification'] = disposition
    y['authority'] = 'docs/invariants.md:132-136'
    catalog.append(y)
assert not any(x['classification'] == 'manual review required' for x in catalog)
write('rejection-path-catalog.json', {'source_head': HEAD, 'items': catalog,
      'dynamic_producer_files': ['crates/tze_hud_protocol/src/session_server/mutations.rs', 'crates/tze_hud_protocol/src/session_server/verbs.rs', 'crates/tze_hud_protocol/src/session_server/upload.rs', 'crates/tze_hud_resource/src/types.rs', 'crates/tze_hud_scene/src/validation.rs', 'crates/tze_hud_runtime/src/mutation_budget_bridge.rs', 'crates/tze_hud_protocol/src/token.rs'],
      'guard_limit': 'verbs grpc_codes_are_in_shared_set scans only two source strings and uppercase literal heuristic, excluding dynamic/error/auth/resource producers and all hint actionability.',
      'separate_owner': 'hud-7j0lf owns whether empty/unknown ClientMessage payload should become a rejection; ignored/no-op payload is not an emitted rejection in this catalog.',
      'generated_wire_limit': 'Generated prost message definitions are schema, not handwritten rejection emitters. Handwritten unified resource constructor represents all actual upload error branches.'})

snapshot_path = HERE / 'beads-all.json'
beads = json.loads(snapshot_path.read_text()) if snapshot_path.exists() else None
terms = ['upload', 'resource', 'cleanup', 'resume', 'heartbeat', 'replay', 'deadline', 'timing', 'hint', 'coalescing', 'freeze', 'pre-reset', 'layer-0']
if beads is not None:
    matched = [b for b in beads if any(t in (str(b.get('title',''))+' '+str(b.get('description',''))+' '+str(b.get('design',''))).lower() for t in terms)]
    write('dedupe-evidence.json', {'source_head': HEAD, 'tracker_snapshot_scope': 'Read-only bd list --all --json --limit0; complete active/closed snapshot before artifact materialization.',
          'complete_snapshot_sha256': hashlib.sha256(snapshot_path.read_bytes()).hexdigest(),
          'searched_issue_count': len(beads), 'terms': terms,
          'matched_issues': [{k:b.get(k) for k in ['id','title','status','external_ref']} for b in matched],
          'conclusion': 'R2/P2/E2/T2 are narrower concrete residuals beyond the closed R/P/E/T repair scopes. R3 remains unexecuted verification only. Existing8.3/4.5/5.6/369u8/7j0lf/6.9 owners are not duplicated.'})
else:
    assert (OUT / 'dedupe-evidence.json').exists(), 'Supply a read-only complete tracker snapshot to refresh dedupe evidence.'

# Tooling is checked into the evidence directory as reproducibility material,
# not added to scripts/, Cargo, tests or CI and not a new test species.
tools = OUT / 'tools'; tools.mkdir(exist_ok=True)
for name in ['source_audit.py','run_gate.py','build_report.py','finalize_evidence.py']:
    source = HERE / name
    if source.resolve() != (tools / name).resolve():
        (tools / name).write_bytes(source.read_bytes())

report = (OUT / 'report.md').read_text()
report = report.replace('Final behavior summaries and named72 execution cross-check are in verification-summary.json once all planned gates terminate. No blanket pass-count closure is claimed.',
    'All planned gates terminated0 at source398. The single normal fullCI passed3250 Rust tests across71 summaries, failed0/ignored0; all72 named invariant definitions were observed passing. Focused resource/protocol/scene passed852/18 and8 Criterion cases. pwsh overlay contract was tool-skipped; the integration default-feature checker skips its tests-only target while the separate integration gate actually ran. No GPU skip. verification-summary.json and named-invariant-execution-inventory.json preserve the exact cross-check; counts do not erase the source/contract gaps.')

def link(match):
    path, line = match.group(1), match.group(2)
    if not Path(path).is_file():
        return match.group(0)
    return f'[{path}:{line}](https://github.com/tzeusy-org/tze-hud/blob/{HEAD}/{path}#L{line})'
report = re.sub(r'(?<![\w/])(crates/[A-Za-z0-9_./-]+\.rs|docs/[A-Za-z0-9_./-]+\.md|scripts/dead_code\.py|justfile):(\d+)',link,report)
(OUT / 'report.md').write_text(report)
print(json.dumps({'source_head': HEAD, 'gate_receipts_verified':len(receipts), 'named_executions':72, 'rust_passed':3250, 'confirmed_residuals':4, 'unexecuted_candidates':1}))
