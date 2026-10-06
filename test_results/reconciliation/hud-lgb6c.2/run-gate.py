import datetime, hashlib, json, os, pathlib, signal, subprocess, sys, time
root=pathlib.Path.cwd(); out=root/'.handoff/hud-lgb6c.2-implementation/gates';out.mkdir(exist_ok=True)
name,limit,*argv=sys.argv[1:];limit=int(limit)
environment=os.environ.copy()
for key in list(environment):
    if key.startswith('GIT_') or key in ('RUSTFLAGS','CARGO_ENCODED_RUSTFLAGS','SKIP_GPU_TESTS','TZE_HUD_SKIP_GPU','TZE_HUD_PERF_ASSERT','TZE_HUD_TEST_BUDGET_SLACK','RUST_TEST_THREADS'):
        environment.pop(key,None)
settings={'CARGO_TARGET_DIR':'/home/orca/orca/projects/tze-hud/target','CARGO_BUILD_JOBS':'1','PROTOC':'/usr/bin/protoc','VK_ICD_FILENAMES':'/usr/share/vulkan/icd.d/lvp_icd.json','HEADLESS_FORCE_SOFTWARE':'1','TZE_HUD_REQUIRE_GPU':'1'}
environment.update(settings)
environment['PATH']='/home/orca/orca/projects/tze-hud/.venv/bin:'+environment['PATH']
def git(*args):return subprocess.check_output(['git',*args],env=environment,text=True).strip()
def inventory():
    files=subprocess.check_output(['git','ls-files','-z'],env=environment).split(b'\0')
    names={os.fsdecode(p) for p in files if p and (pathlib.Path(os.fsdecode(p)).suffix in ('.rs','.toml','.lock','.py','.yml','.yaml','.sh','.ps1') or os.fsdecode(p) in ('justfile','rust-toolchain.toml')) and not os.fsdecode(p).startswith(('test_results/','.beads/'))}
    names.update(('crates/tze_hud_runtime/src/firewall/remote.rs','crates/tze_hud_runtime/src/firewall/remote/windows.rs'))
    return [{'path':p,'bytes':(root/p).stat().st_size,'sha256':hashlib.sha256((root/p).read_bytes()).hexdigest()} for p in sorted(names) if (root/p).is_file()]
def save(): (out/(name+'.json')).write_text(json.dumps(receipt,indent=2)+'\n')
start=time.monotonic();before=inventory();source=out/(name+'-source-before.json');source.write_text(json.dumps(before,indent=2)+'\n')
log=out/(name+'.log')
receipt={'at':datetime.datetime.now(datetime.timezone.utc).isoformat(),'native_pid':os.getpid(),'cwd':str(root),'argv':argv,'timeout_seconds':limit,'source_head':git('rev-parse','HEAD'),'environment':settings,'python_venv_prefix':'/home/orca/orca/projects/tze-hud/.venv/bin','default_test_parallelism':True,'SKIP_GPU_TESTS_unset':True,'PERF_ASSERT_unset_normal_gate':True,'source_before_path':str(source.relative_to(root)),'source_before_sha256':hashlib.sha256(source.read_bytes()).hexdigest(),'exit_code':None,'log_path':str(log.relative_to(root))}
with log.open('wb') as stream:
    process=subprocess.Popen(argv,env=environment,stdout=stream,stderr=subprocess.STDOUT,start_new_session=True)
    receipt['child_pid']=process.pid;receipt['process_group']=process.pid;save()
    print(json.dumps({'gate':name,'native_pid':os.getpid(),'child_pid':process.pid,'log':str(log)},sort_keys=True),flush=True)
    try:code=process.wait(timeout=limit);receipt['timed_out']=False
    except subprocess.TimeoutExpired:
        receipt['timed_out']=True;os.killpg(process.pid,signal.SIGTERM)
        try:code=process.wait(timeout=15)
        except subprocess.TimeoutExpired:os.killpg(process.pid,signal.SIGKILL);code=process.wait()
receipt['exit_code']=code;receipt['elapsed_seconds']=time.monotonic()-start;receipt['head_after']=git('rev-parse','HEAD');receipt['log_sha256']=hashlib.sha256(log.read_bytes()).hexdigest();after=inventory();receipt['source_changes_during_gate']=[r['path'] for r in after if r not in before];receipt['terminal_observed']=True
try:os.killpg(process.pid,0);receipt['process_group_absent']=False
except ProcessLookupError:receipt['process_group_absent']=True
save();print(json.dumps({k:receipt[k] for k in ('exit_code','elapsed_seconds','head_after','log_sha256','source_changes_during_gate','process_group_absent')},sort_keys=True),flush=True)
sys.exit(code if 0<=code<=255 else 1)
