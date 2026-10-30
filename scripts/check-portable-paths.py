#!/usr/bin/env python3
"""Reject personal filesystem literals in shipped source and templates, not test fixtures."""
import argparse
import re
import subprocess
from pathlib import Path

# Treat strings/comments as single tokens so braces inside them do not close items.
TOKENS = re.compile(
    r"""r(?P<hash>\#*)"[\s\S]*?"(?P=hash)|b?"(?:\\.|[^"\\])*"|b?'(?:\\(?:u\{[0-9a-fA-F]+\}|.)|[^'\\])'|//[^\n]*|/\*[\s\S]*?\*/|[{};]|[^{}"/r';]+|.""",
    re.DOTALL,
)
TEST_ITEM = re.compile(r'#\[(?:test|cfg\((?:test|all\(test,[^\n]*|all\([^\n]*,\s*test\))\))\]')
PERSONAL = re.compile(r'(?:\bGits(?:[/\\]|\b)|/Users/|/home/|example\.user|tuic-land(?:-cleanup)?\.sh|(?:^"|^r\#*")(?:personal|projects|repos|work)(?:[/\\]|"|\b)|(?:~|\$HOME|\$\{HOME\})/(?:personal|repos|projects|work|\.tmp)(?:[/\\]|\b))', re.IGNORECASE)


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
            elif value == ';' and not opened:
                break
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
    parser.add_argument('--self-test', action='store_true', help='check scanner regression cases')
    parser.add_argument('--git-ref', help='inspect a committed tree instead of the worktree')
    args = parser.parse_args()
    if args.self_test:
        for text in ['let root = home.join("Gits");', 'let path = r#"/Users/alice/private"#;', 'let path = "/home/alice/work";', 'let root = home.join("personal/tools");']:
            assert list(violations(text)), text
        for text in ['#[cfg(test)] mod tests { let path = "Gits"; }', '#[cfg(all(test, unix))] mod tests { let path = r#"Gits/{fixture}"#; }', 'let path = "https://github.com/sstraus/ego";']:
            assert not list(violations(text)), text
        assert not list(violations("#[cfg(test)] mod tests { let brace = '}'; let fixture = \"Gits\"; }"))
        assert list(violations('#[cfg(test)] use test_only; fn shipped() { let path = "Gits"; }'))
        assert list(violations('#[cfg(test)] mod tests { let path = "Gits"; } let path = "Gits";'))
        print('Portable path scanner regressions: PASS')
        return
    root = Path(__file__).resolve().parent.parent
    if args.git_ref:
        names = subprocess.check_output(['git', 'ls-tree', '-r', '--name-only', args.git_ref, 'crates'], cwd=root, text=True).splitlines()
    else:
        names = subprocess.check_output(['git', 'ls-files', '--cached', '--others', '--exclude-standard', 'crates'], cwd=root, text=True).splitlines()
    bad = []
    for name in sorted(names):
        path = Path(name)
        if path.suffix not in {'.rs', '.txt', '.md', '.json', '.sh', '.py'} or ('src' not in path.parts and path.name != 'build.rs') or {'tests', 'fixtures'}.intersection(path.parts) or path.stem == 'tests' or path.stem.endswith('_tests'):
            continue
        if not args.git_ref and not (root / path).is_file():
            continue
        text = subprocess.check_output(['git', 'show', f'{args.git_ref}:{name}'], cwd=root, text=True) if args.git_ref else (root / path).read_text()
        matches = violations(text) if path.suffix == '.rs' else ((number, line) for number, line in enumerate(text.splitlines(), 1) if PERSONAL.search(line))
        bad.extend(f'{name}:{line}: {value}' for line, value in matches)
    if bad:
        print('\n'.join(bad))
        raise SystemExit(1)
    print('Portable source paths: PASS')


if __name__ == '__main__':
    main()
