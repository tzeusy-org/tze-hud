#!/usr/bin/env python3
"""Bind an explicitly guarded Cargo target to one frozen materialized checkout.

This is a cooperative source policy, not a replacement for Cargo fingerprints
or the caller's Cargo/GPU resource lease. See docs/development/source-freshness.md.
"""

from __future__ import annotations

import argparse
from contextlib import contextmanager
import hashlib
import json
import os
from pathlib import Path
import re
import signal
import stat
import subprocess
import sys
import tomllib
from typing import Iterator


CONTROL = "TZE_HUD_SHARED_TARGET_GUARD"
REFUSED = 86
MARKER = ".tze-hud-source-binding.json"
LOCK = ".tze-hud-source-binding.lock"
SCHEMA = 1
BEADS_ADMIN = {
    ".beads/.gitignore", ".beads/README.md", ".beads/config.yaml",
    ".beads/metadata.json",
}
COMMANDS = {"build", "check", "clippy", "deny", "fmt", "machete", "metadata", "nextest", "run", "test"}


class Refusal(ValueError):
    """The caller cannot establish the supported source/target identity."""


def digest(value: object) -> str:
    return hashlib.sha256(json.dumps(value, sort_keys=True).encode()).hexdigest()


def git(*args: str, cwd: Path, input_data: bytes | None = None) -> bytes:
    # Identity comes from the physical checkout, not an inherited alternate index.
    env = {k: v for k, v in os.environ.items() if k not in {
        "GIT_DIR", "GIT_WORK_TREE", "GIT_INDEX_FILE", "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES", "GIT_COMMON_DIR",
    }}
    env["GIT_OPTIONAL_LOCKS"] = "0"
    result = subprocess.run(
        ["git", *args], cwd=cwd, env=env, input=input_data,
        stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=30,
    )
    if result.returncode or len(result.stdout) > 4 * 1024 * 1024:
        raise Refusal("git-metadata-unavailable")
    return result.stdout


def regular_bytes(path: Path) -> bytes:
    info = path.lstat()
    if not stat.S_ISREG(info.st_mode) or info.st_nlink != 1:
        raise Refusal("unsupported-input-file")
    data = path.read_bytes()
    after = path.lstat()
    if (info.st_dev, info.st_ino, info.st_size, info.st_mtime_ns) != (
        after.st_dev, after.st_ino, after.st_size, after.st_mtime_ns
    ):
        raise Refusal("input-changed-during-snapshot")
    return data


def inside(path: Path, root: Path) -> bool:
    return path == root or root in path.parents


def project_context() -> tuple[Path, Path]:
    cwd = Path.cwd().resolve()
    root = Path(os.fsdecode(git("rev-parse", "--show-toplevel", cwd=cwd).strip())).resolve()
    if cwd != root:
        raise Refusal("checkout-root-required")
    common = Path(os.fsdecode(git("rev-parse", "--path-format=absolute", "--git-common-dir", cwd=root).strip())).resolve()
    actual_dir = Path(os.fsdecode(git("rev-parse", "--absolute-git-dir", cwd=root).strip())).resolve()
    for name, expected in [("GIT_DIR", actual_dir), ("GIT_WORK_TREE", root)]:
        if os.environ.get(name) and Path(os.environ[name]).resolve() != expected:
            raise Refusal("git-context-override")
    if any(os.environ.get(name) for name in (
        "GIT_INDEX_FILE", "GIT_OBJECT_DIRECTORY", "GIT_ALTERNATE_OBJECT_DIRECTORIES", "GIT_COMMON_DIR",
    )):
        raise Refusal("git-context-override")
    if not inside(root, Path.home().resolve()) or not inside(common, Path.home().resolve()):
        raise Refusal("source-outside-home")
    return root, common


