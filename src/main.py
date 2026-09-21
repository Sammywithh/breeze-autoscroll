#!/usr/bin/env python3
"""
Breeze Autoscroll - Main Application Entry Point
Windows-style middle-click autoscroll for Kubuntu 26.04 (KDE Plasma 6 / Breeze).
"""

import os
import signal
import sys
from pathlib import Path

# Add src dir to sys.path
SRC_DIR = Path(__file__).resolve().parent
if str(SRC_DIR) not in sys.path:
    sys.path.insert(0, str(SRC_DIR))

# DO NOT override QT_QPA_PLATFORM - let Qt auto-detect.
# On Wayland sessions (Kubuntu 26.04 default), this correctly picks the wayland
# platform which supports QCursor.pos() and overlay window positioning natively.

from PyQt6.QtCore import Qt, QTimer, QSharedMemory
from PyQt6.QtGui import QIcon
from PyQt6.QtWidgets import QApplication, QMessageBox

from config import Config
from indicator_widget import BreezeIndicatorWidget
from input_manager import InputManager
from tray_icon import BreezeTrayIcon, create_tray_icon_pixmap


class BreezeAutoscrollApp:
    def __init__(self, sys_argv):
        self.app = QApplication(sys_argv)
        self.app.setApplicationName("Breeze Autoscroll")
        self.app.setApplicationDisplayName("Breeze Autoscroll")
        self.app.setDesktopFileName("breeze-autoscroll")
        self.app.setQuitOnLastWindowClosed(False)

        # Set default window icon
        app_icon = QIcon(create_tray_icon_pixmap(True, 128))
        self.app.setWindowIcon(app_icon)

        # Config
        self.config = Config()

        # Indicator Overlay Widget
        self.indicator = BreezeIndicatorWidget()

        # Input & Scroll Engine
        self.input_manager = InputManager(self.config)

        # System Tray Icon
        self.tray = BreezeTrayIcon(self.config)
        self.tray.show()

        # Connect Signal / Slot wiring
        self._connect_signals()

        # Handle Unix Signals (Ctrl+C, SIGTERM)
        signal.signal(signal.SIGINT, self._signal_handler)
        signal.signal(signal.SIGTERM, self._signal_handler)

        # Timer to let Python interpreter process signals
        self.sig_timer = QTimer()
        self.sig_timer.timeout.connect(lambda: None)
        self.sig_timer.start(500)

    def _connect_signals(self):
        self.input_manager.autoscroll_started.connect(self._on_autoscroll_started)
        self.input_manager.offset_changed.connect(self.indicator.update_offset)
        self.input_manager.autoscroll_stopped.connect(self.indicator.hide_indicator)
        self.input_manager.error_occurred.connect(self._on_error)
        self.tray.toggle_requested.connect(self._on_toggle_requested)

    def _on_autoscroll_started(self, x: int, y: int):
        self.indicator.show_at(
            screen_x=x,
            screen_y=y,
            deadzone=self.config.deadzone,
            theme=self.config.theme
        )

    def _on_toggle_requested(self, enabled: bool):
        if not enabled:
            self.input_manager.stop_autoscroll()

    def _on_error(self, message: str):
        print(f"[App Error] {message}")
        if self.tray.isVisible():
            self.tray.showMessage(
                "Breeze Autoscroll Hinweis",
                message,
                QIcon(create_tray_icon_pixmap(False, 64)),
                6000
            )

    def _signal_handler(self, sig, frame):
        print("\n[Breeze Autoscroll] Beende Anwendung...")
        self.input_manager.stop()
        self.app.quit()

    def run(self) -> int:
        # Check CLI arguments
        if "--settings" in sys.argv or "-s" in sys.argv:
            self.tray.show_settings()

        # Start input listener
        self.input_manager.start()

        return self.app.exec()


def main():
    # Enforce single instance via QSharedMemory
    memory_key = "breeze_autoscroll_single_instance_lock"
    
    # Initialize App
    app_runner = BreezeAutoscrollApp(sys.argv)
    
    shared_mem = QSharedMemory(memory_key)
    if not shared_mem.create(1):
        # Already running
        if "--settings" in sys.argv or "-s" in sys.argv:
            print("[Breeze Autoscroll] Eine Instanz läuft bereits. Öffne Einstellungen...")
            app_runner.tray.show_settings()
            sys.exit(app_runner.app.exec())
        else:
            print("[Breeze Autoscroll] Breeze Autoscroll läuft bereits im Hintergrund.")
            sys.exit(0)

    exit_code = app_runner.run()
    sys.exit(exit_code)


if __name__ == "__main__":
    main()
