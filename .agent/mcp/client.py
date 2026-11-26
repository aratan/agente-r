#!/usr/bin/env python3
"""Cliente MCP mínimo: initialize + una llamada, y termina."""
import json, subprocess, sys

server, method = sys.argv[1], sys.argv[2]
params = json.loads(sys.argv[3]) if len(sys.argv) > 3 else {}

proc = subprocess.Popen([sys.executable, server], stdin=subprocess.PIPE,
                        stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)

def send(obj):
    proc.stdin.write(json.dumps(obj) + "\n")
    proc.stdin.flush()

def read():
    while True:
        line = proc.stdout.readline()
        if not line:
            err = proc.stderr.read()[:500]
            raise SystemExit("servidor MCP cerró la conexión: " + err)
        line = line.strip()
        if not line:
            continue
        try:
            return json.loads(line)
        except json.JSONDecodeError:
            continue

try:
    send({"jsonrpc": "2.0", "id": 1, "method": "initialize",
          "params": {"protocolVersion": "2024-11-05", "capabilities": {},
                     "clientInfo": {"name": "local-agent", "version": "0.2"}}})
    read()
    send({"jsonrpc": "2.0", "method": "notifications/initialized", "params": {}})
    send({"jsonrpc": "2.0", "id": 2, "method": method, "params": params})
    while True:
        msg = read()
        if msg.get("id") == 2:
            print(json.dumps(msg, ensure_ascii=False))
            break
finally:
    proc.terminate()
