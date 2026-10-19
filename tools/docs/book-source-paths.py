#!/usr/bin/env python3
"""Keep checkout-only citations readable without broken links in the HTML book.

The Markdown remains unchanged. Only chapters listed in SUMMARY are published by
mdBook. Other Markdown and source citations become explicit repository paths;
non-Markdown assets inside the book source retain their downloadable links.
"""
import json
import posixpath
from pathlib import Path
import re
import sys
from urllib.parse import unquote, urlsplit


# Preserve fenced code and inline code, including literal Markdown examples.
CODE = re.compile(r"(?ms)^(`{3,}|~{3,})[^\n]*\n.*?^\1[^\n]*$")
LINK = re.compile(
    r"(?P<code>`+[^`\n]*`+)|(?<!!)\[(?P<label>[^\]\n]+)\]\((?P<target>[^\s)]+)\)"
)


def render(text, source, chapters):
    def link(match):
        if match.group("code"):
            return match.group(0)
        label, raw = match.group("label", "target")
        parsed = urlsplit(raw)
        if parsed.scheme or parsed.netloc or not parsed.path:
            return match.group(0)
        target = Path(posixpath.normpath(str(source.parent / unquote(parsed.path))))
        inside = not target.is_absolute() and ".." not in target.parts
        if target in chapters:
            # mdBook renders README.md chapters as index.html, not README.html.
            if target.name == "README.md":
                output = parsed._replace(path=str(Path(parsed.path).with_name("index.md"))).geturl()
                return f"[{label}]({output})"
            return match.group(0)
        if inside and target.suffix and target.suffix != ".md":
            return match.group(0)
        repository_path = posixpath.normpath("docs/" + target.as_posix())
        if parsed.fragment:
            repository_path += "#" + unquote(parsed.fragment)
        return f"{label} (repository: `{repository_path}`)"

    result = []
    start = 0
    for match in CODE.finditer(text):
        result.extend([LINK.sub(link, text[start:match.start()]), match.group(0)])
        start = match.end()
    result.append(LINK.sub(link, text[start:]))
    return "".join(result)


def chapters(items):
    for item in items:
        chapter = item.get("Chapter") if isinstance(item, dict) else None
        if chapter:
            yield chapter
            yield from chapters(chapter.get("sub_items", []))


def main():
    if len(sys.argv) > 1 and sys.argv[1] == "supports":
        return 0 if sys.argv[2:] == ["html"] else 1
    _context, book = json.load(sys.stdin)
    pages = list(chapters(book["items"]))
    paths = {Path(page["source_path"]) for page in pages if page.get("source_path")}
    for page in pages:
        if page.get("source_path"):
            page["content"] = render(page["content"], Path(page["source_path"]), paths)
    json.dump(book, sys.stdout)
    return 0


if __name__ == "__main__":
    sys.exit(main())
