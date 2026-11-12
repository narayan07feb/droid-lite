#!/usr/bin/env python3
"""A tiny fake Kotlin language server for Droid Lite's end-to-end tests.

It speaks LSP over stdio and answers initialize, completion, definition and rename, and it
publishes one diagnostic when a document opens. Every answer is fixed or derived from the
text the editor sent, so the tests can assert exact results."""
import json
import re
import sys

docs = {}


def read():
    length = 0
    while True:
        line = sys.stdin.buffer.readline()
        if not line:
            sys.exit(0)
        line = line.decode().strip()
        if not line:
            break
        if line.lower().startswith("content-length:"):
            length = int(line.split(":")[1])
    return json.loads(sys.stdin.buffer.read(length))


def send(msg):
    body = json.dumps(msg).encode()
    sys.stdout.buffer.write(b"Content-Length: %d\r\n\r\n" % len(body) + body)
    sys.stdout.buffer.flush()


def word_at(text, line, col):
    row = text.split("\n")[line]
    for m in re.finditer(r"\w+", row):
        if m.start() <= col <= m.end():
            return m.group(0)
    return ""


while True:
    msg = read()
    method, mid, params = msg.get("method"), msg.get("id"), msg.get("params", {})
    if method == "initialize":
        send({"jsonrpc": "2.0", "id": mid, "result": {"capabilities": {}}})
    elif method in ("textDocument/didOpen", "textDocument/didChange"):
        doc = params["textDocument"]
        docs[doc["uri"]] = doc["text"] if method.endswith("didOpen") else params["contentChanges"][0]["text"]
        if method.endswith("didOpen"):
            send({"jsonrpc": "2.0", "method": "textDocument/publishDiagnostics", "params": {"uri": doc["uri"], "diagnostics": [
                {"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 7}}, "severity": 1, "message": "fake error from lsp"}]}})
    elif method == "textDocument/completion":
        send({"jsonrpc": "2.0", "id": mid, "result": {"isIncomplete": False, "items": [{"label": "lspSuggestion", "detail": "from fake lsp"}]}})
    elif method == "textDocument/definition":
        uri = params["textDocument"]["uri"]
        send({"jsonrpc": "2.0", "id": mid, "result": [{"uri": uri, "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 1}}}]})
    elif method == "textDocument/rename":
        uri, pos, new = params["textDocument"]["uri"], params["position"], params["newName"]
        text = docs.get(uri, "")
        old = word_at(text, pos["line"], pos["character"])
        edits = []
        for i, row in enumerate(text.split("\n")):
            for m in re.finditer(r"\b%s\b" % re.escape(old), row):
                edits.append({"range": {"start": {"line": i, "character": m.start()}, "end": {"line": i, "character": m.end()}}, "newText": new})
        send({"jsonrpc": "2.0", "id": mid, "result": {"changes": {uri: edits}}})
    elif mid is not None:
        send({"jsonrpc": "2.0", "id": mid, "result": None})
