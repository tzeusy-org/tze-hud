#!/usr/bin/env python3
"""Select the exact workspace checks for every proposed pre-push ref."""

from __future__ import annotations

import argparse
from contextlib import contextmanager
import errno
import os
from pathlib import Path
import re
import subprocess
import sys
import time
from typing import Iterator, NamedTuple


class MetadataError(ValueError):
    """Git metadata cannot safely establish a fast path or checked source."""


class RefUpdate(NamedTuple):
    local_ref: str
    local_oid: str
    remote_ref: str
    remote_oid: str


def git(*args: str, cwd: Path | None = None) -> bytes:
    result = subprocess.run(
        ["git", *args], cwd=cwd, stdout=subprocess.PIPE, stderr=subprocess.PIPE
    )
    if result.returncode:
        # Git's stderr can contain remote/config values. Report only the operation.
        raise MetadataError("cannot read Git metadata; check the local refs and origin/main")
    return result.stdout


def parse_updates(lines: Iterator[str]) -> list[RefUpdate]:
    updates = []
    for line in lines:
        if not line.strip():
            continue
        fields = line.split()
        if len(fields) != 4:
            raise MetadataError("expected four fields in each pre-push ref update")
        update = RefUpdate(*fields)
        if not re.fullmatch(r"(?:[0-9a-fA-F]{40}|[0-9a-fA-F]{64})", update.local_oid):
            raise MetadataError("invalid local object ID in pre-push input")
        if len(update.remote_oid) != len(update.local_oid) or not re.fullmatch(
            r"[0-9a-fA-F]+", update.remote_oid
        ):
            raise MetadataError("invalid remote object ID in pre-push input")
        if not update.remote_ref.startswith(("refs/heads/", "refs/tags/")):
            raise MetadataError("unsupported remote ref; use a branch or commit-bearing tag")
        git("check-ref-format", update.remote_ref)
        updates.append(update)
    return updates


def commit(oid: str) -> str:
    return git("rev-parse", "--verify", "--end-of-options", oid + "^{commit}").decode().strip()


def has_rust_changes(baseline: str, proposed: str) -> bool:
    paths = git("diff", "--name-only", "-z", baseline, proposed, "--").split(b"\0")
    return any(
        path.endswith((b".rs", b".toml")) or path.rsplit(b"/", 1)[-1] == b"Cargo.lock"
        for path in paths
        if path
    )


def require_clean_source(head: str) -> None:
    if commit("HEAD") != head:
        raise MetadataError("HEAD changed; check and push the intended head in its own worktree")
    if git("status", "--porcelain", "-z", "--untracked-files=all"):
        raise MetadataError("working tree differs from the pushed commit; commit or remove local changes")


@contextmanager
def check_lock(common_git: Path) -> Iterator[None]:
    """Serialize hook checks, without claiming a lease on other Cargo processes."""
    lock_path = common_git / "pre-push-check.lock"
    if lock_path.is_symlink():
        raise MetadataError("pre-push lock is a symlink; inspect it before checking")
    flags = os.O_RDWR | os.O_CREAT | getattr(os, "O_NOFOLLOW", 0)
    descriptor = os.open(lock_path, flags, 0o664)
    with os.fdopen(descriptor, "r+b") as stream:
        if os.name == "nt":
            # Import lazily: the selector also runs under native Windows Python.
            import msvcrt

            if stream.seek(0, os.SEEK_END) == 0:
                stream.write(b"\0")
                stream.flush()

            def acquire() -> None:
                stream.seek(0)
                msvcrt.locking(stream.fileno(), msvcrt.LK_NBLCK, 1)

            def release() -> None:
                stream.seek(0)
                msvcrt.locking(stream.fileno(), msvcrt.LK_UNLCK, 1)

        else:
            import fcntl

            def acquire() -> None:
                fcntl.flock(stream.fileno(), fcntl.LOCK_EX | fcntl.LOCK_NB)

            def release() -> None:
                fcntl.flock(stream.fileno(), fcntl.LOCK_UN)

        deadline = time.monotonic() + 5.0
        while True:
            try:
                acquire()
                break
            except OSError as error:
                if error.errno not in (errno.EACCES, errno.EAGAIN):
                    raise
                if time.monotonic() >= deadline:
                    raise MetadataError("another pre-push check holds the lock; wait for it to finish")
                time.sleep(0.05)
        try:
            yield
        finally:
            release()


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("mode", choices=("head", "git-stdin"), default="head", nargs="?")
    args = parser.parse_args()
    try:
        if args.mode == "git-stdin":
            updates = parse_updates(iter(sys.stdin))
            proposed = [u.local_oid for u in updates if set(u.local_oid) != {"0"}]
            if not proposed:
                print("pre-push: no new objects to check")
                return 0
        else:
            proposed = ["HEAD"]

        root = Path(os.fsdecode(git("rev-parse", "--show-toplevel").strip()))
        # Run from the checkout root even when just was invoked in a subdirectory.
        os.chdir(root)
        baseline = commit("refs/remotes/origin/main")
        heads = [commit(oid) for oid in proposed]
        current_head = commit("HEAD")
        needs_checks = False
        for head in heads:
            if not has_rust_changes(baseline, head):
                continue
            if head != current_head:
                raise MetadataError(
                    "a Rust/Cargo-bearing ref is not checked out; check and push it in its own worktree"
                )
            needs_checks = True
        if not needs_checks:
            print("pre-push: proposed refs have no Rust/Cargo changes")
            return 0

        common = Path(os.fsdecode(git("rev-parse", "--git-common-dir").strip())).resolve()
        with check_lock(common):
            require_clean_source(current_head)
            # Resource ownership remains the caller's responsibility. Do not select
            # a coordinator's shared target or silently alter Cargo environment.
            result = subprocess.run(["just", "pre-push-check"], cwd=root)
            try:
                require_clean_source(current_head)
            except MetadataError as error:
                if result.returncode == 0:
                    raise
                # Preserve the first checker failure even if it also changed
                # source (for example, Clippy updating Cargo.lock).
                print("pre-push: source verification also failed: " + str(error), file=sys.stderr)
            return result.returncode
    except MetadataError as error:
        print("pre-push: " + str(error), file=sys.stderr)
        return 1
    except OSError as error:
        print(
            "pre-push: required Git/just or local lock unavailable ("
            + type(error).__name__ + ")", file=sys.stderr,
        )
        return 1
    except KeyboardInterrupt:
        return 130


if __name__ == "__main__":
    sys.exit(main())
