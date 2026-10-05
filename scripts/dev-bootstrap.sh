#!/usr/bin/env bash
# dev-bootstrap.sh — idempotent dev-host setup for a Linux/WSL2 checkout.
#
# Installs (or reports) everything `just ci` and the Windows cross-build need.
# Re-run it whenever a dependency is added: the manifest below is the single
# list to edit. Every step skips when already satisfied, so re-runs are cheap.
#
# Usage:
#   scripts/dev-bootstrap.sh            # install what can be installed, report the rest
#   scripts/dev-bootstrap.sh --check    # report only; exit 1 if anything required is missing
#
# apt packages need root. When passwordless sudo is unavailable the script
# prints the exact `sudo apt-get install` line for a human to run, then
# re-run this script to finish the remaining steps.
set -euo pipefail

# ── Manifest ────────────────────────────────────────────────────────────────
# Required apt packages (mirror the CI apt installs in .github/workflows/ci.yml).
APT_PACKAGES=(
    build-essential pkg-config cmake
    protobuf-compiler                       # tze_hud_protocol build (protoc >= 3.15)
    mesa-vulkan-drivers libvulkan1          # llvmpipe Vulkan ICD for GPU tests
    libegl-mesa0 libgl1-mesa-dri
    libssl-dev libfontconfig1-dev
    mingw-w64                               # x86_64-pc-windows-gnu linker + windres
)
# Required commands, as "command:how-to-get-it".
REQUIRED_COMMANDS=(
    "just:apt install just, or a release binary in ~/.local/bin"
    "uv:https://docs.astral.sh/uv/ (installs the Python dev venv)"
)
# Rust targets added to the toolchain pinned by rust-toolchain.toml.
RUST_TARGETS=(x86_64-pc-windows-gnu)
# Optional cargo tools: their `just` gates SKIP loudly when absent.
CARGO_TOOLS=(cargo-deny cargo-machete)
# Python deps live in scripts/requirements-dev.txt (shared with CI).
PY_REQUIREMENTS=scripts/requirements-dev.txt
VENV=.venv

# ── Plumbing ────────────────────────────────────────────────────────────────
CHECK_ONLY=0
case "${1:-}" in
    --check) CHECK_ONLY=1 ;;
    "") ;;
    -h|--help) sed -n '2,14p' "$0"; exit 0 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
esac

cd "$(git -C "$(dirname "$0")" rev-parse --show-toplevel)"

missing=0
ok()   { printf '  ok      %s\n' "$*"; }
fix()  { printf '  fixed   %s\n' "$*"; }
miss() { printf '  MISSING %s\n' "$*"; missing=1; }
warn() { printf '  warn    %s\n' "$*"; }
section() { printf '\n%s\n' "$*"; }

# ── apt packages ────────────────────────────────────────────────────────────
section "apt packages"
apt_missing=()
for pkg in "${APT_PACKAGES[@]}"; do
    if dpkg-query -W -f='${Status}' "$pkg" 2>/dev/null | grep -q 'install ok installed'; then
        ok "$pkg"
    else
        apt_missing+=("$pkg")
    fi
