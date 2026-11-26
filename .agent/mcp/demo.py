#!/usr/bin/env python3
"""Servidor MCP mínimo (stdio). Edita TOOLS para añadir capacidades.

Protocolo: JSON-RPC 2.0 por líneas. Métodos: initialize, tools/list, tools/call.
"""
import json, sys

TOOLS = [
    {
        "name": "echo",
        "description": "Devuelve el texto recibido (ejemplo).",
        "inputSchema": {
            "type": "object",
            "properties": {"text": {"type": "string"}},
            "required": ["text"],
        },
    }
]

def call(name, args):
    if name == "echo":
        return {"content": [{"type": "text", "text": str(args.get("text", ""))}]}
    raise ValueError("herramienta desconocida: " + name)

def handle(msg):
    method = msg.get("method")
    if method == "initialize":
        return {"protocolVersion": "2024-11-05", "capabilities": {"tools": {}},
                "serverInfo": {"name": "custom", "version": "0.1"}}
    if method == "tools/list":
        return {"tools": TOOLS}
    if method == "tools/call":
        params = msg.get("params") or {}
        return call(params.get("name"), params.get("arguments") or {})
    raise ValueError("método no soportado: " + str(method))

for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    try:
        msg = json.loads(line)
    except json.JSONDecodeError:
        continue
    if "id" not in msg:
        continue
    try:
        result = handle(msg)
        reply = {"jsonrpc": "2.0", "id": msg["id"], "result": result}
    except Exception as exc:  # noqa: BLE001
        reply = {"jsonrpc": "2.0", "id": msg["id"],
                 "error": {"code": -32000, "message": str(exc)}}
    print(json.dumps(reply, ensure_ascii=False), flush=True)