def index_entries(root: Path) -> list[tuple[str, str, str]]:
    entries = []
    for record in git("ls-files", "--stage", "-z", cwd=root).split(b"\0"):
        if not record:
            continue
        meta, raw_path = record.split(b"\t", 1)
        mode, oid, stage = meta.decode().split()
        path = os.fsdecode(raw_path)
        if stage != "0" or mode not in {"100644", "100755", "120000"}:
            raise Refusal("unsupported-index-entry")
        entries.append((path, mode, oid))
    tree = {}
    for record in git("ls-tree", "-r", "-z", "HEAD", cwd=root).split(b"\0"):
        if record:
            meta, raw_path = record.split(b"\t", 1)
            mode, kind, oid = meta.decode().split()
            tree[os.fsdecode(raw_path)] = (mode, oid)
            if kind != "blob":
                raise Refusal("unsupported-index-entry")
    if {p: (m, o) for p, m, o in entries} != tree:
        raise Refusal("index-differs-from-head")
    return [entry for entry in entries if entry[0] not in BEADS_ADMIN]


def require_builtin_filters(root: Path, names: list[str]) -> None:
    paths = b"\0".join(os.fsencode(p) for p in names) + b"\0"
    attributes = git("check-attr", "-z", "--stdin", "filter", "working-tree-encoding", cwd=root, input_data=paths).split(b"\0")
    for offset in range(0, len(attributes) - 1, 3):
        if attributes[offset + 2] not in {b"unspecified", b"unset"}:
            raise Refusal("unsupported-git-filter")


def tracked_snapshot(root: Path) -> tuple[str, dict[str, bytes]]:
    entries = index_entries(root)
    require_builtin_filters(root, [p for p, _, _ in entries])
    if git("status", "--porcelain", "-z", "--untracked-files=all", cwd=root):
        raise Refusal("source-not-frozen")
    hash_state = hashlib.sha256()
    manifests = {}
    regular = []
    tracked = {name for name, _, _ in entries}
    for name, mode, oid in entries:
        path = root / name
        resolved = path.resolve(strict=True)
        if not inside(resolved, root):
            raise Refusal("escaping-source-symlink")
        if path.is_symlink():
            destination = resolved.relative_to(root).as_posix()
            if mode != "120000" or not any(p == destination or p.startswith(destination + "/") for p in tracked):
                raise Refusal("untracked-source-symlink")
            target = os.fsencode(os.readlink(path))
            algorithm = hashlib.sha1 if len(oid) == 40 else hashlib.sha256
            if algorithm(b"blob " + str(len(target)).encode() + b"\0" + target).hexdigest() != oid:
                raise Refusal("source-differs-from-index")
            data = target
        else:
            if mode == "120000" or resolved.relative_to(root).as_posix() != name:
                raise Refusal("unsupported-source-symlink")
            data = regular_bytes(path)
            regular.append((name, oid))
        hash_state.update(json.dumps([name, mode, len(data)]).encode() + b"\0")
        hash_state.update(data)
        if name.endswith("Cargo.toml") or name in {".cargo/config", ".cargo/config.toml"}:
            manifests[name] = data
    # Git performs only supported built-in EOL conversion. No -w: this reads
    # and hashes files without writing objects or depending on index stat flags.
    for offset in range(0, len(regular), 32):
        chunk = regular[offset:offset + 32]
        actual = git("hash-object", "--", *(name for name, _ in chunk), cwd=root).decode().splitlines()
        if actual != [oid for _, oid in chunk]:
            raise Refusal("source-differs-from-index")
    return hash_state.hexdigest(), manifests


