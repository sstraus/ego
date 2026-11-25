"""Record native ego ACP stdio against an owned deterministic loopback provider.

No credentials, installed binary, user config, PTY, or remote provider is used.
"""

import argparse
import http.server
import json
import os
from pathlib import Path
import queue
import subprocess
import threading


class Provider(http.server.BaseHTTPRequestHandler):
    requests = []
    cancel_started = threading.Event()
    cancel_release = threading.Event()

    def log_message(self, *_args):
        pass

    def do_GET(self):
        self.do_POST()

    def do_POST(self):
        raw = self.rfile.read(int(self.headers.get("content-length", "0")))
        body = json.loads(raw) if raw else {}
        self.requests.append({"path":self.path,"body":body})
        if self.path == "/api/tags":
            value = {"models": [{"name": "fixture", "capabilities": ["completion", "tools"]}]}
        elif self.path == "/api/ps":
            value = {"models": [{"name": "fixture", "context_length": 32768}]}
        elif self.path == "/api/chat":
            value = {"done_reason": "load"}
        else:
            value = None
        if value is not None:
            data = json.dumps(value).encode()
            content_type = "application/json"
        else:
            messages = body.get("messages", [])
            last_user = max((i for i, m in enumerate(messages) if m.get("role") == "user" and not str(m.get("content", "")).startswith("Current context data.")), default=-1)
            user = str(messages[last_user].get("content", "")) if last_user >= 0 else ""
            called = any(m.get("role") == "tool" for m in messages[last_user + 1:])
            delta = {"role": "assistant", "content": "Recorded ACP completion."}
            finish = "stop"
            tool = None
            if not called:
                if "CAPTURE_CANCEL_CHILD" in user:
                    tool = ("task", {"name": "cancel capture child", "prompt": "CAPTURE_WAIT"})
                elif "CAPTURE_CHILD" in user:
                    tool = ("task", {"name": "capture child", "prompt": "Return a short readiness answer."})
                elif "CAPTURE_QUESTION" in user:
                    tool = ("ask_question", {"prompt": "Recorded ACP question?", "mode": "single", "options": [{"id": "yes", "label": "Yes"}], "free_text": False})
                elif "CAPTURE_DENY" in user:
                    tool = ("exec", {"argv": ["/bin/echo", "capture denied"]})
                elif "CAPTURE_WAIT" in user:
                    self.cancel_started.set()
                    self.cancel_release.wait(10)
            if tool:
                name, arguments = tool
                delta = {"role": "assistant", "tool_calls": [{"index": 0, "id": "capture_" + str(len(self.requests)), "type": "function", "function": {"name": name, "arguments": json.dumps(arguments)}}]}
                finish = "tool_calls"
            chunks = [
                {"choices": [{"index": 0, "delta": delta, "finish_reason": None}]},
                {"choices": [{"index": 0, "delta": {}, "finish_reason": finish}]},
                {"choices": [], "usage": {"prompt_tokens": 10, "completion_tokens": 4, "total_tokens": 14}},
            ]
            data = ("".join("data: " + json.dumps(row) + "\n\n" for row in chunks) + "data: [DONE]\n\n").encode()
            content_type = "text/event-stream"
        try:
            self.send_response(200)
            self.send_header("content-type", content_type)
            self.send_header("content-length", str(len(data)))
            self.end_headers()
            self.wfile.write(data)
        except (BrokenPipeError, ConnectionResetError):
            pass


