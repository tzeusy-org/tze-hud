#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.9"
# dependencies = []
# ///
"""Deterministic CLI for the tze_hud `portal_projection_*` MCP facade.

One subcommand per projection operation. Ownership is the caller's agent
identity (the bearer PSK): the runtime keeps the projection owner token
server-side, so this client never sends or stores one.

The client also retains a bounded tail of content authored through `publish`
under `~/.local/state/tze_hud/portal-continuity/<projection_id>.json` (override
with PORTAL_CONTINUITY_DIR). Attach reuses the original idempotency key, rotates
server-held ownership, and replays that tail before returning. Continuity files
never contain viewer-authored HUD input.

Environment:
  HUD_MCP_URL   MCP endpoint, with or without the /mcp suffix (required).
  HUD_PSK       bearer PSK (your agent's PSK); falls back to MCP_TEST_PSK,
                HUD_MCP_PSK, TZE_HUD_PSK (required via one of them).
  Resolve both with:  eval "$(.claude/skills/user-test/scripts/tzehouse_env.sh)"
  (or hud_vm_env.sh for the autonomous VM testhost).

Dialect: the runtime supports standard MCP `tools/call` as the primary wire
shape. This client uses it first and falls back to the legacy bare-method
dialect only when an older server reports `tools/call` as method-not-found.

Subcommands:
  list     (bounded, caller-scoped projection summaries)
  attach   --projection-id ID [--display-name S] [--provider-kind claude]
           [--workspace-hint S] [--repository-hint S] [--icon-profile S]
           [--classification private] [--idempotency-key S]
  publish  --projection-id ID (--text S | --text-file F | -)  [--kind assistant]
           [--coalesce-key S] [--logical-unit-id S]
  status   --projection-id ID --state active [--text S]
  poll     --projection-id ID [--wait-ms 30000] [--rounds 1] [--max-items 4]
           [--max-bytes 4096] [--ack handled|deferred|none] [--ack-message S]
           Prints one JSON object per received input item (NDJSON).
           Exit 0 = items received, 3 = no items (deterministic signal).
  ack      --projection-id ID --input-id I --state handled|deferred|rejected
           [--message S] [--not-before-us N]
  detach   --projection-id ID [--reason S]
  cleanup  --projection-id ID [--reason S]
  continuity-path --projection-id ID           (prints the continuity file path)
  continuity-clear --projection-id ID          (deletes local continuity state)

Exit codes: 0 success · 1 transport/config error · 2 operation rejected ·
3 poll returned no items.
"""

import argparse
import contextlib
import errno
import hashlib
import json
import os
import re
import secrets
import sys
import time
import urllib.error
import urllib.request

if os.name == "nt":
    import msvcrt
else:
    import fcntl

STATE_ROOT = os.path.join(
    os.environ.get("XDG_STATE_HOME") or os.path.expanduser("~/.local/state"),
    "tze_hud",
)
CONTINUITY_DIR = os.environ.get("PORTAL_CONTINUITY_DIR") or os.path.join(
    STATE_ROOT,
    "portal-continuity",
)
WINDOWS_LOCK_RETRY_SECONDS = 0.05
CONTINUITY_VERSION = 1
CONTINUITY_MAX_ITEMS = 64
CONTINUITY_MAX_BYTES = 64 * 1024
CONTINUITY_MAX_QUARANTINES = 4
CONTINUITY_RECORD_KEYS = frozenset(
    {
        "output_text",
        "output_kind",
        "content_classification",
        "logical_unit_id",
        "coalesce_key",
    }
)
CONTINUITY_REQUIRED_RECORD_KEYS = frozenset(
    {
        "output_text",
        "output_kind",
        "content_classification",
        "logical_unit_id",
    }
)
CONTINUITY_STATE_KEYS = frozenset({"version", "idempotency_key", "records"})
OUTPUT_KINDS = frozenset({"assistant", "tool", "status", "error", "other"})

