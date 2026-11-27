#!/usr/bin/env python3
"""Run an isolated ego binary against local inventory/generation fixtures only."""
import argparse
from datetime import datetime, timezone
import hashlib
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import http.client
import json
from pathlib import Path
import subprocess
import threading
import time
import uuid

ROOT = Path(__file__).resolve().parents[1]
INVENTORY = ROOT / "crates/ego-ai/tests/goldens/openrouter/models-2026-10-15.json"
MODEL = "openai/gpt-6-luna"


def journal_rows(directory):
    rows = []
    for path in sorted(directory.glob("*/events.jsonl")):
        for line in path.read_text().splitlines():
            record = json.loads(line)
            event = record.get("payload", {}).get("event")
            if event:
                rows.append({"seq": record["seq"], **event["kind"]})
    return rows


def stream(tool):
    # Chat Completions fixture shape: the repository's schema-derived stream goldens.
    delta = {"role": "assistant"}
    if tool:
        delta["tool_calls"] = [{"index": 0, "id": "fixture_read", "type": "function",
                                "function": {"name": "read", "arguments": '{"path":"marker.txt","offset":0}'}}]
    else:
        delta["content"] = "inventory survived local outage"
    chunk = {"id": "fixture-generation", "object": "chat.completion.chunk", "created": 1792710000,
             "model": MODEL, "choices": [{"index": 0, "delta": delta, "finish_reason": None}]}
    finish = {**chunk, "choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls" if tool else "stop"}]}
    usage = {**chunk, "choices": [], "usage": {"prompt_tokens": 10, "completion_tokens": 5,
                                                "total_tokens": 15, "cost": 0}}
    return ("".join("data: " + json.dumps(row) + "\n\n" for row in [chunk, finish, usage])
            + "data: [DONE]\n\n").encode()


def run_case(binary, output, mode):
    case = output / mode
    case.mkdir()
    for child in ["home", "ego", "xdg", "tmp", "workspace", "sessions"]:
        (case / child).mkdir()
    (case / "workspace/marker.txt").write_text("local fixture marker\n")
    state = {"inventory_gets": 0, "generation_posts": 0, "requests": [], "outage": False}
    stop = threading.Event()

    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *_args):
            pass

        def reply(self, content, content_type):
            self.send_response(200)
            self.send_header("content-type", content_type)
            self.send_header("content-length", str(len(content)))
            self.send_header("connection", "close")
            self.end_headers()
            self.wfile.write(content)
            self.wfile.flush()

        def do_GET(self):
            probe = self.headers.get("x-fixture-probe") == "1"
            state["requests"].append({"method": "GET", "path": self.path, "probe": probe,
                                      "outage": state["outage"], "time_ns": time.monotonic_ns()})
            if self.path != "/api/v1/models":
                self.send_error(404)
                return
            if not probe:
                state["inventory_gets"] += 1
            if state["outage"] or (mode == "timeout" and state["inventory_gets"] > 1):
                state["outage"] = True
                stop.wait(80)
                return
            if mode == "connect":
                # Close the listening socket before delivering the startup inventory.
                # The startup connection remains valid; the turn's fresh connection refuses.
                self.server.shutdown()
                self.server.server_close()
                state["outage"] = True
            self.reply(INVENTORY.read_bytes(), "application/json")

        def do_POST(self):
            request = json.loads(self.rfile.read(int(self.headers.get("content-length", "0"))))
            state["generation_posts"] += 1
            first = state["generation_posts"] == 1
            state["requests"].append({"method": "POST", "path": self.path,
                                      "message_roles": [row["role"] for row in request["messages"]],
                                      "time_ns": time.monotonic_ns()})
            if mode != "continuation" or self.path != "/api/v1/chat/completions":
                self.send_error(500)
                return
            if first:
                state["outage"] = True
            self.reply(stream(first), "text/event-stream")

    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    server.daemon_threads = True
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    command = [str(binary), "run", "--bare", "--offline", "--mode", "yolo", "--sandbox", "off",
               "--tools", "read", "--retry", "0", "--model", "openrouter/" + MODEL,
               "--thinking", "low", "--system", "Use the local read tool, then finish.",
               "--timeout", "50s", "--output", "jsonl", "--session-id", str(uuid.uuid4()),
               "--session-dir", str(case / "sessions"), "-C", str(case / "workspace"),
               "Read marker.txt, then report success."]
    # Deliberately do not inherit credentials, host connections or user configuration.
    environment = {"PATH": "/usr/bin:/bin", "HOME": str(case / "home"),
                   "EGO_HOME": str(case / "ego"), "XDG_CONFIG_HOME": str(case / "xdg"),
                   "TMPDIR": str(case / "tmp") + "/", "NO_COLOR": "1",
                   "OPENROUTER_API_KEY": "fixture-not-a-real-key",
                   "OPENROUTER_BASE_URL": f"http://127.0.0.1:{server.server_port}/api/v1/"}
    try:
        result = subprocess.run(command, env=environment, cwd=case / "workspace",
                                capture_output=True, text=True, timeout=65)
        if mode == "continuation" and state["outage"]:
            probe = http.client.HTTPConnection("127.0.0.1", server.server_port, timeout=0.25)
            try:
                probe.request("GET", "/api/v1/models", headers={"x-fixture-probe": "1"})
                probe.getresponse()
                state["probe_timed_out"] = False
            except OSError as error:
                state["probe_error"] = str(error)
                state["probe_timed_out"] = "timed out" in str(error).lower()
            finally:
                probe.close()
    finally:
        stop.set()
        server.shutdown()
        server.server_close()
        thread.join(timeout=2)
    (case / "stdout.jsonl").write_text(result.stdout)
    (case / "stderr.txt").write_text(result.stderr)
    events = journal_rows(case / "sessions")
    summary = {"mode": mode, "exit": result.returncode, "state": state, "events": events,
               "stderr": result.stderr, "command": command,
               "journals": [{"path": str(path.relative_to(output)),
                             "sha256": hashlib.sha256(path.read_bytes()).hexdigest()}
                            for path in sorted((case / "sessions").glob("*/events.jsonl"))]}
    (case / "summary.json").write_text(json.dumps(summary, indent=2) + "\n")
    print(json.dumps({"mode": mode, "exit": result.returncode, "inventory_gets": state["inventory_gets"],
                      "generation_posts": state["generation_posts"],
                      "events": [row["kind"] for row in events], "stderr": result.stderr}))
    return summary



