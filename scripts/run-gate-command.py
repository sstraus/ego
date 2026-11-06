#!/usr/bin/env python3
"""Launch the gate without ambient file descriptors from its caller.

Landlock cannot revoke authority carried by inherited descriptors. The gate
launcher owns descriptor hygiene, just as any launcher of ego does. Standard
input, output and error stay connected, and the command's exit code is retained.
"""

import subprocess
import sys


if __name__ == '__main__':
    raise SystemExit(subprocess.call(sys.argv[1:], close_fds=True))
