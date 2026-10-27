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


def lsp_range(start, end):
    return {"start": {"line": start[0], "character": start[1]},
            "end": {"line": end[0], "character": end[1]}}


# What each outline mode's server answers for the names of its source: per
# declaration, how many of the tokens naming it a definition request at any
# of them answers (its declarations, first), and the tokens, which a
# reference request answers. clangd 21 reads a qualifier (`Widget::`) as
# its class; pyright answers each declaration of an overloaded method as
# its definition; the Dart analyzer answers a position where a name ends as
# it answers the name, so it reads the `(` after an enum's name as the enum.
NAMES = {
    "clangd-outline": [
        (1, [((0, 7), (0, 13)), ((1, 3), (1, 9)), ((2, 24), (2, 30)), ((7, 0), (7, 6)),
             ((7, 9), (7, 15)), ((8, 5), (8, 11)), ((8, 30), (8, 36)), ((9, 0), (9, 6)),
             ((10, 5), (10, 11)), ((12, 9), (12, 15))]),
        (1, [((1, 2), (1, 3)), ((7, 8), (7, 9))]),
        (1, [((2, 7), (2, 15)), ((8, 13), (8, 21)), ((12, 41), (12, 43))]),
        (1, [((3, 11), (3, 19)), ((9, 8), (9, 16)), ((12, 54), (12, 55))]),
        (1, [((4, 7), (4, 10)), ((10, 13), (10, 16)), ((12, 24), (12, 27))]),
    ],
    "pyright-outline": [
        (3, [((5, 8), (5, 9)), ((7, 8), (7, 9)), ((8, 8), (8, 9)), ((12, 4), (12, 5))]),
    ],
    "dart-outline": [
        (1, [((0, 21), (0, 24)), ((1, 19), (1, 22))]),
        (1, [((4, 5), (4, 6))]),
    ],
}


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
            "documentSymbolProvider": mode in ("computed-members", *NAMES)
        }}
    elif method == "initialized":
        if mode in ("unrelated-hover", "busy-rename", "ranked-symbols", "computed-members",
                    "dart-outline"):
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
    elif method == "textDocument/documentSymbol" and mode == "dart-outline":
        # The Dart analyzer's outline: an extension type is a namespace, and
        # neither it nor the enum lists its primary constructor or the fields
        # that constructor's parameters declare. The named constructor is
        # selected from its `.`, and kind 6 marks it as this server's answer.
        def symbol(name, kind, start, end, selection, children=()):
            def at(position):
                return {"line": position[0], "character": position[1]}
            return {"name": name, "kind": kind,
                    "range": {"start": at(start), "end": at(end)},
                    "selectionRange": {"start": at(selection[0]), "end": at(selection[1])},
                    "children": list(children)}
        result = [
            symbol("X", 3, (0, 0), (2, 1), ((0, 15), (0, 16)), [
                symbol("twice", 7, (1, 2), (1, 27), ((1, 10), (1, 15)))]),
            symbol("E", 10, (4, 0), (6, 1), ((4, 5), (4, 6)), [
                symbol("a", 22, (5, 2), (5, 6), ((5, 2), (5, 3)))]),
            symbol("N", 5, (8, 0), (8, 27), ((8, 6), (8, 7)), [
                symbol("N.named", 6, (8, 6), (8, 26), ((8, 7), (8, 13))),
                symbol("n", 8, (8, 14), (8, 25), ((8, 24), (8, 25)))]),
        ]
    elif method == "textDocument/documentSymbol" and mode == "clangd-outline":
        # clangd's outline of out-of-line members: a destructor is selected
        # at its `~` and an operator or a conversion operator at its
        # `operator` keyword alone, a member function at its last name.
        def symbol(name, kind, start, end, selection, children=()):
            return {"name": name, "kind": kind, "range": lsp_range(start, end),
                    "selectionRange": lsp_range(*selection),
                    "children": list(children)}
        result = [
            symbol("Widget", 5, (0, 0), (5, 1), ((0, 7), (0, 13)), [
                symbol("~Widget", 9, (1, 2), (1, 11), ((1, 2), (1, 3))),
                symbol("operator==", 6, (2, 2), (2, 39), ((2, 7), (2, 15))),
                symbol("operator bool", 12, (3, 2), (3, 32), ((3, 11), (3, 19))),
                symbol("set", 6, (4, 2), (4, 15), ((4, 7), (4, 10)))]),
            symbol("Widget::~Widget", 9, (7, 0), (7, 20), ((7, 8), (7, 9))),
            symbol("Widget::operator==", 6, (8, 0), (8, 62), ((8, 13), (8, 21))),
            symbol("Widget::operator bool", 12, (9, 0), (9, 46), ((9, 8), (9, 16))),
            symbol("Widget::set", 6, (10, 0), (10, 24), ((10, 13), (10, 16))),
            symbol("use", 12, (12, 0), (12, 59), ((12, 5), (12, 8))),
        ]
    elif method == "textDocument/documentSymbol" and mode == "pyright-outline":
        # pyright's outline of a method with two `@overload` stubs lists the
        # implementation alone.
        def symbol(name, kind, start, end, selection, children=()):
            return {"name": name, "kind": kind, "range": lsp_range(start, end),
                    "selectionRange": lsp_range(*selection),
                    "children": list(children)}
        result = [symbol("O", 5, (3, 0), (9, 16), ((3, 6), (3, 7)), [
            symbol("f", 6, (8, 4), (9, 16), ((8, 8), (8, 9)), [
                symbol("x", 13, (8, 16), (8, 17), ((8, 16), (8, 17)))])])]
    elif method in ("textDocument/definition", "textDocument/references") and mode in NAMES:
        at = message["params"]["position"]
        at = (at["line"], at["character"])
        ends = mode == "dart-outline"
        declared, named = next(((declared, tokens) for declared, tokens in NAMES[mode]
                                if any(start <= at < end or ends and at == end
                                       for start, end in tokens)), (0, []))
        result = [{"uri": source.as_uri(), "range": lsp_range(start, end)}
                  for start, end in named]
        if method == "textDocument/definition":
            result = result[:declared]
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
