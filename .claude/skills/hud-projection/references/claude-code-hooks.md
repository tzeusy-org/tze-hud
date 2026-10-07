# Claude Code portal hooks (owner opt-in)

Mirror a main interactive Claude Code session onto the existing HUD portal
without asking the model to call `hud_publish`. Explicit model/client verbs
remain available. This is cooperative event delivery, not terminal capture.
Nothing in this feature installs settings, pairs a host or changes permissions.

Compatibility is qualified against the observed Claude Code **2.1.290** CLI and
its [official hooks reference](https://code.claude.com/docs/en/hooks), checked
2026-10-07 local time. The [hooks guide](https://code.claude.com/docs/en/hooks-guide)
explains owner configuration. Older event shapes without `prompt_id` cannot be
assumed compatible. Use an interactive POSIX Linux/macOS Claude host with
Python3, not native Windows Python; the HUD itself can run on Windows. Native
Windows hook-host execution silently does nothing until separately supported.
Immediate `claude -p` teardown may cancel async final delivery. No live owner
session or recorded owner payload has been verified by the synthetic fixtures.

## Enable only this block

Pair normally first and set `HUD_HOST`. The existing `hud_env` resolver reads
only that host's paired PSK at network execution. The hook never prints it,
puts it in state, pairs automatically, reads transcripts, or inspects another
credential. The paired agent needs the existing `portal` capability.

In your selected Claude settings file, merge the following entries while
preserving unrelated hooks/settings. Substitute stable absolute interpreter
and repository paths; `command` plus `args` uses exec form, without shell
interpolation of hook payload. Trust the selected workspace explicitly. Do
not add `asyncRewake`, prompt/agent hooks, decision outputs, permission overrides,
additional context or new model calls. Local markers and SessionEnd deliberately
omit `async`; only marker writes (and a background status child on prompt) run
on the synchronous prompt side. SessionStart's matcher excludes compact/fork.

```json
{
  "hooks": {
    "SessionStart": [
      {
        "hooks": [
          {
            "type": "command",
            "command": "/absolute/path/to/python3",
            "args": [
              "/absolute/path/to/tze-hud/.claude/skills/hud-projection/scripts/portal_hook.py"
            ],
            "timeout": 2
          }
        ],
        "matcher": "^(startup|resume|clear)$"
      }
    ],
    "UserPromptSubmit": [
      {
        "hooks": [
          {
            "type": "command",
            "command": "/absolute/path/to/python3",
            "args": [
              "/absolute/path/to/tze-hud/.claude/skills/hud-projection/scripts/portal_hook.py"
            ],
            "timeout": 2
          }
        ]
      }
    ],
    "PreToolUse": [
      {
        "hooks": [
          {
            "type": "command",
            "command": "/absolute/path/to/python3",
            "args": [
              "/absolute/path/to/tze-hud/.claude/skills/hud-projection/scripts/portal_hook.py"
            ],
            "timeout": 2,
            "async": true
          }
        ],
        "matcher": "*"
      }
    ],
    "PostToolUse": [
      {
        "hooks": [
          {
            "type": "command",
            "command": "/absolute/path/to/python3",
            "args": [
              "/absolute/path/to/tze-hud/.claude/skills/hud-projection/scripts/portal_hook.py"
            ],
            "timeout": 2,
            "async": true
          }
        ],
        "matcher": "*"
      }
    ],
    "PostToolUseFailure": [
      {
        "hooks": [
          {
            "type": "command",
            "command": "/absolute/path/to/python3",
            "args": [
              "/absolute/path/to/tze-hud/.claude/skills/hud-projection/scripts/portal_hook.py"
            ],
            "timeout": 2,
            "async": true
          }
        ],
        "matcher": "*"
      }
    ],
    "Stop": [
      {
        "hooks": [
          {
            "type": "command",
            "command": "/absolute/path/to/python3",
            "args": [
              "/absolute/path/to/tze-hud/.claude/skills/hud-projection/scripts/portal_hook.py"
            ],
            "timeout": 2,
            "async": true
          }
        ]
      }
    ],
    "SessionEnd": [
      {
        "hooks": [
          {
            "type": "command",
            "command": "/absolute/path/to/python3",
            "args": [
              "/absolute/path/to/tze-hud/.claude/skills/hud-projection/scripts/portal_hook.py"
            ],
            "timeout": 2
          }
        ]
      }
    ]
  }
}
```

## What is mirrored

`portal_hook.py` accepts one bounded JSON object on stdin (at most 64 KiB).
It validates the configured event and a 1–96-byte ASCII session identifier,
UUID prompt identifier for prompt-bound events, and bounded tool-use identifier.
Subagent events carrying `agent_id` are ignored; `agent_type` alone is not a
subagent identity. There is no invented version field in the documented input.

| Event | Behavior |
|---|---|
| SessionStart startup/resume/clear | Start an explicit metadata generation; no network |
| UserPromptSubmit | Record current prompt immediately; schedule bounded content-free active status |
| PreToolUse | Fixed tool label plus `running`, keyed by a hash of prompt/tool-use IDs |
| PostToolUse / PostToolUseFailure | Same key plus `completed` / `failed`; late start cannot replace terminal stage |
| Stop | Close current prompt; mirror `last_assistant_message` and attached status; no continuation/control output |
| SessionEnd | Mark generation ended first, then attempt bounded owner-surface clear |

Unknown/MCP tool names become fixed `Tool`/`MCP tool` labels. Inputs, results,
errors, command descriptions, cwd, transcript/scratchpad paths and unknown
payload fields are discarded. Stop's documented final-text field is used
instead of a potentially lagging transcript. Missing/nonstring final text can
send attached status, but does not prove final-reply acceptance. Final text
has escape/control sequences removed and is truncated at a valid UTF-8 boundary
to 8 KiB, including a visible truncation marker. Recognizable private-key/token/
credential forms discard the entire final string. This is not a universal
secret classifier: the owner must never authorize mirroring a secret final
reply. There is no generic redaction promise or raw-payload diagnostic log.
Every invocation emits empty stdout/stderr and exits0, including HUD-down,
unpaired, malformed input, unsupported shape and state failures. That exit
status is deliberately not an assertion that mirroring succeeded.

## Ordering, bounds and privacy

Only small metadata lives under `~/.cache/tze-hud/claude-hooks` outside the
repository (own directories 0700, files 0600). The path is a hash of endpoint
hostname, session and profile, never a PSK or raw unchecked path. Metadata
contains generation/prompt identifiers, bounded closed prompts and delivered
stage/content hashes; no final text, tool payload or credential is persisted.
State is capped at 32 KiB, 256 current-prompt tools and 16 closed/final prompts.
New tool identities beyond that cap are dropped rather than evicting terminal
stages and allowing a late start to overwrite them. Closed tombstones survive
24 hours; ordinary invocation examines at most 16 profile-owned directories for
bounded expiry cleanup. Lock files remain to preserve their inode domain.

Marker admission is at most 5 ms and never covers network I/O. The independent
atomic marker records the prompt before Claude's next work. A separate delivery
lock admits ordinary publishers for at most 50 ms. One supervised HTTP child gets
350 ms per request and a 1.0 s total watchdog; SessionEnd has 1.2 s total. The
living supervisor kills/reaps its known child on timeout, errors or catchable
SIGTERM before releasing its delivery lock. The child also arms a default-action
real-time timer for the remaining absolute budget before network I/O and inherits
the delivery-lock descriptor. If the supervisor dies abruptly, that descriptor
keeps ownership until the child terminates; OS adoption/reaping is distinct from
the dead parent reaping it. Incomplete job input exits silently, and a late child
does not start HTTP after its deadline. Interpreter startup, filesystem, OS
scheduling and stopped-process delays are separate from these network deadlines;
real owner-host tool AND prompt-side overhead must be measured below 50 ms. An
async flag or successful local fixture does not establish that live limit.

Publish and then the separate `hud_hold(ttl_ms=600000)` refresh a finite 10 min
quiet hold. No publish TTL field, indefinite hold, daemon, heartbeat or polling
is introduced. Longer quiet periods can degrade/reclaim through existing HUD
behavior. SessionEnd clear is best effort when a previous admitted request
exhausts its remaining budget; NOT_HELD is harmless. Finite hold/reclaim is the
fallback. Explicit client publication/input remain independent.

Observable stale generations/prompts and tool events after Stop are rejected
before send. Stop delivery deduplicates only acknowledged equal final hashes;
a failed final delivery can retry via a duplicate Stop for the same still-current
closed prompt, never reopening tool activity. A newer prompt/ended generation
rejects that retry. Compact/fork/unknown SessionStart sources do not reopen.

After each acknowledgment the child revalidates the atomic marker before
committing delivery hashes or sending a hold. An already-SENT old final may
briefly append its own prompt-keyed text or set attached after a new prompt.
Within the same watchdog, at most one content-free active catch-up restores
the latest open prompt after acknowledgment. An ended marker permits a
compensating clear only for that same generation; no old cleanup is admitted
after a newer generation is observable. If the marker changes again, acknowledgment
fails, or the deadline is exhausted, the next genuine event reconciles.

A timed-out request can still reach the server: killing a client is not rollback.
The existing server has no conditional prompt epoch/status compare-and-set;
an already-sent clear can also arrive after a new generation opens.
Therefore universal ordering/detach is not promised, including reattachment by
an unacknowledged request after clear. Late processes first starting after an
explicit resume also lack a documented event-generation identifier. Tests of
observable rejection/catch-up cannot erase these in-flight and generation
ambiguities; fixing them would require an excluded runtime/API change.

Metadata admission/write failure best-effort writes an independent disabled
sentinel, preventing later jobs from using old metadata. Explicit successful
SessionStart can reopen. If even sentinel/storage fails, no lifecycle proof
survives: mirroring is unavailable, not successfully idle/active. Symlink,
ownership/permission and malformed-state failures publish no new activity.

## Verification and remaining owner acceptance

The existing `test_portal_client.py` uses real subprocess/stdin and a loopback
fake MCP endpoint with **docs-derived synthetic** payloads. It checks keyed
lifecycle/privacy, separate finite holds, before-send replay rejection,
acknowledged rollover catch-up while the fast marker proceeds independently,
failed Stop retry, metadata disable/reopen, malformed/unavailable input and
actual watchdog child termination. Existing client/default timeout tests stay
positive. Synthetic events are not recorded owner payloads or a screenshot.

The original acceptance still requires a separately authorized paired owner
host: copy only this block, run an interactive turn with parallel success and
failure tools, observe progress and final reply/status on the HUD, capture
screenshots and a filtered model transcript proving **zero mirroring model
calls**, and verify explicit intentional publication still works. Record CLI,
script/source, HUD build/config/theme, safe payload field names and actual
max/p95 scheduling/tool AND prompt-side overhead over repeated invocations;
every observed addition must be below 50 ms. Demonstrate HUD-down silence and
prompt rollover/late Post/Stop/SessionEnd. No claim of zero total session tokens,
hard performance, universal secret detection or complete owner acceptance
follows from code merge or these local fixtures.

## Disable and roll back

Remove only the copied entries; preserve unrelated hooks and do not set global
`disableAllHooks`. After owned children terminate, remove only this hook's
metadata cache if desired. Use your ordinary paired client to clear this portal.
No pairing, auth, runtime state/schema or global settings change is needed.
