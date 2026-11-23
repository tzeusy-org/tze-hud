"""In-memory Claude presence and mirror ordering. The adapter owns all I/O."""

from collections import deque
from dataclasses import dataclass
import hashlib
import math

from hud_hook_socket import TOOL_EVENTS

STATE_METHOD = "notifications/tze/session_state"
HOLD_MS = 600000
HOLD_REFRESH_SECONDS = 60
PENDING_LIMIT = 64
CARD_SURFACE = "zone:notification-area"
CARD_DEADLINE_GRACE = 0.25


@dataclass
class CardWait:
    request: dict
    peer: object
    nonce: str
    admission_deadline: float
    deadline: float | None = None

    @property
    def kind(self):
        return self.request["kind"]

    def publish_arguments(self):
        arguments = {
            "surface": CARD_SURFACE,
            "key": self.kind,
            "content": {
                "type": "notification",
                "title": self.request["title"],
                "body": self.request["body"],
                "actions": [
                    {"label": label, "callback_id": f"{self.nonce}.{index}"}
                    for index, label in enumerate(self.request["actions"])
                ],
            },
        }
        if "ttl_ms" in self.request:
            arguments["ttl_ms"] = self.request["ttl_ms"]
        return arguments

    def rearm(self, now, expires_ms):
        self.deadline = now + expires_ms / 1000 + CARD_DEADLINE_GRACE


class HookCards:
    """The bounded live-card table and choice routing; no transport decisions."""

    def __init__(self):
        self.live = {}

    def admit(self, waiter, now, expires_ms):
        prior = self.live.get(waiter.kind)
        self.live[waiter.kind] = waiter
        waiter.rearm(now, expires_ms)
        return prior

    def pop(self, waiter):
        if self.live.get(waiter.kind) is waiter:
            del self.live[waiter.kind]
            return True
        return False

    def wait_ms(self, now):
        if self.live:
            first = min(waiter.deadline for waiter in self.live.values())
            return max(0, min(30_000, math.floor((first - now) * 1000)))
        return 0

    def due(self, now):
        return [waiter for waiter in self.live.values() if waiter.deadline <= now]

    def input(self, items):
        ack, finished = [], []
        for item in items:
            if not isinstance(item, dict) or not isinstance(item.get("id"), str):
                continue
            ack.append(item["id"])
            kind = item.get("key")
            if item.get("s") != CARD_SURFACE or not isinstance(kind, str):
                continue
            waiter = self.live.get(kind)
            if waiter is None:
                continue
            event = item.get("kind")
            if event == "dismiss":
                # Option D: a prior generation's X carries only its reusable key.
                reply = {"end": "dismissed"}
            elif event == "displaced":
                reply = {"end": "expired"}
            elif event is None:
                index = next(
                    (i for i in range(len(waiter.request["actions"]))
                     if item.get("action") == f"{waiter.nonce}.{i}"),
                    None,
                )
                if index is None:
                    continue
                reply = {"end": "answer", "index": index}
            else:
                continue
            self.pop(waiter)
            finished.append((waiter, reply))
        return list(dict.fromkeys(ack)), finished

    def observe_zone(self, waiters, now, held, expires_ms):
        finished = []
        for waiter in waiters:
            if self.live.get(waiter.kind) is not waiter:
                continue
            if held:
                waiter.rearm(now, expires_ms)
            else:
                self.pop(waiter)
                finished.append(waiter)
        return finished


@dataclass(frozen=True)
class PresenceOp:
    key: str
    method: str
    params: dict
    terminal: bool = False


