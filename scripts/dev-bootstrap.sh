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
#   scripts/dev-bootstrap.sh --hooks-only # install/check only the guarded repository hook
#   scripts/dev-bootstrap.sh --hooks-only --check # report hook installation without writes
#
# apt packages need root. When passwordless sudo is unavailable the script
# prints the exact `sudo apt-get install` line for a human to run, then
# re-run this script to finish the remaining steps.
set -euo pipefail

# ── Manifest ────────────────────────────────────────────────────────────────
# Required apt packages (mirror the CI apt installs in .github/workflows/ci.yml).
APT_PACKAGES=(
    build-essential pkg-config cmake
    mold                                    # native Linux links; never replaces system ld
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
# Required test runner, pinned to the CI version (Rust build MSRV 1.88).
NEXTEST_VERSION=0.9.114
# Python deps live in scripts/requirements-dev.txt (shared with CI).
PY_REQUIREMENTS=scripts/requirements-dev.txt
VENV=.venv

# ── Plumbing ────────────────────────────────────────────────────────────────
CHECK_ONLY=0
HOOKS_ONLY=0
while (($#)); do
    case "$1" in
        --check) CHECK_ONLY=1 ;;
        --hooks-only) HOOKS_ONLY=1 ;;
        -h|--help) sed -n '2,16p' "$0"; exit 0 ;;
        *) echo "unknown argument: $1" >&2; exit 2 ;;
    esac
    shift
done

cd "$(git -C "$(dirname "$0")" rev-parse --show-toplevel)"

missing=0
ok()   { printf '  ok      %s\n' "$*"; }
fix()  { printf '  fixed   %s\n' "$*"; }
miss() { printf '  MISSING %s\n' "$*"; missing=1; }
warn() { printf '  warn    %s\n' "$*"; }
section() { printf '\n%s\n' "$*"; }
install_cargo_tool() {
    if command -v cargo-binstall >/dev/null; then cargo binstall -y --locked "$1"
    else cargo install --locked "$1"; fi
}

# ── Repository hook ─────────────────────────────────────────────────────────
# Repository-local config is shared by linked worktrees. Never mask an unowned
# hook, copy a user's config, or install anything during --check.
section "pre-push hook"
if ! command -v python3 >/dev/null; then
    miss "python3 is required to check/install the repository hook"
elif python3 - "$CHECK_ONLY" <<'PY'
import hashlib
import os
from pathlib import Path
import subprocess
import sys

class UnsafeHook(ValueError):
    pass

