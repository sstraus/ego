#!/bin/sh
# Run a client with no inherited credentials, config, or provider daemon.
set -eu
: "${EGO_BIN:=$(command -v ego)}"
base="${HOME}/Gits/.tmp/ego-acp-clients"
mkdir -p "$base"
run=$(mktemp -d "$base/run.XXXXXX")
mkdir -p "$run/home" "$run/workspace"
cat > "$run/home/config.toml" <<'EOF'
[profile.example]
model = "ollama/qwen3"
thinking = "off"
sandbox = "off"
mode = "default"
EOF
printf 'Isolated state: %s\n' "$run"
# Keep the state for inspection; remove this printed directory after use.
exec env -i PATH="$PATH" HOME="$run/home" EGO_HOME="$run/home" \
  OLLAMA_HOST=127.0.0.1:1 EGO_BIN="$EGO_BIN" ACP_WORKSPACE="$run/workspace" "$@"
