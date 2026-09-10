"""Bounded pipe transport for a network-disabled task container.

The host alone calls serve with its ledger/key. The container runs endpoint,
which has only a local gateway token. Container bytes are untrusted requests;
they cannot choose a provider host, API key, budget phase or reservation policy.
The controller must own process/container termination and the overall deadline.
"""
import os
import selectors
import struct
import time

from gemini_dispatch import send
from gemini_gateway import server

REQUEST_LIMIT = 4_000_001
RESPONSE_LIMIT = 32_000_002


def transfer(fd, size, deadline, payload=None, allow_eof=False):
    os.set_blocking(fd, False)
    result = bytearray()
    offset = 0
    with selectors.DefaultSelector() as selector:
        selector.register(fd, selectors.EVENT_READ if payload is None else selectors.EVENT_WRITE)
        while offset < size:
            remaining = deadline - time.monotonic()
            if remaining <= 0 or not selector.select(remaining):
                raise TimeoutError("relay deadline exceeded")
            try:
                if payload is None:
                    chunk = os.read(fd, min(size - offset, 65_536))
                    if not chunk:
                        if allow_eof and offset == 0:
                            return None
                        raise ValueError("truncated relay frame")
                    result.extend(chunk)
                    offset += len(chunk)
                else:
                    offset += os.write(fd, memoryview(payload)[offset:offset + 65_536])
            except BlockingIOError:
                continue
    return bytes(result) if payload is None else None


def read_frame(fd, maximum, deadline):
    prefix = transfer(fd, 4, deadline, allow_eof=True)
    if prefix is None:
        return None
    size, = struct.unpack("!I", prefix)
    if not 0 < size <= maximum:
        raise ValueError("invalid relay frame length")
    return transfer(fd, size, deadline)


def write_frame(fd, payload, maximum, deadline):
    if not isinstance(payload, bytes) or not 0 < len(payload) <= maximum:
        raise ValueError("invalid relay frame length")
    transfer(fd, 4, deadline, struct.pack("!I", len(payload)))
    transfer(fd, len(payload), deadline, payload)


def serve(incoming, outgoing, budget, phase, api_key, deadline):
    """Host side. Dispatcher validates every request and reserves before HTTPS."""
    while (frame := read_frame(incoming, REQUEST_LIMIT, deadline)) is not None:
        if len(frame) < 2 or frame[0] not in (0, 1):
            raise ValueError("invalid relay request")
        try:
            status, _, body = send(budget, phase, frame[1:], api_key, stream=bool(frame[0]))
        except Exception:
            status, body = 502, b'{"error":"dispatch refused or failed"}'
        write_frame(outgoing, struct.pack("!H", status) + body, RESPONSE_LIMIT, deadline)


def endpoint(incoming, outgoing, port, token, deadline):
    """Container side. Serialize requests on the single framed pipe channel."""
    import threading
    lock = threading.Lock()

    def dispatch(_budget, _phase, body, _key, stream=False):
        with lock:
            write_frame(outgoing, bytes([int(stream)]) + body, REQUEST_LIMIT, deadline)
            response = read_frame(incoming, RESPONSE_LIMIT, deadline)
            if response is None or len(response) < 2:
                raise ValueError("missing relay response")
            status, = struct.unpack("!H", response[:2])
            if not 100 <= status <= 599:
                raise ValueError("invalid relay response status")
            return status, "application/json", response[2:]

    return server(("127.0.0.1", port), None, "development", token, "relay-only", dispatch=dispatch)
