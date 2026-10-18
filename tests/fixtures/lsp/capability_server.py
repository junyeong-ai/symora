import json
import sys


def send(message):
    body = json.dumps(message).encode()
    sys.stdout.buffer.write(b"Content-Length: " + str(len(body)).encode() + b"\r\n\r\n" + body)
    sys.stdout.buffer.flush()


dynamic = False
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
        dynamic = message["params"]["capabilities"]["textDocument"]["callHierarchy"].get(
            "dynamicRegistration", False
        )
        result = {"capabilities": {} if dynamic else {"callHierarchyProvider": True}}
    elif method == "initialized":
        if dynamic:
            send({
                "jsonrpc": "2.0", "id": "calls", "method": "client/registerCapability",
                "params": {"registrations": [{
                    "id": "calls", "method": "textDocument/prepareCallHierarchy",
                    "registerOptions": {"documentSelector": [{"language": "python"}]},
                }]},
            })
        continue
    elif method == "textDocument/prepareCallHierarchy":
        result = [{
            "name": "target", "kind": 12, "uri": message["params"]["textDocument"]["uri"],
            "range": {"start": {"line": 0, "character": 4}, "end": {"line": 0, "character": 10}},
            "selectionRange": {"start": {"line": 0, "character": 4}, "end": {"line": 0, "character": 10}},
            "data": {"session": ["target", 7, None], "nested": {"ready": True}},
        }]
    elif method == "callHierarchy/incomingCalls":
        item = message["params"]["item"]
        assert item["data"] == {"session": ["target", 7, None], "nested": {"ready": True}}
        result = [{"from": dict(item, name="caller"), "fromRanges": [item["range"]]}]
    elif method == "exit":
        sys.exit()
    elif "id" not in message or method is None:
        continue
    else:
        result = None
    send({"jsonrpc": "2.0", "id": message["id"], "result": result})
