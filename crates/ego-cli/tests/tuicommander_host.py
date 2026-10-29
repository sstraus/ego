#!/usr/bin/env python3
"""Launch the real-host Rust matrix with task-owned debug daemon prerequisites."""
import argparse
import json
import os
from pathlib import Path
import secrets
import socket
import subprocess
import tempfile
from datetime import datetime, timezone
import urllib.request


def recorded_model_facts(model, scratch, evidence):
    """Record public provider data; constrain the scenario without inferred limits."""
    source = "https://openrouter.ai/api/v1/models"
    with urllib.request.urlopen(source, timeout=20) as response:
        rows = json.load(response)["data"]
    selected = model.removeprefix("openrouter/")
    wanted = {selected, "openai/gpt-oss-20b", "openai/gpt-oss-120b"}
    rows = [row for row in rows if row["id"] in wanted]
    if not any(row["id"] == selected for row in rows):
        raise ValueError("selected model is absent from the current provider catalogue")
    captured = datetime.now(timezone.utc).isoformat()
    public = {"operator_reservation_policy": "cache disabled; absent cache category uses full uncached input rate as reservation bound, not a published tariff", "reasoning_source": "https://openai.com/index/introducing-gpt-oss/", "reasoning_levels": ["low", "medium", "high"], "source": source, "captured_at": captured, "models": rows}
    Path(evidence).with_suffix(".models.json").write_text(json.dumps(public, indent=2) + "\n")
    lines = []
    for row in rows:
        slug = "openrouter/" + row["id"]
        if "tools" not in row["supported_parameters"]:
            raise ValueError("matrix model does not publish tool support")
        lines += [f'[model.{json.dumps(slug)}.capabilities]', 'schema_version = 5',
                  f'max_context_tokens = {row["context_length"]}',
                  f'max_output_tokens = {row["top_provider"]["max_completion_tokens"]}',
                  # Conservative scenario constraints: one tool, no reasoning
                  # state carryover, caching, or images; documented low effort.
                  'supports_parallel_tool_calls = false',
                  'reasoning_carryover = "unsupported"',
                  'reasoning_efforts = { only = ["low", "medium", "high"] }',
                  'default_reasoning_effort = "low"', 'cancel = "cooperative"',
                  'cache = "none"', 'transport = "sse"', 'image_input = "unsupported"',
                  f'[model.{json.dumps(slug)}.pricing]',
                  f'revision = {json.dumps("host-test-openrouter-" + captured)}',
                  'currency = "USD"', f'uncached_input = {json.dumps(row["pricing"]["prompt"])}',
                  f'output = {json.dumps(row["pricing"]["completion"])}']
        # No caching is requested by this fixture (cache = none). The gate
        # requires every input category even when unavailable. Use the full
        # uncached rate as an operator reservation bound for those inactive
        # categories, never claim it is a published cache-write tariff.
        lines.append(f'cache_read = {json.dumps(row["pricing"].get("input_cache_read", row["pricing"]["prompt"]))}')
        lines.append(f'cache_write = {json.dumps(row["pricing"].get("input_cache_write", row["pricing"]["prompt"]))}')
    path = Path(scratch) / "models.toml"
    path.write_text("\n".join(lines) + "\n")
    return path


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("tuic", "ego", "tuic-commit", "ego-commit", "evidence"):
        parser.add_argument("--" + name, required=True)
    parser.add_argument("--test-filter", default="")
    parser.add_argument("--live", action="store_true")
    parser.add_argument("--permission-live", action="store_true")
    parser.add_argument("--model", default="openrouter/openai/gpt-oss-20b")
    parser.add_argument("--key-env-file", type=Path)
    args = parser.parse_args()
    if args.permission_live and not args.live:
        parser.error("--permission-live requires --live")
    temp_root = Path(os.environ["TMPDIR"]).resolve()
    if not temp_root.is_relative_to(Path.home() / "Gits"):
        parser.error("TMPDIR must stay below ~/Gits")
    if "debug" not in Path(args.tuic).parts:
        parser.error("use a debug/tuic-remote artifact; release uses the native vault")
    with tempfile.TemporaryDirectory(prefix="host-test-", dir=temp_root) as scratch:
        home = Path(scratch) / "host-home"
        instance = "host-test-" + secrets.token_hex(6)
        if os.name == "nt":
            platform_config = home / "AppData/Roaming"
        elif os.uname().sysname == "Darwin":
            platform_config = home / "Library/Application Support"
        else:
            platform_config = home / ".config"
        config = platform_config / "com.tuic.commander/instances" / instance
        config.mkdir(parents=True)
        (config / ".ego-host-test-isolated").write_text("task-owned host fixture\n")
        # These are launch settings, not a fabricated ACP/provider recording.
        (config / "config.json").write_text(json.dumps({
            "shell": None, "font_family": "monospace", "font_size": 14,
            "theme": "dark", "ego_executable": str(Path(args.ego).resolve()),
            "services": {"server": {"enabled": True}},
        }))
        header = Path(scratch) / "pairing-header"
        header.write_text("Cookie: tui-session=" + secrets.token_hex(32) + "\n")
        header.chmod(0o600)
        with socket.socket() as listener:
            listener.bind(("127.0.0.1", 0))
            port = listener.getsockname()[1]
        env = os.environ.copy()
        # Refuse ambient live flags, model and credentials as matrix authority.
        for name in list(env):
            if name.startswith("EGO_HOST_TEST_"):
                del env[name]
        values = {
            "TUIC_ARTIFACT": str(Path(args.tuic).resolve()),
            "EGO_EXECUTABLE": str(Path(args.ego).resolve()),
            "TUIC_COMMIT": args.tuic_commit, "EGO_COMMIT": args.ego_commit,
            "TUIC_HOME": str(home), "TUIC_INSTANCE": instance,
            "TUIC_CONFIG_DIR": str(config), "AUTH_HEADER_FILE": str(header),
            "ENDPOINT": f"http://127.0.0.1:{port}",
            "EVIDENCE": str(Path(args.evidence).resolve()),
        }
        if os.name == "nt":
            values["APPDATA"] = str(platform_config)
        elif os.uname().sysname != "Darwin":
            values["XDG_CONFIG_HOME"] = str(platform_config)
        if args.live:
            if not args.key_env_file or not args.model.startswith("openrouter/"):
                parser.error("live mode requires an OpenRouter model and --key-env-file")
            key = next((line.split("=", 1)[1].strip().strip("\"'")
                        for line in args.key_env_file.read_text().splitlines()
                        if line.startswith("OPENROUTER_API_KEY=")), "")
            if not key:
                parser.error("the supplied file has no OPENROUTER_API_KEY")
            env["OPENROUTER_API_KEY"] = key
            env["EGO_MODEL"] = args.model
            values.update(LIVE="1", LIVE_CREDENTIAL_ENV="OPENROUTER_API_KEY",
                          MODELS_FILE=str(recorded_model_facts(args.model, scratch, args.evidence)))
            if args.permission_live:
                values["PERMISSION_LIVE"] = "1"
        env.update({"EGO_HOST_TEST_" + key: value for key, value in values.items()})
        result = subprocess.run([
            "cargo", "test", "-p", "ego-cli", "--test",
            "tuicommander_host", args.test_filter, "--", "--include-ignored", "--test-threads=1",
        ], env=env, check=False)
        return result.returncode


if __name__ == "__main__":
    raise SystemExit(main())
