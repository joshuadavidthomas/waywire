#!/usr/bin/env python3
"""Read compositor state from outside, as an ordinary Wayland client.

Usage, with the session's WAYLAND_DISPLAY and XDG_RUNTIME_DIR:
  wl_probe.py toplevels           JSON list of {title, app_id, activated, minimized}
  wl_probe.py screencopy-destroy  capture one frame, destroy it after `ready`
                                  (the normal lifecycle), round-trip, and report
                                  whether the connection survived

It speaks the wire protocol directly so that it needs only Python.
"""

import array
import json
import os
import socket
import struct
import sys


class Disconnected(Exception):
    pass


class Client:
    def __init__(self):
        path = os.path.join(os.environ["XDG_RUNTIME_DIR"], os.environ["WAYLAND_DISPLAY"])
        self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.sock.connect(path)
        self.next_id = 2
        self.handlers = {}
        self.buffer = b""
        self.error = None
        self.handlers[1] = self.on_display

    def new_id(self, handler=None):
        object_id = self.next_id
        self.next_id += 1
        if handler:
            self.handlers[object_id] = handler
        return object_id

    def send(self, object_id, opcode, payload=b"", fds=()):
        header = struct.pack("<II", object_id, (8 + len(payload)) << 16 | opcode)
        ancillary = []
        if fds:
            ancillary = [(socket.SOL_SOCKET, socket.SCM_RIGHTS, array.array("i", fds))]
        self.sock.sendmsg([header + payload], ancillary)

    def on_display(self, opcode, payload):
        if opcode == 0:
            object_id, code = struct.unpack_from("<II", payload)
            self.error = (object_id, code, string(payload, 8)[0])

    def roundtrip(self):
        done = []
        callback = self.new_id(lambda opcode, payload: done.append(True))
        self.send(1, 0, struct.pack("<I", callback))
        while not done:
            self.dispatch()
            if self.error:
                raise Disconnected(self.error)

    def dispatch(self):
        data = self.sock.recv(65536)
        if not data:
            raise Disconnected(self.error or "connection closed")
        self.buffer += data
        while len(self.buffer) >= 8:
            object_id, word = struct.unpack_from("<II", self.buffer)
            size = word >> 16
            if len(self.buffer) < size:
                break
            payload, self.buffer = self.buffer[8:size], self.buffer[size:]
            handler = self.handlers.get(object_id)
            if handler:
                handler(word & 0xFFFF, payload)


def pack_string(text):
    data = text.encode() + b"\0"
    return struct.pack("<I", len(data)) + data + b"\0" * (-len(data) % 4)


def string(payload, offset):
    (length,) = struct.unpack_from("<I", payload, offset)
    text = payload[offset + 4 : offset + 4 + length - 1].decode()
    return text, offset + 4 + length + (-length % 4)


def globals_of(client):
    found = {}

    def on_registry(opcode, payload):
        if opcode == 0:
            (name,) = struct.unpack_from("<I", payload)
            interface, offset = string(payload, 4)
            (version,) = struct.unpack_from("<I", payload, offset)
            found.setdefault(interface, (name, version))

    registry = client.new_id(on_registry)
    client.send(1, 1, struct.pack("<I", registry))
    client.roundtrip()
    return registry, found


def bind(client, registry, found, interface, version, handler=None):
    name, available = found[interface]
    object_id = client.new_id(handler)
    client.send(
        registry,
        0,
        struct.pack("<I", name)
        + pack_string(interface)
        + struct.pack("<II", min(version, available), object_id),
    )
    return object_id


def toplevels():
    client = Client()
    registry, found = globals_of(client)
    windows = {}

    def on_manager(opcode, payload):
        if opcode == 0:
            (handle,) = struct.unpack_from("<I", payload)
            window = windows[handle] = {"title": "", "app_id": "", "states": []}
            client.handlers[handle] = lambda op, data: on_handle(window, op, data)

    def on_handle(window, opcode, payload):
        if opcode == 0:
            window["title"] = string(payload, 0)[0]
        elif opcode == 1:
            window["app_id"] = string(payload, 0)[0]
        elif opcode == 4:
            (length,) = struct.unpack_from("<I", payload)
            window["states"] = list(struct.unpack_from(f"<{length // 4}I", payload, 4))
        elif opcode == 6:
            window["closed"] = True

    bind(client, registry, found, "zwlr_foreign_toplevel_manager_v1", 3, on_manager)
    client.roundtrip()
    client.roundtrip()
    return [
        {
            "title": window["title"],
            "app_id": window["app_id"],
            "activated": 2 in window["states"],
            "minimized": 1 in window["states"],
        }
        for window in windows.values()
        if not window.get("closed")
    ]


def screencopy_destroy():
    client = Client()
    registry, found = globals_of(client)
    shm = bind(client, registry, found, "wl_shm", 1)
    output = bind(client, registry, found, "wl_output", 1)
    manager = bind(client, registry, found, "zwlr_screencopy_manager_v1", 1)
    state = {}

    def on_frame(opcode, payload):
        if opcode == 0:
            fmt, width, height, stride = struct.unpack_from("<IIII", payload)
            fd = os.memfd_create("screencopy")
            os.ftruncate(fd, stride * height)
            pool = client.new_id()
            client.send(shm, 0, struct.pack("<II", pool, stride * height), fds=[fd])
            buffer = client.new_id()
            client.send(pool, 0, struct.pack("<IiiiiI", buffer, 0, width, height, stride, fmt))
            client.send(pool, 1)
            os.close(fd)
            client.send(frame, 0, struct.pack("<I", buffer))
        elif opcode == 2:
            state["ready"] = True
        elif opcode == 3:
            state["failed"] = True

    frame = client.new_id(on_frame)
    client.send(manager, 0, struct.pack("<IiI", frame, 0, output))
    while not state:
        client.dispatch()
        if client.error:
            raise Disconnected(client.error)
    if "failed" in state:
        return {"captured": False}
    client.send(frame, 1)
    try:
        client.roundtrip()
        client.roundtrip()
    except Disconnected as error:
        return {"captured": True, "connection_survived": False, "error": str(error.args[0])}
    return {"captured": True, "connection_survived": True}


if __name__ == "__main__":
    command = {"toplevels": toplevels, "screencopy-destroy": screencopy_destroy}[sys.argv[1]]
    print(json.dumps(command()))
