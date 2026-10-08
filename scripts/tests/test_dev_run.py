"""One injected client feature fixture; no Windows, network, credentials or Cargo."""
from __future__ import annotations

import hashlib
import importlib.util
import json
from pathlib import Path
import shutil
import signal
import time
import urllib.error

import pytest

spec = importlib.util.spec_from_file_location("dev_run", Path(__file__).parents[1] / "dev_run.py")
dr = importlib.util.module_from_spec(spec)
spec.loader.exec_module(dr)


class Response:
    status = 202

    def __enter__(self):
        return self

    def __exit__(self, *args):
        return False

    def read(self, limit):
        return b'{"restarting":true}'


class DripResponse(Response):
    """Injected blocking body: frequent activity never completes the response."""

    def read(self, limit):
        while True:
            time.sleep(0.002)


class Transport:
    """Inject the transport into the actual NativeOps empty-body request path."""

    def __init__(self, owner):
        self.owner = owner

    def open(self, request, timeout):
        assert request.get_method() == "POST" and request.data == b""
        assert request.full_url == "http://hud.invalid/admin/restart"
        assert 0 < timeout <= 8
        self.owner.posts += 1
        if self.owner.case in {"auth", "busy", "unavailable"}:
            code = {"auth": 403, "busy": 429, "unavailable": 503}[self.owner.case]
            raise urllib.error.HTTPError(request.full_url, code, "fixture", {}, None)
        if self.owner.case == "ambiguous":
            raise OSError("injected loss after POST")
        return DripResponse() if self.owner.case == "drip" else Response()


class Operations:
    """Real temporary filesystem primitives; fake native process/clock/build transport."""

    def __init__(self, root, case):
        self.root = root
        root.mkdir()
        self.case = case
        self.token = "a" * 32
        self.now = 0.0
        self.deadline = None
        self.builds = self.posts = 0
        self.saved = None
        self.receipt_value = None
        self.actions = []
        self.paths = {label: root / label for label in ("exe", "old", "stage", "failed", "lock")}
        self.paths["exe"].write_bytes(b"original executable")
        for name in ("config.toml", "agents.toml", "unrelated.old.exe"):
            (root / name).write_bytes(name.encode())
        if case == "concurrent":
            self.paths["lock"].write_bytes(b"another transaction")
        if case == "stale":
            self.paths["old"].write_bytes(b"someone else's backup")
        if case == "owned-backup":
            self.paths["old"].write_bytes(b"previous own executable")
            self.receipt_value = {"schema": 1, "phase": "complete",
                                  "image": dr.windows_path(r"C:\Users\owner\dev\tze_hud.exe"),
                                  "old_file": self.file("old"), "new_file": self.file("exe")}
        self.sender = dr.NativeOps.__new__(dr.NativeOps)
        self.sender.origin = "http://hud.invalid"
        self.sender.key = "fixture-only-not-a-credential"
        self.sender.http = Transport(self)
        self.sender.clock = self.clock
        self.sender.deadline = None

    def clock(self):
        return self.now

    def sleep(self, seconds):
        assert 0 <= seconds <= 0.25
        self.now += seconds

    def status(self, timeout):
        assert timeout > 0
        self.now += min(2, timeout)
        ready = self.posts and self.case != "timeout"
        return {"pid": 77 if ready else 42, "sha": "a" * 40 if not ready or self.case == "same-head" else "b" * 40,
                "channel": "ci" if self.case == "production" else "local", "cpu_pct_2s": 0.1}

    def process(self, pid, timeout):
        if self.case == "metadata-denied":
            raise dr.DevRunError("simulated inaccessible native process metadata")
        self.now += min(1, timeout)
        image = r"C:\Users\owner\dev\tze_hud.exe"
        prod = r"C:\Users\owner\AppData\Local\Programs\tze_hud\tze_hud.exe"
        if self.case == "production":
            image = prod
        elif self.case == "sibling":
            image = r"C:\Users\owner\dev-sibling\tze_hud.exe"
        elif self.case == "other-drive":
            image = r"D:\dev\tze_hud.exe"
        created = "100" if pid == 42 else "200"
        if ((self.case == "pid-race" and self.builds)
                or (self.case == "staging-race" and self.paths["stage"].exists())):
            created = "101"
        return {"pid": pid, "created": created, "image": image,
                "file_id": self.file("exe")["id"], "directory_id": "directory-identity",
                "production": prod, "production_file_id": self.file("exe")["id"]
                if self.case == "production-alias" else "different-native-file",
                "dev_dir": r"C:\Users\owner\dev", "canonical_verified": self.case != "reparse",
                "local_listener_verified": self.case != "remote-pid"}

    def build(self):
        self.builds += 1
        if self.case == "build":
            raise dr.DevRunError("injected build failure")
        path = self.root / "build"
        path.write_bytes(b"new executable")
        return {"path": path, "sha256": hashlib.sha256(path.read_bytes()).hexdigest(),
                "sha": "a" * 40 if self.case == "same-head" else "b" * 40}

    def lock(self):
        with self.paths["lock"].open("xb") as stream:
            stream.write(self.token.encode())

    def unlock(self):
        self.paths["lock"].unlink()

    def journal(self, state):
        self.saved = json.loads(json.dumps(state))

    def receipt(self):
        return self.receipt_value

    def write_receipt(self, state):
        self.receipt_value = json.loads(json.dumps(state))

    def file(self, label):
        path = self.paths[label]
        if not path.exists():
            return None
        info = path.stat()
        return {"id": f"{info.st_dev}:{info.st_ino}", "sha256": hashlib.sha256(path.read_bytes()).hexdigest()}

    def copy_stage(self, build):
        if self.case == "copy-timeout":
            raise dr.FileOutcomeUnknown("simulated interop writer timeout")
        if self.case == "copy":
            raise dr.DevRunError("injected copy failure")
        shutil.copyfile(build["path"], self.paths["stage"])
        return self.file("stage")

    def rename(self, source, destination, expected):
        self.actions.append((source, destination))
        if ((self.case == "park" and source == "exe" and destination == "old")
                or (self.case == "place" and source == "stage")
                or (self.case == "restore" and ((source == "stage") or (source == "old")))):
            raise dr.DevRunError("injected rename failure")
        if self.case == "file-race" and source == "exe" and destination == "old":
            self.paths["exe"].write_bytes(b"another updater's image")
        if self.file(source) != expected:
            raise dr.DevRunError("primitive rejected changed file identity")
        if self.paths[destination].exists():
            raise dr.DevRunError("destination already exists")
        self.paths[source].rename(self.paths[destination])

    def remove(self, label, expected):
        assert self.file(label) == expected
        self.paths[label].unlink()

    def restart(self, timeout):
        if self.case == "drip":
            # Inject almost-expired observation time; the real request guard
            # gets the remaining budget, without changing production defaults.
            self.now = self.deadline - 0.025
        self.sender.deadline = self.deadline
        self.sender.restart(timeout)

    def accept_process(self, child):
        assert child["pid"] == 77 and child["created"] == "200"


