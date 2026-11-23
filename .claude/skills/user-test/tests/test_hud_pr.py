"""Held-PR lifecycle with a fake gh executable and loopback MCP."""

from contextlib import contextmanager
import http.server
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import threading
import time
import urllib.error
import urllib.request

import pytest


SCRIPTS = Path(__file__).resolve().parents[2] / "hud-projection" / "scripts"
sys.path.insert(0, str(SCRIPTS))
import hud_pr  # noqa: E402


REPO = "example/project"
NUMBER = 7
HEAD = "a" * 40
NEW_HEAD = "b" * 40
SURFACE = "zone:notification-area"
PAIR = "synthetic-pair"
PR_URL = "https://example.com/example/project/pull/7"
TITLE = "A pull request with a title that is substantially longer than sixty characters"

CLEANUP_CHILD = """
from pathlib import Path
import sys
import time
sys.path.insert(0, sys.argv[1])
import hud_pr
barrier = Path(sys.argv[2])
log = hud_pr.PrState.log
def pause_after_final_state(state, phase):
    log(state, phase)
    if phase == "no watcher":
        (barrier / "finalized").touch()
        until = time.monotonic() + 6
        while not (barrier / "exit").exists() and time.monotonic() < until:
            time.sleep(0.01)
hud_pr.PrState.log = pause_after_final_state
sys.exit(hud_pr.main(["watch", "7"]))
"""

FAKE_GH = """
import json
import os
from pathlib import Path
import sys

path = Path(os.environ["FAKE_GH_PLAN"])
args = sys.argv[1:]
command = " ".join(args[:2])
plan = json.loads(path.read_text())
assert "GH_REPO" not in os.environ
if args[0] != "api" and command != "repo view":
    assert args[-2:] == ["--repo", "example.com/example/project"], args

def selected(items, count, default):
    if not items:
        return default
    return items[min(count, len(items) - 1)]

def update():
    scratch = path.with_suffix(f".{os.getpid()}")
    scratch.write_text(json.dumps(plan))
    scratch.replace(path)

fields = {}
if command == "api repos/example/project/rules/branches/main?per_page=100":
    assert args[2:] == ["--hostname", "example.com", "--paginate", "--slurp"]
    command = "branch rules"
elif command == "api graphql":
    assert args[2:4] == ["--hostname", "example.com"]
    fields = dict(arg.split("=", 1) for arg in args[5::2])
    query = fields.pop("query")
    if "query HeldLabel" in query:
        command = "label read"
    elif "query HeldPullRequest" in query:
        command = "pr verified" if fields["checks"] == "true" else "pr read"
    elif "enqueuePullRequest" in query:
        command = "enqueue"
        assert "expectedHeadOid: $head" in query
    else:
        raise AssertionError(query)
count = sum(call["command"] == command for call in plan["calls"])
plan["calls"].append({"command": command, "args": args if not fields else [], "fields": fields})
update()

if command == "repo view":
    value = {"nameWithOwner": "example/project", "url": "https://example.com/example/project"}
elif command == "branch rules":
    value = plan.get("rules", [
        [{"type": "required_status_checks", "parameters": {"required_status_checks": [
            {"context": name, "integration_id": 42} for name in ("static checks", "cargo clippy", "cargo test")
        ]}}],
    ])
elif command == "label read":
    value = {"data": {"repository": {"label": {"id": "label"} if plan["label_exists"] else None}}}
elif command == "label create":
    assert args[2] == "owner-merge" and not plan["label_exists"]
    plan["label_exists"] = True
    update()
    value = {}
elif command == "pr edit":
    assert args[2] == "7" and args[4] == "owner-merge"
    assert args[3] in {"--add-label", "--remove-label"}
    plan["held"] = args[3] == "--add-label"
    update()
    value = {}
elif command == "pr list":
    assert "--label" in args and "--state" not in args
    value = [{"number": n} for n in plan.get("listed", [7] if plan["held"] else [])]
elif command in ("pr read", "pr verified"):
    pr = plan["pr"].copy()
    override = selected(plan.get(command), count, {})
    if override.get("error"):
        print("synthetic GH error", file=sys.stderr)
        sys.exit(1)
    pr.update(override)
    names = [{"name": "owner-merge"}] if pr.pop("held", plan["held"]) else []
    has_label_page = plan.get("label_page") and "labelsAfter" not in fields
    pr["labels"] = {
        "nodes": [] if has_label_page else names,
        "pageInfo": {"hasNextPage": bool(has_label_page), "endCursor": "labels-more"},
    }
    if command == "pr verified":
        first_page = plan.get("check_page") and "checksAfter" not in fields
        contexts = [
            {"name": name, "isRequired": True, "status": "COMPLETED", "conclusion": "SUCCESS",
             "checkSuite": {"app": {"databaseId": 42}}}
            for name in ("static checks", "cargo clippy", "cargo test")
        ]
        if not first_page:
            contexts = pr.pop("contexts", contexts)
        pr["commits"] = {"nodes": [{"commit": {
            "oid": pr.pop("commitOid", pr["headRefOid"]),
            "statusCheckRollup": {"contexts": {
                "nodes": contexts, "pageInfo": {"hasNextPage": bool(first_page), "endCursor": "checks-more"},
            }},
        }}]}
    value = {"data": {"repository": {"pullRequest": pr}}}
elif command == "pr checks":
    assert args[3:9] == ["--watch", "--required", "--fail-fast", "--interval", "60", "--repo"]
    result = selected(plan.get("checks"), count, "green")
    if result == "pending":
        sys.exit(8)
    if result == "unreported":
        print("no required checks reported on the 'example' branch", file=sys.stderr)
        sys.exit(1)
    if result == "red":
        sys.exit(1)
    if result == "error":
        print("synthetic GH error", file=sys.stderr)
        sys.exit(1)
    assert result == "green"
    value = {}
elif command == "enqueue":
    assert fields["id"] == "pr-id"
    if fields["head"] != plan.get("live_head", plan["pr"]["headRefOid"]):
        sys.exit(1)
    value = {"data": {"enqueuePullRequest": {"mergeQueueEntry": {"position": 1}}}}
else:
    raise AssertionError(args)
print(json.dumps(value))
"""


