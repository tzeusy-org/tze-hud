#!/usr/bin/env bash
# Check Git tracking, while allowing local and CI-generated evidence files.
set -euo pipefail

repo_root="$(git rev-parse --show-toplevel)"
# pipefail preserves a Git error instead of treating it as empty output.
tracked_bytes="$(git -C "$repo_root" ls-files -z -- test_results .handoff | wc -c)"
if (( tracked_bytes != 0 )); then
    printf 'Evidence roots must be untracked: test_results/ and .handoff/\n' >&2
    exit 1
fi

printf 'Evidence roots are untracked.\n'
