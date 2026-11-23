#!/usr/bin/env python3
"""Hold a repository PR for the owner's exact-head merge-queue decision."""

import argparse
from contextlib import contextmanager
from datetime import datetime, timezone
import fcntl
import json
import math
import os
from pathlib import Path
import re
import secrets
import select
import signal
import stat
import subprocess
import sys
import tempfile
import time
from urllib.parse import urlsplit

from portal_client import ASK_CONTROL_TIMEOUT, AskInterrupted, AskSession, NOTIFICATION, ToolError, ask_signals, hud_env


ROOT = Path(__file__).resolve().parents[4]
LABEL = "owner-merge"
CHECK_INTERVAL = 60
STATE_INTERVAL = 300
RETURN_INTERVAL = 3_600
WATCH_LIFETIME = 86_400
CARD_TTL_MS = 600_000
NOTICE_TTL_MS = 15_000
GH_TIMEOUT = 8
REPO_PATTERN = r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+"
HEAD_PATTERN = r"[0-9a-f]{40}"
ACTIVE_STATES = {"watching", "card up", "waiting after Later"}

PR_QUERY = """
query HeldPullRequest($owner: String!, $repo: String!, $number: Int!,
                      $labelsAfter: String, $checksAfter: String, $checks: Boolean!) {
  repository(owner: $owner, name: $repo) {
    pullRequest(number: $number) {
      id state baseRefName headRefOid mergeable title url changedFiles additions deletions
      mergeQueueEntry { position }
      labels(first: 100, after: $labelsAfter) { nodes { name } pageInfo { hasNextPage endCursor } }
      commits(last: 1) @include(if: $checks) {
        nodes {
          commit {
            oid
            statusCheckRollup {
              contexts(first: 100, after: $checksAfter) {
                pageInfo { hasNextPage endCursor }
                nodes {
                  ... on CheckRun { isRequired(pullRequestNumber: $number) status conclusion }
                  ... on StatusContext { isRequired(pullRequestNumber: $number) state }
                }
              }
            }
          }
        }
      }
    }
  }
}
"""
LABEL_QUERY = """
query HeldLabel($owner: String!, $repo: String!) {
  repository(owner: $owner, name: $repo) { label(name: "owner-merge") { id } }
}
"""
ENQUEUE = """
mutation EnqueueHeldPullRequest($id: ID!, $head: GitObjectID!) {
  enqueuePullRequest(input: {pullRequestId: $id, expectedHeadOid: $head}) {
    mergeQueueEntry { position }
  }
}
"""


class PrError(Exception):
    """A safe diagnostic that can be shown without forwarding raw gh output."""


class WatchElapsed(Exception):
    """The watcher has used its lifetime."""


def gh(*args, timeout=GH_TIMEOUT):
    """The only GitHub command boundary. Keep raw replies out of logs."""
    environment = {**os.environ, "GH_PROMPT_DISABLED": "1", "GH_NO_UPDATE_NOTIFIER": "1"}
    for name in ("GH_REPO", "GH_DEBUG", "GIT_DIR", "GIT_COMMON_DIR", "GIT_WORK_TREE", "GIT_INDEX_FILE"):
        environment.pop(name, None)
    try:
        return subprocess.run(
            [os.environ.get("HUD_PR_GH", "gh"), *args], cwd=ROOT, env=environment,
            stdin=subprocess.DEVNULL, capture_output=True, text=True, timeout=timeout,
            errors="replace",
        )
    except OSError:
        raise PrError("could not run GitHub CLI") from None


def gh_result(*args, timeout=GH_TIMEOUT):
    try:
        result = gh(*args, timeout=timeout)
    except subprocess.TimeoutExpired:
        raise PrError("GitHub did not respond in time") from None
    if result.returncode:
        raise PrError("GitHub rejected the request")
    return result


def parse_json(result):
    try:
        return json.loads(result.stdout)
    except ValueError:
        raise PrError("GitHub returned invalid JSON") from None