def git(*args, optional=False):
    result = subprocess.run(["git", *args], stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    if optional and result.returncode == 1:
        return b""
    if result.returncode:
        raise UnsafeHook("cannot verify repository hook metadata")
    return result.stdout

def inspect():
    effective = git("config", "--get-all", "core.hooksPath", optional=True).splitlines()
    local = git("config", "--local", "--get-all", "core.hooksPath", optional=True).splitlines()
    if effective not in ([], [b".githooks"]) or local != effective:
        raise UnsafeHook("custom hooksPath retained; merge hooks manually before installing")
    common = Path(os.fsdecode(git("rev-parse", "--git-common-dir").strip())).resolve()
    hooks = common / "hooks"
    if hooks.is_symlink() or (hooks.exists() and any(not p.name.endswith(".sample") for p in hooks.iterdir())):
        raise UnsafeHook("unowned common Git hooks retained; merge hooks manually before installing")
    shipped = Path(".githooks/pre-push")
    if shipped.parent.is_symlink() or shipped.is_symlink() or not shipped.is_file():
        raise UnsafeHook("shipped pre-push hook is absent or unsafe")
    if any(p.name != "pre-push" and not p.name.endswith(".sample") for p in shipped.parent.iterdir()):
        raise UnsafeHook("additional repository hooks retained; inspect them before installing")
    content = shipped.read_bytes()
    if content != git("show", "HEAD:.githooks/pre-push") or not os.access(shipped, os.X_OK):
        raise UnsafeHook("commit the shipped executable pre-push hook before installing")
    return effective, local, hashlib.sha256(content).digest()

try:
    before = inspect()
    if before[0] == [b".githooks"]:
        print("  ok      repository pre-push hook (existing setting retained)")
    elif sys.argv[1] == "1":
        raise UnsafeHook("repository pre-push hook is not installed (re-run bootstrap without --check)")
    else:
        if inspect() != before:
            raise UnsafeHook("hook/config changed during inspection; re-run bootstrap")
        git("config", "--local", "core.hooksPath", ".githooks")
        if inspect()[0] != [b".githooks"]:
            raise UnsafeHook("repository hook installation could not be verified")
        print("  fixed   repository pre-push hook")
except UnsafeHook as error:
    print("  MISSING " + str(error), file=sys.stderr)
    sys.exit(1)
except (OSError, ValueError) as error:
    print("  MISSING safe hook inspection failed (" + type(error).__name__ + ")", file=sys.stderr)
    sys.exit(1)
PY
then
    :
else
    miss "repository pre-push hook (no unsafe overwrite)"
fi

if ((HOOKS_ONLY)); then
    # Hook readiness is separate from apt/toolchain/linker/venv host readiness.
    exit "$missing"
fi

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
    install=(sudo apt-get install -y --no-install-recommends "${apt_missing[@]}")
    if ((!CHECK_ONLY)) && sudo -n true 2>/dev/null; then
        if sudo apt-get update -q && "${install[@]}"; then
            fix "${apt_missing[*]}"
        else
            miss "${apt_missing[*]} (apt-get failed; see output above)"
        fi
    else
        miss "${apt_missing[*]}"
        echo "          run: ${install[*]}"
    fi
fi

# The llvmpipe ICD the justfile pins GPU recipes to (its `lvp` variable).
if compgen -G '/usr/share/vulkan/icd.d/lvp_icd*.json' >/dev/null; then
    ok "llvmpipe Vulkan ICD"
else
    miss "llvmpipe Vulkan ICD (/usr/share/vulkan/icd.d/lvp_icd*.json; mesa-vulkan-drivers)"
fi

# ── Commands ────────────────────────────────────────────────────────────────
section "commands"
for entry in "${REQUIRED_COMMANDS[@]}"; do
    c=${entry%%:*}
    if command -v "$c" >/dev/null; then ok "$c"; else miss "$c (${entry#*:})"; fi
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
    # With no arguments, `rustup toolchain install` installs the toolchain
    # pinned by rust-toolchain.toml.
    if ((CHECK_ONLY)); then
        ok "rustup ($(rustup --version 2>/dev/null | awk '{print $2}'))"
    elif rustup toolchain install --no-self-update >/dev/null; then
        ok "toolchain $(rustup show active-toolchain | awk '{print $1}')"
    else
        miss "pinned toolchain (rustup toolchain install failed)"
    fi
    installed=$(rustup target list --installed)
    for t in "${RUST_TARGETS[@]}"; do
        if grep -qx "$t" <<<"$installed"; then ok "target $t"
        elif ((CHECK_ONLY)); then miss "target $t"
        elif rustup target add "$t" >/dev/null; then fix "target $t"
        else miss "target $t (rustup target add failed)"; fi
    done
    for tool in "${CARGO_TOOLS[@]}"; do
        if command -v "$tool" >/dev/null; then ok "$tool"
        elif ((CHECK_ONLY)); then warn "$tool (optional; its just gate skips)"
        elif install_cargo_tool "$tool"; then fix "$tool"
        else warn "$tool install failed (optional; its just gate skips)"; fi
    done
fi

# ── Required test runner ────────────────────────────────────────────────────
# Lookup/version only in --check: no install, discovery, config or PATH write.
section "cargo-nextest"
nextest_ready() {
    command -v cargo-nextest >/dev/null \
        && cargo nextest --version 2>/dev/null \
            | awk -v wanted="$NEXTEST_VERSION" '$1 == "cargo-nextest" && $2 == wanted { found=1 } END { exit !found }'
}
if nextest_ready; then
    ok "cargo-nextest $NEXTEST_VERSION"
elif ((CHECK_ONLY)) || ! command -v cargo >/dev/null; then
    miss "cargo-nextest $NEXTEST_VERSION (absent or wrong version)"
elif cargo install --locked --version "$NEXTEST_VERSION" cargo-nextest && nextest_ready; then
    fix "cargo-nextest $NEXTEST_VERSION"
else
    miss "cargo-nextest $NEXTEST_VERSION (locked install failed or wrong version)"
fi

# ── Native Linux linker ─────────────────────────────────────────────────────
# Cargo target flags leave MinGW/MSVC and unrelated user settings alone.
# --check reads/reports only. The package version may differ from pinned CI mold.
section "native Linux linker"
if ! command -v mold >/dev/null; then
    miss "mold (apt install mold)"
elif ! command -v rustc >/dev/null || ! command -v python3 >/dev/null; then
    miss "rustc and Python 3.11+ are required to check native Cargo linker settings"
else
    native_target=$(rustc -vV | sed -n 's/^host: //p')
    case "$native_target" in
        *-linux-*)
            cargo_config="${CARGO_HOME:-$HOME/.cargo}/config.toml"
            if python3 - "$CHECK_ONLY" "$cargo_config" "$native_target" <<'PY'
