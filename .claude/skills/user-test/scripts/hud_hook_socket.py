"""The private Linux socket shared by the HUD adapter and Claude hooks."""

import json
import os
import re
import socket
import stat
import struct

import hud_env

MAX_PACKET = 16 * 1024
CARD_KINDS = {"question", "permission", "nudge"}
CARD_TTL_MS = 600_000
TOOL_EVENTS = {"PreToolUse", "PostToolUse", "PostToolUseFailure"}
TOOL_LABELS = frozenset({
    "Bash", "Read", "Write", "Edit", "Grep", "Glob", "WebFetch", "WebSearch",
    "Agent", "Task", "TodoWrite", "AskUserQuestion", "MCP tool", "Tool",
})
NOTICES = {"permission_prompt", "idle_prompt", "elicitation_dialog"}
SESSION_ID = re.compile(r"[A-Za-z0-9_-]{1,96}")
PROMPT_ID = re.compile(r"[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}")
DIGEST = re.compile(r"[0-9a-f]{64}")
TOOL_KEY = re.compile(r"cc-tool-[0-9a-f]{64}")
SOCKET_NAME = re.compile(r"claude-([1-9][0-9]{0,9})\.sock")
PEER_CREDENTIALS = struct.Struct("iII")


def matches(pattern, value):
    return isinstance(value, str) and pattern.fullmatch(value) is not None


def valid_event(event):
    """Accept only normalized fields, before a packet can reach HUD state."""
    if not isinstance(event, dict) or not matches(SESSION_ID, event.get("session")):
        return False
    kind = event.get("event")
    if not isinstance(kind, str):
        return False
    base = {"event", "session"}
    fields = event.keys()
    if kind in {"SessionStart", "SessionEnd"}:
        return fields == base | {"clear"} and type(event.get("clear")) is bool
    actor = "agent" in fields
    if actor and not matches(DIGEST, event["agent"]):
        return False
    if kind == "Notification":
        notice = event.get("notice")
        expected = base | {"notice"} | ({"agent"} if actor else set())
        if "prompt" in fields:
            expected.add("prompt")
            if not matches(PROMPT_ID, event["prompt"]):
                return False
        return isinstance(notice, str) and notice in NOTICES and fields == expected
    if not matches(PROMPT_ID, event.get("prompt")):
        return False
    base.add("prompt")
    if kind == "UserPromptSubmit":
        return fields == base
    if kind in TOOL_EVENTS:
        tool = event.get("tool")
        expected = base | {"tool", "key"} | ({"agent"} if actor else set())
        return (
            fields == expected and isinstance(tool, str) and tool in TOOL_LABELS
            and matches(TOOL_KEY, event.get("key"))
        )
    if kind == "Stop" and fields == base | {"content"}:
        content = event["content"]
        return content is None or (
            isinstance(content, str) and 0 < len(content.encode("utf-8")) <= 8192
        )
    return False


def valid_card(message):
    if not isinstance(message, dict) or type(message.get("v")) is not int or message["v"] != 1:
        return False
    if message.get("op") != "card" or not isinstance(message.get("kind"), str):
        return False
    if message["kind"] not in CARD_KINDS:
        return False
    required = {"v", "op", "kind", "title", "body", "actions"}
    if message.keys() not in (required, required | {"ttl_ms"}):
        return False
    title, body, actions = message["title"], message["body"], message["actions"]
    if not isinstance(title, str) or not isinstance(body, str) or not (title.strip() or body.strip()):
        return False
    if not isinstance(actions, list) or not 1 <= len(actions) <= 4:
        return False
    if not all(isinstance(label, str) and label.strip() for label in actions):
        return False
    ttl_ms = message.get("ttl_ms", CARD_TTL_MS)
    return type(ttl_ms) is int and 1 <= ttl_ms <= CARD_TTL_MS


def packet_bytes(message):
    packet = json.dumps(message, ensure_ascii=False, separators=(",", ":")).encode("utf-8")
    if len(packet) > MAX_PACKET:
        raise ValueError("hook packet too large")
    return packet


def send_packet(peer, packet):
    """Connect once to the immediate parent's same-UID socket and send one packet."""
    path = hud_env.runtime_socket(os.getppid())
    if path is None:
        return False
    peer.setblocking(False)
    peer.connect(os.fspath(path))
    return peer_uid(peer) == os.getuid() and peer.send(packet) == len(packet)


