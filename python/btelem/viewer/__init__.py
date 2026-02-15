"""btelem telemetry viewer — DearPyGui-based time-domain plotter."""

from __future__ import annotations

import argparse
import sys


def launch() -> None:
    """Entry point for ``btelem-viewer`` console script."""
    parser = argparse.ArgumentParser(
        prog="btelem-viewer",
        description="btelem telemetry viewer",
    )
    parser.add_argument("file", nargs="?", default=None,
                        help="Path to a .btlm capture file to open")
    parser.add_argument("--live", metavar="TRANSPORT",
                        help="Connect live (e.g. tcp:localhost:4200, "
                             "udp:0.0.0.0:4200, serial:/dev/ttyUSB0)")
    parser.add_argument("--schema-file", metavar="PATH",
                        help="Schema source for live mode (.btlm file or raw schema blob)")
    args = parser.parse_args()

    if args.file and args.live:
        parser.error("Cannot specify both a file and --live")

    try:
        import dearpygui.dearpygui  # noqa: F401
    except ImportError:
        print("Error: dearpygui is required for the viewer.\n"
              "Install with: pip install 'btelem[viewer]'",
              file=sys.stderr)
        sys.exit(1)

    from .app import ViewerApp

    app = ViewerApp()
    app.setup()

    if args.file:
        app.open_file(args.file)
    elif args.live:
        app.open_live(args.live, args.schema_file)

    app.run()
