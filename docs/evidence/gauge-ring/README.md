# Gauge ring evidence

The approved ring is the 80×80 design in the 128dp-wide card in `Hud.dc.html` from artifact [7U7xiKxWGAptBz7GaqQ2DN](https://claude.ai/artifact/7U7xiKxWGAptBz7GaqQ2DN). The retained owner export has SHA256 `fcc79bbc287f47b291287949c3023bd73e3ad958661b6ae492d911174905ccdb`. Its 18dp centered readout uses `type.gauge.readout.size`; the shared readout role remains unchanged. The resulting logical card is 128×132.

The owner’s `gauge-ring-or-bar = B` decision (2026-11-14) permits Windows CI captures to replace the original owner BEFORE prerequisite. BEFORE consists of twelve real Windows/WARP own-frame captures from [run43582366817](https://github.com/tzeusy-org/tze-hud/actions/runs/43582366817), source `8c8390a43fbdb7d7f982f02b19e92d8e93866261`, fullscreen 1024×768, three themes, four checkpoints of the unchanged `gauge_cycle_test.json`. All three gauge bundle files and the exact git fixture blob match main `04ac25fee05f04cc0c70959e630f99ae9e1fb92d`. The raw Windows fixture hash is for CRLF checkout bytes; the git file uses LF. [before.json](before.json) records that provenance. These are CI observations, and do not establish owner visual acceptance.

AFTER source `8e0cd1249c93efc9ffb9b2e40ed6c5ec92968a9f` uses the real production startup/windowed build and GPU capture seam (`just render-scene`), llvmpipe, 1920×1080, scale1.0, checkpoint1000ms. There are 39 independent frames: the same four cycle payloads plus all nine low/mid/high × info/warning/error payloads in each theme. Each invocation replays a single message, so latest-wins cannot erase earlier cells. [after.json](after.json) records the exact payloads, fixture hashes, compiled-example hash, renderer, source head and PNG hashes. Only evidence files are added after this captured source head.

Open [gallery.html](gallery.html) locally for enlarged crops of the original frames; every crop links to its unchanged full PNG. BEFORE and headless AFTER differ in size and renderer and are not pixel-compatible comparisons. The final PR’s `windows.yml --captures` artifact additionally captures the original cycle and full matrix in the same native Windows harness. Its automatic main-baseline lookup may have no gauge cells; the explicit BEFORE run above remains the manual baseline. GPU tests separately cover endpoint levels, severity/owner fill, readout/label/local hover, per-instance work, unchanged frames, hidden/TTL, theme/resize and stacking. The existing `widget-hover` gesture now exercises both status and gauge with real pointer input.

Live AFTER captures and the owner’s gauge verdict remain in **hud-h51u7.8.6** after deployment. No owner theme, configuration, installed build, pairing or permissions were changed for these captures.

| Theme | Original cycle | Full cross matrix |
|---|---|---|
| tonal-glass | checkpoints1–4: 25%info, 50%warning, 95%error, 42%info | 25%,50%,95% for each of info,warning,error |
| classic | checkpoints1–4: 25%info, 50%warning, 95%error, 42%info | 25%,50%,95% for each of info,warning,error |
| blueprint | checkpoints1–4: 25%info, 50%warning, 95%error, 42%info | 25%,50%,95% for each of info,warning,error |
