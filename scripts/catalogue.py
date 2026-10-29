#!/usr/bin/env python3
"""Refresh and regenerate ego's builtin hosted-model catalogue.

Inputs (all under crates/ego-cli/catalogue/):
  policy.toml                    reviewed, hand-kept selection and wire policy
  upstream/models.dev.json       pinned, filtered snapshot of https://models.dev/api.json
Output:
  models.generated.toml          builtin capability rows, one per current model

Subcommands:
  fetch [--out PATH]   download models.dev, record the commit, write the filtered snapshot
  generate             rewrite models.generated.toml from snapshot + policy
  check                fail when models.generated.toml is not what `generate` writes
  diff OLD NEW         per-provider changes between two snapshots

The snapshot carries no `cost` field: prices are never imported (ADR 2026-08-21
section 1, ADR 2026-09-07 section 2). Standard library only.
"""

import argparse
import datetime
import json
import pathlib
import sys
import tomllib
import urllib.request

ROOT = pathlib.Path(__file__).resolve().parents[1]
CATALOGUE = ROOT / "crates/ego-cli/catalogue"
POLICY = CATALOGUE / "policy.toml"
SNAPSHOT = CATALOGUE / "upstream/models.dev.json"
GENERATED = CATALOGUE / "models.generated.toml"

API_URL = "https://models.dev/api.json"
REPOSITORY = "sst/models.dev"
BRANCH = "dev"
KEPT = ("id", "release_date", "status", "tool_call", "reasoning", "reasoning_options", "limit", "modalities")


def usable(model):
    """A model ego can drive: tools, text out. Deprecation is checked separately."""
    return bool(model.get("tool_call")) and "text" in model["modalities"]["output"]


def current(model):
    return usable(model) and model.get("status") != "deprecated"


def load_policy():
    return tomllib.loads(POLICY.read_text())


def load_snapshot(path=SNAPSHOT):
    return json.loads(pathlib.Path(path).read_text())


def get_json(url):
    # Both hosts refuse the default urllib agent.
    request = urllib.request.Request(url, headers={"User-Agent": "ego-model-catalogue-refresh"})
    with urllib.request.urlopen(request, timeout=60) as response:
        return json.load(response)


def fetch(out):
    policy = load_policy()
    wanted = sorted({p["models_dev"] for p in policy["provider"].values()})
    api = get_json(API_URL)
    revision = get_json(f"https://api.github.com/repos/{REPOSITORY}/commits/{BRANCH}")["sha"]
    providers = {}
    for key in wanted:
        providers[key] = {
            model_id: {field: model[field] for field in KEPT if field in model}
            for model_id, model in sorted(api[key]["models"].items())
            if usable(model)
        }
    snapshot = {
        "source": "models.dev",
        "url": API_URL,
        "repository": REPOSITORY,
        "revision": revision,
        "read": datetime.date.today().isoformat(),
        "providers": providers,
    }
    pathlib.Path(out).write_text(json.dumps(snapshot, indent=1, sort_keys=True) + "\n")


def select(name, provider, models):
    """The ids of one ego provider row, sorted. Raises on a policy gap."""
    exclude = provider.get("exclude", {})
    chosen = {
        i for i, m in models.items() if current(m) and m["release_date"] >= provider["floor"]
    }
    chosen |= set(provider.get("include", []))
    unknown = [i for i in chosen | set(exclude) if i not in models]
    if unknown:
        raise SystemExit(f"{name}: policy names models absent from snapshot: {sorted(unknown)}")
    return sorted(chosen - set(exclude))


def efforts(name, model_id, model, dropped):
    """models.dev reasoning controls as ego's effort set; refuses what it cannot state."""
    if not model.get("reasoning"):
        return ["off"]
    levels, toggle = None, False
    for option in model.get("reasoning_options") or []:
        if option["type"] == "effort":
            levels = ["off" if v == "none" else v for v in option["values"]]
        elif option["type"] == "toggle":
            toggle = True
    if levels is None:
        raise SystemExit(
            f"{name}/{model_id}: reasoning controls are not an effort list; "
            "add it to the provider's `exclude` table with a reason"
        )
    if toggle and "off" not in levels:
        levels.insert(0, "off")
    order = ["off", "minimal", "low", "medium", "high", "xhigh", "max"]
    return sorted(set(levels) - set(dropped), key=order.index)


def toml_list(values):
    return "[" + ", ".join(json.dumps(v) for v in values) + "]"


