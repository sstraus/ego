#!/usr/bin/env python3
"""Run one slice of the test gate, so CI can run, retry and report slices alone.

`--only NAME` runs one integration-test target; `--skip NAME` runs every other
target plus the library, binary and doc tests. Both keep going after a failing
target (`--no-fail-fast`), because a red target must not hide the targets after
it. The command starts without inherited descriptors, as `run-gate-command.py`
does: Landlock cannot revoke authority carried by an open descriptor.
"""

import argparse
import json
import subprocess
import sys


def test_targets():
    metadata = json.loads(
        subprocess.check_output(
            ['cargo', 'metadata', '--no-deps', '--format-version', '1']
        )
    )
    return sorted(
        {
            target['name']
            for package in metadata['packages']
            for target in package['targets']
            if 'test' in target['kind']
        }
    )


def command(only, skip):
    base = ['cargo', 'test', '--workspace', '--no-fail-fast']
    if only:
        if only not in test_targets():
            raise SystemExit(f'test-gate: no integration-test target named {only!r}')
        return base + ['--test', only]
    names = [name for name in test_targets() if name != skip]
    arguments = ['--lib', '--bins', '--doc']
    for name in names:
        arguments += ['--test', name]
    return base + arguments


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    group = parser.add_mutually_exclusive_group(required=True)
    group.add_argument('--only', help='run only this integration-test target')
    group.add_argument('--skip', help='run everything except this target')
    parser.add_argument('--print', action='store_true', help='print the command and stop')
    arguments = parser.parse_args()
    argv = command(arguments.only, arguments.skip)
    if arguments.print:
        print(' '.join(argv))
        sys.exit(0)
    sys.exit(subprocess.call(argv, close_fds=True))