done
if ((${#apt_missing[@]})); then
    cmd="sudo apt-get install -y --no-install-recommends ${apt_missing[*]}"
    if ((!CHECK_ONLY)) && sudo -n true 2>/dev/null; then
        sudo apt-get update -q && $cmd && fix "${apt_missing[*]}"
    else
        miss "${apt_missing[*]}"
        echo "          run: $cmd"
    fi
fi

# ── Commands ────────────────────────────────────────────────────────────────
section "commands"
for entry in "${REQUIRED_COMMANDS[@]}"; do
    c=${entry%%:*}
    if command -v "$c" >/dev/null; then ok "$c"; else miss "$c ($([[ $entry == *:* ]] && echo "${entry#*:}"))"; fi
done
if command -v protoc >/dev/null; then
    v=$(protoc --version | awk '{print $2}')
    if printf '3.15\n%s\n' "$v" | sort -V -C; then ok "protoc $v"; else miss "protoc >= 3.15 (have $v; set PROTOC)"; fi
fi

# ── Rust ────────────────────────────────────────────────────────────────────
section "rust"
if ! command -v rustup >/dev/null; then
    miss "rustup (https://rustup.rs)"
else
    # `rustup show active-toolchain` installs the pinned toolchain if needed.
    if ((CHECK_ONLY)); then
        ok "rustup ($(rustup --version 2>/dev/null | awk '{print $2}'))"
    else
        rustup toolchain install >/dev/null 2>&1 || true
        ok "toolchain $(rustup show active-toolchain | awk '{print $1}')"
    fi
    installed=$(rustup target list --installed)
    for t in "${RUST_TARGETS[@]}"; do
        if grep -qx "$t" <<<"$installed"; then ok "target $t"
        elif ((CHECK_ONLY)); then miss "target $t"
        else rustup target add "$t" >/dev/null && fix "target $t"; fi
    done
    for tool in "${CARGO_TOOLS[@]}"; do
        if command -v "$tool" >/dev/null; then ok "$tool"
        elif ((CHECK_ONLY)); then warn "$tool (optional; its just gate skips)"
        elif command -v cargo-binstall >/dev/null; then cargo binstall -y --locked "$tool" && fix "$tool"
        else cargo install --locked "$tool" && fix "$tool"; fi
    done
fi

# ── Python ──────────────────────────────────────────────────────────────────
section "python ($VENV)"
if command -v uv >/dev/null; then
    if ((CHECK_ONLY)); then
        if [[ -x $VENV/bin/python3 ]] && uv pip install --dry-run --python "$VENV/bin/python3" -r "$PY_REQUIREMENTS" 2>&1 | grep -q 'Would make no changes'; then
            ok "$PY_REQUIREMENTS satisfied"
        else
            miss "$VENV is absent or out of date with $PY_REQUIREMENTS"
        fi
    else
        [[ -x $VENV/bin/python3 ]] || uv venv -q "$VENV"
        uv pip install -q --python "$VENV/bin/python3" -r "$PY_REQUIREMENTS"
        ok "$PY_REQUIREMENTS installed into $VENV"
    fi
fi

# ── WSL2 → Windows HUD (warnings only) ──────────────────────────────────────
if grep -qi microsoft /proc/sys/kernel/osrelease 2>/dev/null; then
    section "wsl2"
    if [[ -e /proc/sys/fs/binfmt_misc/WSLInterop || -e /proc/sys/fs/binfmt_misc/WSLInterop-late ]]; then
        ok "interop enabled (Windows .exe files run from WSL)"
    else
        warn "interop disabled: tze_hud.exe cannot be launched from WSL"
    fi
    command -v taskkill.exe >/dev/null \
        || warn "Windows System32 not on PATH; call /mnt/c/Windows/System32/taskkill.exe by full path"
    # Agents reach the Windows HUD over the tailnet; mirrored mode breaks both
    # loopback and tailnet routes to it (docs/development/windows.md).
    mode=$(wslinfo --networking-mode 2>/dev/null || echo unknown)
    if [[ $mode == mirrored ]]; then
        warn "networking mirrored: WSL can't reach the Windows HUD (use NAT + Tailscale)"
    else
        ok "networking $mode"
    fi
    if ! command -v tailscale >/dev/null; then
        warn "tailscale not installed: WSL agents can't reach the Windows HUD"
    elif ! tailscale status >/dev/null 2>&1; then
        warn "tailscale not logged in (sudo tailscale up)"
    else
        ok "tailscale up ($(tailscale ip -4 2>/dev/null | head -1))"
    fi
    mem_gb=$(awk '/MemTotal/ {printf "%d", $2/1048576}' /proc/meminfo)
    ((mem_gb >= 12)) || warn "${mem_gb} GB RAM: release links may OOM; use -j 8 or raise .wslconfig memory="
    [[ $PWD == /mnt/* ]] && warn "checkout is on /mnt; cargo and git run far slower than on ~/"
fi

echo
if ((missing)); then
    echo "Some required pieces are missing (see MISSING above)."
    exit 1
fi
echo "Dev host ready."
