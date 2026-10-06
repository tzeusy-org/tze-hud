import hashlib,json,os,re,signal,subprocess,time
from pathlib import Path

base=Path('test_results/reconciliation/hud-bstmy.5.7')
pin='5825a05421a9e588edd5c3c05ebf7f5bac36c0e1'
git=lambda *args:subprocess.check_output(['git',*args],text=True).strip()
assert git('rev-parse','HEAD')==pin
assert git('branch','--show-current')=='agent/hud-bstmy.5.7'
assert not git('status','--porcelain','--untracked-files=no')
selected={'CARGO_TARGET_DIR':'/home/orca/orca/projects/tze-hud/target','CARGO_BUILD_JOBS':'1','PROTOC':'/usr/bin/protoc','VK_ICD_FILENAMES':'/usr/share/vulkan/icd.d/lvp_icd.json','HEADLESS_FORCE_SOFTWARE':'1','TZE_HUD_REQUIRE_GPU':'1','TZE_HUD_PERF_ASSERT':'1'}
assert Path(selected['VK_ICD_FILENAMES']).is_file()
env=os.environ.copy();env.update(selected)
for name in ['TZE_HUD_SKIP_GPU_TESTS','TZE_HUD_TEST_BUDGET_SLACK','RUST_TEST_THREADS']:env.pop(name,None)
command=['cargo','test','-p','vertical_slice','--test','budget_assertions','--','--nocapture']
log=base/'vertical-slice-budget-assertions.log';started=time.time()
receipt={'source':pin,'head_before':pin,'runner_native_pid':os.getpid(),'cwd':str(Path.cwd()),'branch':git('branch','--show-current'),'command':command,'environment':selected,'explicitly_unset':['TZE_HUD_SKIP_GPU_TESTS','TZE_HUD_TEST_BUDGET_SLACK','RUST_TEST_THREADS'],'profile':'default cargo test profile; normal harness parallelism','deadline_seconds':900,'lease':'ROOT .handoff/runtime-audit-budget-cargo-lease.json; this one gate only','started_unix':started,'tracked_before':git('status','--porcelain','--untracked-files=no'),'protoc_version':subprocess.check_output(['/usr/bin/protoc','--version'],text=True).strip()}
with log.open('wb') as output:
    proc=subprocess.Popen(command,stdout=output,stderr=subprocess.STDOUT,env=env,start_new_session=True)
    receipt['cargo_pid']=proc.pid;receipt['process_group']=proc.pid
    print(json.dumps({'native_runner_pid':os.getpid(),'cargo_pid':proc.pid,'source':pin,'log':str(log),'deadline_seconds':900}),flush=True)
    try:receipt['exit_code']=proc.wait(timeout=900);receipt['timed_out']=False
    except subprocess.TimeoutExpired:
        receipt['timed_out']=True;os.killpg(proc.pid,signal.SIGTERM)
        try:proc.wait(timeout=10)
        except subprocess.TimeoutExpired:os.killpg(proc.pid,signal.SIGKILL);proc.wait()
        receipt['exit_code']=proc.returncode
receipt['elapsed_seconds']=round(time.time()-started,3);receipt['head_after']=git('rev-parse','HEAD');receipt['tracked_after']=git('status','--porcelain','--untracked-files=no')
raw=log.read_bytes();lines=raw.decode(errors='replace').splitlines();receipt['log']=str(log);receipt['log_sha256']=hashlib.sha256(raw).hexdigest();receipt['log_bytes']=len(raw)
receipt['positive_test_lines']=[{'line':i,'text':line} for i,line in enumerate(lines,1) if re.match(r'^test .* \.\.\. ok$',line)]
receipt['summaries']=[{'line':i,'text':line} for i,line in enumerate(lines,1) if line.startswith('test result:')]
receipt['numeric_p99_pass_lines']=[{'line':i,'text':line} for i,line in enumerate(lines,1) if '[PASS]' in line and re.search(r'p99=\d+us',line)]
receipt['failed_lines']=[{'line':i,'text':line} for i,line in enumerate(lines,1) if re.search(r'FAILED|^error:|panicked at|^failures:',line)]
receipt['skip_lines']=[{'line':i,'text':line} for i,line in enumerate(lines,1) if re.search(r'SKIP|[Gg][Pp][Uu].*[Ss]kip|ignored',line)]
receipt['process_terminal']=proc.poll() is not None
try:os.killpg(proc.pid,0);receipt['cargo_process_group_still_exists']=True
except ProcessLookupError:receipt['cargo_process_group_still_exists']=False
receipt['runner_script_sha256']=hashlib.sha256(Path(__file__).read_bytes()).hexdigest()
(base/'vertical-slice-budget-assertions-receipt.json').write_text(json.dumps(receipt,indent=2)+'\n')
print(json.dumps({k:receipt[k] for k in ['exit_code','elapsed_seconds','head_after','tracked_after','log_sha256','summaries','numeric_p99_pass_lines','failed_lines','skip_lines','process_terminal','cargo_process_group_still_exists']},indent=2),flush=True)
