"""Table and order checks for the adapter-owned Claude session."""

import sys
from pathlib import Path

import pytest

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "scripts"))
from hud_session import CARD_SURFACE, HOLD_MS, PENDING_LIMIT, STATE_METHOD, CardWait, HookCards, SessionPresence

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


def test_promptless_notice_applies_only_while_the_prompt_is_open():
    state = SessionPresence(False)
    permission = {"event": "Notification", "session": "fixture", "notice": "permission_prompt"}
    state.event(event("UserPromptSubmit"))
    state.event(permission)
    assert drain(state)[0].params == {"state": "blocked", "reason": "permission"}
    state.event(event("Stop", content=None))
    assert drain(state)[0].params == {"state": "done"}
    state.event(permission)
    assert state.state == ("done", None)
    assert not drain(state)
    state.event(event("UserPromptSubmit", prompt=NEXT))
    state.event({**permission, "notice": "idle_prompt"})
    assert drain(state)[0].params == {"state": "done"}


def test_idle_notice_for_another_prompt_does_not_close_the_active_prompt():
    state = SessionPresence(False)
    state.event(event("UserPromptSubmit"))
    assert drain(state)[0].params == {"state": "working"}
    state.event(event("Notification", prompt=NEXT, notice="idle_prompt"))
    assert state.state == ("working", None)
    assert not drain(state)


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


def test_restart_can_adopt_a_final_and_priority_state_overtakes_pending_progress():
    restarted = SessionPresence(True)
    restarted.event(event("Stop", content="complete after restart"))
    assert restarted.take().params == {"state": "done"}
    assert restarted.take().params["content"] == "complete after restart"

    state = SessionPresence(True)
    state.event(tool())
    assert state.take().params == {"state": "working"}
    state.event(event("Notification", notice="idle_prompt"))
    assert state.take().params == {"state": "done"}
    assert state.take().params["content"] == "Read: running"


def card(kind, nonce):
    request = {"kind": kind, "title": "Choose", "body": "", "actions": ["dismiss", "Keep"]}
    return CardWait(request, object(), nonce, 1)


@pytest.mark.parametrize("stale_dismiss", [False, True])
def test_option_d_keeps_callbacks_generation_bound(stale_dismiss):
    cards = HookCards()
    old, new = card("question", "old"), card("question", "new")
    cards.admit(old, 0, 100)
    assert cards.admit(new, 1, 100) is old
    assert new.publish_arguments()["content"]["actions"][0] == {
        "label": "dismiss", "callback_id": "new.0",
    }
    input_item = {"s": CARD_SURFACE, "key": "question"}
    callback = {**input_item, "id": "a1", "action": "old.0"}
    assert cards.input([callback, callback]) == (["a1"], [])
    if stale_dismiss:
        answer = {**input_item, "id": "a2", "action": "dismiss", "kind": "dismiss"}
        expected = {"end": "dismissed"}
    else:
        answer = {**input_item, "id": "a2", "action": "new.0"}
        expected = {"end": "answer", "index": 0}
    assert cards.input([answer]) == (["a2"], [(new, expected)])
    assert not cards.live


def test_held_zone_extends_only_the_due_waiters():
    cards = HookCards()
    question = card("question", "one")
    permission = card("permission", "two")
    cards.admit(question, 1, 750)
    cards.admit(permission, 1, 5750)
    assert cards.wait_ms(1) == 1000
    assert cards.due(2) == [question]
    assert cards.observe_zone(cards.due(2), 2, True, 4000) == []
    assert question.deadline == 6.25
    assert permission.deadline == 7
    assert cards.observe_zone(cards.due(6.25), 6.25, False, None) == [question]
    assert list(cards.live.values()) == [permission]
