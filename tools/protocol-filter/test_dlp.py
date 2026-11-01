"""Offline native filter protocol tests; synthetic values only."""
import base64
import json
from pathlib import Path
import subprocess
import unittest

FILTER = Path(__file__).with_name("dlp.py")
SECRET = "-----BEGIN PRIVATE KEY-----\nSYNTHETIC-MATERIAL\n-----END PRIVATE KEY-----"


def envelope(body, direction="request", values=None, sequence=0, eof=False):
    frame = {"version": 1, "direction": direction, "provider": "anthropic",
             "model": "test", "session_id": "session-one", "generation_id": "generation",
             "attempt_id": "attempt", "body_base64": base64.b64encode(body).decode(),
             "state": {"revision": 0 if values is None else 1, "values": values or {}}}
    if direction == "response":
        frame.update(sequence=sequence, eof=eof, status=200)
    return frame


class DlpTest(unittest.TestCase):
    # Catches: developer credentials leaking from pasted files/logs, and new
    # random placeholders breaking the same outbound request after resume.
    def test_developer_leaks_are_masked_and_stable_after_resume(self):
        secrets = [
            "ghp_" + "A" * 36, "github_pat_" + "B" * 82,
            "ghs_12345_eyJhbGciOiJSUzI1NiJ9.payloadsignature.signaturevalue",
            "glpat-" + "C" * 20, "npm_" + "D" * 36,
            "pypi-" + "E" * 40, "xoxb-" + "1234567890-" * 2 + "abcdefghijklmnop",
            "AIza" + "F" * 35, "AKIA" + "G" * 16, "ASIA" + "H" * 16,
            "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiJzeW50aGV0aWMifQ.c3ludGhldGljc2lnbmF0dXJl",
            "sk_live_" + "I" * 24,
        ]
        pasted = ("\n".join(secrets) + '\nexport DATABASE_PASSWORD="synthetic db password"'
                  '\n//registry.npmjs.org/:_authToken=opaque-npm-credential'
                  '\naws_secret_access_key=opaque-aws-secret'
                  '\nAuthorization: Bearer opaque-bearer-token'
                  '\nAuthorization: Basic c3ludGhldGljOnBhc3N3b3Jk'
                  '\nx-api-key: synthetic-api-header'
                  '\n{"Authorization":"Bearer synthetic-json-header"}'
                  '\npostgres://developer:synthetic-db-pass@db.example.test/app'
                  '\nhttps://storage.example.test/file?X-Amz-Signature=synthetic-signature&part=1')
        body = json.dumps({"messages": [{"role": "user", "content": pasted},
                           {"role": "assistant", "content": [{"type": "tool_use",
                            "id": "request-one", "name": "send_request", "input": {
                                "headers": {"Authorization": "Bearer synthetic-structured-header"}}}]}],
                           "metadata": {"client_secret": "synthetic-json-secret"}}).encode()
        first = subprocess.run([str(FILTER)], input=json.dumps(envelope(body)).encode(),
                               capture_output=True, check=True)
        capture = json.loads(first.stdout)
        masked = base64.b64decode(capture["body_base64"]).decode()
        for original in secrets + ["synthetic db password", "opaque-npm-credential",
                "opaque-aws-secret", "opaque-bearer-token", "c3ludGhldGljOnBhc3N3b3Jk",
                "developer:synthetic-db-pass", "synthetic-signature", "synthetic-json-secret",
                "synthetic-api-header", "synthetic-json-header", "Bearer synthetic-structured-header"]:
            with self.subTest(original=original):
                self.assertNotIn(original, masked)
                self.assertIn(original, capture["state_writes"].values())
        self.assertIn("db.example.test/app", masked)
        self.assertIn("&part=1", masked)
        self.assertNotIn("Bearer", capture["state_writes"].values())
        self.assertIn('"Authorization":"Bearer __EGO_DLP_',
                      json.loads(masked)["messages"][0]["content"])
        second = subprocess.run([str(FILTER)],
            input=json.dumps(envelope(body, values=capture["state_writes"])).encode(),
            capture_output=True, check=True)
        reused = json.loads(second.stdout)
        self.assertEqual({}, reused["state_writes"])
        self.assertEqual(capture["body_base64"], reused["body_base64"])
        # A restored, opaque password in later assistant history has no label.
        history = json.dumps({"messages": [{"role": "assistant", "content":
                    "The saved value is synthetic db password."}]}).encode()
        third = subprocess.run([str(FILTER)],
            input=json.dumps(envelope(history, values=capture["state_writes"])).encode(),
            capture_output=True, check=True)
        replayed = json.loads(third.stdout)
        self.assertNotIn(b"synthetic db password", base64.b64decode(replayed["body_base64"]))
        self.assertEqual({}, replayed["state_writes"])

    # Catches: discovery after an earlier string, or within the same string,
    # leaving an opaque copy of a newly discovered credential outbound.
    def test_new_secret_masks_all_occurrences_regardless_of_discovery_order(self):
        secret = "opaquePass123"
        value = {"messages": [
            {"role": "assistant", "content": "Saved value: " + secret},
            {"role": "user", "content": "DB_PASSWORD=" + secret
             + "\nRepeated value: " + secret}]}
        result = subprocess.run([str(FILTER)],
            input=json.dumps(envelope(json.dumps(value).encode())).encode(),
            capture_output=True, check=True)
        capture = json.loads(result.stdout)
        masked = base64.b64decode(capture["body_base64"])
        self.assertNotIn(secret.encode(), masked)
        self.assertEqual([secret], list(capture["state_writes"].values()))
        messages = json.loads(masked)["messages"]
        placeholder = messages[0]["content"].removeprefix("Saved value: ")
        self.assertEqual("DB_PASSWORD=" + placeholder + "\nRepeated value: " + placeholder,
                         messages[1]["content"])

    # Catches: overlapping URL and assignment patterns saving a placeholder
    # inside another mapping, so text/tool restoration leaves a masked URL.
    def test_overlapping_url_assignment_restores_original_text_and_tool_value(self):
        original = 'DATABASE_PASSWORD="https://dev:rainbow-secret@db.example.test"'
        body = json.dumps({"messages": [{"role": "user", "content": original}]}).encode()
        result = subprocess.run([str(FILTER)], input=json.dumps(envelope(body)).encode(),
                                capture_output=True, check=True)
        capture = json.loads(result.stdout)
        masked = json.loads(base64.b64decode(capture["body_base64"]))["messages"][0]["content"]
        self.assertNotIn("rainbow-secret", masked)
        self.assertTrue(all("__EGO_DLP_" not in v for v in capture["state_writes"].values()))
        events = [
            {"type": "content_block_delta", "index": 0,
             "delta": {"type": "text_delta", "text": masked}},
            {"type": "content_block_delta", "index": 1,
             "delta": {"type": "input_json_delta", "partial_json": json.dumps({"content": masked})}},
        ]
        raw = b"".join(b"data: " + json.dumps(v).encode() + b"\n\n" for v in events)
        result = subprocess.run([str(FILTER)],
            input=json.dumps(envelope(raw, "response", capture["state_writes"], eof=True)).encode()+b"\n",
            capture_output=True, check=True)
        restored = base64.b64decode(json.loads(result.stdout)["body_base64"])
        values = [json.loads(line[6:])["delta"] for line in restored.splitlines()
                  if line.startswith(b"data: ")]
        self.assertEqual(original, values[0]["text"])
        self.assertEqual({"content": original}, json.loads(values[1]["partial_json"]))

    # Catches: replay of a short saved password rewriting a provider role;
    # the filter must refuse the request without returning corrupted bytes.
    def test_short_secret_role_collision_fails_closed_on_capture_and_resume(self):
        body = json.dumps({"messages": [{"role": "assistant", "content": "DB_PASSWORD=user"}]}).encode()
        result = subprocess.run([str(FILTER)], input=json.dumps(envelope(body)).encode(),
                                capture_output=True, check=True)
        capture = json.loads(result.stdout)
        self.assertIn("user", capture["state_writes"].values())
        for values in (None, capture["state_writes"]):
            with self.subTest(resumed=values is not None):
                body = json.dumps({"messages": [{"role": "user", "content": "DB_PASSWORD=user"}]}).encode()
                result = subprocess.run([str(FILTER)],
                    input=json.dumps(envelope(body, values=values)).encode(), capture_output=True)
                self.assertEqual(1, result.returncode)
                self.assertEqual(b"", result.stdout)
                self.assertEqual(b"", result.stderr)

    # Catches: a broad entropy matcher corrupting normal development data, or
    # response restoration rewriting provider accounting and cache counters.
    def test_public_development_data_and_provider_usage_are_preserved(self):
        value = {"messages": [{"content": "commit " + "a" * 40
                  + " https://docs.example.test/path?part=1 /tmp/config.env"}],
                 "max_tokens": 128, "temperature": 0.5}
        result = subprocess.run([str(FILTER)], input=json.dumps(envelope(json.dumps(value).encode())).encode(),
                                capture_output=True, check=True)
        capture = json.loads(result.stdout)
        self.assertEqual({}, capture["state_writes"])
        self.assertEqual(value, json.loads(base64.b64decode(capture["body_base64"])))
        events = [{"type": "message_start", "message": {"usage": {
            "input_tokens": 123, "cache_creation_input_tokens": 456,
            "cache_read_input_tokens": 789, "output_tokens": 1}}},
            {"type": "message_delta", "usage": {"output_tokens": 42},
             "delta": {"stop_reason": "end_turn"}}]
        raw = b"".join(b"data: " + json.dumps(v).encode() + b"\n\n" for v in events)
        frame = envelope(raw, "response", eof=True)
        result = subprocess.run([str(FILTER)], input=json.dumps(frame).encode()+b"\n",
                                capture_output=True, check=True)
        restored = base64.b64decode(json.loads(result.stdout)["body_base64"])
        self.assertEqual(events, [json.loads(line[6:]) for line in restored.splitlines()
                                 if line.startswith(b"data: ")])

    # Catches: mappings regenerated between invocations and secrets leaking in
    # outbound bytes; fragmented text/tool data must restore with JSON escaping.
    def test_persisted_mapping_restores_split_text_and_tool_arguments(self):
        body = json.dumps({"messages": [{"role": "user", "content": SECRET}]}).encode()
        first = subprocess.run([str(FILTER)], input=json.dumps(envelope(body)).encode(),
                               capture_output=True, check=True)
        capture = json.loads(first.stdout)
        self.assertNotIn(b"SYNTHETIC-MATERIAL", base64.b64decode(capture["body_base64"]))
        values = capture["state_writes"]
        second = subprocess.run([str(FILTER)],
            input=json.dumps(envelope(body, values=values)).encode(), capture_output=True, check=True)
        reused = json.loads(second.stdout)
        self.assertEqual({}, reused["state_writes"])
        self.assertEqual(capture["body_base64"], reused["body_base64"])
        placeholder = json.loads(base64.b64decode(capture["body_base64"]))["messages"][0]["content"]
        events = []
        def event(value):
            events.append(("event: " + value["type"] + "\ndata: " + json.dumps(value) + "\n\n").encode())
        event({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}})
        for part in (placeholder[:17], placeholder[17:]):
            event({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": part}})
        event({"type": "content_block_stop", "index": 0})
        event({"type": "content_block_start", "index": 1, "content_block": {"type": "tool_use", "id": "t", "name": "write", "input": {}}})
        arguments = json.dumps({"content": placeholder})
        for part in (arguments[:23], arguments[23:]):
            event({"type": "content_block_delta", "index": 1, "delta": {"type": "input_json_delta", "partial_json": part}})
        event({"type": "content_block_stop", "index": 1})
        # Every physical SSE frame is split across transport frames too.
        raw = b"".join(events)
        chunks = [raw[i:i+31] for i in range(0, len(raw), 31)]
        frames = [envelope(chunk, "response", values, i) for i, chunk in enumerate(chunks)]
        frames.append(envelope(b"", "response", values, len(frames), True))
        process = subprocess.run([str(FILTER)], input=b"".join(json.dumps(f).encode()+b"\n" for f in frames),
                                 capture_output=True, check=True)
        restored = b"".join(base64.b64decode(json.loads(line)["body_base64"]) for line in process.stdout.splitlines())
        text = ""
        arguments = ""
        for line in restored.splitlines():
            if line.startswith(b"data: "):
                value = json.loads(line[6:])
                delta = value.get("delta", {})
                text += delta.get("text", "")
                arguments += delta.get("partial_json", "")
        self.assertEqual(SECRET, text)
        self.assertEqual({"content": SECRET}, json.loads(arguments))
        self.assertNotIn(placeholder.encode(), restored)


if __name__ == "__main__":
    unittest.main()