class Clock:
    def __init__(self):
        self.now = 0
        self.sleeps = []
        self.after_sleep = lambda _: None

    def monotonic(self):
        return self.now

    def time(self):
        return 1_800_000_000 + self.now

    def sleep(self, seconds):
        self.sleeps.append(seconds)
        self.advance(seconds)
        self.after_sleep(seconds)

    def advance(self, seconds):
        self.now = round(self.now + seconds, 4)


class Scenario:
    def __init__(self, directory, monkeypatch):
        self.clock = Clock()
        self.plan = directory / "gh.json"
        fake = directory / "gh"
        fake.write_text(f"#!{sys.executable}\n{FAKE_GH}")
        fake.chmod(0o700)
        monkeypatch.setenv("HUD_PR_GH", str(fake))
        monkeypatch.setenv("FAKE_GH_PLAN", str(self.plan))
        monkeypatch.setenv("GH_REPO", "wrong/repository")
        monkeypatch.setenv("XDG_STATE_HOME", str(directory / "state"))
        monkeypatch.setenv("HOME", str(directory / "home"))
        monkeypatch.delenv("HUD_HOST", raising=False)
        self.plan.write_text(json.dumps({
            "calls": [], "held": True, "label_exists": True,
            "pr": {"id": "pr-id", "state": "OPEN", "baseRefName": "main", "headRefOid": HEAD,
                   "mergeable": "MERGEABLE", "mergeQueueEntry": None, "title": TITLE,
                   "url": PR_URL, "changedFiles": 4, "additions": 17, "deletions": 3},
        }))
        self.directory = directory
        self.state = hud_pr.PrState(REPO, NUMBER)

    def replace(self, **data):
        config = json.loads(self.plan.read_text())
        config.update(data)
        staged = self.plan.with_suffix(".new")
        staged.write_text(json.dumps(config))
        staged.replace(self.plan)

    def calls(self, command=None):
        calls = json.loads(self.plan.read_text())["calls"]
        return [call for call in calls if command is None or call["command"] == command]

    def cli(self, *args):
        return subprocess.run(
            [sys.executable, str(SCRIPTS / "hud_pr.py"), *args], capture_output=True, text=True, timeout=12,
        )

    def watch(self, seconds=None):
        watcher = hud_pr.Watcher(hud_pr.GitHub(REPO, "example.com"), self.state, clock=self.clock)
        if seconds is not None:
            watcher.end = seconds
        try:
            watcher.run()
        except hud_pr.WatchElapsed:
            pass

    def paired(self, monkeypatch, port):
        key_dir = self.directory / "home" / ".config" / "tze-hud"
        key_dir.mkdir(mode=0o700, parents=True)
        psk = key_dir / "127.0.0.1.psk"
        psk.touch(mode=0o600)
        psk.write_text(PAIR)
        monkeypatch.setenv("HUD_HOST", f"127.0.0.1:{port}")
        monkeypatch.setenv("NO_PROXY", "127.0.0.1")
        monkeypatch.setenv("no_proxy", "127.0.0.1")


@pytest.fixture
def scenario(tmp_path, monkeypatch):
    return Scenario(tmp_path, monkeypatch)


