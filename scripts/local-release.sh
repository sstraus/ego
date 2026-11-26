#!/usr/bin/env bash
# Build and atomically replace the local executable, including with mbx target views.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="$HOME/.local/bin/ego"
cd "$ROOT"

# Read Cargo's artifact path instead of assuming a target directory. Do not pass
# --message-format overrides: this stream selects the executable to install.
binary=$(cargo build --release --locked -p ego-cli --bin ego "$@" --message-format=json |
  python3 -c '
import json, sys
binary = None
for line in sys.stdin:
    item = json.loads(line)
    if item.get("reason") == "compiler-artifact" and item.get("target", {}).get("name") == "ego" and item.get("executable"):
        binary = item["executable"]
if not binary:
    sys.exit("Cargo did not report an ego executable")
print(binary)
')
mkdir -p "$(dirname "$BIN")"
staged=$(mktemp "${BIN}.new.XXXXXX")
trap 'rm -f "$staged"' EXIT
install -m 755 "$binary" "$staged"
mv -f "$staged" "$BIN"
# ego has no daemon lifecycle command. Existing sessions keep their executable;
# new terminal/ACP/MCP sessions use the replacement. Do not stop other sessions.
"$BIN" --version
