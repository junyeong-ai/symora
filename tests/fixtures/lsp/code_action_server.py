import json
import pathlib
import sys

source = pathlib.Path(sys.argv[1])
mutate = sys.argv[2] == "mutate"

while True:
    headers = {}
    while True:
        line = sys.stdin.buffer.readline()
        if not line:
            sys.exit()
        if line == b"\r\n":
            break
        key, value = line.decode().split(":", 1)
        headers[key.lower()] = value.strip()
    message = json.loads(sys.stdin.buffer.read(int(headers["content-length"])))
    method = message.get("method")
    result = None
    if method == "initialize":
        result = {"capabilities": {"textDocumentSync": 1, "codeActionProvider": True}}
    elif method == "textDocument/codeAction":
        if mutate:
            source.write_text("prefixabc()\n")
        result = [{"title": "Rename first token", "kind": "quickfix", "edit": {
            "changes": {source.as_uri(): [{
                "range": {"start": {"line": 0, "character": 0},
                          "end": {"line": 0, "character": 3}},
                "newText": "renamed"
            }]}
        }}]
    elif method == "exit":
        sys.exit()
    if "id" in message:
        body = json.dumps({"jsonrpc": "2.0", "id": message["id"], "result": result}).encode()
        sys.stdout.buffer.write(b"Content-Length: " + str(len(body)).encode() + b"\r\n\r\n" + body)
        sys.stdout.buffer.flush()
