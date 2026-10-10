# Demand-layout capability and admission

## Current decision

The current decision is **source-evidenced NO-GO for a demand-layout prototype
that claims to solve the named giant-word workload through complete-word
deferral alone**. In pinned `cosmic-text 0.12.1`, that word remains one shaping
atom. A finer-grained continuation with proven full-context equivalence has
not been established through the current public API. This does not establish
that every optimization is impossible.

No backend prototype, package binding, parity experiment or performance
experiment was executed by this investigation. Its parity and performance
prototype gates are **NOT EXECUTED**. The earlier `hud-bke9c` experiments remain
separate, executed evidence; they are neither erased nor relabeled as gates of
this investigation. The original BKE ten acceptance criteria and T7.5 six
acceptance criteria remain pending, with their original budgets unchanged.

## Why complete-word deferral is insufficient

The pinned backend's `buffer.rs::Buffer::set_text` constructs complete
paragraph lines and calls `shape_until_scroll`. Finite-height layout limits
which paragraphs are visited; it does not bound the work inside the first
selected paragraph. `buffer_line.rs` caches complete shape and layout results.
In `shape.rs`, paragraph-wide bidi analysis and span/word/attribute itemization
precede shaping. Deferring later complete words is a credible unit of work,
but the named unbroken 4096-byte word is itself one such unit.

The pinned `rustybuzz 0.14.1` buffer retains at most five characters of pre/post
context. Its public unsafe-to-break flag is not a sufficient concatenation
proof: safe continuation requires compatible evidence on both sides, and the
relevant unsafe-to-concat mask is not publicly exposed through the current
path. Cosmic-text's output glyph representation also does not retain that
proof. A short Advanced prefix that overflows does not establish that the
fully contextual word overflows; later ligatures, kerning, bidi order and font
fallback can affect the complete result. One agreeing prefix, ASCII, a font
name or a repeated-character payload is not a general admission condition.

The HUD presentation includes more than one paragraph. All-fit decisions,
non-final horizontal overflow, whole-line head selection and wrapped tail
selection still need authoritative complete-context evidence. Full shaping
is the correct fallback where a demand path has no such proof; fallback-only
equality is not evidence of bounded demand work.

## Prior measurements and their limits

The earlier paired release samples used the retained burst parent, original
4096-byte payload, five empty warmups, 1000 µs Stage 4 ceiling and 250000 µs
frame ceiling. Both captured an eligible head cache miss: `PRIMED`, elapsed
at least 16 ms, forced false, MISS 1/HIT 0, viewport 332×268. The observed
presentation was 4127 bytes, including surrounding presentation text.

| Earlier source | Stage 4 | Initial paragraph `set_text` | Candidate/ellipsis `set_text` |
|---|---:|---:|---:|
| Matched baseline | 7530 µs | 2.125674 ms | 4.907246 ms |
| Short-prefix candidate | 19319 µs | 17.695395 ms | 1.374982 ms |

Both failed the unchanged Stage 4 assertion. This is one paired sample, not a
causal speedup/regression comparison or proof of font parsing, first-use
initialization or universal cost. Aggregate `set_text` timing does not identify
cosmic-text internals. Older debug measurements are not a release baseline.
Diagnostic eligibility delays and instrumentation can perturb execution and
cannot qualify a normal performance pass.

## Contract a future candidate must preserve

A demand path must retain the complete paragraph, line endings, `AttrsList`,
font selection, Advanced shaping, bidi context, metadata, cluster boundaries
and logical byte offsets. Partial materialization needs a distinct truthful
state; it must never occupy a cache entry whose `Some` means complete shape or
layout. Existing full shape/layout, edit and cursor consumers remain complete.

Preserve all-fit decisions, head/tail and whole-line selection, every relevant
non-final horizontal overflow check, final-line alignment and justification,
and combined prefix-plus-ellipsis shaping. Text/attrs changes, append/split,
cache clear/eviction and geometry changes must invalidate the appropriate
partial and complete state. Full materialization after partial work and clone
behavior must remain equivalent to complete upstream behavior. Per-buffer
lifetimes stay bounded; no unbounded global transcript cache is introduced.

Do not change fonts, Advanced shaping, public signatures, user-visible output,
versions, features, timer boundaries, thresholds or workload to obtain a pass.
Moving work before Stage 4, warming the exact payload, accepting a deferred
frame or raising slack does not solve this outcome.

