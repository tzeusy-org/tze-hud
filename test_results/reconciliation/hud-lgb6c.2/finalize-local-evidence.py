import datetime
import hashlib
import json
import pathlib
import re
import shutil
import subprocess

root = pathlib.Path.cwd()
stage = root / '.handoff/hud-lgb6c.2-implementation'
out = root / 'test_results/reconciliation/hud-lgb6c.2'
receipt = json.loads((stage / 'gates/final-current-context-normal-ci.json').read_text())
assert receipt['exit_code'] == 0 and receipt['process_group_absent']
assert not receipt['source_changes_during_gate']
assert receipt['source_head'] == receipt['head_after']
log = root / receipt['log_path']
raw = log.read_text()
lines = raw.splitlines()

def save(name, data):
    (out / name).write_text(json.dumps(data, indent=2) + '\n')

shutil.copytree(stage / 'gates', out / 'gates', dirs_exist_ok=True)
for name in ['bootstrap-native-receipt.json', 'FIRST-current28-source-applicability.json',
             'canonical-bootstrap-issue.json', 'native-allocation-correction.json',
             'manifest-context-rebuild.json', 'source-review-decisions.json',
             'ci-owned-process-observations.json', 'run-gate.py']:
    shutil.copyfile(stage / name, out / name)

rust = [(i + 1, int(m.group(1)), line) for i, line in enumerate(lines)
        if (m := re.match(r'test result: ok\. (\d+) passed;', line))]
failed = [line for line in lines if line.startswith('test result: FAILED')]
skips = [line for line in lines if 'SKIPPED' in line]
save('final-local-CI-proof.json', {
    'at': datetime.datetime.now(datetime.timezone.utc).isoformat(),
    'source_commit': receipt['source_head'], 'source_head_after': receipt['head_after'],
    'argv': receipt['argv'], 'environment': receipt['environment'],
    'actual_exit': receipt['exit_code'], 'elapsed_seconds': receipt['elapsed_seconds'],
    'native_pid': receipt['native_pid'], 'child_pid': receipt['child_pid'],
    'process_group_absent': receipt['process_group_absent'],
    'source_changes_during_gate': receipt['source_changes_during_gate'],
    'raw_log_path': 'gates/final-current-context-normal-ci.log',
    'raw_log_sha256': hashlib.sha256(log.read_bytes()).hexdigest(),
    'rust_positive_tests_sum': sum(count for _, count, _ in rust),
    'rust_positive_summaries': len(rust),
    'rust_summary_lines': [{'line': line, 'passed': count, 'text': text}
                           for line, count, text in rust],
    'failed_test_summary_lines': failed, 'actual_skip_lines': skips,
    'python_summary_lines': [line for line in lines if re.search(r'\b\d+ passed\b', line)
                             and not line.startswith('test result:')],
    'actual_normal_perf_assert_unset': receipt['PERF_ASSERT_unset_normal_gate'],
    'limits': 'These actual gate counts do not establish native UAC/policy behavior, '
              'measured performance budgets, or universal caller/domain coverage.'
})

inventory = json.loads((out / 'protected-invariant-preservation.json').read_text())
for row in inventory['rows']:
    for identity in row['actual_existing_harness_identity_matches']:
        name = identity['harness_name']
        identity['final_CI_positive_log_lines'] = [
            i + 1 for i, line in enumerate(lines) if line == 'test ' + name + ' ... ok']
        identity['final_execution_commit'] = receipt['source_head']
inventory['all_final_CI_positive_matches'] = all(
    any(identity['final_CI_positive_log_lines'] for identity in row['actual_existing_harness_identity_matches'])
    for row in inventory['rows'])
inventory['final_CI_log_sha256'] = receipt['log_sha256']
inventory['final_CI_execution_commit'] = receipt['source_head']
save('protected-invariant-preservation.json', inventory)

