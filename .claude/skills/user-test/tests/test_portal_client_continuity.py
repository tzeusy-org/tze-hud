"""Contract tests for client-owned portal continuity replay."""

from __future__ import annotations

import importlib.util
import errno
import hashlib
import io
import json
import multiprocessing
import os
import stat
import time
from pathlib import Path
from types import SimpleNamespace
from unittest import mock

import pytest


CLIENT_PATH = (
    Path(__file__).parents[2] / "hud-projection" / "scripts" / "portal_client.py"
)
SPEC = importlib.util.spec_from_file_location("portal_client_continuity", CLIENT_PATH)
assert SPEC is not None and SPEC.loader is not None
portal_client = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(portal_client)


def attach_args(*, idempotency_key: str | None = None) -> SimpleNamespace:
    return SimpleNamespace(
        projection_id="continuity-session",
        provider_kind="codex",
        display_name="Continuity Session",
        classification="private",
        idempotency_key=idempotency_key,
        workspace_hint=None,
        repository_hint=None,
        icon_profile=None,
    )


def publish_args(
    text: str,
    *,
    kind: str = "assistant",
    classification: str = "private",
    logical_unit_id: str | None = None,
    coalesce_key: str | None = None,
) -> SimpleNamespace:
    return SimpleNamespace(
        projection_id="continuity-session",
        text=text,
        text_file=None,
        kind=kind,
        classification=classification,
        logical_unit_id=logical_unit_id,
        coalesce_key=coalesce_key,
    )


@pytest.fixture(autouse=True)
def private_state_dirs(tmp_path: Path) -> None:
    portal_client.CONTINUITY_DIR = str(tmp_path / "continuity")


def record(unit: str, text: str, *, coalesce_key: str | None = None) -> dict:
    item = {
        "output_text": text,
        "output_kind": "assistant",
        "content_classification": "private",
        "logical_unit_id": unit,
    }
    if coalesce_key is not None:
        item["coalesce_key"] = coalesce_key
    return item


def _publish_worker(
    text: str,
    logical_unit_id: str,
    entered: multiprocessing.synchronize.Event | None = None,
    release: multiprocessing.synchronize.Event | None = None,
    rejected: bool = False,
) -> None:
    def fake_call(_tool: str, _args: dict) -> dict:
        if entered is not None:
            entered.set()
        if release is not None and not release.wait(timeout=10):
            raise TimeoutError("publish worker release timed out")
        if rejected:
            return {"error": {"code": -32109, "message": "state conflict"}}
        return {"result": {"accepted": True}}

    with (
        mock.patch.object(portal_client, "call_tool", side_effect=fake_call),
        mock.patch.object(
            portal_client, "new_logical_unit_id", return_value=logical_unit_id
        ),
        mock.patch.object(portal_client, "emit"),
    ):
        try:
            portal_client.cmd_publish(publish_args(text))
        except SystemExit:
            if not rejected:
                raise


def _attach_worker(
    entered: multiprocessing.synchronize.Event,
    release: multiprocessing.synchronize.Event,
) -> None:
    def fake_call(tool: str, _args: dict) -> dict:
        if tool == "portal_projection_attach":
            return {"result": {"accepted": True, "owner_token": "rotated-token"}}
        if tool == "portal_projection_publish":
            entered.set()
            if not release.wait(timeout=10):
                raise TimeoutError("attach replay release timed out")
            return {"result": {"accepted": True}}
        raise AssertionError(tool)

    with (
        mock.patch.object(portal_client, "call_tool", side_effect=fake_call),
        mock.patch.object(portal_client, "emit"),
    ):
        portal_client.cmd_attach(attach_args())


def _clear_worker(
    entered: multiprocessing.synchronize.Event,
    release: multiprocessing.synchronize.Event,
) -> None:
    original_clear = portal_client.clear_continuity

    def blocking_clear(projection_id: str) -> bool:
        entered.set()
        if not release.wait(timeout=10):
            raise TimeoutError("clear release timed out")
        return original_clear(projection_id)

    with (
        mock.patch.object(
            portal_client, "clear_continuity", side_effect=blocking_clear
        ),
        mock.patch.object(portal_client, "emit"),
    ):
        portal_client.cmd_continuity_clear(
            SimpleNamespace(projection_id="continuity-session")
        )