import copy
import fcntl
import json
import os
from pathlib import Path
import re
import stat
import sys
import tempfile

try:
    import tomllib
except ImportError:
    sys.exit("  MISSING Python 3.11+ (safe Cargo TOML parsing requires tomllib)")

check_only, filename, host = sys.argv[1:]
path = Path(filename)
class UnsafeConfig(ValueError):
    pass

try:
    if path.is_symlink() or path.with_name("config").exists() or path.with_name("config").is_symlink():
        raise UnsafeConfig("legacy/symlink Cargo config requires a manual native-linker merge")
    original = path.read_bytes() if path.exists() else b""
    text = original.decode()
    data = tomllib.loads(text)
    targets = data.get("target", {})
    native = targets.get(host, {})
    if not isinstance(targets, dict) or not isinstance(native, dict):
        raise UnsafeConfig("unsupported Cargo target configuration")
    if any(not isinstance(value, dict) for value in targets.values()):
        raise UnsafeConfig("unsupported Cargo target configuration")
    if any(name.startswith("cfg(") and "rustflags" in value for name, value in targets.items()):
        raise UnsafeConfig("conditional target rustflags require a manual native-linker merge")
    flags = native.get("rustflags", data.get("build", {}).get("rustflags", []))
    if not isinstance(flags, list) or not all(isinstance(flag, str) for flag in flags):
        raise UnsafeConfig("non-array rustflags require a manual native-linker merge")
    if any(flag.startswith(("linker=", "-Clinker=")) for flag in flags):
        raise UnsafeConfig("rustflags select a linker directly; existing settings were retained")
    driver = native.get("linker", "cc")
    if not isinstance(driver, str) or not re.search(r"(?:^|-)(?:cc|gcc|clang)(?:-[0-9.]+)?$", Path(driver).name):
        raise UnsafeConfig("native linker is not a recognized compiler driver; existing settings were retained")
    linker_flags = [flag for flag in flags if "fuse-ld=" in flag]
    if any(flag not in ["link-arg=-fuse-ld=mold", "-Clink-arg=-fuse-ld=mold"] for flag in linker_flags):
        raise UnsafeConfig("conflicting native linker flags; existing settings were retained")
    if any(flag == "link-arg=-fuse-ld=mold" and (index == 0 or flags[index - 1] != "-C") for index, flag in enumerate(flags)):
        raise UnsafeConfig("malformed native mold flags; existing settings were retained")
    if "rustflags" in native and linker_flags:
        print("  ok      native Cargo mold flags (existing flags retained)")
        sys.exit(0)
    if check_only == "1":
        raise UnsafeConfig("native Cargo mold flags are absent (re-run bootstrap without --check)")

    desired = flags if linker_flags else flags + ["-C", "link-arg=-fuse-ld=mold"]
    expected = copy.deepcopy(data)
    expected.setdefault("target", {}).setdefault(host, {})["rustflags"] = desired
    header = re.compile(r"(?m)^\s*\[target\.(?:" + re.escape(host) + r'|"' + re.escape(host) + r'")\]\s*(?:#[^\n]*)?$')
    match = header.search(text)
    encoded = json.dumps(desired)
    if host not in targets:
        candidate = text.rstrip() + "\n\n[target." + host + "]\nrustflags = " + encoded + "\n"
    elif match is None:
        raise UnsafeConfig("unsupported target-table layout; existing settings were retained")
    elif "rustflags" not in native:
        candidate = text[:match.end()] + "\nrustflags = " + encoded + text[match.end():]
    else:
        # Append within the existing array, preserving its comments and every
        # unrelated byte. A full TOML semantic comparison selects the real ']'.
        end = re.search(r"(?m)^\s*\[", text[match.end():])
        limit = match.end() + end.start() if end else len(text)
        key = re.search(r"(?m)^\s*rustflags\s*=", text[match.end():limit])
        if key is None:
            raise UnsafeConfig("unsupported rustflags layout; existing settings were retained")
        start = match.end() + key.end()
        candidate = None
        additions = json.dumps(["-C", "link-arg=-fuse-ld=mold"])[1:-1]
        for closing in [index for index in range(start, limit) if text[index] == "]"]:
            for separator in [", " if flags else "", " "]:
                attempt = text[:closing] + separator + additions + text[closing:]
                try:
                    if tomllib.loads(attempt) == expected:
                        candidate = attempt
                        break
                except tomllib.TOMLDecodeError:
                    pass
            if candidate is not None:
                break
        if candidate is None:
            raise UnsafeConfig("unsupported rustflags array; existing settings were retained")
    if tomllib.loads(candidate) != expected:
        raise UnsafeConfig("Cargo config merge changed unrelated settings; no write performed")
    path.parent.mkdir(parents=True, exist_ok=True)
    # Serialize bootstrap writers. External config editors need to remain idle;
    # their intervening content changes fail closed at the final check.
    lock = (path.parent / ".tze-hud-mold-config.lock").open("a")
    fcntl.flock(lock.fileno(), fcntl.LOCK_EX | fcntl.LOCK_NB)
    current = path.read_bytes() if path.exists() else b""
    if current != original or path.is_symlink():
        raise UnsafeConfig("Cargo config changed during merge; re-run bootstrap")
    mode = stat.S_IMODE(path.stat().st_mode) if path.exists() else 0o644
    fd, temporary = tempfile.mkstemp(prefix=".tze-hud-mold-", dir=path.parent)
    try:
        with os.fdopen(fd, "wb") as stream:
            stream.write(candidate.encode())
        if (path.read_bytes() if path.exists() else b"") != original or path.is_symlink():
            raise UnsafeConfig("Cargo config changed during merge; re-run bootstrap")
        os.chmod(temporary, mode)
        os.replace(temporary, path)
    finally:
        if os.path.exists(temporary):
            os.unlink(temporary)
    lock.close()
    print("  fixed   native Cargo mold flags (existing flags retained)")
