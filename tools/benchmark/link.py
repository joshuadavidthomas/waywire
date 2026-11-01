"""A TCP proxy that carries a viewer's traffic over one slow link.

Every connection through the proxy shares one token bucket in the server-to-viewer
direction. The proxy reads from the server only as fast as the link drains, through
small socket buffers, so the server feels backpressure as it would behind a real slow
link. Throttling in the browser cannot do that: Chromium keeps reading the socket
and only delays delivery, so the server never sees the link is slow.
"""

import asyncio
import socket
import threading
import time

CHUNK = 4096
BUFFER = 32 << 10


class Bucket:
    """The link's capacity, `kbps` None when it is not limited."""

    def __init__(self):
        self.kbps = None
        self.tokens = 0.0
        self.updated = time.monotonic()
        self.lock = asyncio.Lock()

    async def take(self, size):
        async with self.lock:
            while self.kbps is not None:
                now = time.monotonic()
                rate = self.kbps * 125
                # Allow a 50 ms burst, and at least one read, so a slow link still moves.
                self.tokens = min(
                    max(rate * 0.05, size), self.tokens + (now - self.updated) * rate
                )
                self.updated = now
                if self.tokens >= size:
                    self.tokens -= size
                    return
                await asyncio.sleep((size - self.tokens) / rate)


async def pump(reader, writer, bucket):
    try:
        while data := await reader.read(CHUNK):
            if bucket:
                await bucket.take(len(data))
            writer.write(data)
            await writer.drain()
    except ConnectionError:
        pass
    finally:
        writer.close()


class Link:
    """Listen on `port` and forward each connection to `target`."""

    def __init__(self, port, target):
        self.target = target
        self.loop = asyncio.new_event_loop()
        self.bucket = None
        self.server = None
        self.error = None
        started = threading.Event()
        self.thread = threading.Thread(
            target=self._run, args=(port, started), daemon=True
        )
        self.thread.start()
        if not started.wait(10) or self.error:
            raise RuntimeError(f"slow link proxy did not start: {self.error}")

    def _run(self, port, started):
        asyncio.set_event_loop(self.loop)
        self.bucket = Bucket()
        try:
            self.server = self.loop.run_until_complete(
                asyncio.start_server(self._connection, "127.0.0.1", port)
            )
        except OSError as error:
            self.error = error
            started.set()
            return
        started.set()
        try:
            self.loop.run_until_complete(self.server.serve_forever())
        except asyncio.CancelledError:
            pass
        connections = asyncio.all_tasks(self.loop)
        for task in connections:
            task.cancel()
        self.loop.run_until_complete(
            asyncio.gather(*connections, return_exceptions=True)
        )
        self.loop.close()

    async def _connection(self, viewer_reader, viewer_writer):
        upstream = socket.create_connection(self.target)
        upstream.setsockopt(socket.SOL_SOCKET, socket.SO_RCVBUF, BUFFER)
        viewer_writer.get_extra_info("socket").setsockopt(
            socket.SOL_SOCKET, socket.SO_SNDBUF, BUFFER
        )
        upstream.setblocking(False)
        reader, writer = await asyncio.open_connection(sock=upstream)
        await asyncio.gather(
            pump(viewer_reader, writer, None),
            pump(reader, viewer_writer, self.bucket),
        )

    def limit(self, kbps):
        """Limit the link to `kbps`, or remove the limit with None."""

        def apply():
            self.bucket.kbps = kbps
            self.bucket.tokens = 0.0
            self.bucket.updated = time.monotonic()

        self.loop.call_soon_threadsafe(apply)

    def close(self):
        """Stop accepting connections and release the port."""
        self.loop.call_soon_threadsafe(self.server.close)
        self.thread.join(10)