# Projection IDs become continuity filenames; reject anything not
# filename-safe BEFORE any RPC (`..`/`/` can never escape the state directory).
PROJECTION_ID_SAFE = re.compile(r"^[A-Za-z0-9][A-Za-z0-9._-]{0,127}$")


class PortalClientExit(SystemExit):
    """CLI exit carrying whether the attempted operation was definitely rejected."""

    def __init__(self, code, *, definitive_rejection=False):
        super().__init__(code)
        self.definitive_rejection = definitive_rejection


def die(msg, code=1, *, definitive_rejection=False):
    print(f"portal_client: ERROR — {msg}", file=sys.stderr)
    raise PortalClientExit(code, definitive_rejection=definitive_rejection)


def mcp_url():
    url = os.environ.get("HUD_MCP_URL") or die(
        "HUD_MCP_URL not set (eval tzehouse_env.sh or hud_vm_env.sh first)"
    )
    return url if url.rstrip("/").endswith("/mcp") else url.rstrip("/") + "/mcp"


def psk():
    for var in (
        "HUD_PSK",
        "MCP_TEST_PSK",
        "HUD_MCP_PSK",
        "TZE_HUD_PSK",
    ):
        if os.environ.get(var):
            return os.environ[var]
    die(
        "no PSK in env (HUD_PSK / MCP_TEST_PSK / HUD_MCP_PSK / TZE_HUD_PSK)"
    )


def continuity_path(projection_id):
    return os.path.join(CONTINUITY_DIR, f"{projection_id}.json")


def continuity_lock_path(projection_id):
    return os.path.join(CONTINUITY_DIR, f"{projection_id}.lock")


def _ensure_private_dir(path):
    os.makedirs(path, mode=0o700, exist_ok=True)
    os.chmod(path, 0o700)