def _join_workers(*workers: multiprocessing.Process) -> None:
    for worker in workers:
        worker.join(timeout=10)
        assert not worker.is_alive(), "continuity worker did not exit"
        assert worker.exitcode == 0


def test_continuity_state_is_private_and_atomically_replaced() -> None:
    state = {
        "version": 1,
        "idempotency_key": "stable-attach-key",
        "records": [record("turn-1", "hello")],
    }

    real_replace = os.replace
    with mock.patch.object(os, "replace", wraps=real_replace) as replace:
        portal_client.save_continuity("continuity-session", state)

    path = Path(portal_client.continuity_path("continuity-session"))
    assert stat.S_IMODE(path.parent.stat().st_mode) == 0o700
    assert stat.S_IMODE(path.stat().st_mode) == 0o600
    assert replace.call_count == 1
    assert replace.call_args.args[1] == str(path)
    assert not list(path.parent.glob("*.tmp-*"))


def test_rolling_tail_enforces_item_and_canonical_utf8_byte_bounds() -> None:
    records = [record(f"turn-{index}", "é" * index) for index in range(1, 5)]

    item_bounded = portal_client.bound_records(
        records, max_items=3, max_bytes=1_000_000
    )
    assert [item["logical_unit_id"] for item in item_bounded] == [
        "turn-2",
        "turn-3",
        "turn-4",
    ]

    last_two_bytes = sum(portal_client.record_size(item) for item in records[-2:])
    byte_bounded = portal_client.bound_records(
        records, max_items=10, max_bytes=last_two_bytes
    )
    assert [item["logical_unit_id"] for item in byte_bounded] == [
        "turn-3",
        "turn-4",
    ]
    assert (
        sum(portal_client.record_size(item) for item in byte_bounded) <= last_two_bytes
    )


def test_generated_identity_keys_respect_the_128_byte_wire_contract() -> None:
    projection_id = "p" * 128

    logical_unit_id = portal_client.new_logical_unit_id(projection_id)
    idempotency_key = portal_client._idempotency_key_for_attach(
        projection_id, None, portal_client.empty_continuity()
    )

    assert len(logical_unit_id.encode("utf-8")) <= 128
    assert len(idempotency_key.encode("utf-8")) <= 128


def test_same_coalesce_key_replaces_local_tail_entry_in_place() -> None:
    state = {
        "version": 1,
        "idempotency_key": "stable-attach-key",
        "records": [
            record("turn-1", "first"),
            record("progress-1", "10%", coalesce_key="progress"),
            record("turn-2", "second"),
        ],
    }

    updated = portal_client.retain_record(
        state, record("progress-2", "80%", coalesce_key="progress")
    )

    assert [item["logical_unit_id"] for item in updated["records"]] == [
        "turn-1",
        "progress-2",
        "turn-2",
    ]


def test_repeated_logical_unit_id_preserves_original_record_once() -> None:
    original = record("turn-1", "first payload")
    state = {
        "version": 1,
        "idempotency_key": "stable-attach-key",
        "records": [original],
    }

    updated = portal_client.retain_record(
        state,
        record("turn-1", "retry payload must remain an authority no-op"),
    )

    assert updated["records"] == [original]


def test_corrupt_state_is_quarantined_without_replaying_untrusted_content(
    capsys: pytest.CaptureFixture[str],
) -> None:
    path = Path(portal_client.continuity_path("continuity-session"))
    path.parent.mkdir(mode=0o700, parents=True)
    corrupt = '{"records":['
    path.write_text(corrupt, encoding="utf-8")
    path.chmod(0o600)

    loaded = portal_client.load_continuity("continuity-session")

    assert loaded == portal_client.empty_continuity()
    assert not path.exists()
    quarantined = path.with_suffix(path.suffix + ".corrupt")
    metadata = json.loads(quarantined.read_text(encoding="utf-8"))
    assert metadata == {
        "byte_length": len(corrupt.encode("utf-8")),
        "reason": "invalid_json",
        "sha256": hashlib.sha256(corrupt.encode("utf-8")).hexdigest(),
        "version": 1,
    }
    assert stat.S_IMODE(quarantined.stat().st_mode) == 0o600
    assert "quarantined sanitized continuity metadata" in capsys.readouterr().err


