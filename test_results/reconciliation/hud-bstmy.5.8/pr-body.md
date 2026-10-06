When a widget was already animating, a zero-duration publication could keep the old animation running. Cancel it before resolving parameters so an instant publish snaps immediately, including when it repeats the old target.

Extend the existing retarget test for same-target and different-target interruption, and clarify the API documentation. The original positive-duration behavior remains covered. The shared test Rig now fails closed when GPU tests are required but explicitly skipped or no adapter is available.

Tests: +0 ~1 -0.

Validation on frozen source `3c45386a7d29de6702d520f150809e9ec38d2e42`:

- Eight GPU transition tests passed; both instant-retarget assertion markers emitted. Existing raster (1), scene widget (44), MCP hold/clear (1), and gRPC widget stream (7) tests passed.
- The intentional required-GPU skip negative selected one test and exited 101 at setup, before adapter initialization. Its raw failure is preserved separately.
- One normal `just ci` passed: 3,245 Rust tests across 70 summaries, no failures/ignored tests, current positive receipts for all 72 named invariants, and all 17 default dependency closures. No GPU skips; PowerShell overlay contract skipped because `pwsh` is unavailable.

PERF_ASSERT stayed off. The original raster fixture still permits a silent NoAdapter return, so its process pass alone is limited evidence. Shared animation/deadline state was tested; live Windows/windowed wake timing was not measured. Unmerged W2 dev counters are outside this source. Durable raw receipts, source hashes, failures and limitations are in `test_results/reconciliation/hud-bstmy.5.8/`.