## Finite next option and verification

The next finite option is an internal, same-version capability investigation
that proves a safe continuation or another sound whole-input witness for the
actual giant word. Start with `cosmic-text`'s `buffer.rs`, `buffer_line.rs` and
`shape.rs`; possible retained backend fixtures are `wrap_stability.rs` and
`wrap_word_fallback.rs`. HUD integration remains in compositor `overflow.rs`
and `text.rs`. Any required extra backend, rustybuzz or API owner needs a
separate evidenced scope decision. Naming files is not grounds to install a
vendor package or to bind an incomplete backend.

An admitted prototype must use a demonstrably **unmodified complete 0.12.1
comparator**, with package/source hashes, fonts and dependency identities
verified. A forced-complete path sharing modified code is insufficient as the
sole oracle. Compare output, truncation state, ordered glyph/font/face and
cluster identities, bidi order, logical ranges, positions and line metrics.
Extend at most the three existing backend parents and eight nearest HUD
parents; retain their original assertions and declarations. These cover the
long word, Arabic/mixed bidi, graphemes, head/tail, styling and cache behavior.
This source-only record adds no tests.

The conditional retained parents are:

- Backend: `stable_wrap`, `wrap_extra_line`, `wrap_word_fallback`.
- Overflow: `single_long_word_truncated_at_grapheme_boundary`,
  `rtl_arabic_truncation_no_leading_byte_drop`,
  `mixed_bidi_ltr_rtl_no_corruption`, `follow_tail_advances_by_whole_lines`,
  `tail_anchored_max_lines_one_shows_newest_with_leading_ellipsis`.
- Text: `regression_708b_effective_key_uses_bold_weight_and_truncates_more`,
  `truncation_cache_hit_after_prime`,
  `truncation_cache_different_geometry_different_entry`.

Require witnesses of actual demand execution, complete output after partial
work, and invalidation after content/attrs/geometry/append/reset/clone changes.
Deliberate partial-as-complete and stale-state controls must fail. Complete
fallback equality earns compatibility credit only. A GO requires the named
giant-word capability **and** parity, not merely deferring other words.

Then use separately admitted, source-bound cold matched release and separate
debug gates under the unchanged burst contract. Keep initial-paragraph and
candidate sites distinct. Stop on the first meaningful failure and retain its
raw outcome; do not retry until green. Source-only NO-GO records unexecuted
prototype gates explicitly; empirical NO-GO preserves every executed result
and marks only unreached stages unexecuted.

## Provenance, resources and rollback

Any separately approved repository-owned source binding must retain exact
`cosmic-text 0.12.1` provenance: crate SHA-256
`59fd57d82eb4bfe7ffa9b1cec0c05e2fd378155b47f255a67983cb4afe0e80c2`,
upstream commit `58c2ccd1fb3daf0abc792f9dd52b5766b7125ccd`, MIT OR Apache-2.0
licenses and upstream asset notices. Preserve the Rust 1.88 project toolchain,
`glyphon 0.8.0`/`wgpu 24.0.5` co-pin, existing features and transitive versions;
upstream's declared MSRV is 1.65. No registry edit, dependency upgrade,
production font substitution or nondefault shape-run-cache feature is implied.

Resource costs are unestablished. Account for simultaneous partial and complete
materialization, full paragraph/attribute/itemization storage, clone peaks,
font/cache residency, and duplicate comparator/candidate build artifacts.
Forecast and admit each source and run stage against the actual resource
ledger; do not count erased artifacts as recovered lifetime budget.

Before a separately admitted prototype run, preserve all five current BKE
diagnostic files as new same-inode archives and restore their exact committed
shipping originals using fresh live copies. Verify the full tracked source,
clean expected HEAD and zero diagnostic tags, retaining all historical variant
maps and failures. That restoration is not performed by this document change.
Before any future binding, preserve manifests, lockfile and caller originals.
A scoped rollback removes the binding and candidate changes, restores exact
upstream behavior, and retains all failed evidence; no data migration is needed.

Both NO-GO and GO require an ordinary deliverable pipeline through independent
review, current checks, normal merge queue and actual main before this
investigation closes. NO-GO uses proportional document verification; a shipping
GO additionally needs its admitted parity, cold release, separate debug and
full normal pipeline. A document landing is not a runtime fix and cannot close
the parent performance bug or migration.
