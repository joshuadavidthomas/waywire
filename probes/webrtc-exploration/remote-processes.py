"""Inspect only descendants of a caller-owned root process. No command lines."""
import json
import os
import pathlib
import sys

root = int(sys.argv[1])
seen = set()
result = []
def visit(pid, expected_parent=None):
    if pid in seen:
        return
    seen.add(pid)
    base = pathlib.Path(f"/proc/{pid}")
    try:
        fields = (base / "stat").read_text().rsplit(")", 1)[1].split()
        if expected_parent is not None and int(fields[1]) != expected_parent:
            return
        result.append({"pid": pid, "ppid": int(fields[1]), "startTicks": fields[19], "exe": os.readlink(base / "exe")})
        children = set()
        for task in (base / "task").iterdir():
            try:
                children.update(map(int, (task / "children").read_text().split()))
            except FileNotFoundError:
                pass
        for child in children:
            visit(child, pid)
    except (FileNotFoundError, ProcessLookupError):
        pass
visit(root)
print(json.dumps(result))
