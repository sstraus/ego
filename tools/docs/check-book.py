#!/usr/bin/env python3
"""Check local HTML links, fragments, and assets in a built mdBook; no network."""
from html.parser import HTMLParser
from pathlib import Path
import sys
from urllib.parse import unquote, urlsplit


class Page(HTMLParser):
    def __init__(self):
        super().__init__()
        self.links = []
        self.ids = set()

    def handle_starttag(self, tag, attrs):
        attrs = dict(attrs)
        if "id" in attrs:
            self.ids.add(attrs["id"])
        if tag in ("a", "link") and "href" in attrs:
            self.links.append(attrs["href"])
        if tag in ("script", "img") and "src" in attrs:
            self.links.append(attrs["src"])


def main():
    if len(sys.argv) != 2:
        raise SystemExit("usage: python3 tools/docs/check-book.py /path/to/built/book")
    root = Path(sys.argv[1]).resolve(strict=True)
    pages = {}
    for path in root.rglob("*.html"):
        page = Page()
        page.feed(path.read_text())
        pages[path] = page
    if not pages:
        raise SystemExit("no HTML pages found; build the book first")
    errors = []
    checked = 0
    for path, page in pages.items():
        for raw in page.links:
            parsed = urlsplit(raw)
            if parsed.scheme or parsed.netloc:
                continue
            checked += 1
            target = (path.parent / unquote(parsed.path)).resolve() if parsed.path else path
            if parsed.path.startswith("/"):
                target = (root / unquote(parsed.path).lstrip("/")).resolve()
            if target.is_dir():
                target /= "index.html"
            if not target.is_relative_to(root) or not target.exists():
                errors.append(f"{path.relative_to(root)}: {raw} (missing book file)")
            elif parsed.fragment and target in pages and unquote(parsed.fragment) not in pages[target].ids:
                errors.append(f"{path.relative_to(root)}: {raw} (missing fragment)")
    for error in errors:
        print(error, file=sys.stderr)
    print(f"Checked {checked} local HTML links/assets across {len(pages)} pages; {len(errors)} errors.")
    return bool(errors)


if __name__ == "__main__":
    sys.exit(main())