class HeldHud:
    def __init__(self, clock):
        self.clock = clock
        self.calls = []
        self.scopes = set()
        self.initialized = 0
        self.expiry_ms = 600_000
        self.inputs = []
        self.publish_result = None
        self.initialize_status = 200
        self.initialize_entered = threading.Event()
        self.initialize_release = None
        self.hold = False
        self.poll_entered = threading.Event()
        self.poll_release = None
        self.delete_entered = threading.Event()
        self.delete_release = None
        fake = self

        class Handler(http.server.BaseHTTPRequestHandler):
            def log_message(self, *_):
                pass

            def reply(self, status, data=None, scope=None):
                payload = b"" if data is None else json.dumps(data).encode()
                self.send_response(status)
                if scope:
                    self.send_header("Mcp-Session-Id", scope)
                self.send_header("Content-Length", str(len(payload)))
                self.end_headers()
                try:
                    self.wfile.write(payload)
                except (BrokenPipeError, ConnectionResetError):
                    pass

            def do_POST(self):
                request = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
                method, params = request["method"], request["params"]
                scope = self.headers.get("Mcp-Session-Id")
                if self.headers.get("Authorization") != f"Bearer {PAIR}":
                    self.reply(401)
                    return
                if method == "initialize":
                    assert scope is None
                    fake.initialized += 1
                    scope = f"scope-{fake.initialized}"
                    fake.calls.append((fake.clock.now, method, scope, params))
                    if fake.initialize_status == 200:
                        fake.scopes.add(scope)
                    fake.initialize_entered.set()
                    if fake.initialize_release is not None:
                        fake.initialize_release.wait(5)
                    self.reply(fake.initialize_status, {"result": {"protocolVersion": "2025-06-18"}}, scope)
                    return
                name, arguments = params["name"], params["arguments"]
                fake.calls.append((fake.clock.now, name, scope, arguments))
                if scope not in fake.scopes:
                    self.reply(404)
                    return
                result = fake.respond(name, scope, arguments)
                if result == "404":
                    fake.scopes.discard(scope)
                    self.reply(404)
                    return
                status = {"content": [{"type": "text", "text": json.dumps(result)}]}
                if "code" in result:
                    status["isError"] = True
                self.reply(200, {"result": status})

            def do_DELETE(self):
                scope = self.headers.get("Mcp-Session-Id")
                assert self.headers.get("Authorization") == f"Bearer {PAIR}"
                fake.calls.append((fake.clock.now, "DELETE", scope, {}))
                fake.delete_entered.set()
                if fake.delete_release is not None:
                    fake.delete_release.wait(10)
                exists = scope in fake.scopes
                fake.scopes.discard(scope)
                self.reply(202 if exists else 404)

        self.server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.thread = threading.Thread(target=self.server.serve_forever, kwargs={"poll_interval": 0.01})
        self.thread.start()

    def close(self):
        if self.initialize_release is not None:
            self.initialize_release.set()
        if self.poll_release is not None:
            self.poll_release.set()
        if self.delete_release is not None:
            self.delete_release.set()
        self.server.shutdown()
        self.server.server_close()
        self.thread.join()

    def request(self, name):
        return [call for call in self.calls if call[1] == name]

    def cards(self):
        return [call for call in self.request("hud_publish") if "key" in call[3]]

    def input_items(self, scope, *choices):
        current = next(call[3] for call in self.cards() if call[2] == scope)
        merge, _, later = current["content"]["actions"]
        callbacks = {"merge": merge["callback_id"], "later": later["callback_id"],
                     "old": "merge.old", "other": merge["callback_id"], "wrong-key": merge["callback_id"]}
        return {"items": [
            {"id": f"{scope}-{index}", "s": "portal:other" if choice == "other" else SURFACE,
             "key": "another-pr" if choice == "wrong-key" else current["key"],
             **({"kind": "dismiss", "action": merge["callback_id"]} if choice == "dismiss"
                else {"action": callbacks[choice]})}
            for index, choice in enumerate(choices)
        ]}

    def respond(self, name, scope, args):
        if name == "hud_publish":
            if "key" in args and self.publish_result is not None:
                return self.publish_result
            return {"ok": True, "expires_in_ms": self.expiry_ms}
        if name == "hud_input":
            if "ack" in args:
                return {"items": [], "remaining": 0}
            self.poll_entered.set()
            if self.poll_release is not None:
                self.poll_release.wait(10)
            if self.inputs:
                item = self.inputs.pop(0)
                if callable(item):
                    return item(self, scope)
                return item
            self.clock.advance(args["wait_ms"] / 1_000)
            return {"items": []}
        if name == "hud_surfaces":
            zone = {"s": SURFACE, "held": True, "expires_in_ms": 0} if self.hold else {"s": SURFACE}
            return {"surfaces": [zone]}
        raise AssertionError(name)