def test_corrupt_quarantine_count_is_bounded() -> None:
    path = Path(portal_client.continuity_path("continuity-session"))
    path.parent.mkdir(mode=0o700, parents=True)

    for index in range(portal_client.CONTINUITY_MAX_QUARANTINES + 3):
        path.write_text(f'{{"invalid":{index},"records":[', encoding="utf-8")
        portal_client.load_continuity("continuity-session")
        time.sleep(0.001)

    quarantines = list(path.parent.glob(path.name + ".corrupt*"))
    assert len(quarantines) == portal_client.CONTINUITY_MAX_QUARANTINES
    assert all(item.stat().st_size < 512 for item in quarantines)


def test_published_record_preserves_semantics_without_persisting_owner_token() -> None:
    response = {"result": {"accepted": True, "status_summary": "published"}}

    with (
        mock.patch.object(portal_client, "call_tool", return_value=response),
        mock.patch.object(portal_client, "emit"),
        mock.patch.object(
            portal_client, "new_logical_unit_id", return_value="stable-unit"
        ),
    ):
        portal_client.cmd_publish(
            publish_args(
                "authored text",
                kind="tool",
                classification="household",
                coalesce_key="tool-progress",
            )
        )

    path = Path(portal_client.continuity_path("continuity-session"))
    raw = path.read_text(encoding="utf-8")
    assert "owner-token-must-not-leak" not in raw
    assert portal_client.load_continuity("continuity-session")["records"] == [
        {
            "output_text": "authored text",
            "output_kind": "tool",
            "content_classification": "household",
            "logical_unit_id": "stable-unit",
            "coalesce_key": "tool-progress",
        }
    ]


def test_attach_reuses_original_key_and_double_replay_is_idempotent() -> None:
    portal_client.save_continuity(
        "continuity-session",
        {
            "version": 1,
            "idempotency_key": "original-key",
            "records": [record("turn-1", "one"), record("turn-2", "two")],
        },
    )
    applied: dict[str, dict] = {}
    attach_keys: list[str] = []

    def fake_call(tool: str, args: dict) -> dict:
        if tool == "portal_projection_attach":
            attach_keys.append(args["idempotency_key"])
            return {"result": {"accepted": True, "owner_token": "rotated-token"}}
        if tool == "portal_projection_publish":
            applied.setdefault(args["logical_unit_id"], args.copy())
            return {"result": {"accepted": True}}
        raise AssertionError(tool)

    with (
        mock.patch.object(portal_client, "call_tool", side_effect=fake_call),
        mock.patch.object(portal_client, "emit"),
    ):
        portal_client.cmd_attach(attach_args())
        portal_client.cmd_attach(attach_args())

    assert attach_keys == ["original-key", "original-key"]
    assert list(applied) == ["turn-1", "turn-2"]
    assert portal_client.load_continuity("continuity-session")["records"] == [
        record("turn-1", "one"),
        record("turn-2", "two"),
    ]


def test_fresh_runtime_attach_replays_authored_tail_with_original_semantics() -> None:
    retained = [
        record("turn-7", "answer"),
        {
            **record("progress-9", "done", coalesce_key="progress"),
            "output_kind": "status",
            "content_classification": "household",
        },
    ]
    portal_client.save_continuity(
        "continuity-session",
        {"version": 1, "idempotency_key": "original-key", "records": retained},
    )
    fresh_runtime: list[dict] = []

    def fake_call(tool: str, args: dict) -> dict:
        if tool == "portal_projection_attach":
            return {"result": {"accepted": True, "owner_token": "fresh-runtime-token"}}
        if tool == "portal_projection_publish":
            fresh_runtime.append(args.copy())
            return {"result": {"accepted": True}}
        raise AssertionError(tool)

    with (
        mock.patch.object(portal_client, "call_tool", side_effect=fake_call),
        mock.patch.object(portal_client, "emit"),
    ):
        portal_client.cmd_attach(attach_args())

    replayed = [
        {
            key: value
            for key, value in item.items()
            if key
            in {
                "output_text",
                "output_kind",
                "content_classification",
                "logical_unit_id",
                "coalesce_key",
            }
        }
        for item in fresh_runtime
    ]
    assert replayed == retained
    assert all("owner_token" not in item for item in fresh_runtime)


