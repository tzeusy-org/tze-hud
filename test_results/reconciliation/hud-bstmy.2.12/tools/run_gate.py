"""Record a real gate's command, source head, raw output and terminal exit."""
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys
import time

label, *command = sys.argv[1:]
out = Path('test_results/reconciliation/hud-bstmy.2.12')
out.mkdir(parents=True, exist_ok=True)
head = subprocess.check_output(['git', 'rev-parse', 'HEAD'], text=True).strip()
start = time.monotonic()
with (out / (label+'.log')).open('wb') as log:
    proc = subprocess.run(command, stdout=log, stderr=subprocess.STDOUT)
receipt = {'label': label, 'command': command, 'source_head': head,
           'head_after': subprocess.check_output(['git', 'rev-parse', 'HEAD'], text=True).strip(),
           'exit_code': proc.returncode, 'elapsed_seconds': time.monotonic()-start,
           'log_sha256': hashlib.sha256((out/(label+'.log')).read_bytes()).hexdigest(),
           'environment': {k:os.environ.get(k) for k in ['CARGO_BUILD_JOBS','CARGO_TARGET_DIR','PROTOC','VK_ICD_FILENAMES','HEADLESS_FORCE_SOFTWARE','TZE_HUD_REQUIRE_GPU','TMPDIR']}}
(out/(label+'-receipt.json')).write_text(json.dumps(receipt,indent=2)+'\n')
print(json.dumps(receipt), flush=True)
sys.exit(proc.returncode)
