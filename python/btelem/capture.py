"""High-level wrappers around the C extension for numpy telemetry extraction.

Capture — file-backed (mmap), uses footer index for fast time-range queries.
LiveCapture — transport-agnostic accumulator, caller feeds raw packets.
"""

from __future__ import annotations

from ._native import Capture, LiveCapture

__all__ = ["Capture", "LiveCapture"]
