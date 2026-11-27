"""A language server for Python-shaped sources that answers what diff-impact
asks: document symbols, references and call hierarchy, read from the files
on disk under the workspace root.

A line `def name` or `class name` declares a function, a method (indented
under a class) or a class, whose range runs to the last line before the next
line indented no deeper; `NAME = …` outside a function declares a variable.
A reference is every whole-word occurrence of the name in a `.py` file under
the root. An incoming call is an occurrence followed by `(` other than a
declaration's own name, inside a function or method, which is the caller.

Every message's method, and the name of the file it asks about, is appended
to the file named by the first argument.
"""

import json
import os
import pathlib
import re
import sys
import urllib.parse

log = pathlib.Path(sys.argv[1])
root = None


def send(message):
    body = json.dumps(message).encode()
    sys.stdout.buffer.write(b"Content-Length: " + str(len(body)).encode() + b"\r\n\r\n" + body)
    sys.stdout.buffer.flush()


def path_of(uri):
    return pathlib.Path(urllib.parse.unquote(urllib.parse.urlparse(uri).path))


def span(line, start, end, end_line=None):
    return {"start": {"line": line, "character": start},
            "end": {"line": line if end_line is None else end_line, "character": end}}


def declarations(path):
    """Each declaration: (name, kind, first line, last line, name column, depth)."""
    lines = path.read_text().splitlines()
    found = []
    for number, text in enumerate(lines):
        declared = re.match(r"^(\s*)(def|class)\s+(\w+)", text)
        assigned = re.match(r"^(\s*)([A-Za-z_]\w*)\s*=", text)
        in_function = any(d[1] in (6, 12) and d[2] < number <= d[3] for d in found)
        if declared:
            indent = len(declared.group(1))
            name = declared.group(3)
            last = number
            for later in range(number + 1, len(lines)):
                if lines[later].strip() and len(lines[later]) - len(lines[later].lstrip()) <= indent:
                    break
                if lines[later].strip():
                    last = later
            if declared.group(2) == "class":
                kind = 5
            else:
                kind = 6 if any(d[1] == 5 and d[2] < number <= d[3] for d in found) else 12
            found.append((name, kind, number, last, text.index(name, declared.end(2)), indent))
        elif assigned and not in_function:
            indent = len(assigned.group(1))
            found.append((assigned.group(2), 13, number, number, indent, indent))
    return found


def document_symbols(path):
    flat = declarations(path)
    lines = path.read_text().splitlines()

    def symbol(entry):
        name, kind, first, last, column, _ = entry
        return {"name": name, "kind": kind,
                "range": span(first, 0, len(lines[last]), last),
                "selectionRange": span(first, column, column + len(name)),
                "children": []}

    top = []
    for entry in flat:
        holder = next((t for t in reversed(top)
                       if t["kind"] == 5 and t["range"]["start"]["line"] < entry[2]
                       <= t["range"]["end"]["line"]), None)
        (holder["children"] if holder else top).append(symbol(entry))
    return top


def name_at(path, position):
    text = path.read_text().splitlines()[position["line"]]
    for match in re.finditer(r"\w+", text):
        if match.start() <= position["character"] <= match.end():
            return match.group(0)
    return None


def sources():
    for directory, subdirectories, files in os.walk(root):
        subdirectories[:] = [d for d in subdirectories if not d.startswith(".")]
        for file in sorted(files):
            if file.endswith(".py"):
                yield pathlib.Path(directory) / file


def occurrences(name):
    for path in sources():
        for number, text in enumerate(path.read_text().splitlines()):
            for match in re.finditer(r"\b" + re.escape(name) + r"\b", text):
                yield path, number, match.start(), match.end(), text[match.end():]


def item(path, entry):
    name, kind, first, last, column, _ = entry
    return {"name": name, "kind": kind, "uri": path.as_uri(),
            "range": span(first, 0, 0, last + 1),
            "selectionRange": span(first, column, column + len(name))}


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
    params = message.get("params") or {}
    asked = (params.get("textDocument") or params.get("item") or {}).get("uri")
    with log.open("a") as written:
        written.write(f"{method} {path_of(asked).name if asked else ''}\n")
    result = None
    if method == "initialize":
        root = path_of(params["rootUri"])
        result = {"capabilities": {
            "textDocumentSync": 1,
            "documentSymbolProvider": True,
            "referencesProvider": True,
            "callHierarchyProvider": True,
        }}
    elif method == "initialized":
        send({"jsonrpc": "2.0", "method": "experimental/serverStatus",
              "params": {"health": "ok", "quiescent": True}})
        continue
    elif method == "textDocument/documentSymbol":
        result = document_symbols(path_of(params["textDocument"]["uri"]))
    elif method == "textDocument/references":
        name = name_at(path_of(params["textDocument"]["uri"]), params["position"])
        result = [{"uri": path.as_uri(), "range": span(line, start, end)}
                  for path, line, start, end, _ in occurrences(name)] if name else []
    elif method == "textDocument/prepareCallHierarchy":
        path = path_of(params["textDocument"]["uri"])
        name = name_at(path, params["position"])
        result = [item(path, entry) for entry in declarations(path) if entry[0] == name][:1]
    elif method == "callHierarchy/incomingCalls":
        calls = {}
        for path, line, start, end, after in occurrences(params["item"]["name"]):
            before = path.read_text().splitlines()[line][:start]
            if not after.lstrip().startswith("(") or re.search(r"\b(def|class)\s+$", before):
                continue
            callers = [entry for entry in declarations(path)
                       if entry[1] in (6, 12) and entry[2] <= line <= entry[3]]
            if callers:
                caller = max(callers, key=lambda entry: entry[2])
                key = (str(path), caller[2])
                calls.setdefault(key, {"from": item(path, caller), "fromRanges": []})
                calls[key]["fromRanges"].append(span(line, start, end))
        result = list(calls.values())
    elif method == "exit":
        sys.exit()
    elif "id" not in message or method is None:
        continue
    send({"jsonrpc": "2.0", "id": message["id"], "result": result})