def https_url(value):
    try:
        parts = urlsplit(value)
        if (parts.scheme != "https" or not parts.hostname or parts.username is not None
                or parts.password is not None or any(
                    character.isspace() or ord(character) < 32 or ord(character) == 127
                    for character in value
                )):
            raise ValueError
        return parts
    except (AttributeError, TypeError, ValueError):
        raise PrError("use an HTTPS URL without credentials") from None


def required_checks_green(contexts):
    required = [item for item in contexts if item["isRequired"]]
    return bool(required) and all(
        item.get("state") == "SUCCESS" if "state" in item else
        item.get("status") == "COMPLETED" and item.get("conclusion") in {"SUCCESS", "NEUTRAL", "SKIPPED"}
        for item in required
    )


def next_cursor(connection):
    page = connection["pageInfo"]
    if page["hasNextPage"]:
        cursor = page["endCursor"]
        if not isinstance(cursor, str) or not cursor:
            raise PrError("GitHub returned an invalid page cursor")
        return cursor
    return None


class GitHub:
    def __init__(self, repo=None, host=None):
        if repo is None:
            result = parse_json(gh_result("repo", "view", "--json", "nameWithOwner,url"))
            repo, host = result["nameWithOwner"], https_url(result["url"]).hostname
        if not isinstance(repo, str) or not re.fullmatch(REPO_PATTERN, repo):
            raise PrError("could not identify the checkout's GitHub repository")
        self.repo = repo.lower()
        self.host = host
        self.owner, self.name = self.repo.split("/")
        self.target = f"{host}/{self.repo}"
        self.remaining = lambda: math.inf

    def timeout(self, limit):
        seconds = min(limit, self.remaining())
        if seconds <= 0:
            raise WatchElapsed
        return seconds

    def execute(self, *args):
        return gh_result(*args, "--repo", self.target, timeout=self.timeout(GH_TIMEOUT))

    def query(self, query, **variables):
        args = ["api", "graphql", "--hostname", self.host, "-f", f"query={query}"]
        for name, value in variables.items():
            if type(value) is bool:
                args.extend(("-F", f"{name}={str(value).lower()}"))
            elif type(value) is int:
                args.extend(("-F", f"{name}={value}"))
            elif value is not None:
                args.extend(("-f", f"{name}={value}"))
        result = parse_json(gh_result(*args, timeout=self.timeout(GH_TIMEOUT)))
        if not isinstance(result, dict) or result.get("errors") or not isinstance(result.get("data"), dict):
            raise PrError("GitHub returned an incomplete reply")
        return result["data"]

    def ensure_label(self):
        def exists():
            return self.query(LABEL_QUERY, owner=self.owner, repo=self.name)["repository"]["label"] is not None

        if not exists():
            try:
                self.execute("label", "create", LABEL, "--description", "Held for the owner's merge decision")
            except PrError:
                # A hold for another PR may have created the repo-wide label.
                if not exists():
                    raise

    def numbers(self):
        entries = parse_json(self.execute(
            "pr", "list", "--label", LABEL, "--limit", str(2**31 - 1), "--json", "number",
        ))
        return sorted(entry["number"] for entry in entries)

    def read(self, number, *, checked_head=None):
        """Read PR metadata and, when requested, required checks for that exact head."""
        labels_cursor = checks_cursor = None
        held = False
        contexts = []
        labels_complete = checks_complete = False
        seen_pages = set()
        try:
            while True:
                cursors = (labels_cursor, checks_cursor)
                if cursors in seen_pages:
                    raise PrError("GitHub repeated a page cursor")
                seen_pages.add(cursors)
                verify = checked_head is not None and not checks_complete
                response = self.query(
                    PR_QUERY, owner=self.owner, repo=self.name, number=number,
                    labelsAfter=labels_cursor, checksAfter=checks_cursor, checks=verify,
                )
                pr = response["repository"]["pullRequest"]
                if not pr:
                    raise PrError("GitHub could not find that pull request")
                if not re.fullmatch(HEAD_PATTERN, pr["headRefOid"]):
                    raise PrError("GitHub returned an invalid pull request head")
                if not labels_complete:
                    labels = pr["labels"]
                    held = held or any(label["name"] == LABEL for label in labels["nodes"])
                    labels_cursor = None if held else next_cursor(labels)
                    labels_complete = labels_cursor is None
                if pr["headRefOid"] != checked_head:
                    checks_complete = True
                    contexts = []
                if verify and not checks_complete:
                    commits = pr["commits"]["nodes"]
                    commit = commits[0]["commit"] if len(commits) == 1 else None
                    if commit and commit["oid"] == checked_head and commit["statusCheckRollup"]:
                        checks = commit["statusCheckRollup"]["contexts"]
                        contexts.extend(checks["nodes"])
                        checks_cursor = next_cursor(checks)
                        checks_complete = checks_cursor is None
                    else:
                        checks_complete = True
                        contexts = []
                if labels_complete and checks_complete:
                    return {**pr, "held": held, "checks_green": required_checks_green(contexts)}
        except (KeyError, IndexError, TypeError, AttributeError):
            raise PrError("GitHub returned incomplete pull request data") from None

    def checks(self, number):
        try:
            result = gh(
                "pr", "checks", str(number), "--watch", "--required", "--fail-fast",
                "--interval", str(CHECK_INTERVAL), "--repo", self.target,
                timeout=self.timeout(CHECK_INTERVAL + GH_TIMEOUT),
            )
        except subprocess.TimeoutExpired:
            return "pending"
        if result.returncode == 0:
            return "green"
        if result.returncode == 8 or re.search(r"no (?:required )?checks reported|no commit found", result.stderr):
            return "pending"
        if result.returncode == 1 and not result.stderr.strip():
            return "red"
        raise PrError("GitHub could not read required checks")

    def enqueue(self, pr_id, head):
        entry = self.query(ENQUEUE, id=pr_id, head=head).get("enqueuePullRequest")
        if not isinstance(entry, dict) or not entry.get("mergeQueueEntry"):
            raise PrError("GitHub did not confirm queueing")


