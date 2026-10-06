"""Offline reconciliation evidence, not a repository test or deletion oracle."""
import hashlib
import json
import re
import subprocess
from collections import defaultdict
from pathlib import Path

from tree_sitter import Language, Parser
import tree_sitter_rust

ROOT = Path.cwd()
OUT = ROOT / 'test_results/reconciliation/hud-bstmy.2.12'
CURRENT_GIT_HEAD = subprocess.check_output(['git', 'rev-parse', 'HEAD'], text=True).strip()
HEAD = json.loads((OUT / 'audit-context.json').read_text())['source_head']
source_delta = subprocess.check_output(['git', 'diff', '--name-only', HEAD, '--', '.', ':!test_results/reconciliation/hud-bstmy.2.12'], text=True)
assert not source_delta, 'Non-evidence source drift: ' + source_delta
PARSER = Parser(Language(tree_sitter_rust.language()))
tracked = subprocess.check_output(['git', 'ls-files', '-z']).decode().split('\0')
files = [x for x in tracked if x.endswith('.rs') and x.startswith(('app/', 'crates/', 'examples/', 'tests/'))]
functions = defaultdict(list)
references = defaultdict(list)
envelopes = []
hashes = []
parse_errors = []

def line(src, pos):
    # Byte offsets also work around Python 3.14 tree-sitter point corruption.
    return src.count(b'\n', 0, pos) + 1

def attrs(node, src):
    result = []
    sibling = node.prev_named_sibling
    while sibling and sibling.type in ('attribute_item', 'line_comment', 'block_comment'):
        if sibling.type == 'attribute_item':
            result.append(src[sibling.start_byte:sibling.end_byte].decode())
        sibling = sibling.prev_named_sibling
    return list(reversed(result))

def context(node, src, path):
    chain = []
    attributes = []
    current = node
    while current:
        if current.type in ('function_item', 'mod_item', 'impl_item', 'struct_item', 'enum_item'):
            name = current.child_by_field_name('name') or current.child_by_field_name('type')
            if name:
                chain.append(src[name.start_byte:name.end_byte].decode())
            attributes.extend(attrs(current, src))
        current = current.parent
    test = any('/'+part+'/' in '/'+path for part in ('tests', 'benches')) or any('::test]' in x or x in ('#[test]', '#[cfg(test)]') for x in attributes)
    fixture_gate = any('test-support' in x or 'dev-mode' in x or 'test-harness' in x for x in attributes)
    return {'owner': '::'.join(reversed(chain)), 'attributes': attributes,
            'source_class': 'test' if test else ('fixture-feature' if fixture_gate else 'default-eligible')}

def visit(node, src, path):
    if node.type == 'function_item':
        name = node.child_by_field_name('name')
        if name:
            functions[src[name.start_byte:name.end_byte].decode()].append({'path': path, 'line': line(src, node.start_byte), **context(node, src, path)})
    if node.type in ('identifier', 'field_identifier', 'type_identifier'):
        name = src[node.start_byte:node.end_byte].decode()
        references[name].append({'path': path, 'line': line(src, node.start_byte), 'node': node.type, **context(node, src, path)})
    if node.type == 'struct_expression':
        name = node.child_by_field_name('name') or node.child_by_field_name('type')
        if name:
            symbol = src[name.start_byte:name.end_byte].decode().split('::')[-1]
            if symbol in ('SessionError', 'AuthRejection', 'RequestResult', 'ResourceErrorResponse', 'SessionResumeResult'):
                envelopes.append({'kind': 'constructor', 'symbol': symbol, 'path': path, 'line': line(src, node.start_byte), **context(node, src, path), 'source': src[node.start_byte:node.end_byte].decode()})
    if node.type == 'call_expression':
        function = node.child_by_field_name('function')
        if function:
            symbol = src[function.start_byte:function.end_byte].decode()
            if symbol.split('::')[-1] in ('fail', 'batch_result', 'send_resource_error', 'resource_error_response'):
                envelopes.append({'kind': 'call', 'symbol': symbol, 'path': path, 'line': line(src, node.start_byte), **context(node, src, path), 'source': src[node.start_byte:node.end_byte].decode()})
    for child in node.named_children:
        visit(child, src, path)

for path in files:
    src = (ROOT / path).read_bytes()
    hashes.append({'path': path, 'sha256': hashlib.sha256(src).hexdigest(), 'bytes': len(src)})
    tree = PARSER.parse(src)
    if tree.root_node.has_error:
        parse_errors.append(path)
    visit(tree.root_node, src, path)

docs = (ROOT / 'docs/invariants.md').read_text()
inventory = []
for match in re.finditer(r'`([^`]+)`', docs):
    token = match.group(1)
    leaf = token.split('::')[-1]
    if leaf in functions:
        defs = [x for x in functions[leaf] if x['source_class'] == 'test']
        if defs:
            found = next((x for x in inventory if x['test'] == leaf), None)
            citation = {'token': token, 'line': docs.count('\n', 0, match.start()) + 1}
            if found:
                found['doc_citations'].append(citation)
            else:
                inventory.append({'test': leaf, 'doc_citations': [citation], 'definitions': defs})
assert len(inventory) == 72, len(inventory)
assert all(len(x['definitions']) == 1 for x in inventory)

matrices = {}
for label in ('R', 'P', 'S'):
    prior = json.loads((OUT / ('prior-'+label+'-diagnostic-classification.json')).read_text())
    groups = prior.get('groups', prior.get('diagnostic_groups', [])) if isinstance(prior, dict) else prior
    result = []
    for group in groups:
        symbols = group.get('symbols', [])
        if label == 'R':
            symbols = [
                ['inc_refcount', 'dec_refcount', 'refcount'],
                ['put_svg', 'asset_count', 'total_bytes_used', 'contains', 'PutOutcome'],
                ['write_atomic'], ['sync_parent_dir'],
                ['dedup_index', 'abort_upload', 'in_flight_count'], ['validate_upload'],
            ][len(result)]
        result.append({k:v for k,v in group.items() if k not in ('current_references', 'references', 'callers')})
        result[-1]['current_references'] = {name: references.get(name, []) for name in symbols}
        result[-1]['symbols'] = symbols
    matrices[label] = result

def dump(name, value):
    (OUT / name).write_text(json.dumps(value, indent=2)+'\n')

dump('invariant-test-inventory.json', {'source_head': HEAD, 'count': len(inventory), 'tests': inventory})
dump('source-blob-inventory.json', {'source_head': HEAD, 'parser_git_context': CURRENT_GIT_HEAD, 'non_evidence_blobs_unchanged_from_source_head': True, 'roots': ['app', 'crates', 'examples', 'tests'], 'tracked_rust_files': hashes, 'parser_error_files': parse_errors})
dump('current-diagnostic-reference-inventory.json', {'source_head': HEAD, 'groups': matrices, 'limits': ['identifier-only references exclude comments/string literals', 'default-eligible is syntactic eligibility, not a dependency-closure proof', 'field/method names still require owner-qualified human analysis', 'cfg on out-of-line module declaration must be inspected separately', 'macro token trees require a supplemental manual scan']})
dump('rejection-envelope-inventory.json', {'source_head': HEAD, 'items': envelopes, 'limits': ['all constructor/call syntax, manually classify success/rejection and follow dynamic hint/code producers', 'default-eligible is not live-call proof', 'macro/generated wire types have no handwritten constructors here']})
print(json.dumps({'head': HEAD, 'rust_files':len(files), 'named_invariants':len(inventory), 'parse_errors':parse_errors, 'diagnostic_groups':{k:len(v) for k,v in matrices.items()}, 'envelopes':len(envelopes)}))
