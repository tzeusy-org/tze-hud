---
name: user-test
description: Pair with a Windows HUD, update to dev, publish test content, capture a HUD screenshot, and read logs from an agent host without remote shell access.
metadata:
  owner: tze
  authors:
    - tze
    - OpenAI Codex
    - Claude
  status: active
  last_reviewed: "2026-11-23"
---

# User Test

Run the existing mechanical checks with `just gesture` and the hosted
[Windows gesture workflow](../../../docs/development/windows.md#running-the-gates).
Use this workflow on a designated dev HUD or for the owner checks listed in
[T13](../../../docs/scope.md).

Run from the repo root. Set `HUD_HOST` to the designated host, `host:port`, or
HTTP URL. Bracket IPv6 addresses. The commands read the selected host's paired
key from its private file. Keep keys out of commands, output, and evidence.
See [connection setup](../th-hud-publish/references/connection.md) for host
selection, pairing files, and the stdio adapter.

1. **Pair.** For a first install the owner launches `tze_hud.exe` and reads the
   six-digit code from the HUD. Use an approved admin pairing for update,
   screenshots, and logs. With a valid existing admin pairing, proceed to update.
   Re-pairing the same agent rotates its key.

   ```bash
   export HUD_HOST=hud-host.example
   PAIR=.claude/skills/user-test/scripts/hud_pair.py
   python3 "$PAIR" --code "$PAIR_CODE" --admin
   ```

   `PAIR_CODE` is the current owner-provided code. If pairing is closed, the
   owner can reopen it with `tze_hud.exe --pair` or Ctrl+Shift+F10.

2. **Update.** Run the commands below. If the update responds with `updating`,
   wait for status to report the returned `sha` after the HUD relaunches. An
   `up_to_date` response confirms dev is already running. Keep the same key.

   ```bash
   ADMIN=.claude/skills/user-test/scripts/hud_admin.py
   python3 "$ADMIN" update --channel dev
   python3 "$ADMIN" status
   ```

3. **Publish.** Discover before publishing test content requested for this run.

   ```bash
   PUBLISH=.claude/skills/th-hud-publish/scripts/publish.py
   python3 "$PUBLISH" --list-surfaces
   ```

   After successful discovery, use the reported surface names and parameter
   types. This example assumes both surfaces are listed. If no widget is
   listed, record that and run the zone step. Use an allocated, unheld widget
   on a dedicated idle pairing.

   ```bash
   python3 "$PUBLISH" --zone subtitle --content "Checking the dev HUD" --ttl-ms 60000
   python3 "$PUBLISH" --surface widget:main-progress --params '{"progress":0.5}' --ttl-ms 60000
   ```

   The same command accepts a mixed `--messages-file`. Check every receipt and
   retain tool failure codes and hints.
   See [sample payloads](references/message-payloads.md).

4. **Screenshot.** Run `python3 "$ADMIN" screenshot -o /tmp/hud.png` while the
   content is live. Open the PNG and confirm the requested content. Record any
   clipping or unexpected placement. Let test content expire after capture.

5. **Logs.** On failure, run `python3 "$ADMIN" logs --tail 200` and check
   `python3 "$ADMIN" status` for `last_update`, `last_restart`, and `safe_mode`.
   Share only the relevant, redacted evidence. For a connection failure, run the
   bounded read-only `python3 "$ADMIN" doctor`.
   [Windows diagnostics](../../../docs/operations/windows-install.md) documents
   the returned status and next steps.

For automated coverage, use the existing Windows smoke with its owned process.
Its authorized `--url` mode publishes on an idle dedicated pairing with no
pending work and reports which owned-process checks were skipped.
See [Windows development](../../../docs/development/windows.md#running-the-gates).
