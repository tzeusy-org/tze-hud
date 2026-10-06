# Portal theme parser verification — hud-x38zn.7

Executed source `b2f58cd757bdfe358d767d9fbb10c5bd92d9ab39`, baseline `e5e8044550352d2df42e0c903ca4006d2353eeaa`. Only themes.rs and docs/api.md change production/docs behavior; no palettes/defaults/schema/API/renderer edits. Tests: +0~2-0, seven existing theme declarations retained.

| Gate | Actual exit | Positive results |
|---|---:|---|
| Initial fmt check | 1 | Import order and one line wrap only |
| Scoped themes.rs formatter | 0 | Formatter-only delta, ROOT refreeze accepted |
| Formatted fmt check | 0 | Frozen b2f source |
| Existing theme fixtures | 0 | 7 passing |
| Existing portal token fixtures | 0 | 33 passing |
| ONE normal just ci | 0 | 3245 Rust / 70 summaries, all72 named owner-qualified, default17, integration170/14 |

CI raw SHA `fa9564127debde86442b7ae53d8d0d182e4af26563c9a50c06a6eb90831cb4b9`, 689.027s. Zero Rust failures/ignored/GPU skips. Only unavailable tool: pwsh; the tests-only integration metadata exclusion is separate. Parent software flags unset; normal recipes set HEADLESS_FORCE_SOFTWARE=1 and receive pinned llvmpipe/LIBGL1/REQUIRE_GPU1. Default parallel, hard PERF off. No redundant standalone workspace/check/Clippy or extra GPU run.

Original gap was source-inferred, not a claimed red behavior run. Initial metadata freeze runner reused freeze-* stdout names; immutable initial JSON and Root qualification preserve that limitation. Raw test/failure logs are lossless and retained.

Theme6/Classic11 values, restyle/live screenshots, host settings/deployment/model/credential actions remain separate. Cargo+GPU released and independently ROOT accepted before publication. Independent PR review/normal merge queue required; no worker lifecycle mutations.