@pytest.fixture
def hud(scenario, monkeypatch):
    fake = HeldHud(scenario.clock)
    scenario.paired(monkeypatch, fake.server.server_port)
    try:
        yield fake
    finally:
        fake.close()


def wait_for(predicate, seconds=5):
    deadline = time.monotonic() + seconds
    while not predicate():
        if time.monotonic() >= deadline:
            pytest.fail("timed out waiting for the synthetic watcher")
        time.sleep(0.02)


def stop_is_pending(pid):
    try:
        lines = Path(f"/proc/{pid}/status").read_text().splitlines()
    except FileNotFoundError:
        return False
    return any(
        int(line.split()[1], 16) & (1 << (signal.SIGTERM - 1))
        for line in lines if line.startswith(("SigPnd:", "ShdPnd:"))
    )


@contextmanager
def held_child(scenario):
    try:
        yield
    finally:
        pid = scenario.state.read().get("pid")
        if pid and hud_pr.process_start(pid) == scenario.state.read().get("pid_start"):
            os.kill(pid, signal.SIGTERM)
            wait_for(lambda: hud_pr.process_start(pid) is None)


def test_hold_reserves_one_detached_child_and_release_clears_it(scenario, hud):
    scenario.replace(held=False, label_exists=False)
    hud.poll_release = threading.Event()
    with held_child(scenario):
        started = scenario.cli("hold", "7", "--review-url", "https://example.com/review/old")
        assert (started.returncode, started.stdout, started.stderr) == (0, "watcher started\n", "")
        assert hud.poll_entered.wait(5)
        pid = scenario.state.read()["pid"]
        assert os.getsid(pid) == pid
        again = scenario.cli("hold", "7", "--review-url", "https://example.com/review/new")
        assert (again.returncode, again.stdout, again.stderr) == (0, "already running\n", "")
        assert scenario.state.read()["pid"] == pid
        assert scenario.state.read()["review_url"] == "https://example.com/review/new"
        assert len(scenario.calls("label create")) == 1
        assert hud.cards()[0][3]["content"]["actions"][1]["url"] == "https://example.com/review/old"
        assert "card up" in scenario.cli("list").stdout
        release = scenario.cli("release", "7")
        assert (release.returncode, release.stdout, release.stderr) == (0, "released\n", "")
        assert hud_pr.process_start(pid) is None
        assert len(hud.request("DELETE")) == 1
        assert not hud.scopes
        assert scenario.cli("list").stdout == ""
    for path in (scenario.state.state_path, scenario.state.lock_path, scenario.state.log_path):
        assert path.stat().st_mode & 0o777 == 0o600
    assert scenario.state.directory.stat().st_mode & 0o777 == 0o700
    log = scenario.state.log_path.read_text()
    assert TITLE not in log and PAIR not in log and "https://" not in log


def test_hold_rearms_after_a_hard_kill(scenario, hud):
    hud.poll_release = threading.Event()
    with held_child(scenario):
        assert scenario.cli("hold", "7").stdout == "watcher started\n"
        assert hud.poll_entered.wait(5)
        first = scenario.state.read()["pid"]
        os.kill(first, signal.SIGKILL)
        wait_for(lambda: hud_pr.process_start(first) is None)
        assert scenario.cli("list").stdout == "#7 no watcher\n"
        assert hud.cards()[0][3]["ttl_ms"] == 600_000
        assert not hud.request("DELETE")
        assert scenario.cli("hold", "7").stdout == "watcher started\n"
        second = scenario.state.read()["pid"]
        assert first != second
        wait_for(lambda: len(hud.cards()) == 2)
        assert scenario.cli("release", "7").returncode == 0
        assert [call[2] for call in hud.request("DELETE")] == [hud.cards()[1][2]]


def test_rehold_waits_for_delayed_release_before_rearming(scenario, hud):
    hud.poll_release = threading.Event()
    hud.delete_release = threading.Event()
    cmd = [sys.executable, str(SCRIPTS / "hud_pr.py")]
    with held_child(scenario):
        assert scenario.cli("hold", "7").stdout == "watcher started\n"
        assert hud.poll_entered.wait(5)
        old = scenario.state.read()["pid"]
        with subprocess.Popen([*cmd, "release", "7"], stdout=subprocess.PIPE, text=True) as release:
            try:
                assert hud.delete_entered.wait(5)
                with subprocess.Popen([*cmd, "hold", "7"], stdout=subprocess.PIPE, text=True) as hold:
                    assert release.poll() is None and hold.poll() is None
                    def waiting_for_previous_watcher():
                        for fd in Path(f"/proc/{hold.pid}/fd").iterdir():
                            try:
                                if os.readlink(fd) == "anon_inode:[pidfd]":
                                    return True
                            except FileNotFoundError:
                                pass
                        return False

                    wait_for(waiting_for_previous_watcher, seconds=2)
                    hud.delete_release.set()
                    assert release.communicate(timeout=5)[0] == "released\n"
                    assert hold.communicate(timeout=5)[0] == "watcher started\n"
            finally:
                hud.delete_release.set()
        assert hud_pr.process_start(old) is None
        assert scenario.state.read()["pid"] != old
        wait_for(lambda: len(hud.cards()) == 2)
        assert hud.cards()[0][2] != hud.cards()[1][2]


