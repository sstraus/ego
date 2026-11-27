#!/usr/bin/env python3
"""Exercise scanner I/O under the Windows legacy text encoding."""
import contextlib
import importlib.util
import io
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch


class ScannerEncodingTests(unittest.TestCase):
    # Catches: locale decoding crashes on UTF-8 source before paths are checked.
    def test_utf8_source_is_scanned_in_worktree_and_git_ref_under_cp1252(self):
        scanner_path = Path(__file__).with_name('check-portable-paths.py')
        spec = importlib.util.spec_from_file_location('portable_paths', scanner_path)
        scanner = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(scanner)
        temporary_root = Path(__file__).resolve().parent.parent / '.tmp'
        temporary_root.mkdir(exist_ok=True)
        with tempfile.TemporaryDirectory(dir=temporary_root) as directory:
            root = Path(directory)
            (root / 'scripts').mkdir()
            (root / 'crates/fixture/src').mkdir(parents=True)
            source = root / 'crates/fixture/src/lib.rs'
            # U+0401 contains UTF-8 byte 0x81, which cp1252 cannot decode.
            source.write_text('fn label() { let text = "Ё"; let path = "/home/alice"; }\n', encoding='utf-8')
            for args in [
                ['init', '-q'], ['add', '.'],
                ['-c', 'user.name=Fixture', '-c', 'user.email=fixture@example.invalid',
                 '-c', 'commit.gpgsign=false', 'commit', '-qm', 'UTF-8 fixture'],
            ]:
                subprocess.run(['git', *args], cwd=root, check=True, capture_output=True)
            scanner.__file__ = str(root / 'scripts/check-portable-paths.py')
            real_open = io.open

            def legacy_open(file, mode='r', buffering=-1, encoding=None, *args, **kwargs):
                if 'b' not in mode and encoding in (None, 'locale'):
                    encoding = 'cp1252'
                return real_open(file, mode, buffering, encoding, *args, **kwargs)

            real_check_output = subprocess.check_output

            def legacy_check_output(*args, **kwargs):
                if kwargs.get('text'):
                    kwargs.setdefault('encoding', 'cp1252')
                return real_check_output(*args, **kwargs)

            for options in [[], ['--git-ref', 'HEAD']]:
                with self.subTest(options=options):
                    output = io.StringIO()
                    with patch.object(sys, 'argv', ['scanner', *options]), \
                         patch('io.open', side_effect=legacy_open), \
                         patch('subprocess.check_output', side_effect=legacy_check_output), \
                         contextlib.redirect_stdout(output):
                        with self.assertRaises(SystemExit) as result:
                            scanner.main()
                    self.assertEqual(result.exception.code, 1)
                    self.assertIn('crates/fixture/src/lib.rs:1:', output.getvalue())
                    self.assertIn('/home/alice', output.getvalue())


if __name__ == '__main__':
    unittest.main()