source = json.loads((out / 'implementation-source.json').read_text())
rows = []
for old in source['paths']:
    name = old['path']
    data = (root / name).read_bytes()
    committed = subprocess.check_output(['git', 'show', receipt['source_head'] + ':' + name])
    rows.append({'path': name, 'bytes': len(data),
                 'sha256': hashlib.sha256(data).hexdigest(),
                 'blob': subprocess.check_output(['git', 'rev-parse', receipt['source_head'] + ':' + name], text=True).strip(),
                 'current_equal_executed_commit': data == committed})
save('final-executed-source.json', {
    'assigned_base': '0082c28fcdbc9627852d90ced2d5cf99303f4b7a',
    'actual_executed_commit': receipt['source_head'], 'paths': rows,
    'all_current_equal_executed_commit': all(row['current_equal_executed_commit'] for row in rows),
    'no_rebase_or_main_freshening': True,
    'later_main_private_clock_owner': 'hud-bstmy.1.18 public reader signatures/default semantics unchanged; '
                                       'assigned source keeps historical private cache; native PR merge job will be separately identified'
})

coverage = json.loads((out / 'acceptance-coverage.json').read_text())
coverage['stage'] = 'Final normal local CI actually passed; native MSVC/non-policy Windows execution pending PR.'
coverage['rows'][8]['proof'] = 'Final local normal just ci actual0 at ' + receipt['source_head'] + '; raw ' + receipt['log_sha256']
coverage['rows'][8]['limit'] = 'Actual Windows MSVC release/native five fixtures pending; no native proof inferred from GNU compilation.'
save('acceptance-coverage.json', coverage)

fixtures = json.loads((out / 'fixture-delta.json').read_text())
for fixture in fixtures['fixtures']:
    if fixture['kind'] == 'new' and 'windows::tests' in fixture['name']:
        fixture['purpose'] += '; production read-only ProgramData query exercises correct task-allocation lifetime'
        fixture['execution'] = 'Actual current app/runtime GNU all-target Clippy0; native execution pending existing Windows job.'
    else:
        fixture['execution'] += '; also actual positive final normal CI'
save('fixture-delta.json', fixtures)

gates = []
for path in sorted((stage / 'gates').glob('*.json')):
    if path.name.endswith('-source-before.json'):
        continue
    data = json.loads(path.read_text())
    assert data['exit_code'] is not None
    gates.append({'gate': path.stem, 'receipt': 'gates/' + path.name,
                  'receipt_sha256': hashlib.sha256(path.read_bytes()).hexdigest(),
                  'source_head': data['source_head'], 'argv': data['argv'],
                  'exit_code': data['exit_code'], 'log_sha256': data['log_sha256'],
                  'native_pid': data['native_pid'], 'child_pid': data['child_pid'],
                  'process_group_absent': data['process_group_absent'],
                  'source_changes_during_gate': data['source_changes_during_gate']})
save('gate-index.json', {
    'rows': gates, 'all_native_terminal': True,
    'actual_failed_commands': [gate for gate in gates if gate['exit_code'] != 0],
    'no_failed_command_removed': True,
    'source_freeze_scheduling_limit': 'Workspace current Clippy and GNU child lifetimes overlapped; '
       'GNU raw records Cargo build-directory lock wait and consumed source unchanged. '
       'No claim that those launches were explicitly serialized.',
    'historical_first_CI': 'First normal pass preceded genuine parser/native fixes. '
       'Two later full runs failed due retired manifest-path test artifacts. '
       'Final unchanged0d source passed after bounded six-package artifact rebuild.'
})
print(json.dumps({'copied_gate_files': len(list((out / 'gates').iterdir())),
                  'rust_passes': sum(count for _, count, _ in rust), 'rust_summaries': len(rust),
                  'all72_positive': inventory['all_final_CI_positive_matches'], 'skips': skips,
                  'source_commit': receipt['source_head'], 'raw_sha256': receipt['log_sha256']}, sort_keys=True))
