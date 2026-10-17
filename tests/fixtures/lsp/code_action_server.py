import json
import pathlib
import sys

source = pathlib.Path(sys.argv[1])
mode = sys.argv[2]
concurrent = mode.startswith("concurrent-")
pending = None
requested = False


def send(request_id, result=None, error=None):
    response = {"jsonrpc": "2.0", "id": request_id}
    response["error" if error else "result"] = error if error else result
    body = json.dumps(response).encode()
    sys.stdout.buffer.write(b"Content-Length: " + str(len(body)).encode() + b"\r\n\r\n" + body)
    sys.stdout.buffer.flush()


def workspace_edit():
    line, start, end = (1, 5, 8) if concurrent else (0, 0, 3)
    return {"changes": {source.as_uri(): [{
        "range": {"start": {"line": line, "character": start},
                  "end": {"line": line, "character": end}},
        "newText": "renamed"
    }]}}

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
        result = {"capabilities": {
            "textDocumentSync": 1,
            "codeActionProvider": {"resolveProvider": True},
            "hoverProvider": True,
            "renameProvider": {"prepareProvider": True}
        }}
    elif method == "textDocument/prepareRename" and mode != "unrelated-hover":
        result = {"placeholder": "abc", "range": {
            "start": {"line": 1, "character": 5},
            "end": {"line": 1, "character": 8}
        }}
    elif method == "textDocument/documentSymbol":
        result = []
    elif method == "textDocument/hover" and mode == "unrelated-hover":
        result = {"contents": "Renamed helper documentation."}
    elif method == "textDocument/rename" and mode == "unrelated-hover":
        result = {"changes": {source.as_uri(): [{
            "range": {"start": {"line": line, "character": column},
                      "end": {"line": line, "character": column + 3}},
            "newText": message["params"]["newName"]
        } for line, column in [(1, 5), (2, 16)]]}}
    elif method == "textDocument/codeAction":
        if mode == "mutate":
            source.write_text("prefixabc()\n")
        action = {"title": "Rename first token", "kind": "quickfix"}
        if mode == "concurrent-resolve":
            action["data"] = {"token": "abc"}
        else:
            action["edit"] = workspace_edit()
        result = [action]
    elif concurrent and method in ("textDocument/rename", "codeAction/resolve"):
        if requested:
            send(message["id"], error={"code": -32801, "message": "Request source changed"})
            continue
        requested = True
        pending = (message["id"], method)
        source.write_text("package main\nfunc prefixabc() {}\n")
        (source.parent / "ready").write_text("ready")
        continue
    elif method == "textDocument/didChange" and pending:
        request_id, operation = pending
        pending = None
        (source.parent / "changed").write_text("changed")
        send(request_id, workspace_edit() if operation == "textDocument/rename" else {
            "title": "Rename first token", "edit": workspace_edit()
        })
    elif method == "exit":
        sys.exit()
    if "id" in message:
        send(message["id"], result)