def test_rehold_can_rearm_as_soon_as_the_old_state_is_finalized(scenario, hud):
    hud.poll_release = threading.Event()
    barrier = scenario.directory
    cmd = [sys.executable, str(SCRIPTS / "hud_pr.py")]
    with held_child(scenario), subprocess.Popen(
        [sys.executable, "-c", CLEANUP_CHILD, str(SCRIPTS), str(barrier)], stdout=subprocess.DEVNULL,
    ) as prior:
        try:
            assert hud.poll_entered.wait(5)
            with subprocess.Popen([*cmd, "release", "7"], stdout=subprocess.PIPE, text=True) as release:
                try:
                    wait_for(lambda: (barrier / "finalized").exists())
                    assert prior.poll() is None
                    assert scenario.cli("hold", "7").stdout == "watcher started\n"
                    assert scenario.state.read()["pid"] != prior.pid
                    wait_for(lambda: len(hud.cards()) == 2)
                finally:
                    (barrier / "exit").touch()
                assert release.communicate(timeout=5)[0] == "released\n"
        finally:
            (barrier / "exit").touch()
            if prior.poll() is None:
                prior.terminate()
        assert scenario.cli("list").stdout == "#7 card up\n"
        assert len(hud.request("DELETE")) == 1


def test_list_combines_open_labelled_prs_with_their_live_state(scenario):
    scenario.replace(listed=[3, 4, 7, 9])
    states = [hud_pr.PrState(REPO, n) for n in (3, 4, 9)]
    for state, phase in zip(states, ("watching", "card up", "waiting after Later")):
        state.mark(phase, next_return_at=1_800_003_600)
    with states[0].lock_file() as watching, states[1].lock_file() as card, states[2].lock_file() as later:
        assert all(hud_pr.try_watch_lock(fd) for fd in (watching, card, later))
        result = scenario.cli("list")
    assert result.returncode == 0
    assert result.stdout.splitlines() == [
        "#3 watching", "#4 card up", "#7 no watcher", "#9 waiting after Later (2027-01-15T09:00:00+00:00)",
    ]


def test_hold_without_a_pairing_still_labels_the_pr(scenario):
    scenario.replace(held=False, label_exists=False)
    result = scenario.cli("hold", "7")
    assert (result.returncode, result.stdout, result.stderr) == (0, "no HUD pairing: no card\n", "")
    assert len(scenario.calls("label create")) == 1
    assert len(scenario.calls("pr edit")) == 1
    assert scenario.cli("list").stdout == "#7 no watcher\n"


@pytest.mark.parametrize("case", ["unreported", "pending", "error"])
def test_absent_pending_or_unreadable_checks_wait_for_the_next_minute(scenario, hud, case):
    scenario.replace(checks=[case])
    scenario.watch(125)
    assert not hud.cards()
    assert scenario.clock.sleeps == [60, 60, 5]
    assert len(scenario.calls("pr checks")) == 3


@pytest.mark.parametrize(("initial", "interval"), [({}, 60), ({"mergeable": "CONFLICTING"}, 300)])
def test_metadata_failure_preserves_the_known_retry_cadence(scenario, hud, initial, interval):
    scenario.replace(checks=["pending"], **{"pr read": [initial, {"error": True}, {"state": "CLOSED"}]})
    scenario.watch(1_000)
    assert scenario.clock.sleeps == [interval, interval]
    assert not hud.cards()


def test_pending_check_timeout_has_one_actual_query_each_minute(scenario, monkeypatch):
    launches = []
    github = hud_pr.GitHub(REPO, "example.com")
    github.read = lambda _number: {
        "state": "OPEN", "held": True, "headRefOid": HEAD,
        "baseRefName": "main", "mergeable": "MERGEABLE", "mergeQueueEntry": None,
    }

    def pending(*args, timeout):
        assert args[:9] == (
            "pr", "checks", "7", "--watch", "--required", "--fail-fast", "--interval", "60", "--repo",
        )
        launches.append(scenario.clock.now)
        if timeout > 60:
            launches.append(scenario.clock.now + 60)
        scenario.clock.advance(timeout)
        raise subprocess.TimeoutExpired(args, timeout)

    monkeypatch.setattr(hud_pr, "gh", pending)
    watcher = hud_pr.Watcher(github, scenario.state, clock=scenario.clock)
    watcher.end = 137
    watcher.run()
    assert launches == [0, 60, 120]
    assert scenario.clock.now == 137


