"""Record one LSP exchange with a real language server, frame by frame.

Provenance of gopls-0.22.0.jsonl: gopls v0.22.0 (go1.27.3, darwin/arm64) on the
`workspace/` module, 2026-11-05. The text of window/logMessage frames was then
replaced, because it names local directories; nothing else was changed.

Usage: python3 record.py <workspace> <out.jsonl> <server argv...>
Every frame is written as {"from": "client"|"server", "message": {...}}. The
workspace path is replaced by /workspace so the recording is portable.
"""
import json, subprocess, sys, threading, time, pathlib, queue

root = pathlib.Path(sys.argv[1]).resolve(); out = sys.argv[2]; argv = sys.argv[3:]
proc = subprocess.Popen(argv, cwd=root, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
frames = []; inbox = queue.Queue(); lock = threading.Lock()
def scrub(value):
    return json.loads(json.dumps(value).replace(root.as_uri(), "file:///workspace").replace(str(root), "/workspace"))
def send(message):
    body = json.dumps(message).encode()
    with lock: frames.append({"from": "client", "message": scrub(message)})
    proc.stdin.write(b"Content-Length: %d\r\n\r\n" % len(body) + body); proc.stdin.flush()
def reader():
    while True:
        headers = {}
        while True:
            line = proc.stdout.readline()
            if not line: inbox.put(None); return
            line = line.decode().strip()
            if not line: break
            k, v = line.split(":", 1); headers[k.lower()] = v.strip()
        body = json.loads(proc.stdout.read(int(headers["content-length"])))
        with lock: frames.append({"from": "server", "message": scrub(body)})
        inbox.put(body)
threading.Thread(target=reader, daemon=True).start()
next_id = [0]
def request(method, params, timeout=60):
    next_id[0] += 1; rid = next_id[0]; send({"jsonrpc": "2.0", "id": rid, "method": method, "params": params})
    deadline = time.time() + timeout
    while time.time() < deadline:
        msg = inbox.get(timeout=deadline - time.time())
        if msg is None: raise SystemExit("server closed")
        handle(msg)
        if msg.get("id") == rid and "method" not in msg: return msg
    raise SystemExit(f"timeout on {method}")
def handle(msg):
    if "method" in msg and "id" in msg:  # server -> client request
        result = [None] * len(msg["params"].get("items", [])) if msg["method"] == "workspace/configuration" else None
        send({"jsonrpc": "2.0", "id": msg["id"], "result": result})
def drain(seconds, until=None):
    deadline = time.time() + seconds
    while time.time() < deadline:
        try: msg = inbox.get(timeout=max(0.01, deadline - time.time()))
        except queue.Empty: return
        if msg is None: return
        handle(msg)
        if until and until(msg): return
uri = lambda name: (root / name).as_uri()
request("initialize", {"processId": None, "rootUri": root.as_uri(), "workspaceFolders": [{"uri": root.as_uri(), "name": "workspace"}],
    "capabilities": {"general": {"positionEncodings": ["utf-16"]}, "textDocument": {"publishDiagnostics": {}, "definition": {}, "references": {}}}})
send({"jsonrpc": "2.0", "method": "initialized", "params": {}})
for name in ("use.go",):
    send({"jsonrpc": "2.0", "method": "textDocument/didOpen", "params": {"textDocument": {"uri": uri(name), "languageId": "go", "version": 1, "text": (root / name).read_text()}}})
drain(20, until=lambda m: m.get("method") == "textDocument/publishDiagnostics" and m["params"]["uri"] == uri("use.go") and m["params"]["diagnostics"])
# line 4 (0-based 3): '\ta := "héllo😀"; b := Greeting(a)'; Greeting starts after the emoji (UTF-16 surrogate pair)
line = (root / "use.go").read_text().splitlines()[3]
col_utf16 = len(line[: line.index("Greeting")].encode("utf-16-le")) // 2
request("textDocument/definition", {"textDocument": {"uri": uri("use.go")}, "position": {"line": 3, "character": col_utf16}})
request("textDocument/references", {"textDocument": {"uri": uri("use.go")}, "position": {"line": 3, "character": col_utf16}, "context": {"includeDeclaration": True}})
request("shutdown", None)
send({"jsonrpc": "2.0", "method": "exit"})
proc.wait(timeout=10)
with open(out, "w") as f:
    for frame in frames: f.write(json.dumps(frame, ensure_ascii=False) + "\n")
print(len(frames), "frames")