def private_file(path, access):
    fd = os.open(path, access | os.O_NOFOLLOW | os.O_CLOEXEC | os.O_NONBLOCK, 0o600)
    info = os.fstat(fd)
    if not stat.S_ISREG(info.st_mode) or info.st_uid != os.getuid() or info.st_mode & 0o077:
        os.close(fd)
        raise PrError("unsafe held-PR state file")
    return fd


def try_watch_lock(fd):
    try:
        fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
    except BlockingIOError:
        return False
    return True


def process_start(pid):
    """Read the Linux process start time so a recycled PID cannot be signaled."""
    try:
        fields = Path(f"/proc/{pid}/stat").read_text().rsplit(")", 1)[1].split()
        return int(fields[19]) if fields[0] != "Z" else None
    except (OSError, ValueError, IndexError):
        return None


def wait_for_exit(pidfd):
    try:
        stopped = select.poll()
        stopped.register(pidfd, select.POLLIN)
        if not stopped.poll(GH_TIMEOUT * 1_000):
            raise PrError("the previous watcher is still stopping")
    finally:
        os.close(pidfd)


class PrState:
    def __init__(self, repo, number):
        base = Path(os.environ.get("XDG_STATE_HOME") or Path.home() / ".local" / "state")
        if not base.is_absolute():
            raise PrError("XDG_STATE_HOME must be an absolute path")
        self.directory = base / "tze-hud" / "hud_pr"
        self.directory.mkdir(mode=0o700, parents=True, exist_ok=True)
        info = self.directory.lstat()
        if (not stat.S_ISDIR(info.st_mode) or info.st_uid != os.getuid() or info.st_mode & 0o077):
            raise PrError("unsafe held-PR state directory")
        self.repo = repo
        self.number = number
        stem = f"{repo.replace('/', '-')}-{number}"
        self.lock_path = self.directory / f"{stem}.lock"
        self.state_path = self.directory / f"{stem}.json"
        self.log_path = self.directory / f"{stem}.log"

    @contextmanager
    def lock_file(self, inherited=None):
        if inherited is None:
            fd = private_file(self.lock_path, os.O_RDWR | os.O_CREAT)
        else:
            fd = inherited
            os.set_inheritable(fd, False)
            if not os.path.samestat(self.lock_path.lstat(), os.fstat(fd)):
                raise PrError("watcher inherited a different pull request's lock")
        try:
            yield fd
        finally:
            os.close(fd)

    @contextmanager
    def edit(self, lock_fd=None):
        # flock spans the watcher lifetime. The separate POSIX record lock
        # serializes commands and state edits on the same file.
        if lock_fd is None:
            with self.lock_file() as fd, self.edit(fd):
                yield fd
            return
        fcntl.lockf(lock_fd, fcntl.LOCK_EX)
        try:
            yield lock_fd
        finally:
            fcntl.lockf(lock_fd, fcntl.LOCK_UN)

    def read(self):
        try:
            fd = private_file(self.state_path, os.O_RDONLY)
        except FileNotFoundError:
            return {}
        try:
            with os.fdopen(fd, encoding="utf-8") as stream:
                value = json.load(stream)
        except ValueError:
            raise PrError("invalid held-PR state") from None
        if not isinstance(value, dict) or value.get("repo") != self.repo:
            raise PrError("held-PR state belongs to a different repository")
        return value

    def write(self, **changes):
        """Replace the state under edit(), preserving a concurrent hold's review URL."""
        value = {**self.read(), **changes, "repo": self.repo}
        fd, name = tempfile.mkstemp(prefix=f".{self.state_path.name}.", dir=self.directory)
        try:
            with os.fdopen(fd, "w", encoding="utf-8") as stream:
                json.dump(value, stream)
            os.replace(name, self.state_path)
        finally:
            Path(name).unlink(missing_ok=True)

    def mark(self, phase, **changes):
        with self.edit():
            self.write(state=phase, **changes)
        self.log(phase)

    def log(self, outcome):
        print(f"#{self.number} {outcome}", flush=True)

    def summary(self):
        with self.edit() as fd:
            if try_watch_lock(fd):
                return "no watcher"
            view = self.read()
        phase = view.get("state")
        if phase == "waiting after Later":
            when = datetime.fromtimestamp(view["next_return_at"], timezone.utc)
            return f"{phase} ({when.isoformat(timespec='seconds')})"
        return phase if phase in ACTIVE_STATES else "watching"

    def spawn(self, github, lock_fd):
        with os.fdopen(private_file(self.log_path, os.O_WRONLY | os.O_CREAT | os.O_APPEND), "a") as log:
            return subprocess.Popen(
                [sys.executable, str(Path(__file__).resolve()), "watch", str(self.number),
                 "--lock-fd", str(lock_fd), "--repo", github.repo, "--hostname", github.host],
                cwd=ROOT, start_new_session=True, pass_fds=(lock_fd,),
                stdin=subprocess.DEVNULL, stdout=log, stderr=log,
            )

    def watcher_pidfd(self):
        view = self.read()
        pid, start, lock_fd = (view.get(name) for name in ("pid", "pid_start", "lock_fd"))
        if (type(pid) is not int or pid <= 0 or type(start) is not int
                or type(lock_fd) is not int or lock_fd < 0):
            return
        try:
            pidfd = os.pidfd_open(pid)
        except ProcessLookupError:
            return
        try:
            open_lock = Path(f"/proc/{pid}/fd/{lock_fd}").stat()
            if process_start(pid) == start and os.path.samestat(self.lock_path.lstat(), open_lock):
                return pidfd
        except OSError:
            pass
        os.close(pidfd)
        return None


