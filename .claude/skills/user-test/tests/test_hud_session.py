"""Table and order checks for the adapter-owned Claude session."""

import sys
from pathlib import Path

import pytest

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "scripts"))
from hud_session import HOLD_MS, PENDING_LIMIT, STATE_METHOD, SessionPresence

PROMPT = "e53cb2ed-891a-4c88-9a7b-c64a41f06cf2"
NEXT = "5f1116d5-223b-48ab-a028-00ae7a2d005c"


def event(name, **fields):
    return {"event": name, "session": "fixture", "prompt": PROMPT, **fields}


def tool(kind="PreToolUse", name="Read", key="read", **fields):
    return event(kind, tool=name, key=key, **fields)


def drain(state):
    ops = []
    while op := state.take():
        ops.append(op)
    return ops


@pytest.mark.parametrize(("notice", "reason"), [
    ("permission_prompt", "permission"),
    ("elicitation_dialog", "input"),
])
def test_blocked_notice_only_clears_for_its_asker(notice, reason):
    state = SessionPresence(False)
    state.event(event("UserPromptSubmit"))
    assert drain(state)[0].params == {"state": "working"}
    state.event(event("Notification", notice=notice, agent="asker"))
    assert drain(state)[0].params == {"state": "blocked", "reason": reason}
    for fields in ({}, {"agent": "other"}):
        state.event(tool(**fields))
        assert not drain(state)
    state.event(tool(agent="asker"))
    assert drain(state)[0].params == {"state": "working"}


def test_question_completion_and_duplicate_working_events():
    state = SessionPresence(False)
    state.event(event("UserPromptSubmit"))
    state.event(tool(name="AskUserQuestion"))
    assert drain(state)[0].params == {"state": "blocked", "reason": "question"}
    state.event(tool(kind="PostToolUse", name="AskUserQuestion", agent="subagent"))
    state.event(tool())
    assert not drain(state)
    state.event(tool(kind="PostToolUseFailure", name="AskUserQuestion"))
    assert drain(state)[0].params == {"state": "working"}
    for _ in range(10):
        state.event(tool())
    assert not drain(state)
    state.resend_state()
    assert drain(state)[0].params == {"state": "working"}


def test_out_of_order_stages_post_stop_events_and_duplicate_final():
    state = SessionPresence(True)
    state.event(tool(kind="PostToolUse"))
    assert [op.params.get("content") for op in drain(state)] == [None, "Read: completed"]
    state.event(tool())
    assert not drain(state)
    end = event("Stop", content="complete")
    state.event(end)
    assert [(op.method, op.params.get("content")) for op in drain(state)] == [
        (STATE_METHOD, None), ("hud_publish", "complete"),
    ]
    state.event(tool(kind="PostToolUse", key="late"))
    state.event(end)
    assert not drain(state)
    state.event(event("UserPromptSubmit", prompt=NEXT))
    assert drain(state)[0].params == {"state": "working"}


def test_hold_is_finite_and_refreshed_by_acknowledgments():
    state = SessionPresence(True)
    state.event(event("UserPromptSubmit"))
    _, publish = drain(state)
    state.acknowledged(publish, False, 0)
    assert not drain(state)
    state.acknowledged(publish, True, 0)
    hold, = drain(state)
    assert hold.params["ttl_ms"] == HOLD_MS
    state.acknowledged(hold, True, 0)
    for seconds in range(5):
        state.acknowledged(publish, True, seconds)
        assert not drain(state)
    state.acknowledged(publish, True, 60)
    assert drain(state) == [hold]


def test_clear_replaces_only_the_owned_portal():
    state = SessionPresence(True)
    state.event(event("UserPromptSubmit"))
    drain(state)
    state.event({"event":"SessionEnd", "session":"fixture", "clear":True})
    state.event({"event":"SessionStart", "session":"next-session", "clear":True})
    state.event(event("UserPromptSubmit", session="next-session", prompt=NEXT))
    clear, publish = drain(state)
    assert (clear.method, clear.params["surface"]) == ("hud_clear", "portal:fixture")
    assert publish.params["surface"] == "portal:next-session"
    assert state.state == ("working", None)


def test_overflow_keeps_stop_and_bounds_pending_deliveries():
    state = SessionPresence(True)
    for index in range(PENDING_LIMIT + 10):
        state.event(tool(key=f"read-{index}"))
    state.event(event("Stop", content="complete"))
    ops = drain(state)
    assert len(ops) == PENDING_LIMIT
    assert all(op.params.get("key") != "read-0" for op in ops)
    assert [op.params.get("content") for op in ops if op.terminal] == [None, "complete"]
