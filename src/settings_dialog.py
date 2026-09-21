"""
Breeze Autoscroll - Settings Dialog
Modern KDE Plasma 6 / Kubuntu 26.04 styled configuration dialog.
"""

from PyQt6.QtCore import Qt, pyqtSignal
from PyQt6.QtGui import QIcon, QFont, QColor
from PyQt6.QtWidgets import (
    QDialog, QVBoxLayout, QHBoxLayout, QLabel, QSlider, QCheckBox,
    QRadioButton, QButtonGroup, QGroupBox, QPushButton, QComboBox,
    QFrame, QWidget, QScrollArea, QMessageBox
)

from config import Config


class BreezeSettingsDialog(QDialog):
    """
    Settings dialog matching KDE Plasma 6 Breeze visual aesthetics.
    """

    config_changed = pyqtSignal()

    def __init__(self, config: Config, parent=None):
        super().__init__(parent)
        self.config = config

        self.setWindowTitle("Breeze Autoscroll - Einstellungen")
        self.setMinimumSize(540, 620)
        self.resize(560, 660)

        self._apply_breeze_styling()
        self._init_ui()
        self._load_values()

    def _apply_breeze_styling(self):
        """Applies KDE Plasma 6 Breeze look and feel."""
        self.setStyleSheet("""
            QDialog {
                background-color: #24272a;
                color: #fcfcfc;
                font-family: 'Noto Sans', 'Segoe UI', sans-serif;
                font-size: 10pt;
            }
            QGroupBox {
                background-color: #2a2e32;
                border: 1px solid #363b40;
                border-radius: 8px;
                margin-top: 14px;
                padding-top: 14px;
                padding-left: 12px;
                padding-right: 12px;
                padding-bottom: 12px;
                font-weight: bold;
                color: #3daee9;
            }
            QGroupBox::title {
                subcontrol-origin: margin;
                subcontrol-position: top left;
                padding: 2px 8px;
                color: #3daee9;
            }
            QLabel {
                color: #eff0f1;
            }
            QLabel#headerTitle {
                font-size: 14pt;
                font-weight: bold;
                color: #ffffff;
            }
            QLabel#headerSubtitle {
                font-size: 9pt;
                color: #8c9398;
            }
            QLabel#unitLabel {
                color: #3daee9;
                font-weight: bold;
                min-width: 50px;
            }
            QSlider::groove:horizontal {
                height: 6px;
                background: #363b40;
                border-radius: 3px;
            }
            QSlider::sub-page:horizontal {
                background: #3daee9;
                border-radius: 3px;
            }
            QSlider::handle:horizontal {
                background: #eff0f1;
                border: 2px solid #3daee9;
                width: 16px;
                margin-top: -6px;
                margin-bottom: -6px;
                border-radius: 8px;
            }
            QSlider::handle:horizontal:hover {
                background: #3daee9;
                border: 2px solid #ffffff;
            }
            QCheckBox, QRadioButton {
                color: #eff0f1;
                spacing: 8px;
                padding: 3px;
            }
            QCheckBox::indicator, QRadioButton::indicator {
                width: 18px;
                height: 18px;
                border: 1px solid #5a6168;
                border-radius: 4px;
                background: #24272a;
            }
            QRadioButton::indicator {
                border-radius: 9px;
            }
            QCheckBox::indicator:checked, QRadioButton::indicator:checked {
                background: #3daee9;
                border-color: #3daee9;
            }
            QPushButton {
                background-color: #31363b;
                border: 1px solid #454c53;
                border-radius: 6px;
                color: #eff0f1;
                padding: 6px 16px;
                min-width: 80px;
                font-weight: 500;
            }
            QPushButton:hover {
                background-color: #3daee9;
                border-color: #3daee9;
                color: #ffffff;
            }
            QPushButton:pressed {
                background-color: #1d99f3;
            }
            QPushButton#primaryButton {
                background-color: #3daee9;
                border-color: #3daee9;
                color: #ffffff;
                font-weight: bold;
            }
            QPushButton#primaryButton:hover {
                background-color: #1d99f3;
            }
            QComboBox {
                background-color: #31363b;
                border: 1px solid #454c53;
                border-radius: 6px;
                color: #eff0f1;
                padding: 4px 10px;
                min-width: 140px;
            }
        """)

    def _init_ui(self):
        main_layout = QVBoxLayout(self)
        main_layout.setContentsMargins(18, 18, 18, 18)
        main_layout.setSpacing(14)

        # Header Section
        header_layout = QHBoxLayout()
        header_text_layout = QVBoxLayout()
        title_label = QLabel("Mausrad-Klick Autoscroll")
        title_label.setObjectName("headerTitle")
        sub_label = QLabel("Klassisches Windows-Mittelklick-Scrollen im Kubuntu 26.04 Breeze-Stil")
        sub_label.setObjectName("headerSubtitle")
        header_text_layout.addWidget(title_label)
        header_text_layout.addWidget(sub_label)
        header_layout.addLayout(header_text_layout)
        header_layout.addStretch()
        main_layout.addLayout(header_layout)

        # 1. Allgemein Box
        group_general = QGroupBox("Allgemein")
        vbox_general = QVBoxLayout(group_general)
        
        self.chk_enabled = QCheckBox("Autoscroll-Funktion aktivieren")
        self.chk_enabled.toggled.connect(self._on_setting_changed)
        vbox_general.addWidget(self.chk_enabled)

        self.chk_autostart = QCheckBox("Automatisch beim Anmelden starten (KDE Autostart)")
        self.chk_autostart.toggled.connect(self._on_setting_changed)
        vbox_general.addWidget(self.chk_autostart)
        main_layout.addWidget(group_general)

        # 2. Modus Box
        group_mode = QGroupBox("Mausrad-Klick Verhalten")
        vbox_mode = QVBoxLayout(group_mode)

        self.btn_group_mode = QButtonGroup(self)
        self.rb_smart = QRadioButton("Intelligenter Hybrid-Modus (Empfohlen)")
        self.rb_smart.setToolTip("Kurzer Klick schaltet Autoscroll an/aus. Gedrückt halten scrollt bis zum Loslassen.")
        self.rb_toggle = QRadioButton("Klick zum Umschalten (Toggle)")
        self.rb_toggle.setToolTip("Einmaliger Mittelklick startet Autoscroll. Nächster Klick stoppt.")
        self.rb_drag = QRadioButton("Gedrückt halten & Ziehen (Drag)")
        self.rb_drag.setToolTip("Autoscroll ist nur aktiv, solange das Mausrad gedrückt gehalten wird.")

        self.btn_group_mode.addButton(self.rb_smart, 0)
        self.btn_group_mode.addButton(self.rb_toggle, 1)
        self.btn_group_mode.addButton(self.rb_drag, 2)
        self.btn_group_mode.buttonToggled.connect(self._on_setting_changed)

        vbox_mode.addWidget(self.rb_smart)
        vbox_mode.addWidget(self.rb_toggle)
        vbox_mode.addWidget(self.rb_drag)

        # Horizontales Scrollen
        self.chk_horizontal = QCheckBox("Horizontales 2D-Scrollen erlauben (Links / Rechts)")
        self.chk_horizontal.toggled.connect(self._on_setting_changed)
        vbox_mode.addWidget(self.chk_horizontal)

        main_layout.addWidget(group_mode)

        # 3. Geschwindigkeit & Physik Box
        group_physics = QGroupBox("Geschwindigkeit & Empfindlichkeit")
        vbox_physics = QVBoxLayout(group_physics)

        # Speed Slider
        speed_row = QHBoxLayout()
        speed_label = QLabel("Scroll-Geschwindigkeit:")
        self.slider_speed = QSlider(Qt.Orientation.Horizontal)
        self.slider_speed.setRange(20, 300)  # 20% to 300%
        self.slider_speed.setValue(100)
        self.slider_speed.valueChanged.connect(self._on_speed_changed)
        self.lbl_speed_val = QLabel("100%")
        self.lbl_speed_val.setObjectName("unitLabel")
        speed_row.addWidget(speed_label)
        speed_row.addWidget(self.slider_speed)
        speed_row.addWidget(self.lbl_speed_val)
        vbox_physics.addLayout(speed_row)

        # Deadzone Slider
        deadzone_row = QHBoxLayout()
        deadzone_label = QLabel("Neutralbereich (Totzone):")
        self.slider_deadzone = QSlider(Qt.Orientation.Horizontal)
        self.slider_deadzone.setRange(4, 30)
        self.slider_deadzone.setValue(12)
        self.slider_deadzone.valueChanged.connect(self._on_deadzone_changed)
        self.lbl_deadzone_val = QLabel("12 px")
        self.lbl_deadzone_val.setObjectName("unitLabel")
        deadzone_row.addWidget(deadzone_label)
        deadzone_row.addWidget(self.slider_deadzone)
        deadzone_row.addWidget(self.lbl_deadzone_val)
        vbox_physics.addLayout(deadzone_row)

        main_layout.addWidget(group_physics)

        # 4. Erscheinungsbild Box
        group_theme = QGroupBox("Erscheinungsbild")
        vbox_theme = QVBoxLayout(group_theme)
        theme_row = QHBoxLayout()
        theme_label = QLabel("Design-Farbschema:")
        self.combo_theme = QComboBox()
        self.combo_theme.addItem("KDE Breeze Dunkel (Standard)", "dark")
        self.combo_theme.addItem("KDE Breeze Hell", "light")
        self.combo_theme.currentIndexChanged.connect(self._on_setting_changed)
        theme_row.addWidget(theme_label)
        theme_row.addWidget(self.combo_theme)
        theme_row.addStretch()
        vbox_theme.addLayout(theme_row)
        main_layout.addWidget(group_theme)

        main_layout.addStretch()

        # Action Buttons
        btn_layout = QHBoxLayout()
        self.btn_reset = QPushButton("Standardwerte")
        self.btn_reset.clicked.connect(self._on_reset_defaults)
        
        self.btn_close = QPushButton("Schließen")
        self.btn_close.setObjectName("primaryButton")
        self.btn_close.clicked.connect(self.accept)

        btn_layout.addWidget(self.btn_reset)
        btn_layout.addStretch()
        btn_layout.addWidget(self.btn_close)
        main_layout.addLayout(btn_layout)

    def _load_values(self):
        """Loads values from config object into UI widgets."""
        self.chk_enabled.setChecked(self.config.enabled)
        self.chk_autostart.setChecked(self.config.get("autostart", True))
        self.chk_horizontal.setChecked(self.config.allow_horizontal)

        mode = self.config.mode
        if mode == "drag":
            self.rb_drag.setChecked(True)
        elif mode == "toggle":
            self.rb_toggle.setChecked(True)
        else:
            self.rb_smart.setChecked(True)

        speed_pct = int(self.config.scroll_speed * 100)
        self.slider_speed.setValue(speed_pct)
        self.lbl_speed_val.setText(f"{speed_pct}%")

        deadzone = self.config.deadzone
        self.slider_deadzone.setValue(deadzone)
        self.lbl_deadzone_val.setText(f"{deadzone} px")

        theme = self.config.theme
        idx = self.combo_theme.findData(theme)
        if idx >= 0:
            self.combo_theme.setCurrentIndex(idx)

    def _on_speed_changed(self, val: int):
        self.lbl_speed_val.setText(f"{val}%")
        self.config.set("scroll_speed", val / 100.0)
        self.config_changed.emit()

    def _on_deadzone_changed(self, val: int):
        self.lbl_deadzone_val.setText(f"{val} px")
        self.config.set("deadzone", val)
        self.config_changed.emit()

    def _on_setting_changed(self):
        self.config.set("enabled", self.chk_enabled.isChecked(), auto_save=False)
        self.config.set("autostart", self.chk_autostart.isChecked(), auto_save=False)
        self.config.set("allow_horizontal", self.chk_horizontal.isChecked(), auto_save=False)

        if self.rb_drag.isChecked():
            self.config.set("mode", "drag", auto_save=False)
        elif self.rb_toggle.isChecked():
            self.config.set("mode", "toggle", auto_save=False)
        else:
            self.config.set("mode", "smart", auto_save=False)

        theme_val = self.combo_theme.currentData()
        self.config.set("theme", theme_val, auto_save=False)

        self.config.save()
        self.config_changed.emit()

    def _on_reset_defaults(self):
        reply = QMessageBox.question(
            self,
            "Standard wiederherstellen",
            "Möchtest du alle Einstellungen auf die Standardwerte zurücksetzen?",
            QMessageBox.StandardButton.Yes | QMessageBox.StandardButton.No
        )
        if reply == QMessageBox.StandardButton.Yes:
            self.config.reset_defaults()
            self._load_values()
            self.config_changed.emit()