def pair():
    try:
        endpoint = hud_env.adapter_endpoint()
        return endpoint["url"], hud_env.adapter_key(endpoint)
    except (hud_env.HudEnvError, OSError, ValueError):
        raise ToolError("HUD_CONFIGURATION", "no HUD pairing") from None


def hold(github, state, review_url):
    while True:
        with state.edit() as fd:
            if state.read().get("state") == "stopping" and not try_watch_lock(fd):
                outgoing = state.watcher_pidfd()
                if outgoing is None:
                    raise PrError("the previous watcher's exit could not be confirmed")
            else:
                github.ensure_label()
                github.execute("pr", "edit", str(state.number), "--add-label", LABEL)
                if not try_watch_lock(fd):
                    state.write(review_url=review_url)
                    return "already running"
                state.write(review_url=review_url, state="no watcher", pid=None, pid_start=None, lock_fd=None)
                try:
                    pair()
                except ToolError:
                    return "no HUD pairing: no card"
                child = state.spawn(github, fd)
                state.write(state="watching", pid=child.pid, pid_start=process_start(child.pid), lock_fd=fd)
                return "watcher started"
        wait_for_exit(outgoing)


def release(github, state):
    with state.edit() as fd:
        github.execute("pr", "edit", str(state.number), "--remove-label", LABEL)
        if try_watch_lock(fd):
            return "released"
        pidfd = state.watcher_pidfd()
        if pidfd is None and not try_watch_lock(fd):
            raise PrError("the label was removed and watcher ownership could not be confirmed")
        if pidfd is not None:
            try:
                state.write(state="stopping")
                signal.pidfd_send_signal(pidfd, signal.SIGTERM)
            except ProcessLookupError:
                pass
            except BaseException:
                os.close(pidfd)
                raise
    if pidfd is not None:
        wait_for_exit(pidfd)
    return "released"


