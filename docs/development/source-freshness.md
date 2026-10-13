# Shared Cargo target source identity

Isolated checkouts can supply incompatible cached types or test binaries when
they reuse one local Cargo target. Two observed runs in October 2026 did so:
three-field `WorkCounts` source loaded seven-field metadata (`E0063`), and a
three-argument projection caller loaded another checkout's two-argument metadata
(`E0061`). The latter run had already passed 3,064 Rust tests with zero skips.
Those passes did not establish that the later integration compilation used the
declared source. The complete Cargo-internal cause remains unproved.

Use the repository's opt-in source-binding policy for shared worker targets.
The coordinator first grants the exclusive Cargo/GPU lease and explicitly
selects a **new empty target under HOME** for a frozen, committed checkout.
It retains old targets and source fixtures. The guard chooses no target names,
changes no source mtimes, redirects no artifacts and deletes no caches.

```sh
# TARGET_EPOCH is an explicit new path supplied by the resource coordinator.
export TZE_HUD_SHARED_TARGET_GUARD=1
export CARGO_TARGET_DIR="$TARGET_EPOCH"
just check
just clippy
just --set py /path/to/existing/venv/bin/python3 ci
just pre-push
git push
```

Keep the same target and opt-in environment through the ordinary push. The
effective pre-push hook selects its existing checks, which inherit this guard
through `just pre-push-check`. A push whose existing selector needs no Cargo
checks still takes that established fast path. Hook configuration, ref selection
and the hook lock are unchanged. That lock and the target binding lock do not
grant the separate Cargo/GPU lease.

Every Cargo command in the canonical `just` gates, including metadata and tool
version probes, passes through `scripts/cargo_source_guard.py` when enabled.
`cargo-deny` distinguishes a completed unsuccessful availability probe from guard
refusal or abnormal child termination; only the completed probe retains its
existing `SKIPPED` behavior. Cargo arguments,
features, profiles, targets, nextest groups/retries, GPU controls, slack and
timeouts remain those of the recipes. With the opt-in variable unset, recipes
use bare Cargo and existing user defaults. Other nonempty opt-in values refuse.
Direct Cargo, arbitrary scripts and GitHub workflow commands are outside this
policy; all writers of a guarded target must cooperate.

## Binding and supported inputs

The guard requires execution from a checkout root, an absolute target outside
the repository/source fixtures, a clean index matching HEAD and materialized
content matching its Git objects. It hashes raw tracked bytes, names and modes,
canonical checkout/common-Git paths, actual HEAD, supported Cargo configuration,
the selected Rust 1.88 compiler identity and guard code. The four tracked
`.beads` administration files are not build inputs. All other tracked files are
included: separate integration tests, build scripts, lockfiles, protos, bundled
fonts, SVG/TOML bundles, manifests, compile-time docs and test fixtures.
`CARGO_MANIFEST_DIR` therefore cannot silently refer to another checkout.

Built-in Git line-ending conversion is supported without changing Git config.
Hidden index flags do not exempt changed bytes. Tracked symlinks must resolve
inside tracked source. Nonignored untracked files, ignored inputs in Cargo
package/asset/config roots, missing/sparse entries, Gitlinks, external path
dependencies and custom Git filters are refused. Generated `OUT_DIR`, receipts,
targets, worktrees, private operations files and Python caches are not hashed as
new source. Future compile/test inputs must be tracked within this supported
closure; external or ignored inputs require an explicit policy change.
Native Windows checkouts must materialize their tracked symlinks; Git's plain
file placeholders for disabled symlinks are outside this supported envelope.

The checkout config and source-owned ancestor Cargo configs under HOME are
fingerprinted. Linked worktrees can inherit the main checkout's tracked
`.cargo/config.toml`; it is included too. Non-project Cargo config presence is
refused before opening its contents. External Cargo-home/config/build-directory
overrides, custom compiler wrappers, Cargo aliases/configured environment or
target runners, untracked/escaping manifest paths and a different `--target-dir`
are unsupported. An explicit override naming the same canonical target or a
tracked manifest is supported. The policy trusts provisioned system toolchains, immutable
host settings and Cargo's registry checksums; it does not inspect credentials or
system configuration outside HOME.

The receipt prints only HEAD and identity/invocation digests plus child status.
Application arguments and relevant compilation environment values are hashed,
never printed or stored. Registry/credential variables are not read. Cargo keeps
its existing responsibility for same-source features, profiles, target-specific
flags and build-script environment variants such as `TZE_HUD_CHANNEL` and the
Windows-GNU Clippy `CARGO_FEATURE_PURE` setting. This guard binds source; it does
not implement Cargo's general configuration fingerprinting.

## Refusal, interruption and lifetime

One bounded versioned marker and one nonblocking OS lock belong to each explicit
target. The marker contains hashes, HEAD and validity only. An existing nonempty
unbound target, foreign binding, malformed marker, symlink state or contention
refuses before Cargo; there is no adoption, reset, cleanup or rebind option.
The guard marks the binding invalid before starting its owned Cargo child and
holds the target lock through the terminal result and source/config postcheck.
An interrupted, abnormally terminated or changed-source run cannot leave a
certified binding. A failed
child with a complete unchanged-source postcheck preserves its first exit code
and source binding; this does not authorize retrying a failed gate.
These snapshots detect observed changes within the cooperative frozen-source
contract; they do not prove prevention of arbitrary concurrent edits, an ABA
change restored between observations, or artifact tampering by another writer.

For a different checkout, commit, materialized bytes, source-owned config or
compiler identity, the coordinator must select a fresh empty epoch under its
lease. Old artifacts and source evidence remain protected. The deliberate costs
are conservative source hashing and cold compilation for each new frozen source;
same-source gates and pushes reuse their warm epoch. No epoch is allocated per
command and no retention journal accumulates. Source-mutating `fmt-fix`, host
`bootstrap`/install and `dead-code`'s temporary-source compile refuse in guarded
mode; run those separately with source-isolated resources.

Preserve the first failed receipt and its source/target identity. A passing test
count or a later successful run does not diagnose the historical failure. Source
freshness and the separate unknown cache-deletion investigation are distinct.
Cleanup eligibility and retention still require an operator-owned policy;
mtime, a closed bead or this marker grants no deletion permission.

To roll back the harness, return to source-isolated targets before disabling the
opt-in guard. Keep suspect shared epochs protected rather than continuing their
cross-source reuse. Linux/Windows-GNU build checks do not establish actual native
Windows execution, owner-visible HUD pixels or a production rendering repair.
