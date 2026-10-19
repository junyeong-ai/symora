import json
import sys


def send(message):
    body = json.dumps(message).encode()
    sys.stdout.buffer.write(b"Content-Length: " + str(len(body)).encode() + b"\r\n\r\n" + body)
    sys.stdout.buffer.flush()


observed = {}
waiting = []
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
        observed["initialization_options"] = message["params"].get("initializationOptions")
        result = {"capabilities": {}}
    elif method == "initialized":
        send({"jsonrpc": "2.0", "id": "configuration", "method": "workspace/configuration",
              "params": {"items": [{}, {"section": "python.analysis"}, {"section": "missing"}]}})
        continue
    elif method == "workspace/didChangeConfiguration":
        observed["notification"] = message["params"]["settings"]
        continue
    elif method is None and message.get("id") == "configuration":
        observed["pull"] = message["result"]
        for request_id in waiting:
            send({"jsonrpc": "2.0", "id": request_id, "result": observed})
        waiting.clear()
        continue
    elif method == "test/configuration":
        if "pull" not in observed:
            waiting.append(message["id"])
            continue
        result = observed
    elif method == "exit":
        sys.exit()
    elif "id" not in message or method is None:
        continue
    else:
        result = None
    send({"jsonrpc": "2.0", "id": message["id"], "result": result})
