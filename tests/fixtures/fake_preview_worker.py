"""A stand-in for preview-worker/PreviewWorker.java in the end-to-end tests: same arguments (socket,
work dir), same one-JSON-line protocol, no JVM. Each preview gets a copy of FAKE_PREVIEW_PNG; a class
name containing "Broken" answers with a compile error instead."""
import json
import os
import shutil
import socket
import sys

sock_path, work = sys.argv[1], sys.argv[2]
os.makedirs(work, exist_ok=True)
if os.path.exists(sock_path):
    os.remove(sock_path)
server = socket.socket(socket.AF_UNIX)
server.bind(sock_path)
server.listen(1)
while True:
    conn, _ = server.accept()
    f = conn.makefile("rw")
    for line in f:
        r = json.loads(line)
        if "Broken" in r["class"]:
            reply = {"id": r["id"], "ok": False, "error": r["file"] + ":3:5: error: unresolved reference 'Oops'."}
        else:
            images = []
            for p in r["previews"]:
                png = os.path.join(work, f"{r['id']}-{p}.png")
                shutil.copy(os.environ["FAKE_PREVIEW_PNG"], png)
                images.append({"name": p, "png": png, "ms": 1})
            reply = {"id": r["id"], "ok": True, "images": images, "compile_ms": 1}
        f.write(json.dumps(reply) + "\n")
        f.flush()