def card_content(number, details, nonce, review_url):
    try:
        files, additions, deletions = (details[field] for field in ("changedFiles", "additions", "deletions"))
        if any(type(value) is not int or value < 0 for value in (files, additions, deletions)):
            raise ValueError
        title = details["title"][:60]
        if not isinstance(title, str):
            raise ValueError
        url = review_url or details["url"]
        https_url(url)
    except (KeyError, TypeError, AttributeError, ValueError):
        raise PrError("GitHub returned invalid card details") from None
    return {
        "title": f"#{number} ready to merge",
        "body": f"{title} · {files} files +{additions} −{deletions} · CI green",
        "actions": [
            {"label": "Merge", "callback_id": f"merge.{nonce}"},
            {"label": "Review", "url": url},
            {"label": "Later", "callback_id": f"later.{nonce}"},
        ],
    }


def is_held(pr):
    return pr["state"] == "OPEN" and pr["held"]


def can_show_card(pr):
    return (pr["baseRefName"] == "main" and pr["mergeable"] != "CONFLICTING"
            and pr["mergeQueueEntry"] is None)


class Watcher:
    def __init__(self, github, state, *, clock=time):
        self.github = github
        self.state = state
        self.number = state.number
        self.key = f"pr:{self.number}"
        self.clock = clock
        self.end = clock.monotonic() + WATCH_LIFETIME
        self.github.remaining = self.remaining

    def remaining(self):
        return self.end - self.clock.monotonic()

    def sleep(self, seconds):
        delay = min(seconds, self.remaining())
        if delay > 0:
            self.clock.sleep(delay)

    def returning(self):
        self.state.mark(
            "waiting after Later", nonce=None, next_return_at=self.clock.time() + RETURN_INTERVAL,
        )
        self.sleep(RETURN_INTERVAL)

    def run(self):
        failed_head = None
        interval = STATE_INTERVAL
        while self.remaining() > 0:
            start = self.clock.monotonic()
            try:
                pr = self.github.read(self.number)
                if not is_held(pr):
                    return
                head = pr["headRefOid"]
                watching_checks = can_show_card(pr) and head != failed_head
                interval = CHECK_INTERVAL if watching_checks else STATE_INTERVAL
                state = self.state.read()
                if head != state.get("head") or state.get("state") != "watching":
                    self.state.mark("watching", head=head, nonce=None, next_return_at=None)
                if watching_checks:
                    checks = self.github.checks(self.number)
                    if checks == "red":
                        current = self.github.read(self.number)
                        if not is_held(current):
                            return
                        if current["headRefOid"] == head:
                            failed_head = head
                            interval = STATE_INTERVAL
                    elif checks == "green":
                        current = self.github.read(self.number, checked_head=head)
                        if not is_held(current):
                            return
                        if current["headRefOid"] == head and current["checks_green"] and can_show_card(current):
                            interval = STATE_INTERVAL
                            result = self.card(current)
                            if result == "stop":
                                return
                            if result == "later":
                                self.returning()
                            continue
            except PrError:
                self.state.log("GitHub read failed")
            self.sleep(start + interval - self.clock.monotonic())

    def card(self, pr):
        if self.remaining() <= 0:
            return "stop"
        nonce = secrets.token_hex(8)
        review_url = self.state.read().get("review_url")
        content = card_content(self.number, pr, nonce, review_url)
        outcome = "later"
        try:
            with AskSession(label=f"PR #{self.number}", connection=pair()) as hud:
                result = hud.call_tool("hud_publish", {
                    "surface": NOTIFICATION, "content": content, "ttl_ms": CARD_TTL_MS, "key": self.key,
                })
                expiry = result.get("expires_in_ms")
                if result.get("ok") is not True or type(expiry) is not int or expiry < 0:
                    raise ToolError("MCP_PROTOCOL", "HUD did not confirm the card's expiry")
                self.state.mark("card up", nonce=nonce, next_return_at=None)
                deadline = self.clock.monotonic() + (expiry + 250) / 1_000
                action = self.card_input(hud, nonce, deadline)
                if action == "merge":
                    outcome = self.merge(hud, pr)
                elif action == "stop":
                    outcome = "stop"
        except ToolError:
            self.state.log("HUD unavailable")
        return outcome

    def card_input(self, hud, nonce, deadline):
        next_pr_read = self.clock.monotonic() + STATE_INTERVAL
        while self.remaining() > 0:
            now = self.clock.monotonic()
            if now >= next_pr_read:
                try:
                    if not is_held(self.github.read(self.number)):
                        return "stop"
                except PrError:
                    self.state.log("GitHub read failed")
                next_pr_read = self.clock.monotonic() + STATE_INTERVAL
                now = self.clock.monotonic()
            wait_until = min(deadline, next_pr_read, self.end)
            wait_ms = max(0, min(30_000, math.ceil((wait_until - now) * 1_000)))
            result = hud.call_tool("hud_input", {"wait_ms": wait_ms}, timeout=wait_ms / 1_000 + 3)
            if result.get("gone"):
                return "later"
            try:
                items = result["items"]
                ids = [item["id"] for item in items]
                if any(not isinstance(item_id, str) or not item_id for item_id in ids):
                    raise ValueError
            except (TypeError, KeyError, ValueError):
                raise ToolError("MCP_PROTOCOL", "HUD returned invalid input") from None
            if ids:
                acked = hud.call_tool("hud_input", {"ack": ids, "max_items": 0})
                if acked.get("gone"):
                    return "later"
            for item in items:
                if item.get("s") != NOTIFICATION or item.get("key") != self.key:
                    continue
                if item.get("kind") == "dismiss":
                    return "later"
                if item.get("kind") is None:
                    if item.get("action") == f"later.{nonce}":
                        return "later"
                    if item.get("action") == f"merge.{nonce}" and self.remaining() > 0:
                        return "merge"
            if self.clock.monotonic() >= deadline:
                surfaces = hud.call_tool("hud_surfaces", {}).get("surfaces", [])
                active = any(
                    s.get("s") == NOTIFICATION and s.get("held")
                    for s in surfaces
                )
                if not active:
                    return "later"
                deadline = self.clock.monotonic() + 60
        return "stop"

    def merge(self, hud, shown):
        head = shown["headRefOid"]
        reason = None
        stop = False
        current = None
        try:
            # Serialize a local release with the fresh held-label check and enqueue.
            with self.state.edit():
                current = self.github.read(self.number)
                if not is_held(current):
                    reason = "no longer held" if not current["held"] else "pull request is closed"
                    stop = True
                else:
                    self.github.enqueue(shown["id"], head)
                    stop = True
        except PrError as error:
            reason = str(error)
            if current is not None:
                if current["headRefOid"] == head:
                    try:
                        current = self.github.read(self.number)
                    except PrError:
                        pass
                if current["headRefOid"] != head:
                    reason = "the head changed"
        if reason is None:
            title = f"#{self.number} queued"
            self.state.log("queued")
        else:
            title = f"Couldn't queue #{self.number}: {reason[:100]}"
            self.state.log("queue request failed")
        remaining = self.remaining()
        notice_ttl = min(NOTICE_TTL_MS, math.floor(remaining * 1_000))
        if notice_ttl > 0:
            try:
                notice = hud.call_tool("hud_publish", {
                    "surface": NOTIFICATION, "content": {"title": title}, "ttl_ms": notice_ttl,
                }, timeout=min(ASK_CONTROL_TIMEOUT, remaining))
                if notice.get("ok") is True:
                    self.sleep(notice_ttl / 1_000)
            except ToolError:
                self.state.log("HUD notice unavailable")
        return "stop" if stop else "watch"