def test_red_head_is_skipped_until_a_new_green_head(scenario, hud):
    scenario.replace(checks=["red", "green"], **{
        "pr read": [{}, {}, {}, {"headRefOid": NEW_HEAD}, {"state": "MERGED"}],
        "pr verified": [{"headRefOid": NEW_HEAD, "title": "New head"}],
    })
    hud.inputs.append(lambda hud, scope: hud.input_items(scope, "later"))
    scenario.watch(5_000)
    assert len(hud.cards()) == 1
    assert hud.cards()[0][0] == 600
    assert hud.cards()[0][3]["content"]["body"].startswith("New head ·")
    assert scenario.clock.sleeps == [300, 300, 3_600]


@pytest.mark.parametrize("override", [
    {"baseRefName": "other"}, {"mergeable": "CONFLICTING"}, {"mergeQueueEntry": {"position": 1}},
])
def test_ineligible_pr_has_no_card_and_uses_the_five_minute_cadence(scenario, hud, override):
    scenario.replace(**{"pr read": [override]})
    scenario.watch(625)
    assert not hud.cards()
    assert scenario.clock.sleeps == [300, 300, 25]
    assert not scenario.calls("pr checks")


def test_green_watch_does_not_authorize_a_moved_or_wrong_check_commit(scenario, hud):
    scenario.replace(**{
        "pr read": [{}, {"headRefOid": NEW_HEAD}, {"headRefOid": NEW_HEAD}, {"state": "CLOSED"}],
        "pr verified": [
            {"headRefOid": NEW_HEAD, "commitOid": NEW_HEAD},
            {"headRefOid": NEW_HEAD, "commitOid": HEAD},
            {"headRefOid": NEW_HEAD, "commitOid": NEW_HEAD, "title": "Exact new head"},
        ],
    })
    hud.inputs.append(lambda hud, scope: hud.input_items(scope, "later"))
    scenario.watch(5_000)
    card, = hud.cards()
    assert card[0] == 120
    assert card[3]["content"]["body"].startswith("Exact new head ·")
    assert len(scenario.calls("pr checks")) == 3


def test_green_card_payload_is_exact_with_both_graphql_connections_paginated(scenario, hud):
    scenario.replace(label_page=True, check_page=True, **{"pr read": [{}, {}, {"state": "CLOSED"}]})
    hud.inputs.append(lambda hud, scope: hud.input_items(scope, "later"))
    scenario.watch(5_000)
    opened, = hud.request("initialize")
    assert opened[3]["_meta"] == {"tze-hud/label": "PR #7"}
    published, = hud.cards()
    assert 0 <= published[0] < 10
    args = published[3]
    assert args["surface"] == SURFACE and args["key"] == "pr:7" and args["ttl_ms"] == 600_000
    content = args["content"]
    nonce = content["actions"][0]["callback_id"].split(".")[1]
    assert len(nonce) == 16 and int(nonce, 16) >= 0
    assert content == {
        "title": "#7 ready to merge", "body": f"{TITLE[:60]} · 4 files +17 −3 · CI green",
        "actions": [
            {"label": "Merge", "callback_id": f"merge.{nonce}"},
            {"label": "Review", "url": PR_URL},
            {"label": "Later", "callback_id": f"later.{nonce}"},
        ],
    }
    assert any(call["fields"].get("checksAfter") == "checks-more" for call in scenario.calls("pr verified"))
    assert any(call["fields"].get("labelsAfter") == "labels-more" for call in scenario.calls())
    assert [call[2] for call in hud.request("DELETE")] == [opened[2]]


@pytest.mark.parametrize("failure", ["unreported", "wrong-app", "not-required", "empty-rules", "malformed-rules"])
def test_green_requires_every_configured_context_with_matching_provider_evidence(scenario, hud, failure):
    contexts = [
        {"name": name, "isRequired": True, "status": "COMPLETED", "conclusion": "SUCCESS",
         "checkSuite": {"app": {"databaseId": 42}}}
        for name in ("static checks", "cargo clippy", "cargo test")
    ]
    if failure == "unreported":
        contexts = contexts[:1]
    elif failure == "wrong-app":
        contexts[-1]["checkSuite"]["app"]["databaseId"] = 999
    elif failure == "not-required":
        contexts[-1]["isRequired"] = False
    elif failure == "empty-rules":
        scenario.replace(rules=[[]])
    else:
        scenario.replace(rules=[[{"type": "required_status_checks", "parameters": {}}]])
    scenario.replace(**{"pr verified": [{"contexts": contexts}]})
    scenario.watch(1)
    assert not hud.cards() and not scenario.calls("enqueue")
    assert len(scenario.calls("branch rules")) == 1


