import datetime, hashlib, json, os, re, subprocess, tomllib
from pathlib import Path
stage=Path('.handoff/hud-bstmy.5.8')
gate=stage/'gates/09-final-normal-ci'
terminal=json.loads((gate/'terminal.json').read_text())
assert terminal['exit']==0 and terminal['source_manifest_equal']
raw=(gate/'raw.log').read_bytes()
assert hashlib.sha256(raw).hexdigest()==terminal['raw_SHA256']
prior=Path('/home/orca/orca/projects/tze-hud/.handoff/review-evidence/hud-bstmy.5.9/final-author-d0d/.handoff/hud-bstmy.5.9/final-CI-qualified-invariant-receipts.json')
expected=json.loads(prior.read_text())
source=json.loads((stage/'current-invariant-definition-and-T7-preservation.json').read_text())
owners={}
for entry in json.loads((stage/'frozen-source-manifest.json').read_text()):
    p=Path(entry['path'])
    if p.name!='Cargo.toml': continue
    m=tomllib.loads(p.read_text())
    if 'package' not in m: continue
    pkg=m['package']['name']; root=p.parent
    if (root/'src/lib.rs').exists() or 'lib' in m:
        owners.setdefault(m.get('lib',{}).get('name',pkg.replace('-','_')),set()).add(pkg)
    for test in m.get('test',[]): owners.setdefault(test['name'],set()).add(pkg)
    if m['package'].get('autotests',True):
        for test in (root/'tests').glob('*.rs'): owners.setdefault(test.stem,set()).add(pkg)
    if (root/'src/main.rs').exists(): owners.setdefault(pkg.replace('-','_'),set()).add(pkg)
    for binary in m.get('bin',[]): owners.setdefault(binary['name'],set()).add(pkg)
context=None; positives=[]; summaries=[]
for number,line in enumerate(raw.decode(errors='replace').splitlines(),1):
    if re.search(r'\bRunning\s',line):
        found=re.search(r'\(([^()]*)\)\s*$',line)
        context=None
        if found:
            binary=Path(found.group(1)).name.removesuffix('.exe')
            context=re.sub(r'-[0-9a-f]{8,}$','',binary)
    found=re.fullmatch(r'test ([A-Za-z0-9_:]+) \.\.\. ok',line)
    if found: positives.append({'line':number,'qualified_test':found.group(1),'target':context,'target_owner_candidates':sorted(owners.get(context,set()))})
    found=re.match(r'^test result: (ok|FAILED)\. (\d+) passed; (\d+) failed; (\d+) ignored;',line)
    if found: summaries.append({'line':number,'status':found.group(1),'passed':int(found.group(2)),'failed':int(found.group(3)),'ignored':int(found.group(4)),'target':context})
rows=[]
for item in expected['rows']:
    exact={r['qualified_test'] for r in item['positive_same_owner_receipts']}
    matches=[p for p in positives if p['qualified_test'] in exact and set(p['target_owner_candidates']).intersection(item['docs_owner'])]
    rows.append({'name':item['name'],'docs_owner':item['docs_owner'],'positive_same_owner_receipts':matches,'same_owner_positive':bool(matches)})
missing=[r['name'] for r in rows if not r['same_owner_positive']]
receipt={'at':datetime.datetime.now(datetime.timezone.utc).isoformat(),'native_metadata_pid':os.getpid(),'source_head':terminal['source_head'],'raw_sha256':terminal['raw_SHA256'],'scope':'Current72 documentation assertions matched to actual current qualified positive test lines under current Cargo manifest package/test binary owner. Scoped fixture execution, not universal invariant proof.','source_assertion_names':len(rows),'same_owner_raw_positive':len(rows)-len(missing),'missing_same_owner':missing,'rows':rows,'PERF_ASSERT':'normal off','map_reuse':{'path':str(prior),'sha256':hashlib.sha256(prior.read_bytes()).hexdigest(),'historical_execution_not_reused':True,'current_definition_inventory_sha256':hashlib.sha256((stage/'current-invariant-definition-and-T7-preservation.json').read_bytes()).hexdigest()},'current_target_owner_map':{k:sorted(v) for k,v in owners.items()},'raw_header_parser':'All actual Running headers including root-level poc_acceptance.rs and integration .rs; no tests/ or unittests-only restriction.'}
(stage/'final-CI-qualified-invariant-receipts.json').write_text(json.dumps(receipt,indent=2,ensure_ascii=False)+'\n')
summary={'at':datetime.datetime.now(datetime.timezone.utc).isoformat(),'native_metadata_pid':os.getpid(),'source_head':terminal['source_head'],'terminal_exit':terminal['exit'],'raw_sha256':terminal['raw_SHA256'],'Rust_summary_count':len(summaries),'Rust_passed':sum(r['passed'] for r in summaries),'Rust_failed':sum(r['failed'] for r in summaries),'Rust_ignored':sum(r['ignored'] for r in summaries),'Rust_summaries':summaries,'skipped_lines':[{'line':i,'text':s} for i,s in enumerate(raw.decode(errors='replace').splitlines(),1) if re.search(r'\bSKIPPED\b',s)],'GPU_skip_lines':[{'line':i,'text':s} for i,s in enumerate(raw.decode(errors='replace').splitlines(),1) if re.search(r'\bSKIPPED\b',s) and re.search(r'GPU|adapter|widget_transition|pixel|compositor',s,re.I)],'limits':'PERF_ASSERT normal off; pass counts are not universal coverage or measured p99 proof.'}
(stage/'final-CI-actual-outcomes.json').write_text(json.dumps(summary,indent=2,ensure_ascii=False)+'\n')
print(json.dumps({k:summary[k] for k in ['native_metadata_pid','terminal_exit','Rust_summary_count','Rust_passed','Rust_failed','Rust_ignored','skipped_lines','GPU_skip_lines']}))
print(json.dumps({'qualified_positive':len(rows)-len(missing),'missing':missing}))
assert len(rows)==72 and not missing and not summary['Rust_failed'] and not summary['Rust_ignored'] and not summary['GPU_skip_lines']