def input_closure(root: Path, manifests: dict[str, bytes]) -> None:
    # Only this set was hashed. Tracked administration exemptions cannot be
    # repurposed as a target or reached through a declared source symlink.
    tracked = {Path(name) for name, _, _ in index_entries(root)}
    documents = {
        root / name: tomllib.loads(data.decode())
        for name, data in manifests.items() if name.endswith("Cargo.toml")
    }
    roots = {"assets", "targets", ".cargo"}

    def table(value: object) -> dict:
        if not isinstance(value, dict):
            raise Refusal("unsupported-manifest-table")
        return value

    def declared_path(parent: Path, value: object, *, package: bool = False) -> Path:
        if not isinstance(value, str) or not value:
            raise Refusal("unsupported-manifest-path")
        # Normalize native path syntax before resolving or opening anything.
        # An escaping declaration is refused without visiting its source root.
        path = Path(os.path.abspath(parent / value))
        if package:
            path /= "Cargo.toml"
        if not inside(path, root):
            raise Refusal("external-declared-input")
        if path.relative_to(root) not in tracked:
            raise Refusal("unhashed-declared-input")
        resolved = path.resolve(strict=True)
        if not inside(resolved, root) or resolved.relative_to(root) not in tracked:
            raise Refusal("unhashed-declared-input")
        if not resolved.is_file():
            raise Refusal("unsupported-declared-input")
        return resolved

    def dependencies(parent: Path, value: object) -> None:
        for dependency in table(value).values():
            if isinstance(dependency, str):
                continue
            dependency = table(dependency)
            if "path" in dependency:
                declared_path(parent, dependency["path"], package=True)

    def dependency_tables(parent: Path, node: dict) -> None:
        for key in ("dependencies", "dev-dependencies", "build-dependencies"):
            if key in node:
                dependencies(parent, node[key])

    for manifest, document in documents.items():
        parent = manifest.parent
        if parent != root:
            roots.add(parent.relative_to(root).as_posix())
        elif "package" in document:
            roots.update({"src", "tests", "examples", "benches", "build.rs"})
        if "replace" in document:
            raise Refusal("unsupported-manifest-replace")
        if "lib" in document:
            library = table(document["lib"])
            if "path" in library:
                declared_path(parent, library["path"])
        for key in ("bin", "example", "test", "bench"):
            if key not in document:
                continue
            if not isinstance(document[key], list):
                raise Refusal("unsupported-manifest-targets")
            for target in document[key]:
                target = table(target)
                if "path" in target:
                    declared_path(parent, target["path"])
        dependency_tables(parent, document)
        for target in table(document.get("target", {})).values():
            dependency_tables(parent, table(target))
        for patch in table(document.get("patch", {})).values():
            dependencies(parent, patch)
        if "workspace" in document:
            workspace = table(document["workspace"])
            if "dependencies" in workspace:
                dependencies(parent, workspace["dependencies"])
            for key in ("members", "default-members"):
                if key not in workspace:
                    continue
                if not isinstance(workspace[key], list):
                    raise Refusal("unsupported-workspace-members")
                for member in workspace[key]:
                    if isinstance(member, str) and any(c in member for c in "*?[]"):
                        raise Refusal("unsupported-workspace-glob")
                    declared_path(parent, member, package=True)
        if "package" not in document:
            continue
        package = table(document["package"])
        if "build" in package and package["build"] is not False:
            declared_path(parent, package["build"])
        if "workspace" in package:
            anchor = declared_path(parent, package["workspace"], package=True)
            if "workspace" in document or "workspace" not in documents.get(anchor, {}):
                raise Refusal("unsupported-workspace-anchor")
            table(documents[anchor]["workspace"])
        else:
            # Cargo config ancestry is separate. Never discover a workspace
            # from an unhashed ancestor Cargo.toml outside this checkout.
            directory = parent
            while inside(directory, root):
                anchor = documents.get(directory / "Cargo.toml", {})
                if "workspace" in anchor:
                    table(anchor["workspace"])
                    break
                directory = directory.parent
            else:
                raise Refusal("internal-workspace-anchor-required")
    # Enumerate names only. Never open ignored .env/credentials or scan target,
    # worktree, private operations, Python-cache or receipt roots.
    ignored = git("ls-files", "--others", "--ignored", "--exclude-standard", "-z", "--", *sorted(roots), cwd=root)
    for raw_path in ignored.split(b"\0"):
        path = Path(os.fsdecode(raw_path))
        if raw_path and "__pycache__" not in path.parts and path.suffix != ".pyc":
            raise Refusal("untracked-build-input")


