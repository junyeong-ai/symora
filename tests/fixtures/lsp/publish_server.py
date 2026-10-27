import json
import sys


def send(message):
    body = json.dumps(message).encode()
    sys.stdout.buffer.write(b"Content-Length: " + str(len(body)).encode() + b"\r\n\r\n" + body)
    sys.stdout.buffer.flush()


# Publishes an empty list for a document whenever it opens or changes,
# stamped with the version it was sent at.
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
    if method in ("textDocument/didOpen", "textDocument/didChange"):
        document = message["params"]["textDocument"]
        send({"jsonrpc": "2.0", "method": "textDocument/publishDiagnostics", "params": {
            "uri": document["uri"], "version": document["version"], "diagnostics": [],
        }})
        continue
    if method == "exit":
        sys.exit()
    if "id" not in message or method is None:
        continue
    result = {"capabilities": {"textDocumentSync": 1}} if method == "initialize" else None
    send({"jsonrpc": "2.0", "id": message["id"], "result": result})
