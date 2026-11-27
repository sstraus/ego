"""Replay must send recorded prompt bytes, not the newly constructed case prompt."""
import hashlib
import http.server
import json
import os
import subprocess
import sys
import threading
import unittest
from pathlib import Path


class Capture(http.server.BaseHTTPRequestHandler):
    def do_POST(self):
        self.server.body = json.loads(self.rfile.read(int(self.headers['Content-Length'])))
        self.send_response(200)
        self.end_headers()
        self.wfile.write(b'data: [DONE]\n\n')

    def log_message(self, *_args):
        pass


class ReplayTest(unittest.TestCase):
    def test_recorded_event_replaces_the_case_prompt_at_the_wire(self):
        # Catches replay silently sending reconstructed rather than recorded facts.
        server = http.server.HTTPServer(('127.0.0.1', 0), Capture)
        thread = threading.Thread(target=server.handle_request)
        thread.start()
        instructions = 'Recorded instructions'
        facts = '{"action":"recorded"}'
        event = {'kind': 'JudgeResponse', 'input': {'instructions': instructions, 'facts': facts,
                 'instructions_sha256': hashlib.sha256(instructions.encode()).hexdigest()}}
        request = {'body': {'messages': [{'role': 'user', 'content': 'reconstructed'}]},
                   'path': '/v1/chat/completions', 'method': 'POST', 'judge_event': event}
        env = os.environ | {'EGO_JUDGE_MODEL': 'ollama/test',
                            'OLLAMA_BASE': 'http://127.0.0.1:%d' % server.server_port}
        try:
            subprocess.run([sys.executable, str(Path(__file__).with_name('bridge.py'))],
                           input=json.dumps(request), text=True, capture_output=True, env=env,
                           check=True, timeout=10)
            thread.join(10)
            self.assertFalse(thread.is_alive())
            self.assertEqual(server.body['messages'], [
                {'role': 'system', 'content': instructions},
                {'role': 'user', 'content': '<untrusted_action_json>\n' + facts + '\n</untrusted_action_json>'}])
        finally:
            server.server_close()

    def test_recorded_event_with_changed_prompt_is_rejected(self):
        # Catches an inaccurate replay of an event whose prompt text changed.
        request = {'body': {}, 'path': '/v1/chat/completions', 'method': 'POST',
                   'judge_event': {'kind': 'JudgeResponse', 'input': {'instructions': 'changed',
                                   'facts': '{}', 'instructions_sha256': 'wrong'}}}
        env = os.environ | {'EGO_JUDGE_MODEL': 'ollama/test'}
        result = subprocess.run([sys.executable, str(Path(__file__).with_name('bridge.py'))],
                                input=json.dumps(request), text=True, capture_output=True, env=env, timeout=10)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('hash mismatch', result.stderr)


if __name__ == '__main__':
    unittest.main()
