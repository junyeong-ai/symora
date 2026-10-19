import json
import os
from pathlib import Path
import sys
import time


def send(message):
    body = json.dumps(message).encode()
    sys.stdout.buffer.write(b"Content-Length: " + str(len(body)).encode() + b"\r\n\r\n" + body)
    sys.stdout.buffer.flush()


observed = {}
waiting = []


def answer(request):
    result = observed
    if request["method"] == "textDocument/hover":
        result = {"contents": {"kind": "plaintext", "value": observed["pull"][1]["marker"]}}
    send({"jsonrpc": "2.0", "id": request["id"], "result": result})


while True:
    length = None
    while True:
        header = sys.stdin.buffer.readline()
        if not header:
            sys.exit()
        if header == b"\r\n":
            break
        if header.lower().startswith(b"content-length:"):
            length = int(header.split(b":", 1)[1])
    message = json.loads(sys.stdin.buffer.read(length))
    method = message.get("method")
    if method == "initialize":
        if len(sys.argv) > 1 and sys.argv[1] != "--shutdown-barrier":
            Path(sys.argv[1]).write_text(str(os.getpid()))
            while not Path(sys.argv[2]).exists():
                time.sleep(0.01)
        observed["initialization_options"] = message["params"].get("initializationOptions")
        result = {"capabilities": {"hoverProvider": True}}
    elif method == "initialized":
        send({"jsonrpc": "2.0", "id": "configuration", "method": "workspace/configuration",
              "params": {"items": [{}, {"section": "python.analysis"}, {"section": "missing"}]}})
        continue
    elif method == "workspace/didChangeConfiguration":
        observed["notification"] = message["params"]["settings"]
        continue
    elif method is None and message.get("id") == "configuration":
        observed["pull"] = message["result"]
        for request in waiting:
            answer(request)
        waiting.clear()
        continue
    elif method in ("test/configuration", "textDocument/hover"):
        if "pull" not in observed:
            waiting.append(message)
            continue
        answer(message)
        continue
    elif method == "exit":
        if len(sys.argv) > 1 and sys.argv[1] == "--shutdown-barrier":
            Path(sys.argv[2]).write_text(str(os.getpid()))
            while not Path(sys.argv[3]).exists():
                time.sleep(0.01)
        sys.exit()
    elif "id" not in message or method is None:
        continue
    else:
        result = None
    send({"jsonrpc": "2.0", "id": message["id"], "result": result})
