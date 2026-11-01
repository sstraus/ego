"""Record a real ego binary against an owned, deterministic loopback provider."""

import argparse
import errno
import fcntl
import http.server
import json
import os
from pathlib import Path
import pty
import struct
import subprocess
import termios
import threading


class Provider(http.server.BaseHTTPRequestHandler):
    def log_message(self, *_args):
        pass

    def do_GET(self):
        self.reply()

    def do_POST(self):
        self.reply()

    def reply(self):
        self.rfile.read(int(self.headers.get("content-length", "0")))
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
            chunks = [
                {"choices": [{"index": 0, "delta": {"role": "assistant", "content": "Recorded native run."}, "finish_reason": None}]},
                {"choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]},
                {"choices": [], "usage": {"prompt_tokens": 10, "completion_tokens": 4, "total_tokens": 14}},
            ]
            data = ("".join("data: " + json.dumps(row) + "\n\n" for row in chunks) + "data: [DONE]\n\n").encode()
            content_type = "text/event-stream"
        self.send_response(200)
        self.send_header("content-type", content_type)
        self.send_header("content-length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)


def record(args, provider):
    root = args.output_dir.resolve()
    root.mkdir(parents=True, exist_ok=True)
    env = {
        "PATH": os.environ["PATH"],
        "HOME": os.environ["HOME"],
        "EGO_HOME": str(root / "home"),
        "OLLAMA_HOST": f"127.0.0.1:{provider.server_port}",
    }
    binary = str(args.binary.resolve())
    command = [binary, "run", "--bare", "--no-tools", "--thinking", "off", "--model", "ollama/fixture", "--session-dir", str(root / "sessions"), "--retry", "0", "--timeout", "15s", "--output", "text" if args.pty else "jsonl", "Report readiness."]
    version = subprocess.check_output([binary, "--version"], env=env, text=True).strip()
    metadata = {"version": version, "command": command, "fixture_provider": "owned loopback server; no remote provider or paid turn", "order": "raw PTY bytes" if args.pty else "stdout rows in emission order"}
    if args.pty:
        env["TERM"] = "xterm-256color"
        if os.environ.get("TUIC_SOCKET"):
            env["TUIC_SOCKET"] = os.environ["TUIC_SOCKET"]
        master, slave = pty.openpty()
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 24, 80, 0, 0))
        process = subprocess.Popen(command, env=env, stdin=slave, stdout=slave, stderr=slave, start_new_session=True)
        os.close(slave)
        output = bytearray()
        try:
            while True:
                try:
                    chunk = os.read(master, 65536)
                except OSError as error:
                    if error.errno == errno.EIO:
                        break
                    raise
                if not chunk:
                    break
                output.extend(chunk)
            code = process.wait(timeout=5)
        finally:
            os.close(master)
            if process.poll() is None:
                process.kill()
                process.wait()
        metadata.update(exit=code, geometry=[80, 24], tuic_socket_present=bool(env.get("TUIC_SOCKET")))
        suffix = "pty.bin"
    else:
        result = subprocess.run(command, env=env, capture_output=True, timeout=25)
        output = result.stdout
        metadata.update(exit=result.returncode, terminal=False)
        (root / f"{args.name}.stderr.txt").write_bytes(result.stderr)
        suffix = "jsonl"
    (root / f"{args.name}.{suffix}").write_bytes(output)
    (root / f"{args.name}.meta.json").write_text(json.dumps(metadata, indent=2) + "\n")
    print(f"exit={metadata['exit']} bytes={len(output)} output={root / (args.name + '.' + suffix)}")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--output-dir", type=Path, required=True)
    parser.add_argument("--name", required=True)
    parser.add_argument("--pty", action="store_true")
    args = parser.parse_args()
    provider = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Provider)
    worker = threading.Thread(target=provider.serve_forever, daemon=True)
    worker.start()
    try:
        record(args, provider)
    finally:
        provider.shutdown()
        provider.server_close()
        worker.join()


if __name__ == "__main__":
    main()
