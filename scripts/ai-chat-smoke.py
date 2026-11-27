#!/usr/bin/env python3
"""Exercise the candidate ACP binary with the live TUIC MCP catalogue.

Uses the real user profile and credentials through ego itself. Writes only safe
status/tool-name diagnostics under ~/Gits/.tmp. Never installs the candidate.
"""
import argparse
import http.client
import json
import os
from pathlib import Path
import queue
import re
import socket
import subprocess
import threading
import time


class UnixHTTP(http.client.HTTPConnection):
    def __init__(self, path, timeout):
        super().__init__("localhost", timeout=timeout)
        self.path = path

    def connect(self):
        self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.sock.settimeout(self.timeout)
        self.sock.connect(self.path)


class Host:
    def __init__(self, binary, workspace, sessions, log):
        env = dict(os.environ, EGO_LOG="ego_ai::tool_names=debug,ego_ai::provider_status=debug", EGO_LOG_FILE=str(log))
        self.child = subprocess.Popen([binary, "acp", "-C", str(workspace), "--session-dir", str(sessions)], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True, env=env)
        self.frames = queue.Queue()
        self.next_id = 0
        self.reply = ""
        self.options = []
        self.mcp_id = 0
        self.socket = os.environ.get("TUIC_SOCKET", str(Path.home() / "Library/Application Support/com.tuic.commander/mcp.sock"))
        threading.Thread(target=self.read, daemon=True).start()

    def read(self):
        for line in self.child.stdout:
            try:
                self.frames.put(json.loads(line))
            except ValueError:
                self.frames.put({"error": {"message": "invalid ACP JSON"}})
        self.frames.put({"error": {"message": "ACP process exited"}})

    def send(self, frame):
        self.child.stdin.write(json.dumps({"jsonrpc": "2.0", **frame}) + "\n")
        self.child.stdin.flush()

    def mcp(self, method, params):
        self.mcp_id += 1
        params = dict(params or {})
        meta = params.setdefault("_meta", {})
        meta.setdefault("io.modelcontextprotocol/clientInfo", {"name": "ego", "version": "smoke"})
        meta.setdefault("io.modelcontextprotocol/protocolVersion", "2026-07-28")
        connection = UnixHTTP(self.socket, timeout=30)
        try:
            connection.request("POST", "/mcp", json.dumps({"jsonrpc": "2.0", "id": self.mcp_id, "method": method, "params": params}), {"Content-Type": "application/json", "Accept": "application/json, text/event-stream"})
            response = connection.getresponse()
            data = response.read().decode()
            if response.status != 200:
                raise RuntimeError(f"TUIC MCP HTTP {response.status}")
            if data.startswith("event:") or data.startswith("data:"):
                data = next(line[5:].strip() for line in data.splitlines() if line.startswith("data:"))
            frame = json.loads(data)
            if "error" in frame:
                raise RuntimeError("TUIC MCP " + frame["error"].get("message", "failed"))
            return frame["result"]
        finally:
            connection.close()

    def request(self, method, params):
        self.next_id += 1
        wanted = self.next_id
        self.send({"id": wanted, "method": method, "params": params})
        deadline = time.monotonic() + 120
        while True:
            frame = self.frames.get(timeout=max(0.01, deadline - time.monotonic()))
            if "error" in frame:
                # Do not echo arbitrary upstream data, which may contain secrets.
                raise RuntimeError(f"{method}: ACP error {frame['error'].get('code', 'process')}: {frame['error'].get('message', 'failed')}")
            if frame.get("id") == wanted and "method" not in frame:
                return frame["result"]
            self.handle(frame)

    def handle(self, frame):
        name = frame.get("method")
        result = None
        if name == "mcp/connect":
            result = {"connectionId": "smoke-tuic"}
        elif name == "mcp/disconnect":
            result = {}
        elif name == "mcp/message" and "id" in frame:
            p = frame["params"]
            result = self.mcp(p["method"], p.get("params"))
        elif name == "session/request_permission":
            raise RuntimeError("unexpected permission request")
        elif name == "session/update":
            update = frame["params"]["update"]
            if update.get("sessionUpdate") == "config_option_update":
                self.options = update.get("configOptions", [])
            if update.get("sessionUpdate") == "agent_message_chunk":
                self.reply += update.get("content", {}).get("text", "")
        if result is not None:
            self.send({"id": frame["id"], "result": result})

    def wait_controls(self):
        deadline = time.monotonic() + 120
        while not self.options:
            self.handle(self.frames.get(timeout=max(0.01, deadline - time.monotonic())))

    def close(self):
        self.child.stdin.close()
        try:
            self.child.wait(timeout=5)
        except subprocess.TimeoutExpired:
            self.child.terminate()
            self.child.wait(timeout=5)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary")
    parser.add_argument("--workspace", type=Path, default=Path.home() / "Gits")
    parser.add_argument("--models", nargs="+", help="Explicit models; default: one available model per configured provider")
    args = parser.parse_args()
    binary = str(Path(args.binary).resolve())
    catalogue = json.loads(subprocess.check_output([binary, "models", "--json"], text=True))
    configured = json.loads(subprocess.check_output([binary, "config", "ls", "--json"], text=True))["values"].get("model")
    models = {}
    for model in catalogue["models"]:
        if model["availability"]["status"] == "available":
            models.setdefault(model["slug"].split("/")[0], model["slug"])
    if configured:
        models[configured.split("/")[0]] = configured
    selected = args.models or sorted(models.values(), key=lambda m: (not m.startswith("openai-codex/"), m))
    if not selected:
        raise RuntimeError("no configured provider has an available model")
    run = Path.home() / "Gits/.tmp" / f"ai-chat-smoke-{time.time_ns()}"
    run.mkdir(parents=True)
    print(f"Evidence: {run}", flush=True)
    servers = [{"type": "acp", "name": "tuicommander", "serverId": "tuicommander"}]
    failures = 0
    for model in selected:
        host = None
        try:
            log = run / (model.replace("/", "-").replace(":", "-") + ".log")
            host = Host(binary, args.workspace, run / "sessions", log)
            host.request("initialize", {"protocolVersion": 1, "clientCapabilities": {"mcp": {"acp": True}}, "clientInfo": {"name": "TUICommander smoke", "version": "1"}})
            opened = host.request("session/new", {"cwd": str(args.workspace), "mcpServers": servers})
            session = opened["sessionId"]
            if model != configured:
                host.options = opened.get("configOptions", [])
                host.wait_controls()
                host.request("session/set_config_option", {"sessionId": session, "configId": "model", "value": model})
            for phase in ("new", "resumed"):
                if phase == "resumed":
                    host.close()
                    host = Host(binary, args.workspace, run / "sessions", log)
                    host.request("initialize", {"protocolVersion": 1, "clientCapabilities": {"mcp": {"acp": True}}})
                    loaded = host.request("session/load", {"sessionId": session, "cwd": str(args.workspace), "mcpServers": servers})
                    if model != configured:
                        host.options = loaded.get("configOptions", [])
                        host.wait_controls()
                        host.request("session/set_config_option", {"sessionId": session, "configId": "model", "value": model})
                host.reply = ""
                before = log.stat().st_size if log.exists() else 0
                result = host.request("session/prompt", {"sessionId": session, "prompt": [{"type": "text", "text": "Reply only OK. Do not use tools."}]})
                diagnostic = log.read_text()[before:]
                if result.get("stopReason") != "end_turn" or not host.reply.strip():
                    raise RuntimeError(f"{phase}: no successful non-empty reply")
                request_model = model.split("/", 1)[1]
                if not re.search(r"\bmodel=" + re.escape(request_model) + r"(?:\s|$)", diagnostic):
                    raise RuntimeError(f"{phase}: requested model was not sent to the provider")
                if "status=200" not in diagnostic:
                    raise RuntimeError(f"{phase}: no recorded provider HTTP 200")
                print(f"PASS {model} {phase} HTTP 200 non-empty reply session={session}", flush=True)
        except Exception as error:
            print(f"FAIL {model}: {error}", flush=True)
            failures += 1
        finally:
            if host:
                host.close()
    return bool(failures)


if __name__ == "__main__":
    raise SystemExit(main())