def test_dev_run_production_refusal_transaction_and_bounded_restart(tmp_path):
    cases = ("production", "production-alias", "sibling", "other-drive", "reparse", "remote-pid", "metadata-denied", "pid-race", "staging-race",
             "concurrent", "stale", "build", "copy", "copy-timeout", "park", "place", "restore", "file-race",
             "auth", "busy", "unavailable", "ambiguous", "drip", "timeout", "success", "owned-backup", "same-head")
    for case in cases:
        ops = Operations(tmp_path / case, case)
        protected = {name: (ops.root / name).read_bytes()
                     for name in ("config.toml", "agents.toml", "unrelated.old.exe")}
        prior_handler = signal.getsignal(signal.SIGALRM)
        prior_timer = signal.getitimer(signal.ITIMER_REAL)
        if case in {"success", "owned-backup", "same-head"}:
            result = dr.deploy(ops)
            assert result["sha_changed"] is (case != "same-head")
            assert result["pid"] == 77 and result["created"] == "200"
            assert ops.paths["exe"].read_bytes() == b"new executable"
            assert ops.paths["old"].read_bytes() == b"original executable"
            assert not ops.paths["lock"].exists() and ops.receipt_value["phase"] == "complete"
            assert ops.builds == ops.posts == 1
        else:
            with pytest.raises((dr.DevRunError, FileExistsError)):
                dr.deploy(ops)
            assert ops.posts <= 1 and ops.builds <= 1
            if case in {"production", "production-alias", "sibling", "other-drive", "reparse", "remote-pid", "metadata-denied", "concurrent", "stale"}:
                assert ops.builds == ops.posts == 0 and not ops.actions
            if case in {"ambiguous", "drip", "timeout"}:
                assert ops.paths["old"].read_bytes() == b"original executable"
                assert ops.paths["lock"].exists() and ops.paths["exe"].read_bytes() == b"new executable"
            elif case == "restore":
                assert ops.paths["old"].read_bytes() == b"original executable"
                assert ops.paths["lock"].exists()
            elif case == "file-race":
                assert ops.paths["exe"].read_bytes() == b"another updater's image"
                assert ops.paths["lock"].exists()
            elif case == "copy-timeout":
                assert ops.paths["lock"].exists() and ops.paths["exe"].read_bytes() == b"original executable"
            else:
                assert ops.paths["exe"].read_bytes() == b"original executable"
            if case == "concurrent":
                assert ops.paths["lock"].read_bytes() == b"another transaction"
            if case == "stale":
                assert ops.paths["old"].read_bytes() == b"someone else's backup"
            if case == "timeout":
                assert ops.now <= ops.deadline and ops.posts == 1
        assert all((ops.root / name).read_bytes() == data for name, data in protected.items())
        assert signal.getsignal(signal.SIGALRM) is prior_handler
        assert signal.getitimer(signal.ITIMER_REAL) == prior_timer == (0.0, 0.0)

    # An existing timer is not paused, extended, or stolen. The same feature
    # fixture checks refusal before its HTTP transport is reached.
    sender = ops.sender
    original_handler = signal.getsignal(signal.SIGALRM)
    original_timer = signal.getitimer(signal.ITIMER_REAL)
    try:
        signal.setitimer(signal.ITIMER_REAL, 10)
        before_posts = ops.posts
        with pytest.raises(dr.Rejected, match="alarm already active"):
            sender.request("POST", "/admin/restart", 0.1)
        assert ops.posts == before_posts
        assert signal.getsignal(signal.SIGALRM) is original_handler
        remaining, interval = signal.getitimer(signal.ITIMER_REAL)
        assert 0 < remaining <= 10 and interval == 0
    finally:
        signal.setitimer(signal.ITIMER_REAL, 0)
        signal.signal(signal.SIGALRM, original_handler)
        signal.setitimer(signal.ITIMER_REAL, *original_timer)
