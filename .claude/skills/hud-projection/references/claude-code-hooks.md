# Claude Code session state and optional portal mirror

Report the state of an interactive Claude Code session through its paired
stdio MCP adapter. To show safe main-session tool progress and final replies
in a portal, set `TZE_HUD_MIRROR=1` in that workspace's Claude settings
`env`. Explicit portal publication and input collection remain available.
The [five-hour usage gauge](claude-code-usage.md) has separate settings.
This transport supersedes the independent hooks from `hud-o4ixb.1`.

Use an interactive Claude Code host on Linux or WSL2 with Python 3.10+.
The HUD may run on Windows. Claude Code 2.1.330 was verified against the
[official hooks reference](https://code.claude.com/docs/en/hooks) and in an
isolated scripted session using the project and user-scope stdio launchers.
Prompt-bound events require the documented `prompt_id`.

## Enable this block

Start the paired stdio MCP adapter from the same Claude session. Merge these
entries into your chosen Claude settings file and preserve unrelated settings.
Use absolute interpreter and repository paths. The separate `command` and
`args` make Claude exec the hook directly. Trust the workspace explicitly.
Keep the SessionStart matcher restricted to startup, resume and clear. Use an
ordinary hook configuration without `asyncRewake`, decision output or new
model calls.

For an existing hand-pasted hook block, copy the Notification entry to pick
up permission, input and idle signals. Set `TZE_HUD_MIRROR=1` in the
workspace's `env` to keep the earlier visible portal mirror.

<!-- example: partial -->
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
    "Notification": [
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
        "matcher": "^(permission_prompt|idle_prompt|elicitation_dialog)$"
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

## Events and privacy

| Event | Adapter behavior |
|---|---|
| UserPromptSubmit, or a tool event in an open prompt | Report working |
| Main PreToolUse AskUserQuestion | Report blocked with reason question |
| Notification permission_prompt or elicitation_dialog | Report blocked with reason permission or input and remember the asker |
| Main completion of AskUserQuestion, or a later tool call from the remembered permission/input asker | Resume working |
| Main Stop or Notification idle_prompt | Report done |
| SessionEnd and SessionStart during /clear | Clear the old mirrored portal and keep the MCP scope |
| SessionEnd on exit, EOF, or a clean shutdown signal | Release the MCP scope and its owned content |

The hook accepts up to 64 KiB of JSON on stdin and emits one normalized
packet of at most 16 KiB. It drops raw tool inputs and results, command
descriptions, cwd, transcript names, and notification message text. Known
tool names become fixed labels. The optional mirror displays main-session
tool events and the final reply. It removes escape and control characters from
final replies, rejects recognized credential forms and truncates at an
8 KiB UTF-8 boundary. Keep secret final replies out of sessions with the
mirror enabled.

## Transport and ordering

The hook and adapter find the socket by their immediate Claude parent PID.
The path is `$XDG_RUNTIME_DIR/tze-hud/claude-<pid>.sock` when the runtime
directory belongs to the user, with `~/.cache/tze-hud/run` as the fallback.
The adapter checks ownership and private modes. Both ends verify that the
peer has the same Unix UID. The hook uses one nonblocking connect and send,
then exits silently. The hook has no credential or HTTP path. Only the existing
adapter worker uses the paired credential for a HUD request. The design uses
one adapter per session and no daemon, heartbeat or polling.

The adapter keeps prompt boundaries, activity and mirror ordering in memory.
It drops late tool events for closed prompts and replaces a running tool
stage only with a terminal stage. It coalesces queued updates and uses one
bounded internal HTTP lane beside the four model-call slots. The mirror renews
a finite `hud_hold(ttl_ms=600000)` after a successful publish when the last
acknowledged hold is at least 60 seconds old. A failed delivery can leave an
already accepted request visible until its hold expires or the scope ends.

## Verification and remaining acceptance

Production-path tests cover silent delivery, runtime path and peer checks,
ordering, isolated scopes, model capacity, cleanup and re-initialization.
The live Claude Code PID check compares the adapter with SessionStart, main
PreToolUse, subagent PreToolUse and a permission Notification in both launcher
forms. The overhead gate times 200 invocations per event on the hook host,
with p95 below 50 ms and a recorded maximum. Paired owner visualization still
requires the separate mirrored portal acceptance.

## Disable

Remove these hook entries to stop session state updates, or remove
`TZE_HUD_MIRROR` from the workspace settings to keep state updates with the
portal mirror disabled. The adapter tears down its private socket and
ends its scope on exit.

## Held pull requests

`hud_pr.py` runs on the Linux or WSL agent host with Python 3.10+, an authenticated
`gh`, and a paired HUD. From this checkout:

```sh
python3 .claude/skills/hud-projection/scripts/hud_pr.py hold 123
python3 .claude/skills/hud-projection/scripts/hud_pr.py list
python3 .claude/skills/hud-projection/scripts/hud_pr.py release 123
```

`hold` adds `owner-merge` and starts one detached watcher. Pass
`--review-url https://example.com/review/123` to use a specific HTTPS review
page. Run `hold` again to change the next card's Review link or re-arm an
exited watcher. When multiple HUDs are paired, set `HUD_HOST` to select one.
Without a selected pairing, `hold` still labels the PR and reports
`no HUD pairing: no card`. The fleet and agent sessions skip labelled PRs.

When a held PR targeting `main` passes its required checks, the watcher can
show a ten-minute card captioned `PR #123`. Review opens the page and keeps
the card. Merge re-checks the label and asks GitHub to enqueue the exact
commit shown by the card. A push can make that request fail, and the watcher
reports the failure before watching the current head. Later, ×, and expiry
bring the card back after an hour if the same head is still green and held.

`list` shows open held PRs and their local status, including the UTC return
time after Later. A watcher stops on merge, closure, release, successful
queueing, or after 24 hours. A queue outcome stays up for up to 15 seconds
within that window. `release` removes the label and stops the local watcher,
which releases its card's HUD session. A hard-killed watcher's card
expires by its TTL. State and logs are private files under
`${XDG_STATE_HOME:-~/.local/state}/tze-hud/hud_pr/`.