def watch(github, state, inherited=None):
    with ask_signals(), state.lock_file(inherited) as lock_fd:
        if not try_watch_lock(lock_fd):
            return
        with state.edit(lock_fd):
            state.write(state="watching", pid=os.getpid(), pid_start=process_start(os.getpid()), lock_fd=lock_fd)
        try:
            Watcher(github, state).run()
        except (WatchElapsed, AskInterrupted):
            pass
        finally:
            state.mark("no watcher", pid=None, pid_start=None, lock_fd=None, nonce=None, next_return_at=None)


def parse_number(value):
    try:
        number = int(value)
        if not 0 < number < 2**31:
            raise ValueError
        return number
    except ValueError:
        raise argparse.ArgumentTypeError("use a positive pull request number") from None


def parse_review_url(value):
    try:
        https_url(value)
    except PrError as error:
        raise argparse.ArgumentTypeError(str(error)) from None
    return value


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    hold_parser = commands.add_parser("hold", help="hold a PR and start its watcher")
    hold_parser.add_argument("number", type=parse_number)
    hold_parser.add_argument("--review-url", type=parse_review_url)
    commands.add_parser("list", help="list held PRs and their watcher status")
    commands.add_parser("release", help="remove a PR's hold and stop its watcher").add_argument(
        "number", type=parse_number,
    )
    watch_parser = commands.add_parser("watch")
    watch_parser.add_argument("number", type=parse_number)
    watch_parser.add_argument("--lock-fd", type=int, help=argparse.SUPPRESS)
    watch_parser.add_argument("--repo", help=argparse.SUPPRESS)
    watch_parser.add_argument("--hostname", help=argparse.SUPPRESS)
    args = parser.parse_args(argv)
    if args.command == "watch":
        inherited = (args.lock_fd, args.repo, args.hostname)
        if any(value is not None for value in inherited) and any(value is None for value in inherited):
            parser.error("incomplete detached watcher handoff")
    try:
        github = GitHub(getattr(args, "repo", None), getattr(args, "hostname", None))
        if args.command == "list":
            for number in github.numbers():
                state = PrState(github.repo, number)
                print(f"#{number} {state.summary()}")
            return 0
        state = PrState(github.repo, args.number)
        if args.command == "hold":
            print(hold(github, state, args.review_url))
        elif args.command == "release":
            print(release(github, state))
        else:
            watch(github, state, args.lock_fd)
    except AskInterrupted:
        return 0
    except (PrError, OSError, KeyError, TypeError, ValueError) as error:
        detail = str(error) if isinstance(error, PrError) else "held-PR operation failed"
        if args.command == "watch":
            print(f"#{args.number} watcher failed", file=sys.stderr)
        else:
            print(f"hud_pr: {detail}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