@pytest.mark.parametrize("condition", ["held", "moved", "moved-during-enqueue", "unheld"])
def test_merge_requires_fresh_hold_and_exact_shown_head_and_keeps_notice(scenario, hud, condition):
    if condition == "moved":
        scenario.replace(live_head=NEW_HEAD, **{
            "pr read": [{}, {"headRefOid": NEW_HEAD}, {"state": "CLOSED"}],
        })
    if condition == "moved-during-enqueue":
        scenario.replace(live_head=NEW_HEAD, **{
            "pr read": [{}, {}, {"headRefOid": NEW_HEAD}, {"state": "CLOSED"}],
        })
    if condition == "unheld":
        scenario.replace(**{"pr read": [{}, {"held": False}]})
    hud.inputs.append(lambda hud, scope: hud.input_items(scope, "merge"))
    scenario.watch(120)
    calls = scenario.calls("enqueue")
    if condition == "unheld":
        assert not calls
        title = "Couldn't queue #7: no longer held"
    else:
        mutation, = calls
        assert mutation["fields"] == {"id": "pr-id", "head": HEAD}
        title = "#7 queued" if condition == "held" else "Couldn't queue #7: the head changed"
    card, notice = hud.request("hud_publish")
    closed, = hud.request("DELETE")
    assert notice[3] == {"surface": SURFACE, "content": {"title": title}, "ttl_ms": 15_000}
    assert card[2] == notice[2] == closed[2]
    assert closed[0] - notice[0] == 15
    assert not hud.request("hud_clear")
    assert not hud.scopes


@pytest.mark.parametrize(("choice", "first_return"), [("later", 3_600), ("dismiss", 3_600), ("expiry", 4_200.25)])
def test_card_returns_after_one_hour_in_a_new_scope_and_reads_latest_url(scenario, hud, choice, first_return):
    def replace_url(seconds):
        if seconds == 3_600:
            assert not hud.scopes
            with scenario.state.edit():
                scenario.state.write(review_url="https://example.com/changed-review")

    scenario.clock.after_sleep = replace_url
    if choice != "expiry":
        hud.inputs.append(lambda hud, scope: hud.input_items(scope, choice))
        hud.inputs.append(lambda hud, scope: hud.input_items(scope, "later"))
    scenario.watch(first_return + 1)
    first, second = hud.cards()
    assert first[2] != second[2] and first[0] == 0 and second[0] == first_return
    assert second[3]["content"]["actions"][1]["url"] == "https://example.com/changed-review"
    assert not scenario.calls("enqueue")
    assert [call[2] for call in hud.request("DELETE")] == [first[2], second[2]]


def test_stale_callback_and_other_surfaces_are_acked_in_scope_and_never_merge(scenario, hud):
    hud.inputs.extend([
        lambda hud, scope: hud.input_items(scope, "old", "other", "wrong-key"),
        lambda hud, scope: {"items": [{"id": f"{scope}-dismiss", "s": SURFACE, "key": "pr:7",
                                       "kind": "dismiss"}]},
    ])
    scenario.watch(61)
    opened, = hud.request("initialize")
    scope = opened[2]
    assert [call[3]["ack"] for call in hud.request("hud_input") if "ack" in call[3]] == [
        [f"{scope}-0", f"{scope}-1", f"{scope}-2"], [f"{scope}-dismiss"],
    ]
    assert all(call[2] == scope for call in hud.request("hud_input"))
    assert not scenario.calls("enqueue")
    assert not hud.scopes


@pytest.mark.parametrize("state", [{"state": "CLOSED"}, {"state": "MERGED"}, {"held": False}])
@pytest.mark.parametrize("active", [False, True])
def test_closed_merged_or_unheld_pr_exits_from_watch_or_live_card(scenario, hud, state, active):
    scenario.replace(**{"pr read": [{}, state] if active else [state]})
    scenario.watch(1_000)
    assert len(hud.cards()) == int(active)
    assert len(hud.request("DELETE")) == int(active)
    assert scenario.clock.now == (300 if active else 0)
    assert not scenario.calls("enqueue")
    assert not hud.scopes


