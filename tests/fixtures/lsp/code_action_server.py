import json
import pathlib
import sys

source = pathlib.Path(sys.argv[1])
mode = sys.argv[2]
concurrent = mode.startswith("concurrent-")
pending = None
requested = False


def send_message(message):
    body = json.dumps(message).encode()
    sys.stdout.buffer.write(b"Content-Length: " + str(len(body)).encode() + b"\r\n\r\n" + body)
    sys.stdout.buffer.flush()


def send(request_id, result=None, error=None):
    response = {"jsonrpc": "2.0", "id": request_id}
    response["error" if error else "result"] = error if error else result
    send_message(response)


def server_status(health="ok", quiescent=True):
    send_message({"jsonrpc": "2.0", "method": "experimental/serverStatus",
                  "params": {"health": health, "quiescent": quiescent}})


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
            "renameProvider": {"prepareProvider": True},
            "workspaceSymbolProvider": mode in ("ranked-symbols", "computed-members"),
            "documentSymbolProvider": mode == "computed-members"
        }}
    elif method == "initialized":
        if mode in ("unrelated-hover", "busy-rename", "ranked-symbols", "computed-members"):
            server_status()
        elif mode in ("warning-rename", "error-rename"):
            server_status(health=mode.split("-")[0])
    elif method == "textDocument/prepareRename" and mode != "unrelated-hover":
        result = {"placeholder": "abc", "range": {
            "start": {"line": 1, "character": 5},
            "end": {"line": 1, "character": 8}
        }}
    elif method == "textDocument/documentSymbol" and mode == "computed-members":
        # A class holding a computed member, which no workspace search lists.
        def span(start, end):
            return {"start": {"line": start[0], "character": start[1]},
                    "end": {"line": end[0], "character": end[1]}}
        result = [{
            "name": "C", "kind": 5, "range": span((1, 0), (5, 1)),
            "selectionRange": span((1, 13), (1, 14)),
            "children": [{
                "name": "[key]", "kind": 6, "range": span((2, 2), (4, 3)),
                "selectionRange": span((2, 2), (2, 7))
            }]
        }]
    elif method == "textDocument/documentSymbol":
        result = []
    elif method == "workspace/symbol" and mode == "computed-members":
        result = [{"name": "C", "kind": 5, "location": {"uri": source.as_uri(), "range": {
            "start": {"line": 1, "character": 13}, "end": {"line": 1, "character": 14}
        }}}]
    elif method == "workspace/symbol" and mode == "ranked-symbols":
        # The test file's declaration first, as a server may list it.
        result = [{
            "name": "Go", "kind": 6, "containerName": "Svc",
            "location": {"uri": path.as_uri(), "range": {
                "start": {"line": 1, "character": 13},
                "end": {"line": 1, "character": 15}
            }}
        } for path in (source.with_name("svc_test.go"), source)]
    elif method == "textDocument/hover" and mode == "unrelated-hover":
        result = {"contents": "Renamed helper documentation."}
    elif method == "textDocument/rename" and mode in (
        "unrelated-hover", "unconfirmed-rename", "warning-rename", "error-rename",
        "busy-rename", "late-ready-rename"
    ):
        if mode == "busy-rename":
            server_status(quiescent=False)
            server_status()
        elif mode == "late-ready-rename":
            server_status()
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
            edit = workspace_edit()
            edit["changes"][source.as_uri()][0]["range"]["end"]["character"] = 14
            send(message["id"], edit if method == "textDocument/rename" else {
                "title": "Rename first token", "edit": edit
            })
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
