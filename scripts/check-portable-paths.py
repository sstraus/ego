#!/usr/bin/env python3
"""Reject personal filesystem literals in shipped Rust source, not test fixtures."""
import argparse
import re
import subprocess
from pathlib import Path

# Treat strings/comments as single tokens so braces inside them do not close items.
TOKENS = re.compile(r'r(?P<hash>\#*)"[\s\S]*?"(?P=hash)|b?"(?:\\.|[^"\\])*"|//[^\n]*|/\*[\s\S]*?\*/|[{}]|[^{}"/r]+|.', re.DOTALL)
TEST_ITEM = re.compile(r'#\[cfg\((?:test|all\(test,[^\n]*|all\([^\n]*,\s*test\))\)\]')
PERSONAL = re.compile(r'(?:\bGits(?:[/\\]|\b)|/Users/|/home/|example\.user|(?:~|\$HOME|\$\{HOME\})/(?:personal|repos|projects|work|\.tmp)(?:[/\\]|\b))', re.IGNORECASE)


def production(text):
    """Remove cfg(test) items before inspecting literals, including inline test modules."""
    chars = list(text)
    for match in TEST_ITEM.finditer(text):
        depth = 0
        opened = False
        end = match.end()
        for token in TOKENS.finditer(text, match.end()):
            value = token.group()
            if value == '{':
                depth += 1
                opened = True
            elif value == '}':
                depth -= 1
                if opened and depth == 0:
                    end = token.end()
                    break
        if opened:
            chars[match.start():end] = ['\n' if c == '\n' else ' ' for c in text[match.start():end]]
    return ''.join(chars)


def violations(text):
    for token in TOKENS.finditer(production(text)):
        value = token.group()
        if value.startswith(('"', 'b"', 'r"', 'r#')) and PERSONAL.search(value):
            yield text.count('\n', 0, token.start()) + 1, value[:160]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--git-ref', help='inspect a committed tree instead of the worktree')
    args = parser.parse_args()
    root = Path(__file__).resolve().parent.parent
    if args.git_ref:
        names = subprocess.check_output(['git', 'ls-tree', '-r', '--name-only', args.git_ref, 'crates'], cwd=root, text=True).splitlines()
    else:
        names = [str(p.relative_to(root)) for p in (root / 'crates').rglob('*.rs')]
    bad = []
    for name in sorted(names):
        path = Path(name)
        if path.suffix != '.rs' or 'src' not in path.parts or 'tests' in path.parts or path.stem == 'tests' or path.stem.endswith('_tests'):
            continue
        text = subprocess.check_output(['git', 'show', f'{args.git_ref}:{name}'], cwd=root, text=True) if args.git_ref else (root / path).read_text()
        bad.extend(f'{name}:{line}: {value}' for line, value in violations(text))
    if bad:
        print('\n'.join(bad))
        raise SystemExit(1)
    print('Portable source paths: PASS')


if __name__ == '__main__':
    main()
