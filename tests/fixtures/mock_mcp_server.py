#!/usr/bin/env python3
"""Mock MCP server for E2E testing.

Reads newline-delimited JSON-RPC from stdin, writes responses to stdout.
Simulates a minimal MCP stdio server supporting initialize and tools/list.
"""

import json
import sys


def main():
    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        try:
            req = json.loads(line)
        except json.JSONDecodeError:
            continue

        method = req.get("method", "")
        req_id = req.get("id")

        if method == "initialize":
            resp = {
                "jsonrpc": "2.0",
                "id": req_id,
                "result": {
                    "protocolVersion": "2024-11-05",
                    "serverInfo": {"name": "mock-mcp", "version": "1.0.0"},
                    "capabilities": {"tools": {}},
                },
            }
        elif method == "tools/list":
            resp = {
                "jsonrpc": "2.0",
                "id": req_id,
                "result": {
                    "tools": [
                        {
                            "name": "echo",
                            "description": "Echo the input back",
                            "parameters": {
                                "type": "object",
                                "properties": {
                                    "message": {"type": "string"}
                                },
                            },
                            "annotations": {"readOnlyHint": True},
                        }
                    ]
                },
            }
        elif method == "tools/call":
            args = (req.get("params") or {}).get("arguments") or {}
            message = args.get("message", "")
            resp = {
                "jsonrpc": "2.0",
                "id": req_id,
                "result": {
                    "content": [{"type": "text", "text": f"echo:{message}"}],
                    "isError": False,
                },
            }
        else:
            # Notifications like "notifications/initialized" have no id — skip
            if req_id is None:
                continue
            resp = {"jsonrpc": "2.0", "id": req_id, "result": {}}

        print(json.dumps(resp), flush=True)


if __name__ == "__main__":
    main()
