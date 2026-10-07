Classic now declares all 77 existing semantic roles using the installed black/navy/white palette. The 13 legacy values, built-in names/default, canonical token registry and production parser remain unchanged. Explicit shared values are declarations, rather than fallback inheritance.

Tests: +0~2-0. The existing full semantic-set fixture includes Classic, proves declared/resolved value provenance and checks 14 opaque text/fill pairs. The existing layering fixture proves explicit semantic overrides and unchanged nonsemantic fallback. All other five theme fixture bodies are unchanged. This is not a 77-value Rust golden.

Focused themes: 7 passed. One normal local CI: actual exit 0 in 579.134s; 3245 Rust passes across 70 summaries, 0 failed/ignored; all72 named source/binary-qualified passes; default17 passed; integration 170 passes across 14 summaries; GPU skips 0. Raw SHA256 37d56d50b95ffbd952f16c91efcfe0ff01795c6631b8386262dc78ad3fad8066.

Execution source: d7a9fb9a280869dc5ee44e1b5a14039a2848758a on baseline de71d179d27c84a034ed61c1917bb963cdb9dff0; full543 source files before/after equal. Publication adds evidence only and must preserve these bytes. Later main/parser composition was not locally executed. Initial formatting failure and owned-file formatter recovery are retained, as are metadata-only schema/comment-boundary diagnostics; no behavior failure is inferred from those diagnostics.

Limits: ordinary Linux llvmpipe execution and GNU cross-compile; hard PERF_ASSERT off. No native Windows/constrained-host, screenshot, visual owner signoff, transparency/wallpaper/state-layer or arbitrary-override contrast claim. The pwsh tool-gated check and integration test-only metadata skips remain qualified in the raw evidence. No host/settings/font installation, renderer, portal palette, Blueprint12 or theme6 change.

Rollback: revert the bounded Classic/theme-tests/docs change, without persistence migration. Downstream theme6 and parent restyle/physical owner obligations remain separate. Root alone owns Beads lifecycle and merge disposition.
