"""Regression tests for attach replay in the preferred portal client.

The runtime holds the owner token server-side; the client never stores one.
"""

from __future__ import annotations

import importlib.util
from pathlib import Path
from types import SimpleNamespace
from unittest import mock

import pytest


CLIENT_PATH = (
    Path(__file__).parents[2] / "hud-projection" / "scripts" / "portal_client.py"
)
SPEC = importlib.util.spec_from_file_location(
    "portal_client_token_rotation", CLIENT_PATH
)
assert SPEC is not None and SPEC.loader is not None
portal_client = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(portal_client)


@pytest.fixture(autouse=True)
def continuity_dir(tmp_path: Path) -> None:
    portal_client.CONTINUITY_DIR = str(tmp_path / "continuity")


def attach_args() -> SimpleNamespace:
    return SimpleNamespace(
        projection_id="review-session",
        provider_kind="codex",
        display_name="Review Session",
        classification="private",
        idempotency_key="stable-attach-key",
        workspace_hint=None,
        repository_hint=None,
        icon_profile=None,
    )


def test_attach_conflict_is_reported_as_definitive_rejection() -> None:
    response = {
        "error": {
            "code": -32603,
            "data": {"error_code": "PROJECTION_ALREADY_ATTACHED"},
        }
    }

    with (
        mock.patch.object(portal_client, "call_tool", return_value=response),
        pytest.raises(SystemExit) as exc,
    ):
        portal_client.cmd_attach(attach_args())

    assert exc.value.code == 2


def test_attach_never_sends_or_stores_owner_token() -> None:
    sent: list[dict] = []

    def fake_call(tool: str, args: dict) -> dict:
        sent.append(args.copy())
        return {"result": {"accepted": True}}

    with (
        mock.patch.object(portal_client, "call_tool", side_effect=fake_call),
        mock.patch.object(portal_client, "emit"),
    ):
        portal_client.cmd_attach(attach_args())

    assert sent and all("owner_token" not in args for args in sent)
    assert not hasattr(portal_client, "TOKEN_DIR")
