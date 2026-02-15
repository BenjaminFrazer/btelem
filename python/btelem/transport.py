"""Transport adapters for btelem streams."""

from __future__ import annotations

import socket
from typing import Protocol


class Transport(Protocol):
    """Abstract transport interface."""

    def read(self, n: int) -> bytes: ...
    def write(self, data: bytes) -> None: ...
    def close(self) -> None: ...


class SerialTransport:
    """UART / serial port transport (requires pyserial)."""

    def __init__(self, port: str, baudrate: int = 115200, timeout: float = 1.0):
        import serial
        self._ser = serial.Serial(port, baudrate, timeout=timeout)

    def read(self, n: int) -> bytes:
        return self._ser.read(n)

    def write(self, data: bytes) -> None:
        self._ser.write(data)

    def close(self) -> None:
        self._ser.close()


class UDPTransport:
    """UDP datagram transport."""

    def __init__(self, host: str = "0.0.0.0", port: int = 4200,
                 remote: tuple[str, int] | None = None):
        self._sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        self._sock.bind((host, port))
        self._sock.settimeout(1.0)
        self._remote = remote

    def read(self, n: int) -> bytes:
        try:
            data, addr = self._sock.recvfrom(n)
            if self._remote is None:
                self._remote = addr
            return data
        except socket.timeout:
            return b""

    def write(self, data: bytes) -> None:
        if self._remote:
            self._sock.sendto(data, self._remote)

    def close(self) -> None:
        self._sock.close()


class TCPTransport:
    """TCP stream transport (client mode)."""

    def __init__(self, host: str, port: int, timeout: float = 5.0):
        self._sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        self._sock.settimeout(timeout)
        self._sock.connect((host, port))

    def read(self, n: int) -> bytes:
        try:
            return self._sock.recv(n)
        except socket.timeout:
            return b""

    def recv_exact(self, n: int) -> bytes:
        """Read exactly *n* bytes from the socket (blocking)."""
        buf = bytearray()
        while len(buf) < n:
            chunk = self._sock.recv(n - len(buf))
            if not chunk:
                raise ConnectionError(
                    "connection closed before receiving all data"
                )
            buf.extend(chunk)
        return bytes(buf)

    def write(self, data: bytes) -> None:
        self._sock.sendall(data)

    def close(self) -> None:
        self._sock.close()


class FileTransport:
    """Read from / write to a raw binary file (for replay or logging)."""

    def __init__(self, path: str, mode: str = "rb"):
        self._f = open(path, mode)

    def read(self, n: int) -> bytes:
        return self._f.read(n) or b""

    def write(self, data: bytes) -> None:
        self._f.write(data)

    def close(self) -> None:
        self._f.close()
