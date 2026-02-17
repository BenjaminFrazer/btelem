"""DearPyGui application shell — dockable layout, viewport menu bar, main loop."""

from __future__ import annotations

import logging
import os
import zlib
from pathlib import Path

import numpy as np
import dearpygui.dearpygui as dpg

from .provider import Provider, BtelemFileProvider, BtelemLiveProvider
from .tree import TreeExplorer, FieldStats
from .plots import PlotPanel, TimingSubPlot
from .event_log import EventLogManager

logger = logging.getLogger(__name__)


def _imgui_hash(s: str, seed: int = 0) -> int:
    """Replicate ImGui's ImHashStr for null-terminated strings.

    Handles the ### convention: CRC32 resets at ``###`` so only the part
    from ``###`` onwards determines the ID.
    """
    data = s.encode("utf-8")
    idx = data.find(b"###")
    if idx >= 0:
        return zlib.crc32(data[idx:]) & 0xFFFFFFFF
    return zlib.crc32(data, seed) & 0xFFFFFFFF


class ViewerApp:
    """Top-level viewer application."""

    def __init__(self) -> None:
        self._provider: Provider | None = None
        self._tree: TreeExplorer | None = None
        self._plot_panel: PlotPanel | None = None
        self._event_log_mgr: EventLogManager | None = None

    # ------------------------------------------------------------------
    # Setup
    # ------------------------------------------------------------------

    def setup(self) -> None:
        logging.basicConfig(level=logging.INFO,
                            format="%(name)s: %(message)s")

        dpg.create_context()

        ini_path = self._get_ini_path()
        dpg.configure_app(docking=True, docking_space=True,
                          init_file=ini_path, auto_save_init_file=True)

        dpg.create_viewport(title="btelem viewer", width=1280, height=720)

        self._build_layout()
        self._build_file_dialog()

        # Write default docked layout if no ini exists yet.
        # Must happen after _build_layout (UUIDs assigned) and before
        # setup_dearpygui (which loads the ini).
        if not os.path.exists(ini_path):
            self._write_default_ini(ini_path)

        dpg.setup_dearpygui()
        dpg.show_viewport()

    @staticmethod
    def _get_ini_path() -> str:
        config_dir = Path.home() / ".config" / "btelem"
        config_dir.mkdir(parents=True, exist_ok=True)
        return str(config_dir / "layout.ini")

    @staticmethod
    def _write_default_ini(path: str) -> None:
        """Write a default docked layout ini for first launch.

        Computes ImGui window IDs from DearPyGui's internal labels
        (``"Label###<uuid>"``) using the same CRC32 hash that ImGui uses.
        If any ID is wrong (e.g. different ImGui version), ImGui simply
        ignores the ini and windows float — graceful degradation.
        """
        # DearPyGui internal labels: "Label###<uuid>"
        tree_uuid = dpg.get_alias_id("tree_window")
        plot_uuid = dpg.get_alias_id("plot_window")
        elog_uuid = dpg.get_alias_id("event_log_window_1")

        tree_label = f"Tree###{tree_uuid}"
        plot_label = f"Plots###{plot_uuid}"
        elog_label = f"Event Log 1###{elog_uuid}"

        tree_wid = _imgui_hash(tree_label)
        plot_wid = _imgui_hash(plot_label)
        elog_wid = _imgui_hash(elog_label)

        # Viewport dockspace: ImGui's IMGUI_VIEWPORT_DEFAULT_ID = 0x11111111
        viewport_id = 0x11111111
        host_name = f"WindowOverViewport_{viewport_id:08X}"
        host_wid = _imgui_hash(host_name)
        ds_id = _imgui_hash("DockSpaceOverViewport", host_wid)

        # Arbitrary dock node IDs (must be unique)
        n_left = 0x00000003
        n_right_split = 0x00000006
        n_center = 0x00000004
        n_right = 0x00000005

        # Layout: Tree(250) | Plots(740) | EventLog(290)  total ~1280
        ini = (
            f"[Window][{host_name}]\n"
            f"Pos=0,0\nSize=1280,720\nCollapsed=0\n\n"
            f"[Window][{tree_label}]\n"
            f"Pos=0,19\nSize=250,701\nCollapsed=0\n"
            f"DockId=0x{n_left:08X},0\n\n"
            f"[Window][{plot_label}]\n"
            f"Pos=252,19\nSize=740,701\nCollapsed=0\n"
            f"DockId=0x{n_center:08X},0\n\n"
            f"[Window][{elog_label}]\n"
            f"Pos=994,19\nSize=286,701\nCollapsed=0\n"
            f"DockId=0x{n_right:08X},0\n\n"
            f"[Docking][Data]\n"
            f"DockSpace "
            f"ID=0x{ds_id:08X} Window=0x{host_wid:08X} "
            f"Pos=0,19 Size=1280,701 Split=X\n"
            f"  DockNode  "
            f"ID=0x{n_left:08X} Parent=0x{ds_id:08X} "
            f"SizeRef=250,701 Selected=0x{tree_wid:08X}\n"
            f"  DockNode  "
            f"ID=0x{n_right_split:08X} Parent=0x{ds_id:08X} "
            f"SizeRef=1030,701 Split=X\n"
            f"    DockNode "
            f"ID=0x{n_center:08X} Parent=0x{n_right_split:08X} "
            f"SizeRef=740,701 Selected=0x{plot_wid:08X}\n"
            f"    DockNode "
            f"ID=0x{n_right:08X} Parent=0x{n_right_split:08X} "
            f"SizeRef=286,701 Selected=0x{elog_wid:08X}\n"
        )

        try:
            with open(path, "w") as f:
                f.write(ini)
            logger.info("wrote default layout ini: %s", path)
        except OSError as e:
            logger.warning("could not write default ini: %s", e)

    def _build_layout(self) -> None:
        # Viewport menu bar — stays visible regardless of docking
        with dpg.viewport_menu_bar():
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

            with dpg.menu(label="View"):
                dpg.add_menu_item(label="+ Add Event Log",
                                  callback=self._on_add_event_log)
                dpg.add_separator()
                dpg.add_menu_item(label="Reset Layout",
                                  callback=self._on_reset_layout)

            # Status text in the menu bar
            dpg.add_text("Status: No source loaded.", tag="status_bar")

        # Dockable tree window
        with dpg.window(label="Tree", tag="tree_window", no_close=True,
                        width=250, height=500, pos=[0, 30]):
            dpg.add_text("No source loaded.", tag="tree_placeholder")

        # Dockable plot window
        with dpg.window(label="Plots", tag="plot_window", no_close=True,
                        width=800, height=500, pos=[260, 30]):
            pass

        # Create tree explorer and plot panel (they accept parent tags)
        self._tree = TreeExplorer("tree_window")
        self._plot_panel = PlotPanel("plot_window", on_drop=self._on_field_drop)

        # Event log manager — default first table with stable tag for ini
        self._event_log_mgr = EventLogManager()
        self._event_log_mgr.add_table(window_tag="event_log_window_1")

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

        # Set up event log
        if self._event_log_mgr is not None:
            self._event_log_mgr.set_provider(provider)
            events = provider.recent_events()
            self._event_log_mgr.append_events(events)

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
        if self._event_log_mgr is not None:
            self._event_log_mgr.clear()
        if dpg.does_item_exist("tree_placeholder"):
            dpg.show_item("tree_placeholder")
        self._set_status("No source loaded.")

    def _on_close_source(self) -> None:
        self._close_source()

    # ------------------------------------------------------------------
    # Callbacks
    # ------------------------------------------------------------------

    def _on_field_drop(self, subplot_id: int, entry_name: str,
                       field_name: str | None) -> None:
        """Called when a field or entry is dragged onto a specific subplot.

        When *field_name* is ``None`` the entire entry was dragged.  For
        value plots all fields are added; for timing plots a single row
        is added for the entry.
        """
        assert self._plot_panel is not None
        if self._provider is None:
            return

        for sp in self._plot_panel.subplots:
            if sp.id != subplot_id:
                continue

            if isinstance(sp, TimingSubPlot) and field_name is None:
                # Entry-level drop on timing plot → single row
                channels = [ch for ch in self._provider.channels()
                            if ch.entry_name == entry_name]
                if channels:
                    sp.add_entry_row(entry_name, channels[0].field_name)
            elif field_name is None:
                # Entry-level drop on value plot → all fields
                for ch in self._provider.channels():
                    if ch.entry_name == entry_name:
                        sp.add_series(ch.entry_name, ch.field_name,
                                      enum_labels=ch.enum_labels)
            else:
                # Single field drop
                enum_labels = None
                for ch in self._provider.channels():
                    if ch.entry_name == entry_name and ch.field_name == field_name:
                        enum_labels = ch.enum_labels
                        break
                sp.add_series(entry_name, field_name, enum_labels=enum_labels)

            self._plot_panel.mark_dirty()
            self._plot_panel.update_cache()
            self._plot_panel.push_data()
            self._plot_panel.request_fit()
            break

    def _on_add_event_log(self) -> None:
        if self._event_log_mgr is not None:
            self._event_log_mgr.add_table()

    def _on_reset_layout(self) -> None:
        ini_path = self._get_ini_path()
        try:
            os.remove(ini_path)
        except FileNotFoundError:
            pass
        self._set_status("Layout reset. Restart to apply.")

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
                # Append new events to event log
                if self._event_log_mgr is not None and self._provider is not None:
                    events = self._provider.recent_events()
                    self._event_log_mgr.append_events(events)

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