except UnsafeConfig as error:
    # Do not print file contents or inherited flag values.
    print("  MISSING " + str(error), file=sys.stderr)
    sys.exit(1)
except (OSError, UnicodeError, ValueError, TypeError, AttributeError) as error:
    print("  MISSING Cargo config could not be safely read or merged (" + type(error).__name__ + ")", file=sys.stderr)
    sys.exit(1)
PY
            then
                ok "$(mold --version)"
            else
                miss "native Cargo linker configuration (no unsafe overwrite)"
            fi
            # Higher-priority environment flags may deliberately override Cargo
            # config. Report names only; never discard or print their values.
            native_flag_name="CARGO_TARGET_${native_target^^}_RUSTFLAGS"
            native_flag_name="${native_flag_name//-/_}"
            for flag_name in RUSTFLAGS CARGO_ENCODED_RUSTFLAGS "$native_flag_name"; do
                if [[ -v $flag_name ]]; then warn "$flag_name is set; verify the effective native linker"; fi
            done
            ;;
        *) warn "mold configuration applies only to native Linux targets" ;;
    esac
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
        if { [[ -x $VENV/bin/python3 ]] || uv venv -q "$VENV"; } \
            && uv pip install -q --python "$VENV/bin/python3" -r "$PY_REQUIREMENTS"; then
            ok "$PY_REQUIREMENTS installed into $VENV"
        else
            miss "$VENV install failed (see uv output above)"
        fi
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
    if [[ $PWD == /mnt/* ]]; then
        warn "checkout is on /mnt; cargo and git run far slower than on ~/"
    fi
fi

echo
if ((missing)); then
    echo "Some required pieces are missing (see MISSING above)."
    exit 1
fi
echo "Dev host ready."