def configuration(root: Path, common: Path, manifests: dict[str, bytes]) -> str:
    home = Path.home().resolve()
    cargo_home = Path(os.environ.get("CARGO_HOME", str(home / ".cargo"))).resolve()
    if not inside(cargo_home, home):
        raise Refusal("external-cargo-home")
    configs = {}
    main = common.parent if common.name == ".git" else root
    for parent in [root, *root.parents]:
        if not inside(parent, home):
            continue
        for filename in ("config", "config.toml"):
            path = parent / ".cargo" / filename
            if not path.exists() and not path.is_symlink():
                continue
            name = ".cargo/" + filename
            if not inside(path.resolve(), home):
                raise Refusal("external-cargo-config")
            if parent == root and name in manifests:
                data = manifests[name]
            elif parent == main and name in {
                os.fsdecode(p) for p in git("ls-files", "-z", "--", name, cwd=main).split(b"\0") if p
            }:
                if path.resolve().relative_to(main).as_posix() != name:
                    raise Refusal("external-cargo-config")
                require_builtin_filters(main, [name])
                indexed = git("ls-files", "--stage", "--", name, cwd=main).decode().split()
                if len(indexed) < 3 or indexed[2] != "0":
                    raise Refusal("unsupported-cargo-config")
                expected = git("rev-parse", "--verify", "HEAD:" + name, cwd=main).strip()
                if indexed[1].encode() != expected or git("hash-object", "--", name, cwd=main).strip() != expected:
                    raise Refusal("source-config-not-frozen")
                data = regular_bytes(path)
            else:
                raise Refusal("external-cargo-config")
            document = tomllib.loads(data.decode())
            # Config paths have a different base from manifest paths. These
            # unused source overrides are outside the finite guarded envelope,
            # even when their values point into tracked source.
            if any(key in document for key in ("paths", "patch", "source")):
                raise Refusal("unsupported-cargo-source-override")

            def supported(node: dict) -> None:
                for key, value in node.items():
                    if key in {"include", "build-dir", "alias", "env", "runner", "rustc", "rustdoc", "rustc-wrapper", "rustc-workspace-wrapper"}:
                        raise Refusal("unsupported-cargo-config")
                    if isinstance(value, dict):
                        supported(value)

            supported(document)
            configs[str(path.resolve())] = hashlib.sha256(data).hexdigest()
    for filename in ("config", "config.toml"):
        path = cargo_home / filename
        if (path.exists() or path.is_symlink()) and str(path.resolve()) not in configs:
            raise Refusal("external-cargo-config")
    return digest(configs)


def compiler() -> str:
    if any(os.environ.get(name) for name in (
        "RUSTC", "RUSTDOC", "RUSTC_WRAPPER", "RUSTC_WORKSPACE_WRAPPER",
        "CARGO_BUILD_RUSTC", "CARGO_BUILD_RUSTDOC", "CARGO_BUILD_RUSTC_WRAPPER",
        "CARGO_BUILD_RUSTC_WORKSPACE_WRAPPER",
    )):
        raise Refusal("unsupported-compiler-override")
    toolchain = os.environ.get("RUSTUP_TOOLCHAIN", "")
    if toolchain and not re.fullmatch(r"1\.88(?:\.0)?(?:-[a-z0-9_-]+)?", toolchain):
        raise Refusal("unsupported-toolchain")
    result = subprocess.run(["rustc", "--version", "--verbose"], stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=30)
    if result.returncode or len(result.stdout) > 4096 or not re.search(rb"(?m)^release: 1\.88\.0$", result.stdout):
        raise Refusal("unsupported-compiler-identity")
    return hashlib.sha256(result.stdout).hexdigest()


def snapshot(root: Path, common: Path, compiler_identity: str) -> dict[str, str]:
    head = git("rev-parse", "--verify", "HEAD^{commit}", cwd=root).decode().strip()
    source, manifests = tracked_snapshot(root)
    input_closure(root, manifests)
    config = configuration(root, common, manifests)
    if git("rev-parse", "--verify", "HEAD^{commit}", cwd=root).decode().strip() != head:
        raise Refusal("head-changed-during-snapshot")
    return {
        "head": head,
        "identity": digest([str(root), str(common), head, source, config, compiler_identity,
                            hashlib.sha256(Path(__file__).read_bytes()).hexdigest()]),
    }


def invocation(args: list[str]) -> str:
    # Only compilation-related values; registry/credential variables are never
    # inspected. Values and application argv are hashed, never printed/stored.
    names = {"RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS", "CARGO_BUILD_RUSTFLAGS",
             "CARGO_BUILD_TARGET", "CARGO_FEATURE_PURE", "TZE_HUD_CHANNEL",
             "CC", "CXX", "AR", "CFLAGS", "CXXFLAGS", "LDFLAGS"}
    names.update(k for k in os.environ if re.fullmatch(r"CARGO_TARGET_[A-Z0-9_]+_(RUSTFLAGS|LINKER|RUNNER)", k))
    return digest([args, {k: os.environ[k] for k in sorted(names) if k in os.environ}])