def test_rejected_publish_rolls_back_prepared_local_record() -> None:
    initial = {
        "version": 1,
        "idempotency_key": "original-key",
        "records": [record("turn-1", "committed")],
    }
    portal_client.save_continuity("continuity-session", initial)

    with (
        mock.patch.object(
            portal_client,
            "call_tool",
            return_value={"error": {"code": -32105, "message": "too large"}},
        ),
        mock.patch.object(portal_client, "new_logical_unit_id", return_value="turn-2"),
        pytest.raises(SystemExit) as exc,
    ):
        portal_client.cmd_publish(publish_args("rejected"))

    assert exc.value.code == 2
    assert portal_client.load_continuity("continuity-session") == initial


def test_ambiguous_transport_failure_retains_record_for_idempotent_replay(
    capsys: pytest.CaptureFixture[str],
) -> None:
    initial = {
        "version": 1,
        "idempotency_key": "original-key",
        "records": [record("turn-1", "committed")],
    }
    portal_client.save_continuity("continuity-session", initial)

    with (
        mock.patch.object(portal_client, "call_tool", side_effect=SystemExit(1)),
        mock.patch.object(portal_client, "new_logical_unit_id", return_value="turn-2"),
        pytest.raises(SystemExit) as exc,
    ):
        portal_client.cmd_publish(publish_args("acceptance-unknown"))

    assert exc.value.code == 1
    assert portal_client.load_continuity("continuity-session") == {
        **initial,
        "records": [
            record("turn-1", "committed"),
            record("turn-2", "acceptance-unknown"),
        ],
    }
    assert (
        "run attach to replay logical_unit_id='turn-2' safely"
        in capsys.readouterr().err
    )


