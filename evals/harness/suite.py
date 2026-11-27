"""Frozen task suite: digest and oracle verification (standard library only)."""
import hashlib
import json
import shutil
import subprocess
import sys
import tempfile
import tomllib
from pathlib import Path

DIGESTS = "DIGESTS.sha256"
RUNNER = (
    "import json,sys,unittest\n"
    "def ids(x):\n"
    "    return [i for t in x for i in ([t.id()] if isinstance(t,unittest.TestCase) else ids(t))]\n"
    "s=unittest.defaultTestLoader.loadTestsFromName(sys.argv[1])\n"
    "names=ids(s)\n"
    "r=unittest.TextTestRunner(stream=open('/dev/null','w'),verbosity=0).run(s)\n"
    "bad={t.id() for t,_ in r.failures+r.errors}\n"
    "print(json.dumps({i:i not in bad for i in names}))\n"
)


def suite_files(suite):
    return sorted(
        p.relative_to(suite).as_posix()
        for p in suite.rglob("*")
        if p.is_file() and p.name != DIGESTS and "__pycache__" not in p.parts
    )


def digest_lines(suite):
    return [f"{hashlib.sha256((suite / p).read_bytes()).hexdigest()}  {p}" for p in suite_files(suite)]


def write_digests(suite):
    (suite / DIGESTS).write_text("\n".join(digest_lines(suite)) + "\n")


def check_digests(suite):
    recorded = (suite / DIGESTS).read_text().splitlines() if (suite / DIGESTS).is_file() else []
    changed = set(recorded) ^ set(digest_lines(suite))
    return sorted({f"digest mismatch: {line.split('  ', 1)[1]}" for line in changed})


def run_hidden(workdir, hidden_name):
    """Return {test id: passed}; a crash or timeout marks the run as no results."""
    module = Path(hidden_name).stem
    try:
        done = subprocess.run([sys.executable, "-B", "-c", RUNNER, module], cwd=workdir,
                              capture_output=True, text=True, timeout=120)
        return json.loads(done.stdout.strip().splitlines()[-1])
    except (subprocess.TimeoutExpired, json.JSONDecodeError, IndexError):
        return {}


def materialise(task, with_reference):
    tmp = Path(tempfile.mkdtemp(prefix="harness-verify-"))
    shutil.copytree(task / "workspace", tmp, dirs_exist_ok=True)
    if with_reference:
        shutil.copytree(task / "reference", tmp, dirs_exist_ok=True)
    hidden = tomllib.loads((task / "task.toml").read_text())["hidden"]
    shutil.copy(task / hidden, tmp / Path(hidden).name)
    return tmp, Path(hidden).name


def check_task(task):
    meta = tomllib.loads((task / "task.toml").read_text())
    errors = []
    outside = [p.relative_to(task / "reference").as_posix() for p in (task / "reference").rglob("*")
               if p.is_file() and p.relative_to(task / "reference").as_posix() not in meta["editable"]]
    errors += [f"{meta['id']}: reference file not editable: {p}" for p in outside]
    for with_reference in (True, False):
        tmp, name = materialise(task, with_reference)
        try:
            results = run_hidden(tmp, name)
        finally:
            shutil.rmtree(tmp, ignore_errors=True)
        if not results:
            errors.append(f"{meta['id']}: hidden tests produced no results")
        elif with_reference:
            errors += [f"{meta['id']}: reference fails {t}" for t, ok in results.items() if not ok]
        else:
            errors += [f"{meta['id']}: untouched workspace passes {t}" for t, ok in results.items() if ok]
    return errors


def verify_suite(suite):
    errors = check_digests(suite)
    for task in sorted(p.parent for p in suite.glob("*/task.toml")):
        errors += check_task(task)
    return errors