def row(name, provider, wire, model_id, model, snapshot, provenance=None):
    slug = f"{name}/{model_id}"
    limit = model["limit"]
    context = provider.get("context_override", limit.get("context"))
    output = limit.get("output")
    if not context or not output:
        raise SystemExit(f"{slug}: snapshot states no context or output limit")
    levels = (None if model.get("reasoning_efforts") == "unknown" else
              efforts(name, model_id, model, provider.get("drop_efforts", [])))
    default = provider.get("default_effort", {}).get(model_id)
    if default is not None and (levels is None or default not in levels):
        raise SystemExit(f"{slug}: default effort {default!r} is outside {levels}")
    lines = [
        provenance or f"# {snapshot['source']} {snapshot['revision'][:8]} read {snapshot['read']}: "
        f"{provider['models_dev']}/{model_id}",
        f'[model."{slug}".capabilities]',
        "schema_version = 6",
        f"max_context_tokens = {context}",
        f"max_output_tokens = {output}",
        f"supports_parallel_tool_calls = {str(wire['supports_parallel_tool_calls']).lower()}",
        f'reasoning_carryover = "{wire["reasoning_carryover"]}"',
    ]
    if "reasoning_state_dialect" in wire:
        lines.append(f'reasoning_state_dialect = "{wire["reasoning_state_dialect"]}"')
    lines.append('reasoning_efforts = "unknown"' if levels is None else
                 f"reasoning_efforts = {{ only = {toml_list(levels)} }}")
    if default is not None:
        lines.append(f'default_reasoning_effort = "{default}"')
    lines += [f'cancel = "{wire["cancel"]}"', f'cache = "{wire["cache"]}"', f'transport = "{wire["transport"]}"']
    image = wire.get("image_input")
    if image is not None and "image" not in model["modalities"]["input"]:
        lines.append('image_input = "unsupported"')
    lines.append("")
    if image is not None and "image" in model["modalities"]["input"]:
        lines.append(f'[model."{slug}".capabilities.image_input]')
        lines.append('kind = "supported"')
        lines.append(f"accepted_media_types = {toml_list(image['accepted_media_types'])}")
        for key in ("max_images", "max_bytes_per_image", "max_width", "max_height"):
            if key in image:
                lines.append(f"{key} = {image[key]}")
        lines.append("")
    return lines


def generate():
    policy, snapshot = load_policy(), load_snapshot()
    out = [
        "# GENERATED by scripts/catalogue.py. Do not edit.",
        "# Inputs: catalogue/policy.toml and catalogue/upstream/models.dev.json.",
        "# Capabilities only: no row states a price (ADR 2026-09-07 section 2).",
        f"# source = {snapshot['source']}",
        f"# repository = {snapshot['repository']}",
        f"# revision = {snapshot['revision']}",
        f"# read = {snapshot['read']}",
        "",
    ]
    for name in sorted(policy["provider"]):
        provider = policy["provider"][name]
        models = snapshot["providers"][provider["models_dev"]]
        for model_id in select(name, provider, models):
            out += row(name, provider, provider["wire"], model_id, models[model_id], snapshot)
    for slug, model in sorted(policy.get("stated", {}).items()):
        name, model_id = slug.split("/", 1)
        if model_id in snapshot["providers"][policy["provider"][name]["models_dev"]]:
            raise SystemExit(f"{slug}: now present upstream; review the stated row")
        out += row(name, {}, model["wire"], model_id, model, snapshot,
                   f"# {model['source']} read {model['read']}: {slug}")
    return "\n".join(out).rstrip("\n") + "\n"


def diff(old_path, new_path):
    old, new = load_snapshot(old_path), load_snapshot(new_path)
    print(f"snapshot {old['revision'][:8]} ({old['read']}) -> {new['revision'][:8]} ({new['read']})")
    for key in sorted(new["providers"]):
        before, after = old["providers"].get(key, {}), new["providers"][key]
        added = sorted(set(after) - set(before))
        removed = sorted(set(before) - set(after))
        deprecated = sorted(i for i in after if i in before and after[i].get("status") == "deprecated"
                            and before[i].get("status") != "deprecated")
        changed = sorted(i for i in after if i in before and after[i] != before[i] and i not in deprecated)
        print(f"{key}: +{len(added)} -{len(removed)} deprecated {len(deprecated)} changed {len(changed)}")
        for label, ids in (("new", added), ("removed", removed), ("deprecated", deprecated), ("changed", changed)):
            for i in ids:
                print(f"  {label}: {i}")
    print("review: a new model released on or after a provider floor enters the catalogue unless policy.toml excludes it")


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    sub = parser.add_subparsers(dest="command", required=True)
    sub.add_parser("generate")
    sub.add_parser("check")
    fetch_parser = sub.add_parser("fetch")
    fetch_parser.add_argument("--out", default=str(SNAPSHOT))
    diff_parser = sub.add_parser("diff")
    diff_parser.add_argument("old")
    diff_parser.add_argument("new")
    args = parser.parse_args()
    if args.command == "fetch":
        fetch(args.out)
    elif args.command == "generate":
        GENERATED.write_text(generate())
    elif args.command == "check":
        if GENERATED.read_text() != generate():
            sys.exit("models.generated.toml is not what the generator writes; run `generate`")
    else:
        diff(args.old, args.new)


if __name__ == "__main__":
    main()
