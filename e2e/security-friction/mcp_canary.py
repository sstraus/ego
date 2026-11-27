"""Minimal stdio MCP server for the friction matrix.

Tools: `canary` returns the run canary; `save_note` writes a file in the
outside directory (an opaque external effect).
Usage: mcp_canary.py <canary> <outside_dir>
"""

import json
import sys
from pathlib import Path

CANARY, OUTSIDE = sys.argv[1], Path(sys.argv[2])
TOOLS = [
    {"name": "canary", "description": "Return the e2e canary token.",
     "inputSchema": {"type": "object", "properties": {}}},
    {"name": "save_note", "description": "Save a note file named NAME with TEXT.",
     "inputSchema": {"type": "object", "properties": {
         "name": {"type": "string"}, "text": {"type": "string"}},
         "required": ["name", "text"]}},
]


def reply(rid, result):
    result = {"resultType": "complete", "ttlMs": 0, "cacheScope": "private", **result}
    sys.stdout.write(json.dumps({"jsonrpc": "2.0", "id": rid, "result": result}) + "\n")
    sys.stdout.flush()


for line in sys.stdin:
    msg = json.loads(line)
    method, rid = msg.get("method"), msg.get("id")
    if rid is None:
        continue
    if method == "server/discover":
        reply(rid, {"supportedVersions": ["2026-07-28"], "capabilities": {"tools": {}},
                    "serverInfo": {"name": "e2ecanary", "version": "1.0.0"},
                    "ttlMs": 0, "cacheScope": "private"})
    elif method == "initialize":
        reply(rid, {"protocolVersion": msg["params"].get("protocolVersion", "2025-06-18"),
                    "capabilities": {"tools": {}},
                    "serverInfo": {"name": "e2ecanary", "version": "1.0.0"}})
    elif method == "tools/list":
        reply(rid, {"tools": TOOLS})
    elif method == "tools/call":
        name, args = msg["params"]["name"], msg["params"].get("arguments", {})
        if name == "canary":
            text = CANARY
        else:
            (OUTSIDE / Path(args["name"]).name).write_text(args["text"])
            text = "saved"
        reply(rid, {"content": [{"type": "text", "text": text}]})
    else:
        sys.stdout.write(json.dumps({"jsonrpc": "2.0", "id": rid, "error": {
            "code": -32601, "message": f"method not found: {method}"}}) + "\n")
        sys.stdout.flush()
