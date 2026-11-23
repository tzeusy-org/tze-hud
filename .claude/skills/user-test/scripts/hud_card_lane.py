"""One serialized sibling-scope worker for live Claude hook cards."""

from dataclasses import dataclass
import hashlib
import json
import secrets
import sys
from time import monotonic

from hud_session import CARD_SURFACE, CardWait, HookCards

PARENT_META_KEY = "tze.hud/parent"
CARD_CALL_ID = "tze-hud-card"
CARD_ADMISSION_SECONDS = 1.0
CARD_INPUT_GRACE = 0.75


@dataclass(frozen=True)
class CardOp:
    name: str
    waiter: CardWait | None = None
    due: tuple = ()


def tool_result(response):
    try:
        result = response["result"]
        if result.get("isError") is True:
            return None
        content, = result["content"]
        decoded = json.loads(content["text"])
        return decoded if isinstance(decoded, dict) else None
    except (AttributeError, KeyError, TypeError, ValueError, RecursionError):
        return None


class CardLane:
    def __init__(self, adapter):
        self.adapter = adapter
        self.cards = HookCards()
        self.pending = {}
        self.session = None
        self.check_parent = False
        self.clears = {}
        self.ack = []
        self.final_ack = False

    def task(self):
        return next((task for task in self.adapter.tasks if task.card is not None), None)

    def interrupt_poll(self):
        task = self.task()
        if task is not None and task.card.name == "poll" and not task.cancelled:
            self.adapter.abort([task])

    def offer(self, request, peer, deadline):
        waiter = CardWait(request, peer, secrets.token_hex(8), deadline)
        prior = self.pending.get(waiter.kind)
        if prior is not None:
            self.finish(prior, {"end": "error"})
        self.pending[waiter.kind] = waiter
        self.interrupt_poll()

    def finish(self, waiter, result):
        self.adapter.reply_card(waiter.peer, result, terminal=True)

    def disconnect(self, peer):
        for waiter in tuple(self.pending.values()):
            if waiter.peer is peer:
                del self.pending[waiter.kind]
                return
        for waiter in tuple(self.cards.live.values()):
            if waiter.peer is peer and self.cards.pop(waiter):
                self.clears[waiter.kind] = None
                self.final_ack = not self.cards.live
                self.interrupt_poll()
                return
        # A published-but-unacknowledged waiter is cleared by its result handler.

    def reset(self, *, probe_parent=False):
        self.session = None
        self.check_parent = probe_parent
        self.clears.clear()
        self.ack.clear()
        self.final_ack = False
        task = self.task()
        if task is not None and not task.cancelled:
            self.adapter.abort([task])
            if task.card.waiter is not None:
                self.finish(task.card.waiter, {"end": "error"})
        for waiter in tuple(self.cards.live.values()):
            self.cards.pop(waiter)
            self.finish(waiter, {"end": "cancelled"})

    def shutdown(self):
        self.reset()
        for waiter in self.pending.values():
            self.finish(waiter, {"end": "error"})
        self.pending.clear()

    def pending_deadlines(self):
        return (waiter.admission_deadline for waiter in self.pending.values())

    def select_deadlines(self):
        yield from self.pending_deadlines()
        if self.cards.live and self.task() is None and not self.adapter.reinitializing and not self.adapter.stop:
            yield min(waiter.deadline for waiter in self.cards.live.values())

    def start(self, now):
        for waiter in tuple(self.pending.values()):
            if now >= waiter.admission_deadline:
                del self.pending[waiter.kind]
                self.finish(waiter, {"end": "error"})
        if self.task() is not None or self.adapter.stop or self.adapter.reinitializing:
            return
        parent = self.adapter.session
        if parent is None:
            if self.pending and self.adapter.had_session:
                self.adapter.reinitialize(deadline=min(self.pending_deadlines()))
            return
        if self.clears:
            key = next(iter(self.clears))
            del self.clears[key]
            self.call(CardOp("clear"), "hud_clear", {"surface": CARD_SURFACE, "key": key},
                      now + CARD_ADMISSION_SECONDS)
            return
        if self.final_ack:
            self.final_ack = False
            self.call(CardOp("ack"), "hud_input", {"wait_ms": 0, "max_items": 0, "ack": self.ack},
                      now + CARD_ADMISSION_SECONDS)
            return
        if self.pending:
            waiter = min(self.pending.values(), key=lambda waiter: waiter.admission_deadline)
            deadline = waiter.admission_deadline
            if self.session is None:
                if self.check_parent:
                    self.send(CardOp("parent"), "ping", {}, deadline, session=parent)
                else:
                    params = {"protocolVersion": self.adapter.protocol, "_meta": {PARENT_META_KEY: parent}}
                    self.send(CardOp("link"), "initialize", params, deadline)
            else:
                del self.pending[waiter.kind]
                self.call(CardOp("publish", waiter=waiter), "hud_publish",
                          waiter.publish_arguments(), deadline)
            return
        if not self.cards.live:
            return
        due = self.cards.due(now)
        if due:
            self.call(CardOp("inspect", due=tuple(due)), "hud_surfaces", {},
                      now + CARD_ADMISSION_SECONDS)
        else:
            wait_ms = self.cards.wait_ms(now)
            if wait_ms:
                self.call(CardOp("poll"), "hud_input", {"ack": self.ack, "wait_ms": wait_ms},
                          now + wait_ms / 1000 + CARD_INPUT_GRACE)

    def call(self, op, name, arguments, deadline):
        self.send(op, "tools/call", {"name": name, "arguments": arguments},
                  deadline, session=self.session)

    def send(self, op, method, params, deadline, *, session=None):
        request = {"jsonrpc": "2.0", "id": CARD_CALL_ID, "method": method, "params": params}
        if not self.adapter.admit(request, deadline, card=op, card_session=session):
            self.result(op, None, None, False)

    def result(self, op, response, opened, unknown, *, now=None):
        now = monotonic() if now is None else now
        if unknown:
            if op.waiter is not None:
                self.finish(op.waiter, {"end": "error"})
            self.reset(probe_parent=op.name != "parent")
            if op.name == "parent":
                self.adapter.set_session(None)
            return
        if op.name == "link":
            if isinstance(opened, str) and isinstance(response, dict) and isinstance(response.get("result"), dict):
                self.session = opened
                return
            self.check_parent = True
            self.fail_pending(op)
            return
        if op.name == "parent":
            if isinstance(response, dict) and isinstance(response.get("result"), dict):
                self.check_parent = False
            else:
                self.fail_pending(op)
            return
        result = tool_result(response)
        if op.name == "publish":
            waiter = op.waiter
            expires = result.get("expires_in_ms") if result is not None and result.get("ok") is True else None
            if type(expires) is not int or not 0 <= expires <= (1 << 64) - 1:
                self.log_failure(op)
                self.finish(waiter, {"end": "error"})
                return
            prior = self.cards.admit(waiter, now, expires)
            if prior is not None:
                self.finish(prior, {"end": "superseded"})
            if not self.adapter.reply_card(waiter.peer, {"ok": True, "expires_in_ms": expires}):
                self.disconnect(waiter.peer)
            return
        if op.name == "clear":
            if result is None:
                self.log_failure(op)
            return
        if op.name == "ack":
            self.ack.clear()
            if result is None:
                self.log_failure(op)
            return
        if op.name == "poll" and result is not None and isinstance(result.get("items"), list):
            self.ack, finished = self.cards.input(result["items"])
            for waiter, reply in finished:
                self.finish(waiter, reply)
            self.final_ack = not self.cards.live
            return
        if op.name == "inspect" and result is not None and isinstance(result.get("surfaces"), list):
            zone = next((entry for entry in result["surfaces"]
                         if isinstance(entry, dict) and entry.get("s") == CARD_SURFACE), {})
            held = zone.get("held") is True
            expires = zone.get("expires_in_ms")
            if not held or (type(expires) is int and 0 <= expires <= (1 << 64) - 1):
                for waiter in self.cards.observe_zone(op.due, now, held, expires):
                    self.finish(waiter, {"end": "expired"})
                self.final_ack = not self.cards.live
                return
        self.log_failure(op)
        for waiter in tuple(self.cards.live.values()):
            self.cards.pop(waiter)
            self.finish(waiter, {"end": "error"})
        self.final_ack = True

    def fail_pending(self, op):
        self.log_failure(op)
        for waiter in self.pending.values():
            self.finish(waiter, {"end": "error"})
        self.pending.clear()

    @staticmethod
    def log_failure(op):
        label = op.waiter.kind if op.waiter is not None else op.name
        key_hash = hashlib.sha256(label.encode()).hexdigest()[:12]
        print(f"hud_mcp_stdio: card delivery failed [{key_hash}]", file=sys.stderr)