def selected_target(root: Path, common: Path, args: list[str]) -> Path:
    value = os.environ.get("CARGO_TARGET_DIR", "")
    path = Path(value)
    if not value or not path.is_absolute() or path.is_symlink():
        raise Refusal("explicit-absolute-target-required")
    target = path.resolve()
    if not inside(target, Path.home().resolve()) or inside(target, root) or inside(root, target) or inside(target, common.parent):
        raise Refusal("unsafe-target-location")
    if os.environ.get("CARGO_BUILD_BUILD_DIR"):
        raise Refusal("unsupported-build-directory")
    if any(os.environ.get(name) for name in os.environ if name.startswith("CARGO_ALIAS_")):
        raise Refusal("unsupported-cargo-alias")
    if any(os.environ.get(name) for name in os.environ if re.fullmatch(r"CARGO_TARGET_[A-Z0-9_]+_RUNNER", name)):
        raise Refusal("unsupported-target-runner")
    command = next((arg for arg in args if not arg.startswith("-")), "")
    if command not in COMMANDS or (command == "fmt" and "--check" not in args):
        raise Refusal("unsupported-frozen-command")
    for offset, arg in enumerate(args):
        if arg == "--":
            break  # cargo run's application arguments are not Cargo overrides.
        if arg == "--config" or arg.startswith("--config="):
            raise Refusal("unsupported-cargo-config")
        for flag in ("--target-dir", "--manifest-path"):
            if arg == flag or arg.startswith(flag + "="):
                value = arg.split("=", 1)[1] if "=" in arg else args[offset + 1] if offset + 1 < len(args) else ""
                resolved = Path(value).resolve()
                if not value or (flag == "--target-dir" and resolved != target) or (flag == "--manifest-path" and not inside(resolved, root)):
                    raise Refusal("escaping-cargo-override")
                if flag == "--manifest-path":
                    name = resolved.relative_to(root).as_posix()
                    if resolved.name != "Cargo.toml" or name not in {
                        os.fsdecode(p) for p in git("ls-files", "-z", "--", name, cwd=root).split(b"\0") if p
                    }:
                        raise Refusal("untracked-manifest-override")
    return target


def read_binding(target: Path) -> dict | None:
    marker = target / MARKER
    if marker.exists() or marker.is_symlink():
        if marker.lstat().st_size > 4096:
            raise Refusal("invalid-binding")
        raw = regular_bytes(marker)
        if len(raw) > 4096:
            raise Refusal("invalid-binding")
        value = json.loads(raw)
        if (not isinstance(value, dict) or set(value) != {"schema", "head", "identity", "valid"}
                or type(value["schema"]) is not int or value["schema"] != SCHEMA
                or not isinstance(value["valid"], bool) or not isinstance(value["head"], str)
                or not isinstance(value["identity"], str)
                or not re.fullmatch(r"[0-9a-f]{40}|[0-9a-f]{64}", value["head"])
                or not re.fullmatch(r"[0-9a-f]{64}", value["identity"])):
            raise Refusal("invalid-binding")
        return value
    if target.exists() and any(target.iterdir()):
        raise Refusal("unbound-nonempty-target")
    return None


def write_binding(target: Path, source: dict[str, str], valid: bool) -> None:
    temporary = target / (MARKER + ".tmp")
    flags = os.O_WRONLY | os.O_CREAT | os.O_EXCL | getattr(os, "O_NOFOLLOW", 0)
    fd = os.open(temporary, flags, 0o664)
    try:
        with os.fdopen(fd, "w") as stream:
            json.dump({"schema": SCHEMA, **source, "valid": valid}, stream, sort_keys=True)
            stream.write("\n")
            stream.flush()
            os.fsync(stream.fileno())
        os.replace(temporary, target / MARKER)
    finally:
        # Only the temporary metadata file this invocation created, never Cargo
        # artifacts, unknown legacy files or another run's stale temporary file.
        temporary.unlink(missing_ok=True)