@pytest.mark.parametrize("failure", ["unreachable", "auth", "refused", "safe-mode", "404", "gone"])
def test_hud_failure_defers_one_hour_without_any_enqueue(scenario, hud, monkeypatch, failure):
    if failure == "unreachable":
        def no_connection(*_args, **_kwargs):
            raise urllib.error.URLError("synthetic connection failure")

        monkeypatch.setattr(urllib.request, "urlopen", no_connection)
    elif failure == "auth":
        hud.initialize_status = 401
    elif failure in ("refused", "safe-mode"):
        hud.publish_result = {"code": "SAFE_MODE_ACTIVE" if failure == "safe-mode" else "FORBIDDEN",
                              "hint": "synthetic refusal"}
    else:
        hud.inputs.append("404" if failure == "404" else {"gone": "cleared"})
    scenario.watch(60)
    state = scenario.state.read()
    assert state["state"] == "waiting after Later" and state["next_return_at"] == 1_800_003_600
    assert not scenario.calls("enqueue")
    assert not hud.scopes


def test_card_with_zero_expiry_but_a_runtime_holding_remains_actionable(scenario, hud):
    hud.expiry_ms = 0
    hud.hold = True

    def after_hold(hud, scope):
        if not hud.request("hud_surfaces"):
            hud.clock.advance(0.25)
            return {"items": []}
        return hud.input_items(scope, "later")

    hud.inputs.extend([after_hold, after_hold])
    scenario.watch(61)
    assert len(hud.request("hud_surfaces")) == 1
    polls = hud.request("hud_input")
    assert [call[3]["wait_ms"] for call in polls if "wait_ms" in call[3]] == [250, 30_000]
    assert len(hud.request("DELETE")) == 1


@pytest.mark.parametrize("signum", [signal.SIGTERM, signal.SIGINT, signal.SIGHUP])
def test_clean_signal_releases_the_detached_watchers_own_scope(scenario, hud, signum):
    hud.poll_release = threading.Event()
    with held_child(scenario):
        assert scenario.cli("hold", "7").stdout == "watcher started\n"
        assert hud.poll_entered.wait(5)
        pid = scenario.state.read()["pid"]
        os.kill(pid, signum)
        wait_for(lambda: hud_pr.process_start(pid) is None)
        assert [call[2] for call in hud.request("DELETE")] == [hud.cards()[0][2]]
        assert scenario.cli("list").stdout == "#7 no watcher\n"


@pytest.mark.parametrize("phase", ["initialize", "delete"])
def test_release_during_scope_transition_deletes_the_scope_before_returning(scenario, hud, phase):
    unblock = threading.Event()
    if phase == "initialize":
        hud.initialize_release = unblock
        entered = hud.initialize_entered
    else:
        hud.inputs.append({"gone": "cleared"})
        hud.delete_release = unblock
        entered = hud.delete_entered
    cmd = [sys.executable, str(SCRIPTS / "hud_pr.py")]
    with held_child(scenario):
        assert scenario.cli("hold", "7").stdout == "watcher started\n"
        assert entered.wait(5)
        pid = scenario.state.read()["pid"]
        assert hud.scopes == {"scope-1"}
        with subprocess.Popen([*cmd, "release", "7"], stdout=subprocess.PIPE, text=True) as release:
            try:
                wait_for(lambda: stop_is_pending(pid) or hud_pr.process_start(pid) is None, seconds=2)
            finally:
                unblock.set()
            assert release.communicate(timeout=5)[0] == "released\n"
        assert len(hud.request("DELETE")) == 1
        assert not hud.scopes and len(hud.cards()) == int(phase == "delete")


def test_watch_stops_at_24_hours_without_a_scope_during_long_wait(scenario, hud):
    scenario.clock.now = hud_pr.WATCH_LIFETIME - 30
    hud.inputs.append(lambda hud, scope: hud.input_items(scope, "later"))
    scenario.watch(hud_pr.WATCH_LIFETIME)
    assert scenario.clock.now == 86_400
    assert scenario.clock.sleeps == [30]
    assert len(hud.request("DELETE")) == 1
    assert not hud.scopes
    assert scenario.cli("list").stdout == "#7 no watcher\n"


def test_merge_at_the_watch_boundary_uses_a_shorter_notice(scenario, hud):
    scenario.clock.now = hud_pr.WATCH_LIFETIME - 8
    hud.inputs.append(lambda hud, scope: hud.input_items(scope, "merge"))
    scenario.watch(hud_pr.WATCH_LIFETIME)
    assert len(scenario.calls("enqueue")) == 1
    _, notice = hud.request("hud_publish")
    closed, = hud.request("DELETE")
    assert notice[3]["ttl_ms"] == 8_000
    assert notice[0] + 8 == closed[0] == hud_pr.WATCH_LIFETIME