class SessionPresence:
    def __init__(self, mirror):
        self.mirror = mirror
        self.session = None
        self.prompt = None
        self.closed = deque(maxlen=16)
        self.state = None
        self.asker = None
        self.stages = {}
        self.finals = {}
        self.last_hold = None
        self.pending = {}

    def enqueue(self, op):
        if op.key not in self.pending and len(self.pending) == PENDING_LIMIT:
            for key, candidate in self.pending.items():
                if not candidate.terminal:
                    del self.pending[key]
                    break
            else:
                return
        self.pending[op.key] = op

    def take(self):
        state = self.pending.pop("state", None)
        if state is not None:
            return state
        if self.pending:
            return self.pending.pop(next(iter(self.pending)))
        return None

    def set_state(self, state, reason=None, asker=None):
        self.asker = asker
        current = (state, reason)
        if self.state != current:
            self.state = current
            self.resend_state()

    def resend_state(self):
        if self.state is not None:
            state, reason = self.state
            params = {"state": state}
            if reason is not None:
                params["reason"] = reason
            self.enqueue(PresenceOp("state", STATE_METHOD, params, True))

    def mirror_op(self, operation_key, name, *, terminal=False, **arguments):
        if self.mirror:
            surface = "portal:" + self.session
            self.enqueue(PresenceOp(surface + "/" + operation_key, name,
                                    {"surface": surface, **arguments}, terminal))

    def reset_mirror(self):
        if self.mirror and self.session is not None:
            surface = "portal:" + self.session
            self.pending = {key: op for key, op in self.pending.items()
                            if op.params.get("surface") != surface}
            self.mirror_op("clear", "hud_clear", terminal=True)
        self.close_prompt()
        self.prompt = None
        self.stages = {}
        self.finals = {}
        self.last_hold = None

    def close_prompt(self):
        if self.prompt is not None and self.prompt not in self.closed:
            self.closed.append(self.prompt)

    def adopt_prompt(self, prompt):
        if self.prompt != prompt:
            self.close_prompt()
            self.prompt = prompt
            self.stages = {}

    def acknowledged(self, op, ok, now):
        if not ok or not self.mirror or op.params.get("surface") != "portal:" + (self.session or ""):
            return
        if op.method == "hud_hold":
            self.last_hold = now
        elif op.method == "hud_publish" and (self.last_hold is None or now - self.last_hold >= HOLD_REFRESH_SECONDS):
            self.mirror_op("hold", "hud_hold", ttl_ms=HOLD_MS)

    def event(self, item):
        event, session = item["event"], item["session"]
        prompt, actor = item.get("prompt"), item.get("agent")
        if event == "SessionEnd":
            if item["clear"] and session == self.session:
                self.reset_mirror()
            return
        if event == "SessionStart":
            if item["clear"] and self.prompt is not None:
                self.reset_mirror()
            self.session = session
            return
        if self.session is None:
            self.session = session
        if session != self.session:
            return

        if event == "Notification":
            if prompt is None:
                prompt = self.prompt
            if prompt is None or prompt in self.closed:
                return
            if self.prompt is None:
                self.adopt_prompt(prompt)
            if prompt != self.prompt:
                return
            notice = item["notice"]
            if notice == "idle_prompt":
                self.close_prompt()
                self.set_state("done")
            else:
                self.set_state("blocked", "permission" if notice == "permission_prompt" else "input", actor)
            return
        if event == "Stop":
            if self.prompt is None and prompt not in self.closed:
                self.adopt_prompt(prompt)
            if self.prompt != prompt:
                return
            self.close_prompt()
            self.set_state("done")
            if not self.mirror:
                return
            content = item["content"]
            final_hash = hashlib.sha256(repr(content).encode()).digest()
            if self.finals.get(prompt) != final_hash:
                self.finals[prompt] = final_hash
                if len(self.finals) > 16:
                    del self.finals[next(iter(self.finals))]
                key = "cc-final-" + hashlib.sha256(prompt.encode()).hexdigest()
                arguments = {"content": content, "key": key} if content else {}
                self.mirror_op(key, "hud_publish", terminal=True, **arguments)
            return
        if prompt in self.closed:
            return
        self.adopt_prompt(prompt)
        if event == "UserPromptSubmit":
            self.set_state("working")
            self.mirror_op("active", "hud_publish", status="active")
        elif event in TOOL_EVENTS:
            pre = event == "PreToolUse"
            tool = item["tool"]
            if pre and actor is None and tool == "AskUserQuestion":
                self.set_state("blocked", "question")
            elif self.state is None or self.state[0] != "blocked":
                self.set_state("working")
            elif actor == self.asker:
                if self.state[1] != "question" or (tool == "AskUserQuestion" and not pre):
                    self.set_state("working")
            if self.mirror and actor is None:
                key, stage = item["key"], 1 if pre else 2
                prior = self.stages.get(key, 0)
                if stage > prior and (prior or len(self.stages) < 256):
                    self.stages[key] = stage
                    label = "running" if pre else "failed" if event == "PostToolUseFailure" else "completed"
                    self.enqueue(PresenceOp("portal:" + session + "/" + key, "hud_publish",
                                            {"surface": "portal:" + session, "key": key,
                                             "status": "active", "content": f"{tool}: {label}"}))