def test_http_client_rejection_is_classified_as_definitive(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.setenv("HUD_MCP_URL", "http://127.0.0.1:9090/mcp")
    monkeypatch.setenv("HUD_PSK", "test-psk")
    rejection = portal_client.urllib.error.HTTPError(
        "http://127.0.0.1:9090/mcp",
        401,
        "Unauthorized",
        {},
        io.BytesIO(b"denied"),
    )

    with (
        mock.patch.object(
            portal_client.urllib.request, "urlopen", side_effect=rejection
        ),
        pytest.raises(portal_client.PortalClientExit) as exc,
    ):
        portal_client.rpc("tools/call", {})

    assert exc.value.code == 1
    assert exc.value.definitive_rejection is True


@pytest.mark.parametrize("status_code", [408, 425])
def test_transient_http_rejection_is_classified_as_ambiguous(
    monkeypatch: pytest.MonkeyPatch,
    status_code: int,
) -> None:
    monkeypatch.setenv("HUD_MCP_URL", "http://127.0.0.1:9090/mcp")
    monkeypatch.setenv("HUD_PSK", "test-psk")
    rejection = portal_client.urllib.error.HTTPError(
        "http://127.0.0.1:9090/mcp",
        status_code,
        "Transient rejection",
        {},
        io.BytesIO(b"retry later"),
    )

    with (
        mock.patch.object(
            portal_client.urllib.request, "urlopen", side_effect=rejection
        ),
        pytest.raises(portal_client.PortalClientExit) as exc,
    ):
        portal_client.rpc("tools/call", {})

    assert exc.value.code == 1
    assert exc.value.definitive_rejection is False


def test_windows_lock_retries_past_msvcrt_built_in_timeout() -> None:
    locking_api = SimpleNamespace(
        LK_NBLCK=2,
        locking=mock.Mock(
            side_effect=[
                PermissionError(13, "lock held"),
                PermissionError(13, "lock held"),
                None,
            ]
        ),
    )
    sleeps: list[float] = []

    portal_client._acquire_windows_lock(17, locking_api, sleeps.append)

    assert locking_api.locking.call_args_list == [
        mock.call(17, locking_api.LK_NBLCK, 1),
        mock.call(17, locking_api.LK_NBLCK, 1),
        mock.call(17, locking_api.LK_NBLCK, 1),
    ]
    assert sleeps == [portal_client.WINDOWS_LOCK_RETRY_SECONDS] * 2


@pytest.mark.parametrize(
    "error_number", [errno.EAGAIN, errno.EDEADLK, errno.EBADF, errno.EINVAL]
)
def test_windows_lock_does_not_mask_non_contention_errors(error_number: int) -> None:
    failure = OSError(error_number, "not a documented lock-contention result")
    locking_api = SimpleNamespace(
        LK_NBLCK=2,
        locking=mock.Mock(side_effect=[failure, AssertionError("retried")]),
    )

    with pytest.raises(OSError) as exc:
        portal_client._acquire_windows_lock(17, locking_api, mock.Mock())

    assert exc.value is failure
    locking_api.locking.assert_called_once_with(17, locking_api.LK_NBLCK, 1)


def test_windows_lock_wait_is_interruptible() -> None:
    locking_api = SimpleNamespace(
        LK_NBLCK=2,
        locking=mock.Mock(side_effect=PermissionError(errno.EACCES, "lock held")),
    )

    with pytest.raises(KeyboardInterrupt):
        portal_client._acquire_windows_lock(
            17, locking_api, mock.Mock(side_effect=KeyboardInterrupt)
        )


def test_definitive_http_rejection_rolls_back_prepared_record() -> None:
    initial = {
        "version": 1,
        "idempotency_key": "original-key",
        "records": [record("turn-1", "committed")],
    }
    portal_client.save_continuity("continuity-session", initial)
    rejection = portal_client.PortalClientExit(1, definitive_rejection=True)

    with (
        mock.patch.object(portal_client, "call_tool", side_effect=rejection),
        mock.patch.object(portal_client, "new_logical_unit_id", return_value="turn-2"),
        pytest.raises(SystemExit) as exc,
    ):
        portal_client.cmd_publish(publish_args("rejected-before-acceptance"))

    assert exc.value.code == 1
    assert portal_client.load_continuity("continuity-session") == initial


def test_failed_atomic_replace_preserves_previous_state() -> None:
    initial = {
        "version": 1,
        "idempotency_key": "original-key",
        "records": [record("turn-1", "committed")],
    }
    portal_client.save_continuity("continuity-session", initial)

    with (
        mock.patch.object(os, "replace", side_effect=OSError("disk unavailable")),
        pytest.raises(OSError, match="disk unavailable"),
    ):
        portal_client.save_continuity(
            "continuity-session",
            portal_client.retain_record(initial, record("turn-2", "new")),
        )

    assert portal_client.load_continuity("continuity-session") == initial
    assert not list(Path(portal_client.CONTINUITY_DIR).glob("*.tmp-*"))


def test_local_continuity_cleanup_is_explicit_and_idempotent() -> None:
    portal_client.save_continuity(
        "continuity-session",
        {
            "version": 1,
            "idempotency_key": "original-key",
            "records": [record("turn-1", "committed")],
        },
    )

    assert portal_client.clear_continuity("continuity-session") is True
    assert portal_client.clear_continuity("continuity-session") is False
    assert not Path(portal_client.continuity_path("continuity-session")).exists()


@pytest.mark.skipif(
    os.name == "nt", reason="fork barrier exercises POSIX process locks"
)
def test_concurrent_accepted_publishes_preserve_both_records() -> None:
    portal_client.save_continuity(
        "continuity-session",
        {
            "version": 1,
            "idempotency_key": "original-key",
            "records": [record("turn-0", "initial")],
        },
    )
    context = multiprocessing.get_context("fork")
    first_entered, first_release, second_entered = (
        context.Event(),
        context.Event(),
        context.Event(),
    )
    first = context.Process(
        target=_publish_worker,
        args=("first", "turn-a", first_entered, first_release, False),
    )
    second = context.Process(
        target=_publish_worker,
        args=("second", "turn-b", second_entered, None, False),
    )

    first.start()
    assert first_entered.wait(timeout=5)
    second.start()
    assert not second_entered.wait(timeout=0.25)
    first_release.set()
    _join_workers(first, second)

    assert [
        item["logical_unit_id"]
        for item in portal_client.load_continuity("continuity-session")["records"]
    ] == ["turn-0", "turn-a", "turn-b"]


@pytest.mark.skipif(
    os.name == "nt", reason="fork barrier exercises POSIX process locks"
)
def test_rejected_writer_rollback_cannot_erase_concurrent_accepted_publish() -> None:
    portal_client.save_continuity(
        "continuity-session",
        {
            "version": 1,
            "idempotency_key": "original-key",
            "records": [record("turn-0", "initial")],
        },
    )
    context = multiprocessing.get_context("fork")
    first_entered, first_release, second_entered = (
        context.Event(),
        context.Event(),
        context.Event(),
    )
    rejected = context.Process(
        target=_publish_worker,
        args=("rejected", "turn-a", first_entered, first_release, True),
    )
    accepted = context.Process(
        target=_publish_worker,
        args=("accepted", "turn-b", second_entered, None, False),
    )

    rejected.start()
    assert first_entered.wait(timeout=5)
    accepted.start()
    assert not second_entered.wait(timeout=0.25)
    first_release.set()
    _join_workers(rejected, accepted)

    assert [
        item["logical_unit_id"]
        for item in portal_client.load_continuity("continuity-session")["records"]
    ] == ["turn-0", "turn-b"]


@pytest.mark.skipif(
    os.name == "nt", reason="fork barrier exercises POSIX process locks"
)
def test_attach_replay_serializes_with_concurrent_publish() -> None:
    portal_client.save_continuity(
        "continuity-session",
        {
            "version": 1,
            "idempotency_key": "original-key",
            "records": [record("turn-0", "initial")],
        },
    )
    context = multiprocessing.get_context("fork")
    replay_entered, replay_release, publish_entered = (
        context.Event(),
        context.Event(),
        context.Event(),
    )
    attach = context.Process(
        target=_attach_worker, args=(replay_entered, replay_release)
    )
    publish = context.Process(
        target=_publish_worker,
        args=("after attach", "turn-b", publish_entered, None, False),
    )

    attach.start()
    assert replay_entered.wait(timeout=5)
    publish.start()
    assert not publish_entered.wait(timeout=0.25)
    replay_release.set()
    _join_workers(attach, publish)

    assert [
        item["logical_unit_id"]
        for item in portal_client.load_continuity("continuity-session")["records"]
    ] == ["turn-0", "turn-b"]


@pytest.mark.skipif(
    os.name == "nt", reason="fork barrier exercises POSIX process locks"
)
def test_clear_serializes_before_concurrent_publish() -> None:
    portal_client.save_continuity(
        "continuity-session",
        {
            "version": 1,
            "idempotency_key": "original-key",
            "records": [record("turn-0", "initial")],
        },
    )
    context = multiprocessing.get_context("fork")
    clear_entered, clear_release, publish_entered = (
        context.Event(),
        context.Event(),
        context.Event(),
    )
    clear = context.Process(target=_clear_worker, args=(clear_entered, clear_release))
    publish = context.Process(
        target=_publish_worker,
        args=("after clear", "turn-b", publish_entered, None, False),
    )

    clear.start()
    assert clear_entered.wait(timeout=5)
    publish.start()
    assert not publish_entered.wait(timeout=0.25)
    clear_release.set()
    _join_workers(clear, publish)

    state = portal_client.load_continuity("continuity-session")
    assert state["idempotency_key"] is None
    assert [item["logical_unit_id"] for item in state["records"]] == ["turn-b"]


def test_continuity_json_never_accepts_unknown_or_token_fields() -> None:
    path = Path(portal_client.continuity_path("continuity-session"))
    path.parent.mkdir(mode=0o700, parents=True)
    path.write_text(
        json.dumps(
            {
                "version": 1,
                "idempotency_key": "original-key",
                "owner_token": "forbidden",
                "records": [
                    {
                        **record("turn-1", "safe"),
                        "owner_token": "also-forbidden",
                        "input_items": [{"text": "viewer-private"}],
                    }
                ],
            }
        ),
        encoding="utf-8",
    )
    path.chmod(0o600)

    loaded = portal_client.load_continuity("continuity-session")

    assert loaded == portal_client.empty_continuity()
    assert not path.exists()
    artifacts = list(path.parent.glob(path.name + ".corrupt*"))
    assert artifacts
    for artifact in artifacts:
        serialized = artifact.read_text(encoding="utf-8")
        assert "forbidden" not in serialized
        assert "viewer-private" not in serialized
        assert "owner_token" not in serialized
        assert "input_items" not in serialized