@contextmanager
def target_lock(target: Path) -> Iterator[None]:
    if (target / LOCK).is_symlink():
        raise Refusal("invalid-target-lock")
    flags = os.O_RDWR | os.O_CREAT | getattr(os, "O_NOFOLLOW", 0)
    fd = os.open(target / LOCK, flags, 0o664)
    with os.fdopen(fd, "r+b") as stream:
        if not stat.S_ISREG(os.fstat(stream.fileno()).st_mode) or os.fstat(stream.fileno()).st_nlink != 1:
            raise Refusal("invalid-target-lock")
        if os.name == "nt":
            import msvcrt
            if stream.seek(0, os.SEEK_END) == 0:
                stream.write(b"\0")
                stream.flush()
            stream.seek(0)
            msvcrt.locking(stream.fileno(), msvcrt.LK_NBLCK, 1)
        else:
            import fcntl
            fcntl.flock(stream.fileno(), fcntl.LOCK_EX | fcntl.LOCK_NB)
        try:
            yield
        finally:
            if os.name == "nt":
                stream.seek(0)
                msvcrt.locking(stream.fileno(), msvcrt.LK_UNLCK, 1)
            else:
                fcntl.flock(stream.fileno(), fcntl.LOCK_UN)


def run_cargo(args: list[str], root: Path) -> tuple[int, bool]:
    child = subprocess.Popen(["cargo", *args], cwd=root)
    interrupted = False

    def forward(number: int, _frame: object) -> None:
        nonlocal interrupted
        interrupted = True
        if child.poll() is None:
            child.send_signal(number)

    previous = {number: signal.signal(number, forward) for number in (signal.SIGINT, signal.SIGTERM)}
    try:
        code = child.wait()
    finally:
        for number, handler in previous.items():
            signal.signal(number, handler)
    return code, interrupted


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--tool-available", action="store_true")
    parser.add_argument("--refuse-frozen-helper", action="store_true")
    parser.add_argument("args", nargs=argparse.REMAINDER)
    options = parser.parse_args()
    args = options.args[1:] if options.args[:1] == ["--"] else options.args
    enabled = os.environ.get(CONTROL, "")
    if not enabled:
        if options.refuse_frozen_helper:
            return 0
        return subprocess.call(["cargo", *args])
    try:
        if enabled != "1":
            raise Refusal("invalid-opt-in-control")
        if options.refuse_frozen_helper:
            raise Refusal("unsupported-frozen-helper")
        if options.tool_available and args != ["deny", "--version"]:
            raise Refusal("unsupported-tool-probe")
        root, common = project_context()
        target = selected_target(root, common, args)
        compiler_identity = compiler()
        source = snapshot(root, common, compiler_identity)
        if snapshot(root, common, compiler_identity) != source:
            raise Refusal("input-changed-during-snapshot")
        binding = read_binding(target)
        if binding and (not binding["valid"] or any(binding[k] != source[k] for k in source)):
            raise Refusal("target-source-mismatch")
        # Unknown nonempty targets were refused before any local metadata write.
        target.mkdir(parents=True, exist_ok=True)
        with target_lock(target):
            binding = read_binding(target) if binding is not None else None
            if binding and (not binding["valid"] or any(binding[k] != source[k] for k in source)):
                raise Refusal("target-source-mismatch")
            if binding is None and any(p.name != LOCK for p in target.iterdir()):
                raise Refusal("unbound-nonempty-target")
            # Invalid before spawn: termination cannot leave a certified target.
            write_binding(target, source, False)
            print(f"source-guard: admitted head={source['head']} identity={source['identity']} invocation={invocation(args)}", file=sys.stderr)
            code, interrupted = run_cargo(args, root)
            try:
                abnormal = code < 0 or (os.name == "nt" and code >= 0x80000000)
                if interrupted or abnormal or compiler() != compiler_identity or snapshot(root, common, compiler_identity) != source:
                    raise Refusal("source-integrity-postcheck")
                write_binding(target, source, True)
            except (Refusal, OSError, ValueError, subprocess.SubprocessError):
                print(f"source-guard: postcheck-failed child_exit={code}", file=sys.stderr)
                return REFUSED if options.tool_available or code == 0 else 128 - code if code < 0 else code
            print(f"source-guard: complete child_exit={code}", file=sys.stderr)
            if options.tool_available:
                return 0 if code == 0 else 1
            return 128 - code if code < 0 else code
    except Refusal as error:
        print("source-guard: refused " + str(error), file=sys.stderr)
        return REFUSED
    except (OSError, ValueError, subprocess.SubprocessError):
        print("source-guard: refused required-metadata-or-tool-unavailable", file=sys.stderr)
        return REFUSED


if __name__ == "__main__":
    sys.exit(main())