def send_event(event):
    """Attempt one packet on the immediate parent's socket without waiting."""
    try:
        with socket.socket(socket.AF_UNIX, socket.SOCK_SEQPACKET) as peer:
            send_packet(peer, packet_bytes(event))
    except (OSError, ValueError, TypeError):
        pass


def peer_uid(peer):
    credentials = peer.getsockopt(socket.SOL_SOCKET, socket.SO_PEERCRED, PEER_CREDENTIALS.size)
    return PEER_CREDENTIALS.unpack(credentials)[1]


def receive_event(peer, *, allow_card=False):
    """Return one valid packet, or None. An unreadable peer stays nonblocking."""
    try:
        if peer_uid(peer) != os.getuid():
            return None
        packet, _, flags, _ = peer.recvmsg(MAX_PACKET, 0, socket.MSG_DONTWAIT)
        if not packet or flags & (socket.MSG_TRUNC | socket.MSG_CTRUNC):
            return None
        event = json.loads(packet.decode("utf-8"))
        if allow_card and valid_card(event):
            # JSON escapes can encode lone surrogates despite valid wire UTF-8.
            packet_bytes(event)
            return event
        return event if valid_event(event) else None
    except BlockingIOError:
        raise
    except (OSError, ValueError, RecursionError):
        return None


def sweep_dead_sockets(directory_fd):
    removed = 0
    with os.scandir(directory_fd) as entries:
        for entry in entries:
            match = SOCKET_NAME.fullmatch(entry.name)
            if match is None:
                continue
            try:
                if not stat.S_ISSOCK(entry.stat(follow_symlinks=False).st_mode):
                    continue
                os.kill(int(match[1]), 0)
            except ProcessLookupError:
                try:
                    os.unlink(entry.name, dir_fd=directory_fd)
                except OSError:
                    continue
                removed += 1
                if removed == 16:
                    break
            except (FileNotFoundError, PermissionError, OverflowError):
                continue


class HookListener:
    def __init__(self, path, listener, directory_fd, inode):
        self.path = path
        self.socket = listener
        self.directory_fd = directory_fd
        self.inode = inode

    @classmethod
    def open(cls, pid):
        """Publish a new listener atomically, with absence on setup failure."""
        path = hud_env.runtime_socket(pid, create=True)
        if path is None:
            return None
        import fcntl

        temporary = ".s" + os.urandom(5).hex()
        directory_fd = None
        listener = None
        bound = False
        ready = False
        try:
            directory_fd = os.open(path.parent, os.O_DIRECTORY | os.O_NOFOLLOW)
            info = os.fstat(directory_fd)
            if info.st_uid != os.getuid() or stat.S_IMODE(info.st_mode) != 0o700:
                return None
            fcntl.flock(directory_fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
            sweep_dead_sockets(directory_fd)
            listener = socket.socket(socket.AF_UNIX, socket.SOCK_SEQPACKET)
            listener.setblocking(False)
            listener.bind(os.fspath(path.parent / temporary))
            bound = True
            os.chmod(temporary, 0o600, dir_fd=directory_fd)
            listener.listen(64)
            inode = os.stat(temporary, dir_fd=directory_fd, follow_symlinks=False).st_ino
            os.replace(temporary, path.name, src_dir_fd=directory_fd, dst_dir_fd=directory_fd)
            ready = True
            return cls(path, listener, directory_fd, inode)
        except OSError:
            return None
        finally:
            if directory_fd is not None:
                if ready:
                    fcntl.flock(directory_fd, fcntl.LOCK_UN)
                else:
                    if bound:
                        try:
                            os.unlink(temporary, dir_fd=directory_fd)
                        except OSError:
                            pass
                    os.close(directory_fd)
            if listener is not None and not ready:
                listener.close()

    def close(self):
        """Serialize the inode check with any replacement listener's rename."""
        import fcntl

        self.socket.close()
        try:
            fcntl.flock(self.directory_fd, fcntl.LOCK_EX)
            info = os.stat(self.path.name, dir_fd=self.directory_fd, follow_symlinks=False)
            if stat.S_ISSOCK(info.st_mode) and info.st_ino == self.inode:
                os.unlink(self.path.name, dir_fd=self.directory_fd)
        except OSError:
            pass
        finally:
            os.close(self.directory_fd)
