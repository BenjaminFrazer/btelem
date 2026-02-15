"""btelem command-line tool."""

from __future__ import annotations

import argparse
import sys
import time

from .schema import Schema
from .decoder import PacketDecoder, DecodedEntry, decode_packet, read_stream_schema
from .storage import LogReader


def _format_entry(entry: DecodedEntry) -> str:
    ts_s = entry.timestamp / 1_000_000_000
    name = entry.name or f"id={entry.id}"
    fields_str = ", ".join(f"{k}={v}" for k, v in entry.fields.items())
    return f"[{ts_s:12.6f}] {name}: {fields_str}"


def cmd_dump(args: argparse.Namespace) -> None:
    """Dump a log file to stdout."""
    with LogReader(args.file) as reader:
        for entry in reader.entries():
            print(_format_entry(entry))


def cmd_schema(args: argparse.Namespace) -> None:
    """Print the schema from a log file."""
    with LogReader(args.file) as reader:
        schema = reader.schema
        for e in schema.entries.values():
            print(f"[{e.id:3d}] {e.name} - {e.description}")
            print(f"      payload_size={e.payload_size}")
            for f in e.fields:
                print(f"        {f.name:20s} offset={f.offset:3d} "
                      f"size={f.size:2d} type={f.type.name} count={f.count}")
            print()


def _format_duration(ns: int) -> str:
    """Format a nanosecond duration as a human-readable string."""
    if ns < 1_000:
        return f"{ns}ns"
    if ns < 1_000_000:
        return f"{ns / 1_000:.1f}us"
    if ns < 1_000_000_000:
        return f"{ns / 1_000_000:.1f}ms"
    s = ns / 1_000_000_000
    if s < 60:
        return f"{s:.2f}s"
    if s < 3600:
        return f"{s / 60:.1f}m"
    return f"{s / 3600:.1f}h"


def _format_timestamp(ns: int) -> str:
    """Format a nanosecond timestamp as seconds.microseconds."""
    return f"{ns / 1_000_000_000:.6f}"


def cmd_info(args: argparse.Namespace) -> None:
    """Print summary info about a .btlm log file."""
    import os

    file_size = os.path.getsize(args.file)

    with LogReader(args.file) as reader:
        schema = reader.schema
        index = reader.index

        # Collect per-signal counts by scanning all entries
        signal_counts: dict[int, int] = {}
        ts_min: int | None = None
        ts_max: int | None = None
        total_entries = 0

        for entry in reader.entries():
            signal_counts[entry.id] = signal_counts.get(entry.id, 0) + 1
            total_entries += 1
            if ts_min is None or entry.timestamp < ts_min:
                ts_min = entry.timestamp
            if ts_max is None or entry.timestamp > ts_max:
                ts_max = entry.timestamp

        num_packets = len(index) if index else "unknown"

        # Header
        print(f"File:       {args.file}")
        print(f"Size:       {file_size:,} bytes")
        print(f"Packets:    {num_packets}")
        print(f"Entries:    {total_entries:,}")

        if ts_min is not None and ts_max is not None:
            duration = ts_max - ts_min
            print(f"Time range: {_format_timestamp(ts_min)}s — {_format_timestamp(ts_max)}s")
            print(f"Duration:   {_format_duration(duration)}")
        else:
            print(f"Time range: (empty)")

        # Per-signal table
        print(f"\nSignals ({len(schema.entries)}):")
        print(f"  {'ID':>4s}  {'Name':<24s}  {'Samples':>8s}  {'Payload':>8s}  Fields")
        print(f"  {'—' * 4}  {'—' * 24}  {'—' * 8}  {'—' * 8}  {'—' * 20}")
        for e in schema.entries.values():
            count = signal_counts.get(e.id, 0)
            field_names = ", ".join(f.name for f in e.fields)
            print(f"  {e.id:4d}  {e.name:<24s}  {count:8,}  {e.payload_size:5d}  B  {field_names}")


def cmd_live(args: argparse.Namespace) -> None:
    """Live decode from a transport."""
    # Determine transport
    if args.serial:
        from .transport import SerialTransport
        transport = SerialTransport(args.serial, baudrate=args.baud)
    elif args.udp:
        from .transport import UDPTransport
        host, port = args.udp.rsplit(":", 1)
        transport = UDPTransport(host, int(port))
    elif args.tcp:
        from .transport import TCPTransport
        host, port = args.tcp.rsplit(":", 1)
        transport = TCPTransport(host, int(port))
    else:
        print("Error: specify --serial, --udp, or --tcp", file=sys.stderr)
        sys.exit(1)

    # Load schema: from TCP stream if available, or from file
    if args.schema_file:
        with LogReader(args.schema_file) as r:
            schema = r.schema
    elif args.tcp:
        schema = read_stream_schema(transport)
    else:
        print("Error: --schema-file required for non-TCP transports",
              file=sys.stderr)
        sys.exit(1)

    decoder = PacketDecoder(schema)

    try:
        while True:
            data = transport.read(4096)
            if data:
                for entry in decoder.feed(data):
                    print(_format_entry(entry))
            else:
                time.sleep(0.01)
    except KeyboardInterrupt:
        pass
    finally:
        transport.close()


def main() -> None:
    parser = argparse.ArgumentParser(prog="btelem", description="btelem telemetry tool")
    sub = parser.add_subparsers(dest="command")

    # dump
    p_dump = sub.add_parser("dump", help="Dump a log file")
    p_dump.add_argument("file", help="Path to .btlm log file")

    # schema
    p_schema = sub.add_parser("schema", help="Show schema from a log file")
    p_schema.add_argument("file", help="Path to .btlm log file")

    # info
    p_info = sub.add_parser("info", help="Show summary info about a log file")
    p_info.add_argument("file", help="Path to .btlm log file")

    # live
    p_live = sub.add_parser("live", help="Live decode from transport")
    p_live.add_argument("--serial", help="Serial port (e.g. /dev/ttyUSB0)")
    p_live.add_argument("--baud", type=int, default=115200, help="Baud rate")
    p_live.add_argument("--udp", help="UDP host:port to listen on")
    p_live.add_argument("--tcp", help="TCP host:port to connect to")
    p_live.add_argument("--schema-file", help="Log file to read schema from")

    args = parser.parse_args()
    if args.command == "dump":
        cmd_dump(args)
    elif args.command == "schema":
        cmd_schema(args)
    elif args.command == "info":
        cmd_info(args)
    elif args.command == "live":
        cmd_live(args)
    else:
        parser.print_help()


if __name__ == "__main__":
    main()
