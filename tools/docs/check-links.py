#!/usr/bin/env python3
"""Check repository-local Markdown paths and fragments in the public doc tree.

No network requests. Markdown code fences and external links are excluded.
Default scope includes README and docs, plus the top-level design authorities.
"""
from pathlib import Path
import re
import sys
from urllib.parse import unquote, urlsplit

ROOT = Path(__file__).resolve().parents[2]

def prose(text):
    return re.sub(r"(?ms)^(`{3,}|~{3,}).*?^\1[^\n]*$", "", text)

def anchors(path):
    result = set()
    counts = {}
    for line in prose(path.read_text()).splitlines():
        match = re.match(r"^#{1,6}\s+(.+?)\s*#*\s*$", line)
        if match:
            title = re.sub(r"<[^>]+>", "", match[1]).lower()
            title = re.sub(r"[^\w\- ]", "", title).replace(" ", "-")
            duplicate = counts.get(title, 0)
            counts[title] = duplicate + 1
            result.add(title + (f"-{duplicate}" if duplicate else ""))
        result.update(re.findall(r'(?:id|name)=["\']([^"\']+)["\']', line))
    return result

def main():
    files = [ROOT / p for p in ("README.md", "ARCHITECTURE.md", "CLI.md", "SPEC.md")]
    files += sorted((ROOT / "docs").rglob("*.md"))
    errors = []
    checked = 0
    for path in files:
        text = prose(path.read_text())
        targets = re.findall(r"!?\[[^\]\n]*\]\(<?([^\s)>]+)>?(?:\s+[^)]*)?\)", text)
        targets += re.findall(r"(?m)^\s*\[[^\]]+\]:\s*<?([^\s>]+)>?", text)
        for raw in targets:
            parsed = urlsplit(raw)
            if parsed.scheme or parsed.netloc:
                continue
            checked += 1
            target = (ROOT / unquote(parsed.path).lstrip("/")) if parsed.path.startswith("/") else (path.parent / unquote(parsed.path))
            if not parsed.path:
                target = path
            if not target.exists():
                errors.append(f"{path.relative_to(ROOT)}: {raw} (missing path)")
            elif parsed.fragment and target.suffix == ".md" and unquote(parsed.fragment) not in anchors(target):
                errors.append(f"{path.relative_to(ROOT)}: {raw} (missing fragment)")
    for error in errors:
        print(error, file=sys.stderr)
    print(f"Checked {checked} local links across {len(files)} Markdown files; {len(errors)} errors.")
    return bool(errors)

if __name__ == "__main__":
    sys.exit(main())
