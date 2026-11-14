# Progress/status current-owner baseline

Seven unaltered own-frame PNGs from the paired owner's already-running build
`24818c39c44dba4ffbe596aa1edc92d849f893d9`, before this package's restyle.
The exact [fixture](../../../.claude/skills/user-test/scripts/progress-status-restyle-test.json)
contains progress 0/50/100 (info severity) and all four declared status values,
online/away/busy/offline. It sends no owner-only theme or fill parameters.

Authenticated discovery and admin status confirmed both target widgets were
not held by this pairing and absent from the drawn-widget observations before
publishing. Capture was one second after each accepted update at 3840×2160.
Each publish had a finite 15-second TTL. Progress was allowed to expire before
status captures; cleanup used TTL only. No shared clear, input read/ACK,
deployment, restart, pairing, permissions, theme, font or scale change occurred.
The owner's selected theme and DPI/UI scale are not exposed by status and were
left unchanged. All seven originals were inspected and hashes verified;
[before.json](before.json) binds the fixture, build, accepted publications and PNGs.

| Fixture index | Case | Original frame |
| --- | --- | --- |
| 0 | progress 0 | [PNG](before-owner-01.png) |
| 1 | progress 50 | [PNG](before-owner-02.png) |
| 2 | progress 100 | [PNG](before-owner-03.png) |
| 3 | online | [PNG](before-owner-04.png) |
| 4 | away | [PNG](before-owner-05.png) |
| 5 | busy | [PNG](before-owner-06.png) |
| 6 | offline | [PNG](before-owner-07.png) |

This is baseline evidence only; no widget restyle has been implemented in this
package yet. Actual headless AFTER captures under all three built-in themes,
and the owner-only style-group matrix under Tonal Glass, remain required.
Live owner AFTER/verdict remains in hud-h51u7.8.6. These streaming checkpoints
are not owner acceptance or pixel-equivalent cross-environment comparisons.
