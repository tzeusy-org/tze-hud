import datetime, hashlib, json, os, subprocess, sys, time
from pathlib import Path
stage = Path(".handoff/hud-bstmy.5.9")
name, limit, *command = sys.argv[1:]
limit = int(limit)
gate = stage / "gates" / name
gate.mkdir(parents=True, exist_ok=True)
assert not (gate / "terminal.json").exists(), "do not overwrite a previous actual gate"
env = {k: v for k, v in os.environ.items() if not k.startswith("GIT_")}
for key in ("SKIP_GPU_TESTS", "TZE_HUD_SKIP_GPU_TESTS", "TZE_HUD_PERF_ASSERT", "RUST_TEST_THREADS"):
    env.pop(key, None)
fixed = {"CARGO_TARGET_DIR": "/home/orca/orca/projects/tze-hud/target", "CARGO_BUILD_JOBS": "1", "PROTOC": "/usr/bin/protoc", "VK_ICD_FILENAMES": "/usr/share/vulkan/icd.d/lvp_icd.json", "HEADLESS_FORCE_SOFTWARE": "1", "TZE_HUD_REQUIRE_GPU": "1", "LLVMPIPE_CI": "1"}
env.update(fixed)
env["PATH"] = "/home/orca/orca/projects/tze-hud/.venv/bin:" + env.get("PATH", "")
def git(*args):
    r = subprocess.run(["git", *args], capture_output=True, env=env)
    assert r.returncode == 0
    return r.stdout
def manifest():
    rows = []
    for name in git("ls-files", "-z").decode().split("\0"):
        if not name or name.startswith((".beads/", ".handoff/", "test_results/")):
            continue
        path = Path(name)
        if path.is_file():
            data = path.read_bytes()
            rows.append({"path": name, "bytes": len(data), "sha256": hashlib.sha256(data).hexdigest()})
    return rows
def processes():
    out = {}
    for path in Path("/proc").iterdir():
        if not path.name.isdigit():
            continue
        try:
            data = (path / "stat").read_text()
            rest = data[data.rfind(")") + 2:].split()
            out[int(path.name)] = {"ppid": int(rest[1]), "group": int(rest[2]), "start_ticks": rest[19]}
        except (OSError, ValueError, IndexError):
            pass
    return out
before = manifest()
head_before = git("rev-parse", "HEAD").decode().strip()
(gate / "source-before.json").write_text(json.dumps(before, indent=2) + "\n")
started = time.monotonic()
observed = {}
with (gate / "raw.log").open("wb") as log:
    child = subprocess.Popen(["timeout", str(limit) + "s", *command], stdout=log, stderr=subprocess.STDOUT, env=env, start_new_session=True)
    running = {"at": datetime.datetime.now(datetime.timezone.utc).isoformat(), "native_pid": os.getpid(), "child_pid": child.pid, "process_group": child.pid, "source_head": head_before, "command": command, "deadline_seconds": limit, "environment": fixed}
    (gate / "running.json").write_text(json.dumps(running, indent=2) + "\n")
    while child.poll() is None:
        snapshot = processes()
        owned = {child.pid}
        changed = True
        while changed:
            changed = False
            for pid, data in snapshot.items():
                if data["ppid"] in owned and pid not in owned:
                    owned.add(pid)
                    changed = True
        for pid, data in snapshot.items():
            if pid in owned or data["group"] == child.pid:
                observed[pid] = data
        time.sleep(0.2)
    code = child.wait()
after = manifest()
(gate / "source-after.json").write_text(json.dumps(after, indent=2) + "\n")
raw = (gate / "raw.log").read_bytes()
snapshot = processes()
observed.setdefault(child.pid, {"group": child.pid})
groups = sorted({row["group"] for row in observed.values()})
current_members = {str(group): sorted(pid for pid, row in snapshot.items() if row["group"] == group) for group in groups}
receipt = {**running, "ended_at": datetime.datetime.now(datetime.timezone.utc).isoformat(), "exit": code, "seconds": time.monotonic() - started, "source_head_after": git("rev-parse", "HEAD").decode().strip(), "source_files": len(before), "source_manifest_equal": before == after, "before_manifest_SHA256": hashlib.sha256((gate / "source-before.json").read_bytes()).hexdigest(), "after_manifest_SHA256": hashlib.sha256((gate / "source-after.json").read_bytes()).hexdigest(), "raw_bytes": len(raw), "raw_SHA256": hashlib.sha256(raw).hexdigest(), "observed_processes": observed, "observed_pids_absent": sorted(pid for pid in observed if pid not in snapshot), "process_group_members_after": current_members, "child_absent": child.pid not in snapshot, "normal_parallelism": "RUST_TEST_THREADS unset; no --test-threads override", "PERF_ASSERT": "unset, normal off", "SKIP_GPU": "both skip variables unset", "metadata_only_runner": True}
(gate / "terminal.json").write_text(json.dumps(receipt, indent=2) + "\n")
print(json.dumps({k: receipt[k] for k in ("native_pid", "child_pid", "command", "source_head", "source_head_after", "source_manifest_equal", "exit", "seconds", "raw_bytes", "raw_SHA256", "child_absent", "process_group_members_after")}, indent=2), flush=True)
if code:
    print(raw.decode(errors="replace")[-10000:], flush=True)
assert all(not members for members in current_members.values()), current_members
sys.exit(code)
