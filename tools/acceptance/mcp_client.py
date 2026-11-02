#!/usr/bin/env python3
"""A minimal stdio MCP client for acceptance scenarios.

    mcp_client.py SERVER [ARG...]

Starts the server, initializes it and prints {"ready": true, "tools": [...]}.
Then reads one {"name": ..., "arguments": ...} JSON object per line and prints
the server's whole JSON-RPC response to each tools/call on one line.
"""

import json
import select
import subprocess
import sys

process = subprocess.Popen(
    sys.argv[1:],
    stdin=subprocess.PIPE,
    stdout=subprocess.PIPE,
    stderr=sys.stderr,
    text=True,
    bufsize=1,
)


def send(message):
    process.stdin.write(json.dumps(message) + "\n")
    process.stdin.flush()


def call(identifier, method, params):
    send({"jsonrpc": "2.0", "id": identifier, "method": method, "params": params})
    while True:
        if not select.select([process.stdout], [], [], 60)[0]:
            raise TimeoutError(f"MCP {method} did not respond")
        line = process.stdout.readline()
        if not line:
            raise RuntimeError("MCP server exited")
        message = json.loads(line)
        if message.get("id") == identifier and "method" not in message:
            return message


call(
    1,
    "initialize",
    {
        "protocolVersion": "2024-11-05",
        "capabilities": {},
        "clientInfo": {"name": "waywire-acceptance", "version": "1"},
    },
)
send({"jsonrpc": "2.0", "method": "notifications/initialized"})
tools = call(2, "tools/list", {}).get("result", {}).get("tools", [])
print(json.dumps({"ready": True, "tools": [tool["name"] for tool in tools]}), flush=True)
try:
    for identifier, line in enumerate(sys.stdin, start=3):
        print(json.dumps(call(identifier, "tools/call", json.loads(line))), flush=True)
finally:
    process.stdin.close()
    try:
        process.wait(timeout=10)
    except subprocess.TimeoutExpired:
        process.terminate()
        process.wait(timeout=10)