def _atomic_write_private(path, text):
    """Replace a private state file without exposing a partial write."""
    directory = os.path.dirname(path)
    _ensure_private_dir(directory)
    temporary = f"{path}.tmp-{os.getpid()}-{secrets.token_hex(4)}"
    fd = None
    try:
        fd = os.open(temporary, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
        with os.fdopen(fd, "w", encoding="utf-8") as stream:
            fd = None
            stream.write(text)
            stream.flush()
            os.fsync(stream.fileno())
        os.chmod(temporary, 0o600)
        os.replace(temporary, path)
        os.chmod(path, 0o600)
        try:
            directory_fd = os.open(directory, os.O_RDONLY)
            try:
                os.fsync(directory_fd)
            finally:
                os.close(directory_fd)
        except OSError:
            # Not every filesystem permits fsync on a directory. The file
            # replacement remains atomic and the file itself is already synced.
            pass
    except BaseException:
        if fd is not None:
            os.close(fd)
        try:
            os.remove(temporary)
        except FileNotFoundError:
            pass
        raise


def _acquire_windows_lock(fd, locking_api, sleep=time.sleep):
    """Block until the one-byte Windows lock is acquired or interrupted."""
    while True:
        try:
            locking_api.locking(fd, locking_api.LK_NBLCK, 1)
            return
        except OSError as error:
            if error.errno != errno.EACCES:
                raise
            sleep(WINDOWS_LOCK_RETRY_SECONDS)


@contextlib.contextmanager
def continuity_lock(projection_id):
    """Serialize one projection's continuity transaction across CLI processes."""
    _ensure_private_dir(CONTINUITY_DIR)
    path = continuity_lock_path(projection_id)
    fd = os.open(path, os.O_RDWR | os.O_CREAT, 0o600)
    os.chmod(path, 0o600)
    locked = False
    try:
        if os.name == "nt":
            if os.fstat(fd).st_size == 0:
                os.write(fd, b"\0")
                os.fsync(fd)
            os.lseek(fd, 0, os.SEEK_SET)
            _acquire_windows_lock(fd, msvcrt)
        else:
            fcntl.flock(fd, fcntl.LOCK_EX)
        locked = True
        yield
    finally:
        if locked:
            if os.name == "nt":
                os.lseek(fd, 0, os.SEEK_SET)
                msvcrt.locking(fd, msvcrt.LK_UNLCK, 1)
            else:
                fcntl.flock(fd, fcntl.LOCK_UN)
        os.close(fd)


def empty_continuity():
    return {
        "version": CONTINUITY_VERSION,
        "idempotency_key": None,
        "records": [],
    }


def record_size(record):
    encoded = json.dumps(
        record,
        ensure_ascii=False,
        separators=(",", ":"),
        sort_keys=True,
    ).encode("utf-8")
    return len(encoded)


def bound_records(
    records, *, max_items=CONTINUITY_MAX_ITEMS, max_bytes=CONTINUITY_MAX_BYTES
):
    """Keep the newest deterministic item/UTF-8-byte bounded record tail."""
    retained = [dict(record) for record in records]
    while len(retained) > max_items:
        retained.pop(0)
    total_bytes = sum(record_size(record) for record in retained)
    while retained and total_bytes > max_bytes:
        total_bytes -= record_size(retained.pop(0))
    return retained


def retain_record(state, record):
    """Return state with an authored record appended or coalesced in place."""
    records = [dict(item) for item in state.get("records", [])]
    logical_unit_id = record["logical_unit_id"]
    if any(item["logical_unit_id"] == logical_unit_id for item in records):
        # The authority treats a repeated logical-unit identity as an accepted
        # no-op, even if the retry's payload differs. Mirror that behavior in
        # the durable tail by preserving the first authoritative record once.
        return {
            "version": CONTINUITY_VERSION,
            "idempotency_key": state.get("idempotency_key"),
            "records": bound_records(records),
        }
    if coalesce_key := record.get("coalesce_key"):
        matching = [
            index
            for index, item in enumerate(records)
            if item.get("coalesce_key") == coalesce_key
        ]
        if matching:
            records[matching[0]] = dict(record)
            for index in reversed(matching[1:]):
                records.pop(index)
        else:
            records.append(dict(record))
    else:
        records.append(dict(record))
    return {
        "version": CONTINUITY_VERSION,
        "idempotency_key": state.get("idempotency_key"),
        "records": bound_records(records),
    }


def _valid_record(record):
    if not isinstance(record, dict):
        return False
    keys = frozenset(record)
    if not CONTINUITY_REQUIRED_RECORD_KEYS.issubset(keys):
        return False
    if not keys.issubset(CONTINUITY_RECORD_KEYS):
        return False
    for key in CONTINUITY_REQUIRED_RECORD_KEYS:
        if not isinstance(record.get(key), str):
            return False
    if record["output_kind"] not in OUTPUT_KINDS:
        return False
    if not record["logical_unit_id"] or not record["content_classification"]:
        return False
    if "coalesce_key" in record and not isinstance(record["coalesce_key"], str):
        return False
    return True


def _valid_continuity(state):
    if not isinstance(state, dict) or frozenset(state) != CONTINUITY_STATE_KEYS:
        return False
    if state.get("version") != CONTINUITY_VERSION:
        return False
    key = state.get("idempotency_key")
    if key is not None and (not isinstance(key, str) or not key):
        return False
    records = state.get("records")
    return isinstance(records, list) and all(
        _valid_record(record) for record in records
    )


def _quarantine_candidates(path):
    directory = os.path.dirname(path)
    prefix = os.path.basename(path) + ".corrupt"
    try:
        names = [
            name
            for name in os.listdir(directory)
            if name == prefix or name.startswith(prefix + ".")
        ]
    except FileNotFoundError:
        return []
    return [os.path.join(directory, name) for name in names]


def _quarantine_corrupt_continuity(path, reason):
    """Replace corrupt state with bounded metadata, never its private payload."""
    digest = hashlib.sha256()
    byte_length = 0
    try:
        with open(path, "rb") as stream:
            while chunk := stream.read(64 * 1024):
                digest.update(chunk)
                byte_length += len(chunk)
    except FileNotFoundError:
        return
    except OSError as error:
        try:
            os.remove(path)
        except FileNotFoundError:
            pass
        print(
            f"portal_client: WARNING — discarded unreadable continuity state at {path}: {error}",
            file=sys.stderr,
        )
        return

    candidate = f"{path}.corrupt"
    suffix = 0
    while os.path.exists(candidate):
        suffix += 1
        candidate = f"{path}.corrupt.{suffix}"
    os.remove(path)
    metadata = {
        "byte_length": byte_length,
        "reason": reason,
        "sha256": digest.hexdigest(),
        "version": 1,
    }
    _atomic_write_private(
        candidate,
        json.dumps(metadata, separators=(",", ":"), sort_keys=True) + "\n",
    )
    quarantines = sorted(
        _quarantine_candidates(path),
        key=lambda item: (os.stat(item).st_mtime_ns, item),
    )
    for expired in quarantines[:-CONTINUITY_MAX_QUARANTINES]:
        os.remove(expired)
    print(
        f"portal_client: WARNING — quarantined sanitized continuity metadata at {candidate}",
        file=sys.stderr,
    )


def load_continuity(projection_id):
    path = continuity_path(projection_id)
    try:
        with open(path, encoding="utf-8") as stream:
            state = json.load(stream)
    except FileNotFoundError:
        return empty_continuity()
    except (json.JSONDecodeError, UnicodeDecodeError):
        _quarantine_corrupt_continuity(path, "invalid_json")
        return empty_continuity()
    except OSError as error:
        die(f"cannot read continuity state at {path}: {error}")
    if not _valid_continuity(state):
        _quarantine_corrupt_continuity(path, "invalid_schema")
        return empty_continuity()
    bounded = bound_records(state["records"])
    if bounded != state["records"]:
        state = {**state, "records": bounded}
        save_continuity(projection_id, state)
    return state


def save_continuity(projection_id, state):
    if not _valid_continuity(state):
        raise ValueError("refusing to persist invalid portal continuity state")
    bounded = {**state, "records": bound_records(state["records"])}
    payload = (
        json.dumps(
            bounded,
            ensure_ascii=False,
            separators=(",", ":"),
            sort_keys=True,
        )
        + "\n"
    )
    _atomic_write_private(continuity_path(projection_id), payload)


def clear_continuity(projection_id):
    """Delete local continuity and quarantined copies, never remote state."""
    path = continuity_path(projection_id)
    removed = False
    directory = os.path.dirname(path)
    prefix = os.path.basename(path) + ".corrupt"
    candidates = [path]
    try:
        candidates.extend(
            os.path.join(directory, name)
            for name in os.listdir(directory)
            if name == prefix or name.startswith(prefix + ".")
        )
    except FileNotFoundError:
        pass
    for candidate in candidates:
        try:
            os.remove(candidate)
            removed = True
        except FileNotFoundError:
            pass
    return removed


def new_logical_unit_id(_projection_id):
    return f"client-{time.time_ns():x}-{secrets.token_hex(12)}"


def redact(node):
    """Defensively strip any owner_token before a response is printed."""
    if isinstance(node, dict):
        if "owner_token" in node:
            node["owner_token"] = "<REDACTED>"
        for v in node.values():
            redact(v)
    elif isinstance(node, list):
        for v in node:
            redact(v)
    return node


def rpc(method, params):
    body = json.dumps(
        {"jsonrpc": "2.0", "id": 1, "method": method, "params": params}
    ).encode()
    req = urllib.request.Request(
        mcp_url(),
        data=body,
        headers={
            "Authorization": f"Bearer {psk()}",
            "Content-Type": "application/json",
            "Accept": "application/json, text/event-stream",
        },
    )
    try:
        with urllib.request.urlopen(req, timeout=60) as resp:
            raw = resp.read().decode("utf-8", errors="replace")
            ctype = resp.headers.get("Content-Type", "")
    except urllib.error.HTTPError as e:
        definitive_rejection = 400 <= e.code < 500 and e.code not in {408, 425}
        die(
            f"HTTP {e.code} from {mcp_url()}: {e.read().decode('utf-8', errors='replace')[:400]}",
            definitive_rejection=definitive_rejection,
        )
    except urllib.error.URLError as e:
        die(f"cannot reach {mcp_url()}: {e.reason}")
    if "text/event-stream" in ctype:
        lines = [
            line[5:].strip() for line in raw.splitlines() if line.startswith("data:")
        ]
        raw = lines[-1] if lines else "{}"
    try:
        return json.loads(raw)
    except json.JSONDecodeError:
        die(f"malformed JSON from {mcp_url()}: {raw[:400]!r}")


def call_tool(tool, args, *, include_contract_metadata=True):
    """Use standard MCP tools/call first; retain bare-method compatibility."""
    if include_contract_metadata:
        args.setdefault("client_timestamp_wall_us", int(time.time() * 1_000_000))
        args.setdefault("request_id", f"req-{tool}-{int(time.time() * 1000)}")
    resp = rpc("tools/call", {"name": tool, "arguments": args})
    if (resp.get("error") or {}).get("code") == -32601:
        return rpc(tool, args)
    tools_call_result = resp.get("result", {})
    content = tools_call_result.get("content")
    if isinstance(content, list) and content and content[0].get("type") == "text":
        text = content[0]["text"]
        if tools_call_result.get("isError") is True:
            return {
                "jsonrpc": "2.0",
                "error": {"code": -32000, "message": text},
                "id": resp.get("id"),
            }
        resp = {"jsonrpc": "2.0", "result": json.loads(text), "id": resp.get("id")}
    return resp


def result_or_die(resp):
    if resp.get("error") is not None:
        print(json.dumps(redact(resp), indent=2))
        raise PortalClientExit(2, definitive_rejection=True)
    result = resp.get("result") or {}
    if result.get("accepted") is False:
        print(json.dumps(redact(result), indent=2))
        raise PortalClientExit(2, definitive_rejection=True)
    return result


def emit(result):
    print(json.dumps(redact(result), indent=2))


def _idempotency_key_for_attach(projection_id, requested, continuity):
    retained = continuity.get("idempotency_key")
    if retained and requested and requested != retained:
        die(
            "the supplied idempotency key differs from retained continuity state — "
            "reuse the original key or run continuity-clear explicitly",
            2,
        )
    if requested or retained:
        return requested or retained
    date = time.strftime("%Y%m%d")
    candidate = f"{projection_id}-{date}"
    if len(candidate.encode("utf-8")) <= 128:
        return candidate
    projection_hash = hashlib.sha256(projection_id.encode("utf-8")).hexdigest()[:32]
    return f"client-{projection_hash}-{date}"


def replay_continuity(projection_id, continuity):
    """Replay only locally authored records with their stable identity keys."""
    replayed = 0
    for record in continuity["records"]:
        args = {
            "operation": "publish_output",
            "projection_id": projection_id,
            **record,
        }
        result_or_die(call_tool("portal_projection_publish", args))
        replayed += 1
    return replayed


def cmd_attach(a):
    with continuity_lock(a.projection_id):
        _cmd_attach_locked(a)


def cmd_list(_a):
    """List only the caller's bounded, content-free summaries."""
    emit(
        result_or_die(
            call_tool(
                "portal_projection_list", {}, include_contract_metadata=False
            )
        )
    )


def _cmd_attach_locked(a):
    continuity = load_continuity(a.projection_id)
    idempotency_key = _idempotency_key_for_attach(
        a.projection_id, a.idempotency_key, continuity
    )
    args = {
        "operation": "attach",
        "projection_id": a.projection_id,
        "provider_kind": a.provider_kind,
        "display_name": a.display_name or a.projection_id,
        "content_classification": a.classification,
        "hud_target": "default",
        "idempotency_key": idempotency_key,
    }
    if a.workspace_hint:
        args["workspace_hint"] = a.workspace_hint
    if a.repository_hint:
        args["repository_hint"] = a.repository_hint
    if a.icon_profile:
        args["icon_profile_hint"] = a.icon_profile
    resp = call_tool("portal_projection_attach", args)
    result = result_or_die(resp)
    continuity["idempotency_key"] = idempotency_key
    save_continuity(a.projection_id, continuity)
    result["continuity_replayed_count"] = replay_continuity(a.projection_id, continuity)
    result["continuity_file"] = continuity_path(a.projection_id)
    emit(result)


def cmd_publish(a):
    with continuity_lock(a.projection_id):
        _cmd_publish_locked(a)


def _cmd_publish_locked(a):
    if a.text is not None:
        text = a.text
    elif a.text_file == "-":
        text = sys.stdin.read()
    elif a.text_file:
        with open(a.text_file, encoding="utf-8", errors="replace") as f:
            text = f.read()
    else:
        die("provide --text or --text-file (use '-' for stdin)")
    logical_unit_id = a.logical_unit_id or new_logical_unit_id(a.projection_id)
    record = {
        "output_text": text,
        "output_kind": a.kind,
        "content_classification": a.classification,
        "logical_unit_id": logical_unit_id,
    }
    if a.coalesce_key:
        record["coalesce_key"] = a.coalesce_key
    args = {
        "operation": "publish_output",
        "projection_id": a.projection_id,
        **record,
    }
    previous_exists = os.path.exists(continuity_path(a.projection_id))
    previous = load_continuity(a.projection_id)
    prepared = retain_record(previous, record)
    save_continuity(a.projection_id, prepared)
    try:
        result = result_or_die(call_tool("portal_projection_publish", args))
    except SystemExit as error:
        definitive_rejection = getattr(error, "definitive_rejection", error.code == 2)
        if definitive_rejection:
            if previous_exists:
                save_continuity(a.projection_id, previous)
            else:
                clear_continuity(a.projection_id)
        else:
            print(
                "portal_client: continuity retained after an ambiguous outcome; "
                f"run attach to replay logical_unit_id={logical_unit_id!r} safely",
                file=sys.stderr,
            )
        raise
    emit(result)


def cmd_status(a):
    args = {
        "operation": "publish_status",
        "projection_id": a.projection_id,
        "lifecycle_state": a.state,
    }
    if a.text:
        args["status_text"] = a.text
    emit(result_or_die(call_tool("portal_projection_publish_status", args)))


def do_ack(projection_id, input_id, state, message, not_before_us=None):
    args = {
        "operation": "acknowledge_input",
        "projection_id": projection_id,
        "input_id": input_id,
        "ack_state": state,
    }
    if message:
        args["ack_message"] = message
    if not_before_us:
        args["not_before_wall_us"] = not_before_us
    return result_or_die(call_tool("portal_projection_acknowledge_input", args))


def cmd_poll(a):
    got = []
    for _ in range(a.rounds):
        result = result_or_die(
            call_tool(
                "portal_projection_get_pending_input",
                {
                    "operation": "get_pending_input",
                    "projection_id": a.projection_id,
                    "max_items": a.max_items,
                    "max_bytes": a.max_bytes,
                    "wait_ms": min(a.wait_ms, 30000),
                },
            )
        )
        items = result.get("items") or []
        for item in items:
            if a.ack != "none":
                item["ack"] = redact(
                    do_ack(a.projection_id, item["input_id"], a.ack, a.ack_message)
                )
            print(json.dumps(redact(item)))
        got.extend(items)
        if items:
            break
    if not got:
        print("portal_client: no pending input", file=sys.stderr)
        sys.exit(3)


def cmd_ack(a):
    emit(do_ack(a.projection_id, a.input_id, a.state, a.message, a.not_before_us))


def cmd_continuity_clear(a):
    with continuity_lock(a.projection_id):
        path = continuity_path(a.projection_id)
        emit({"continuity_file": path, "removed": clear_continuity(a.projection_id)})


def _terminal(a, op, tool, extra=None):
    args = {
        "operation": op,
        "projection_id": a.projection_id,
        "reason": a.reason,
    }
    args.update(extra or {})
    emit(result_or_die(call_tool(tool, args)))


def main():
    p = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter
    )
    sub = p.add_subparsers(dest="cmd", required=True)

    def base(sp):
        sp.add_argument("--projection-id", required=True)
        return sp

    sp = sub.add_parser("list")
    sp.set_defaults(fn=cmd_list)

    sp = base(sub.add_parser("attach"))
    sp.add_argument("--display-name")
    sp.add_argument(
        "--provider-kind",
        default="claude",
        choices=["claude", "codex", "opencode", "other"],
    )
    sp.add_argument("--workspace-hint")
    sp.add_argument("--repository-hint")
    sp.add_argument("--icon-profile")
    sp.add_argument("--classification", default="private")
    sp.add_argument("--idempotency-key")
    sp.set_defaults(fn=cmd_attach)

    sp = base(sub.add_parser("publish"))
    sp.add_argument("--text")
    sp.add_argument("--text-file")
    sp.add_argument(
        "--kind",
        default="assistant",
        choices=["assistant", "tool", "status", "error", "other"],
    )
    sp.add_argument("--classification", default="private")
    sp.add_argument("--coalesce-key")
    sp.add_argument("--logical-unit-id")
    sp.set_defaults(fn=cmd_publish)

    sp = base(sub.add_parser("status"))
    sp.add_argument(
        "--state",
        required=True,
        choices=[
            "attached",
            "active",
            "degraded",
            "hud_unavailable",
            "detached",
            "cleanup_pending",
            "expired",
        ],
    )
    sp.add_argument("--text")
    sp.set_defaults(fn=cmd_status)

    sp = base(sub.add_parser("poll"))
    sp.add_argument("--wait-ms", type=int, default=30000)
    sp.add_argument("--rounds", type=int, default=1)
    sp.add_argument("--max-items", type=int, default=4)
    sp.add_argument("--max-bytes", type=int, default=4096)
    sp.add_argument("--ack", default="none", choices=["none", "handled", "deferred"])
    sp.add_argument("--ack-message", default="received")
    sp.set_defaults(fn=cmd_poll)

    sp = base(sub.add_parser("ack"))
    sp.add_argument("--input-id", required=True)
    sp.add_argument(
        "--state", required=True, choices=["handled", "deferred", "rejected"]
    )
    sp.add_argument("--message")
    sp.add_argument("--not-before-us", type=int)
    sp.set_defaults(fn=cmd_ack)

    sp = base(sub.add_parser("detach"))
    sp.add_argument("--reason", default="session complete")
    sp.set_defaults(fn=lambda a: _terminal(a, "detach", "portal_projection_detach"))

    sp = base(sub.add_parser("cleanup"))
    sp.add_argument("--reason", default="remove stale portal")
    # The MCP cleanup handler requires cleanup_authority; this client acts
    # with owner authority (the runtime supplies the server-held token)
    # (operator cleanup uses separate daemon authority, out of scope here).
    sp.set_defaults(
        fn=lambda a: _terminal(
            a, "cleanup", "portal_projection_cleanup", {"cleanup_authority": "owner"}
        )
    )

    sp = base(sub.add_parser("continuity-path"))
    sp.set_defaults(fn=lambda a: print(continuity_path(a.projection_id)))

    sp = base(sub.add_parser("continuity-clear"))
    sp.set_defaults(fn=cmd_continuity_clear)

    args = p.parse_args()
    if getattr(args, "projection_id", None) and not PROJECTION_ID_SAFE.match(
        args.projection_id
    ):
        die(
            f"unsafe projection id {args.projection_id!r} — must match {PROJECTION_ID_SAFE.pattern} "
            "(it becomes the continuity filename)"
        )
    args.fn(args)


if __name__ == "__main__":
    main()
