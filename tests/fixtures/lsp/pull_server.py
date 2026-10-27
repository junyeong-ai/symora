import json
import sys


def send(message):
    body = json.dumps(message).encode()
    sys.stdout.buffer.write(b"Content-Length: " + str(len(body)).encode() + b"\r\n\r\n" + body)
    sys.stdout.buffer.flush()


def status(quiescent, health="ok"):
    send({"jsonrpc": "2.0", "method": "experimental/serverStatus",
          "params": {"health": health, "quiescent": quiescent}})


# Answers diagnostic pulls with an error for each line of a document's
# current text that reads `error`, and never publishes. By argument:
#   registered: registers its provider once told `initialized`, as pyright does
#   workspace: declares its provider at initialize and reports workspace
#     status, as rust-analyzer does. Its workspace loads until a document
#     reads `ready`, `degraded` or `failed`, which sets its health; while it
#     loads or has failed, a pull is answered with an empty report, and while
#     degraded, with the errors it finds. A
#     document reading `reload` has its next pull answered empty across a
#     busy spell, one reading `cancel` its next pull cancelled, and one
#     reading `library` is in no project.
mode = sys.argv[1]
analyzed = False
texts = {}
pending = {}
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
    if method == "initialize":
        capabilities: dict = {"textDocumentSync": 1}
        if mode == "workspace":
            capabilities["diagnosticProvider"] = {
                "identifier": "fake", "interFileDependencies": False, "workspaceDiagnostics": False,
            }
            capabilities["experimental"] = {"openCargoToml": True}
        send({"jsonrpc": "2.0", "id": message["id"], "result": {"capabilities": capabilities}})
    elif method == "initialized":
        if mode == "registered":
            send({"jsonrpc": "2.0", "id": "register", "method": "client/registerCapability",
                  "params": {"registrations": [{
                      "id": "pull", "method": "textDocument/diagnostic",
                      "registerOptions": {"documentSelector": None, "interFileDependencies": False,
                                          "workspaceDiagnostics": False},
                  }]}})
        else:
            status(False)
    elif method in ("textDocument/didOpen", "textDocument/didChange"):
        document = params["textDocument"]
        text = document["text"] if method == "textDocument/didOpen" else params["contentChanges"][-1]["text"]
        lines = text.splitlines()
        texts[document["uri"]] = lines
        pending[document["uri"]] = [word for word in ("reload", "cancel") if word in lines]
        for word, health in (("ready", "ok"), ("degraded", "warning"), ("failed", "error")):
            if mode == "workspace" and word in lines:
                analyzed = health != "error"
                status(True, health)
    elif method == "textDocument/diagnostic":
        uri = params["textDocument"]["uri"]
        lines = texts.get(uri, [])
        if "cancel" in pending.get(uri, []):
            pending[uri].remove("cancel")
            send({"jsonrpc": "2.0", "id": message["id"], "error": {
                "code": -32802, "message": "cancelled", "data": {"retriggerRequest": True},
            }})
            continue
        if "reload" in pending.get(uri, []):
            pending[uri].remove("reload")
            status(False)
            status(True)
            lines = []
        if mode == "workspace" and not analyzed:
            lines = []
        items = [{
            "range": {"start": {"line": line, "character": 0}, "end": {"line": line, "character": 5}},
            "severity": 1, "message": "error",
        } for line, content in enumerate(lines) if content == "error"]
        send({"jsonrpc": "2.0", "id": message["id"], "result": {"kind": "full", "items": items}})
    elif method == "experimental/openCargoToml":
        lines = texts.get(params["textDocument"]["uri"], [])
        project = None if "library" in lines else {
            "uri": "file:///Cargo.toml",
            "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 0}},
        }
        send({"jsonrpc": "2.0", "id": message["id"], "result": project})
    elif method == "exit":
        sys.exit()
    elif "id" in message and method is not None:
        send({"jsonrpc": "2.0", "id": message["id"], "result": None})
