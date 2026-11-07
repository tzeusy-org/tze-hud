#!/usr/bin/env bash
# Regenerate the sole Python gRPC bindings from the canonical protocol sources.
set -euo pipefail

if [[ $# -gt 1 || ${1:-} == --help || ${1:-} == -h ]]; then
    echo "Usage: $0 [output-directory]"
    echo "Requires grpcio-tools; see user-test-performance/SKILL.md."
    [[ $# -le 1 ]] && exit 0
    exit 2
fi

repo_dir="$(cd -- "$(dirname -- "$0")/.." && pwd)"
output_dir="${1:-$repo_dir/.claude/skills/user-test-performance/scripts/proto_gen}"
proto_python="${TZE_HUD_PROTO_PYTHON:-python3}"

"$proto_python" - "$repo_dir" "$output_dir" <<'PY'
import re
import sys
from pathlib import Path

try:
    from grpc_tools import protoc
except ImportError:
    raise SystemExit("grpcio-tools is required; see user-test-performance/SKILL.md") from None

source = Path(sys.argv[1]) / "crates/tze_hud_protocol/proto"
output = Path(sys.argv[2]).resolve()
files = [source / f"{name}.proto" for name in ("types", "events", "session")]
for path in files:
    if not path.is_file():
        raise SystemExit(f"Missing protocol source: {path}")
output.mkdir(parents=True, exist_ok=True)
result = protoc.main([
    "protoc", f"-I{source}", f"--python_out={output}",
    f"--grpc_python_out={output}", *(str(path) for path in files),
])
if result:
    raise SystemExit(result)
for path in output.glob("*_pb2*.py"):
    text = re.sub(
        r"^import ((?:types|events|session)_pb2) as (\w+)$",
        r"from . import \1 as \2", path.read_text(), flags=re.MULTILINE,
    )
    path.write_text(text)
(output / "__init__.py").touch()
print(f"Generated Python bindings in {output}")
PY
