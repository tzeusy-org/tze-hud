# Subtitle pill baseline

These three unaltered PNGs capture the paired owner HUD's own rendered frame
on build `24818c39c44dba4ffbe596aa1edc92d849f893d9`, before the subtitle restyle.
The build was already running; no update, restart, theme, font or scale setting
was changed. Its admin status does not expose the selected theme or UI scale.

The fixture is
[subtitle-pill-test.json](../../../.claude/skills/user-test/scripts/subtitle-pill-test.json).
Its three sequential publications exercise a short line, a longer paragraph and
an oversized paragraph. The script captured the default primary display one
second after each accepted publication. These are observed streaming checkpoints,
not proof that the old build renders every intermediate or final line correctly.
Each publication used a 20-second TTL; cleanup used expiry, with no shared clear,
input polling or ACK. Authenticated discovery found the single subtitle surface
free before publication. Original PNG hashes, dimensions and publish receipts
are in [before.json](before.json).

| Fixture index | Original frame |
| --- | --- |
| 0 — short | [before-owner-01.png](before-owner-01.png) |
| 1 — paragraph | [before-owner-02.png](before-owner-02.png) |
| 2 — overflow | [before-owner-03.png](before-owner-03.png) |

The implementation and three-theme AFTER evidence are pending. This baseline
neither delivers the pill nor records owner visual acceptance. Headless AFTER
captures must replay the same fixture and report source, renderer, resolution,
scale, checkpoint and hashes; they cannot claim pixel parity with this host.