def verify(summaries):
    for path in summaries:
        path = Path(path).resolve(strict=True)
        case = path.parent
        summary = json.loads(path.read_text())
        events = journal_rows(case / "sessions")
        assert events == summary["events"], "summary does not match its durable journal"
        for journal in summary["journals"]:
            actual = case.parent / journal["path"]
            assert hashlib.sha256(actual.read_bytes()).hexdigest() == journal["sha256"]
        state = summary["state"]
        completed = [row for row in events if row["kind"] == "TurnCompleted"]
        assert len(completed) == 1
        if summary["mode"] == "continuation":
            assert summary["exit"] == 0 and completed[0]["ok"]
            assert state["inventory_gets"] == 2 and state["generation_posts"] == 2
            assert state["probe_timed_out"]
            tools = [row for row in events if row["kind"] == "ToolCallCompleted"]
            generations = [row for row in events if row["kind"] == "GenerationStarted"]
            assert len(tools) == 1 and tools[0]["ok"] and len(generations) == 2
            assert generations[0]["seq"] < tools[0]["seq"] < generations[1]["seq"] < completed[0]["seq"]
            first_post = next(row["time_ns"] for row in state["requests"] if row["method"] == "POST")
            assert not any(row["method"] == "GET" and not row["probe"] and row["time_ns"] > first_post
                           for row in state["requests"])
            assert any(row["kind"] == "AssistantMessage" and row["text"] == "inventory survived local outage"
                       for row in events)
        else:
            assert summary["exit"] == (4 if summary["mode"] == "connect" else 124)
            assert not completed[0]["ok"] and state["generation_posts"] == 0
            errors = [row for row in events if row["kind"] == "Error"]
            assert len(errors) == 1
            message = errors[0]["message"]
            assert "127.0.0.1" in message and "openrouter models" in message
            assert "fixture-not-a-real-key" not in message
            expected = ["connection failed", "Connect", "tcp connect error", "Connection refused"] if summary["mode"] == "connect" else ["HTTP call timed out"]
            assert all(fragment in message for fragment in expected)
            displayed = [json.loads(line) for line in (case / "stdout.jsonl").read_text().splitlines()]
            assert any(row.get("type") == "error" and row["message"] == message for row in displayed)
        print("VERIFIED " + summary["mode"] + ": " + str(path))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path)
    parser.add_argument("--output", type=Path)
    parser.add_argument("--case", action="append", choices=["continuation", "connect", "timeout"])
    parser.add_argument("--verify", action="append", type=Path, help="Verify an existing case summary against its raw journal and stdout")
    args = parser.parse_args()
    if args.verify:
        verify(args.verify)
        return
    if args.binary is None or args.output is None:
        parser.error("--binary and --output are required unless --verify is used")
    binary = args.binary.resolve(strict=True)
    binary_digest = hashlib.sha256(binary.read_bytes()).hexdigest()
    output = args.output.resolve()
    output.mkdir(exist_ok=False)
    summaries = [run_case(binary, output, mode) for mode in (args.case or ["continuation", "connect", "timeout"])]
    result = {"recorded_at": datetime.now(timezone.utc).isoformat(), "binary": str(binary),
              "binary_sha256": binary_digest,
              "inventory_sha256": hashlib.sha256(INVENTORY.read_bytes()).hexdigest(), "cases": summaries}
    (output / "result.json").write_text(json.dumps(result, indent=2) + "\n")
    verify([output / row["mode"] / "summary.json" for row in summaries])


if __name__ == "__main__":
    main()
