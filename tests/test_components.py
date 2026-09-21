#!/usr/bin/env python3
"""
Unit tests and headless verification for Breeze Autoscroll components.
"""

import os
import sys
import tempfile
import unittest
from pathlib import Path

# Add src to python path
SRC_DIR = Path(__file__).resolve().parent.parent / "src"
sys.path.insert(0, str(SRC_DIR))

# Ensure Qt runs offscreen for testing in sandbox/headless environment
os.environ["QT_QPA_PLATFORM"] = "offscreen"

from PyQt6.QtWidgets import QApplication
from PyQt6.QtCore import QPoint
from PyQt6.QtGui import QColor

# Initialize QApplication for widget tests
app = QApplication.instance() or QApplication(sys.argv)

from config import Config, DEFAULT_CONFIG
from cursor_helper import get_global_cursor_pos
from indicator_widget import BreezeIndicatorWidget
from tray_icon import create_tray_icon_pixmap
from settings_dialog import BreezeSettingsDialog


class TestConfig(unittest.TestCase):
    def setUp(self):
        self.temp_dir = tempfile.TemporaryDirectory()
        self.config = Config(config_dir=Path(self.temp_dir.name))

    def tearDown(self):
        self.temp_dir.cleanup()

    def test_default_values(self):
        self.assertTrue(self.config.enabled)
        self.assertEqual(self.config.deadzone, 12)
        self.assertEqual(self.config.scroll_speed, 1.0)
        self.assertEqual(self.config.mode, "smart")
        self.assertTrue(self.config.allow_horizontal)
        self.assertEqual(self.config.theme, "dark")

    def test_set_and_save(self):
        self.config.set("scroll_speed", 2.5)
        self.config.set("deadzone", 20)
        self.config.set("mode", "drag")

        # Reload from disk
        reloaded = Config(config_dir=Path(self.temp_dir.name))
        self.assertEqual(reloaded.scroll_speed, 2.5)
        self.assertEqual(reloaded.deadzone, 20)
        self.assertEqual(reloaded.mode, "drag")

    def test_reset_defaults(self):
        self.config.set("scroll_speed", 3.0)
        self.config.reset_defaults()
        self.assertEqual(self.config.scroll_speed, 1.0)


class TestCursorHelper(unittest.TestCase):
    def test_cursor_pos(self):
        pos = get_global_cursor_pos()
        self.assertIsInstance(pos, tuple)
        self.assertEqual(len(pos), 2)
        self.assertIsInstance(pos[0], int)
        self.assertIsInstance(pos[1], int)


class TestIndicatorWidget(unittest.TestCase):
    def setUp(self):
        self.widget = BreezeIndicatorWidget()

    def test_compact_dimensions(self):
        self.assertEqual(self.widget.base_diameter, 32)
        self.assertEqual(self.widget.width(), 44)
        self.assertEqual(self.widget.height(), 44)
        self.assertFalse(self.widget.is_active)

    def test_show_and_update(self):
        self.widget.show_at(500, 400, deadzone=8, theme="dark")
        self.assertTrue(self.widget.is_active)
        self.assertEqual(self.widget.anchor_x, 500)
        self.assertEqual(self.widget.anchor_y, 400)

        # Update relative offsets
        self.widget.update_offset(5.0, -15.0)
        self.assertEqual(self.widget.delta_x, 5.0)
        self.assertEqual(self.widget.delta_y, -15.0)

        # Test rendering offscreen
        self.widget.repaint()


class TestTrayIconPixmap(unittest.TestCase):
    def test_pixmap_generation(self):
        pix_active = create_tray_icon_pixmap(enabled=True, size=64)
        self.assertFalse(pix_active.isNull())
        self.assertEqual(pix_active.width(), 64)
        self.assertEqual(pix_active.height(), 64)

        pix_inactive = create_tray_icon_pixmap(enabled=False, size=32)
        self.assertFalse(pix_inactive.isNull())
        self.assertEqual(pix_inactive.width(), 32)


class TestSettingsDialog(unittest.TestCase):
    def setUp(self):
        self.temp_dir = tempfile.TemporaryDirectory()
        self.config = Config(config_dir=Path(self.temp_dir.name))
        self.dialog = BreezeSettingsDialog(self.config)

    def tearDown(self):
        self.temp_dir.cleanup()

    def test_dialog_init(self):
        self.assertIsNotNone(self.dialog)
        self.assertEqual(self.dialog.windowTitle(), "Breeze Autoscroll - Einstellungen")
        self.assertTrue(self.dialog.chk_enabled.isChecked())
        self.assertEqual(self.dialog.slider_speed.value(), 100)


if __name__ == "__main__":
    unittest.main()