class Host:
    def __init__(self, binary, root, env, transcript, extension):
        self.transcript = transcript
        self.process = subprocess.Popen([str(binary), "acp", "--bare", "--tools", "task,ask_question,exec", "--session-dir", str(root / "sessions"), "-C", str(root / "workspace")], env=env, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        self.lines = queue.Queue()
        self.reader = threading.Thread(target=self.read, daemon=True)
        self.reader.start()
        self.next_id = 1
        self.responses = {}
        caps = {"elicitation": {"form": {}}}
        if extension:
            caps["_meta"] = {"ego": {"childActivity": {"version": 1}}}
        self.request("initialize", {"protocolVersion": 1, "clientCapabilities": caps})

    def read(self):
        for line in self.process.stdout:
            self.lines.put(line)
        self.lines.put(None)

    def send(self, message):
        self.transcript.append({"direction": "client_to_ego", "message": message})
        self.process.stdin.write(json.dumps(message) + "\n")
        self.process.stdin.flush()

    def send_request(self, method, params):
        request_id = self.next_id
        self.next_id += 1
        self.send({"jsonrpc": "2.0", "id": request_id, "method": method, "params": params})
        return request_id

    def response(self, request_id):
        if request_id in self.responses:
            return self.responses.pop(request_id)
        while True:
            line = self.lines.get(timeout=45)
            if line is None:
                raise RuntimeError("ego exited: " + self.process.stderr.read())
            message = json.loads(line)
            self.transcript.append({"direction": "ego_to_client", "message": message})
            if "method" in message and "id" in message:
                if message["method"] == "session/request_permission":
                    option = next(o for o in message["params"]["options"] if o["kind"] == "reject_once")
                    result = {"outcome": {"outcome": "selected", "optionId": option["optionId"]}}
                elif "elicitation" in message["method"]:
                    result = {"action": "accept", "content": {"answer": "yes"}}
                else:
                    raise RuntimeError("unexpected human request: " + line)
                self.send({"jsonrpc": "2.0", "id": message["id"], "result": result})
            elif message.get("id") == request_id:
                return message
            elif "id" in message:
                self.responses[message["id"]] = message

    def request(self, method, params):
        return self.response(self.send_request(method, params))

    def new(self, root):
        return self.request("session/new", {"cwd": str(root / "workspace"), "mcpServers": []})["result"]["sessionId"]

    def prompt(self, session, text):
        return self.request("session/prompt", {"sessionId": session, "prompt": [{"type": "text", "text": text}]})

    def close(self):
        self.process.stdin.close()
        try:
            code = self.process.wait(timeout=10)
        except subprocess.TimeoutExpired:
            self.process.kill()
            code = self.process.wait()
        return {"exit": code, "stderr": self.process.stderr.read()}


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--output-dir", type=Path, required=True)
    args = parser.parse_args()
    root = args.output_dir.resolve()
    root.mkdir(parents=True, exist_ok=False)
    (root / "home").mkdir()
    (root / "workspace").mkdir()
    (root / "home" / "config.toml").write_text("model = 'ollama/fixture'\nthinking = 'off'\nsandbox = 'off'\nmode = 'default'\n")
    provider = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Provider)
    thread = threading.Thread(target=provider.serve_forever, daemon=True)
    thread.start()
    env = {"PATH": os.environ["PATH"], "HOME": os.environ["HOME"], "EGO_HOME": str(root / "home"), "OLLAMA_HOST": f"127.0.0.1:{provider.server_port}"}
    binary = args.binary.resolve()
    transcript = []
    metadata = {"version": subprocess.check_output([str(binary), "--version"], env=env, text=True).strip(), "binary": str(binary), "transport": "stdio", "provider": "owned loopback; no remote or paid turn", "cases": {}}
    host = None
    try:
        host = Host(binary, root, env, transcript, True)
        first = host.new(root)
        second = host.new(root)
        metadata["sessions"] = [first, second]
        metadata["cases"]["child"] = host.prompt(first, "CAPTURE_CHILD")
        metadata["cases"]["no_child"] = host.prompt(second, "CAPTURE_NO_CHILD")
        question_session = host.new(root)
        metadata["cases"]["question"] = host.prompt(question_session, "CAPTURE_QUESTION")
        deny_session = host.new(root)
        metadata["cases"]["permission"] = host.prompt(deny_session, "CAPTURE_DENY")
        cancel_session = host.new(root)
        request = host.send_request("session/prompt", {"sessionId": cancel_session, "prompt": [{"type": "text", "text": "CAPTURE_CANCEL_CHILD"}]})
        if not Provider.cancel_started.wait(15):
            raise RuntimeError("cancel generation did not start")
        host.send({"jsonrpc": "2.0", "method": "session/cancel", "params": {"sessionId": cancel_session}})
        host.request("session/list", {})
        Provider.cancel_release.set()
        metadata["cases"]["cancel"] = host.response(request)
        assert metadata["cases"]["cancel"]["result"]["stopReason"] == "cancelled"
        metadata["first_process"] = host.close()
        host = None
        host = Host(binary, root, env, transcript, True)
        metadata["cases"]["reconnect"] = host.request("session/load", {"sessionId": first, "cwd": str(root / "workspace"), "mcpServers": []})
        metadata["cases"]["close"] = host.request("session/close", {"sessionId": first})
        metadata["second_process"] = host.close()
        host = None
        host = Host(binary, root, env, transcript, False)
        plain = host.new(root)
        metadata["cases"]["extension_absent"] = host.prompt(plain, "CAPTURE_CHILD")
    finally:
        if host:
            metadata["last_process"] = host.close()
        Provider.cancel_release.set()
        provider.shutdown()
        provider.server_close()
        thread.join()
        (root / "wire.jsonl").write_text("".join(json.dumps(row) + "\n" for row in transcript))
        (root / "provider.json").write_text(json.dumps(Provider.requests, indent=2) + "\n")
        (root / "metadata.json").write_text(json.dumps(metadata, indent=2) + "\n")
    print(json.dumps(metadata, indent=2))


if __name__ == "__main__":
    main()
