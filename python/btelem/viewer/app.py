"""DearPyGui application shell — layout, menu bar, dialogs, main loop."""

from __future__ import annotations

import logging

import numpy as np
import dearpygui.dearpygui as dpg

from .provider import Provider, BtelemFileProvider, BtelemLiveProvider
from .tree import TreeExplorer, FieldStats
from .plots import PlotPanel

logger = logging.getLogger(__name__)


class ViewerApp:
    """Top-level viewer application."""

    def __init__(self) -> None:
        self._provider: Provider | None = None
        self._tree: TreeExplorer | None = None
        self._plot_panel: PlotPanel | None = None

    # ------------------------------------------------------------------
    # Setup
    # ------------------------------------------------------------------

    def setup(self) -> None:
        logging.basicConfig(level=logging.INFO,
                            format="%(name)s: %(message)s")

        dpg.create_context()
        dpg.create_viewport(title="btelem viewer", width=1280, height=720)

        self._build_layout()
        self._build_file_dialog()

        dpg.setup_dearpygui()
        dpg.show_viewport()

    def _build_layout(self) -> None:
        with dpg.window(tag="primary_window"):
            # Menu bar
            with dpg.menu_bar():
                with dpg.menu(label="File"):
                    dpg.add_menu_item(label="Open File...",
                                      callback=self._on_open_file)
                    dpg.add_menu_item(label="Connect Live...",
                                      callback=self._on_show_live_dialog)
                    dpg.add_separator()
                    dpg.add_menu_item(label="Close Source",
                                      callback=self._on_close_source)
                    dpg.add_separator()
                    dpg.add_menu_item(label="Quit",
                                      callback=lambda: dpg.stop_dearpygui())

            # Status bar (above panels so height=-1 doesn't push it off)
            dpg.add_text("Status: No source loaded.", tag="status_bar")

            # Main horizontal group: tree | plot
            with dpg.group(horizontal=True):
                # Left panel: tree explorer
                with dpg.child_window(width=250, height=-1, tag="tree_panel",
                                      border=True):
                    dpg.add_text("No source loaded.", tag="tree_placeholder")

                # Right panel: plot area
                with dpg.child_window(width=-1, height=-1, tag="plot_panel",
                                      border=True):
                    pass

        dpg.set_primary_window("primary_window", True)

        # Create tree explorer and plot panel
        self._tree = TreeExplorer("tree_panel")
        self._plot_panel = PlotPanel("plot_panel", on_drop=self._on_field_drop)

    def _build_file_dialog(self) -> None:
        with dpg.file_dialog(directory_selector=False, show=False,
                             callback=self._on_file_selected,
                             tag="file_dialog", width=600, height=400):
            dpg.add_file_extension(".btlm", color=(0, 255, 0, 255))
            dpg.add_file_extension(".*")

    # ------------------------------------------------------------------
    # File open
    # ------------------------------------------------------------------

    def _on_open_file(self) -> None:
        dpg.show_item("file_dialog")

    def _on_file_selected(self, sender: int, app_data: dict) -> None:
        path = app_data.get("file_path_name")
        if not path:
            return
        self._close_source()
        try:
            provider = BtelemFileProvider(path)
        except Exception as e:
            self._set_status(f"Error opening file: {e}")
            return
        self._set_provider(provider)

    # ------------------------------------------------------------------
    # Live connect dialog
    # ------------------------------------------------------------------

    def _on_show_live_dialog(self) -> None:
        if dpg.does_item_exist("live_dialog"):
            dpg.delete_item("live_dialog")

        with dpg.window(label="Connect Live", modal=True, tag="live_dialog",
                        width=420, height=250, no_resize=True):
            dpg.add_combo(["TCP", "UDP", "Serial"], label="Transport",
                          default_value="TCP", tag="live_transport")
            dpg.add_input_text(label="Address", tag="live_address",
                               default_value="localhost:4200", width=250)
            dpg.add_input_text(label="Schema File", tag="live_schema_path",
                               default_value="", width=250)
            dpg.add_spacer(height=10)
            with dpg.group(horizontal=True):
                dpg.add_button(label="Connect", callback=self._on_live_connect)
                dpg.add_button(label="Cancel",
                               callback=lambda: dpg.delete_item("live_dialog"))

    def _on_live_connect(self) -> None:
        transport_type = dpg.get_value("live_transport")
        address = dpg.get_value("live_address")
        schema_path = dpg.get_value("live_schema_path") or None

        try:
            transport = self._create_transport(transport_type, address)
        except Exception as e:
            self._set_status(f"Error creating transport: {e}")
            return

        try:
            schema, schema_bytes = self._load_schema(
                transport, transport_type, schema_path)
        except Exception as e:
            transport.close()
            self._set_status(f"Error loading schema: {e}")
            return

        self._close_source()
        provider = BtelemLiveProvider(transport, schema_bytes, schema)
        self._set_provider(provider)
        self._set_status(f"Live: {transport_type} {address}")

        if dpg.does_item_exist("live_dialog"):
            dpg.delete_item("live_dialog")

    def _create_transport(self, transport_type: str, address: str):
        from btelem.transport import TCPTransport, UDPTransport, SerialTransport

        if transport_type == "TCP":
            host, port = self._parse_host_port(address)
            return TCPTransport(host, int(port), timeout=0.01)
        elif transport_type == "UDP":
            host, port = self._parse_host_port(address)
            return UDPTransport(host, int(port))
        elif transport_type == "Serial":
            return SerialTransport(address, timeout=0.01)
        else:
            raise ValueError(f"Unknown transport type: {transport_type}")

    @staticmethod
    def _parse_host_port(address: str) -> tuple[str, str]:
        if ":" in address:
            host, port = address.rsplit(":", 1)
            return host, port
        return address, "4200"

    @staticmethod
    def _load_schema(transport, transport_type: str,
                     schema_path: str | None):
        """Return (schema, schema_bytes) from file or TCP stream."""
        from btelem.schema import Schema

        if schema_path:
            with open(schema_path, "rb") as f:
                schema_bytes = f.read()
            if schema_bytes[:4] == b"BTLM":
                from btelem.storage import LogReader
                reader = LogReader(schema_path)
                schema = reader.open()
                reader.close()
                schema_bytes = schema.to_bytes()
            else:
                schema = Schema.from_bytes(schema_bytes)
            return schema, schema_bytes

        if transport_type == "TCP":
            from btelem.decoder import read_stream_schema
            schema = read_stream_schema(transport)
            return schema, schema.to_bytes()

        raise ValueError(
            "Schema file is required for non-TCP transports"
        )

    # ------------------------------------------------------------------
    # Provider management
    # ------------------------------------------------------------------

    def _set_provider(self, provider: Provider) -> None:
        self._provider = provider
        assert self._tree is not None
        assert self._plot_panel is not None

        # Hide placeholder
        if dpg.does_item_exist("tree_placeholder"):
            dpg.hide_item("tree_placeholder")

        self._plot_panel.set_provider(provider)

        # Get channel stats and build tree
        channels = provider.channels()
        logger.info("loaded %d channels", len(channels))
        stats = self._compute_stats(provider)
        for (en, fn), s in stats.items():
            logger.info("  %s.%s: %d samples", en, fn, s.count)
        self._tree.build(channels, stats)

        # Update status
        tr = provider.time_range()
        if tr:
            dur = (tr[1] - tr[0]) / 1e9
            self._set_status(f"Loaded  |  {len(channels)} channels  |  {dur:.1f}s")
        else:
            self._set_status(f"Loaded  |  {len(channels)} channels  |  (no data)")

    def _close_source(self) -> None:
        if self._provider is not None:
            self._provider.close()
            self._provider = None
        if self._plot_panel is not None:
            self._plot_panel.clear()
        if self._tree is not None:
            self._tree.clear()
        if dpg.does_item_exist("tree_placeholder"):
            dpg.show_item("tree_placeholder")
        self._set_status("No source loaded.")

    def _on_close_source(self) -> None:
        self._close_source()

    # ------------------------------------------------------------------
    # Callbacks
    # ------------------------------------------------------------------

    def _on_field_drop(self, subplot_id: int, entry_name: str,
                       field_name: str) -> None:
        """Called when a field is dragged onto a specific subplot."""
        assert self._plot_panel is not None
        # Look up enum_labels from the provider's channel info
        enum_labels = None
        if self._provider is not None:
            for ch in self._provider.channels():
                if ch.entry_name == entry_name and ch.field_name == field_name:
                    enum_labels = ch.enum_labels
                    break
        for sp in self._plot_panel.subplots:
            if sp.id == subplot_id:
                sp.add_series(entry_name, field_name, enum_labels=enum_labels)
                self._plot_panel.mark_dirty()
                self._plot_panel.update_cache()
                self._plot_panel.push_data()
                self._plot_panel.request_fit()
                break

    # ------------------------------------------------------------------
    # Stats
    # ------------------------------------------------------------------

    @staticmethod
    def _compute_stats(provider: Provider) -> dict[tuple[str, str], FieldStats]:
        stats: dict[tuple[str, str], FieldStats] = {}
        for ch in provider.channels():
            try:
                data = provider.query(ch.entry_name, ch.field_name)
                n = len(data.timestamps)
                if n > 0:
                    vals = data.values.astype(np.float64)
                    stats[(ch.entry_name, ch.field_name)] = FieldStats(
                        n, float(np.min(vals)), float(np.max(vals)))
                else:
                    stats[(ch.entry_name, ch.field_name)] = FieldStats(0, None, None)
            except Exception:
                stats[(ch.entry_name, ch.field_name)] = FieldStats(0, None, None)
        return stats

    # ------------------------------------------------------------------
    # Status bar
    # ------------------------------------------------------------------

    def _set_status(self, text: str) -> None:
        if dpg.does_item_exist("status_bar"):
            dpg.set_value("status_bar", f"Status: {text}")

    # ------------------------------------------------------------------
    # Main loop
    # ------------------------------------------------------------------

    def run(self) -> None:
        while dpg.is_dearpygui_running():
            # 1. Poll provider for new data (live mode)
            new_data = False
            if self._provider is not None:
                new_data = self._provider.poll()

            # 2. If new data, re-query active series and push
            if new_data and self._plot_panel is not None:
                self._plot_panel.mark_dirty()
                self._plot_panel.update_cache()
                self._plot_panel.push_data()
                # Update stats in tree
                if self._tree is not None and self._provider is not None:
                    self._tree.update_stats(self._compute_stats(self._provider))

            # 3. Per-frame tick (deferred fit, live scrolling)
            if self._plot_panel is not None:
                self._plot_panel.tick()

            dpg.render_dearpygui_frame()

        self._cleanup()

    def _cleanup(self) -> None:
        if self._provider is not None:
            self._provider.close()
            self._provider = None
        dpg.destroy_context()

    # ------------------------------------------------------------------
    # Quick-open for CLI usage
    # ------------------------------------------------------------------

    def open_file(self, path: str) -> None:
        """Open a file immediately (called from CLI args)."""
        try:
            provider = BtelemFileProvider(path)
        except Exception as e:
            self._set_status(f"Error opening file: {e}")
            return
        self._set_provider(provider)

    def open_live(self, transport_str: str,
                  schema_path: str | None = None) -> None:
        """Connect live immediately (called from CLI args).

        transport_str: "tcp:host:port", "udp:host:port", or "serial:/dev/ttyX"
        """
        parts = transport_str.split(":", 1)
        transport_type = parts[0].upper()
        address = parts[1] if len(parts) > 1 else ""

        try:
            transport = self._create_transport(transport_type, address)
        except Exception as e:
            self._set_status(f"Error creating transport: {e}")
            return

        try:
            schema, schema_bytes = self._load_schema(
                transport, transport_type, schema_path)
        except Exception as e:
            transport.close()
            self._set_status(f"Error loading schema: {e}")
            return

        provider = BtelemLiveProvider(transport, schema_bytes, schema)
        self._set_provider(provider)
