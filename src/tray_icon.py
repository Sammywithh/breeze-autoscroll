"""
Breeze Autoscroll - System Tray Applet
Provides KDE Plasma panel integration, context menu, and quick toggles.
"""

from PyQt6.QtCore import Qt, pyqtSignal
from PyQt6.QtGui import QIcon, QPixmap, QPainter, QColor, QPen, QBrush, QPolygonF
from PyQt6.QtCore import QPointF
from PyQt6.QtWidgets import QSystemTrayIcon, QMenu, QMessageBox

from config import Config
from settings_dialog import BreezeSettingsDialog


def create_tray_icon_pixmap(enabled: bool = True, size: int = 64) -> QPixmap:
    """Dynamically generates a crisp KDE Breeze styled tray icon."""
    pix = QPixmap(size, size)
    pix.fill(Qt.GlobalColor.transparent)

    painter = QPainter(pix)
    painter.setRenderHint(QPainter.RenderHint.Antialiasing, True)

    center = size / 2.0
    r = size * 0.42

    # Background circle
    if enabled:
        bg_color = QColor(35, 38, 41, 230)
        border_color = QColor(61, 174, 233, 220)
        accent_color = QColor(61, 174, 233, 255)
    else:
        bg_color = QColor(35, 38, 41, 160)
        border_color = QColor(140, 145, 150, 140)
        accent_color = QColor(140, 145, 150, 180)

    painter.setPen(QPen(border_color, 2.5))
    painter.setBrush(QBrush(bg_color))
    painter.drawEllipse(QPointF(center, center), r, r)

    # Center dot
    painter.setPen(Qt.PenStyle.NoPen)
    painter.setBrush(QBrush(accent_color))
    painter.drawEllipse(QPointF(center, center), 4.0, 4.0)

    # 4 directional arrow indicators
    arrow_dist = r * 0.62
    arrow_size = 5.0

    for angle in [0, 90, 180, 270]:
        painter.save()
        painter.translate(center, center)
        painter.rotate(angle)
        
        poly = QPolygonF([
            QPointF(0, -arrow_dist),
            QPointF(-arrow_size, -arrow_dist + arrow_size * 1.2),
            QPointF(arrow_size, -arrow_dist + arrow_size * 1.2),
        ])
        painter.drawPolygon(poly)
        painter.restore()

    painter.end()
    return pix


class BreezeTrayIcon(QSystemTrayIcon):
    """
    KDE System Tray integration for Breeze Autoscroll.
    """

    toggle_requested = pyqtSignal(bool)

    def __init__(self, config: Config, parent=None):
        super().__init__(parent)
        self.config = config
        self.settings_dialog = None

        self._update_icon()
        self._create_menu()
        self.activated.connect(self._on_activated)

    def _update_icon(self):
        pix = create_tray_icon_pixmap(self.config.enabled)
        self.setIcon(QIcon(pix))
        status = "Aktiviert" if self.config.enabled else "Deaktiviert"
        self.setToolTip(f"Breeze Autoscroll ({status})")

    def _create_menu(self):
        menu = QMenu()
        menu.setStyleSheet("""
            QMenu {
                background-color: #24272a;
                color: #fcfcfc;
                border: 1px solid #363b40;
                border-radius: 6px;
                padding: 4px;
                font-family: 'Noto Sans', sans-serif;
            }
            QMenu::item {
                padding: 6px 24px 6px 12px;
                border-radius: 4px;
            }
            QMenu::item:selected {
                background-color: #3daee9;
                color: #ffffff;
            }
            QMenu::separator {
                height: 1px;
                background: #363b40;
                margin: 4px 8px;
            }
        """)

        self.action_toggle = menu.addAction("Autoscroll aktiviert")
        self.action_toggle.setCheckable(True)
        self.action_toggle.setChecked(self.config.enabled)
        self.action_toggle.toggled.connect(self._on_menu_toggle)

        menu.addSeparator()

        action_settings = menu.addAction("Einstellungen...")
        action_settings.triggered.connect(self.show_settings)

        action_about = menu.addAction("Über Breeze Autoscroll")
        action_about.triggered.connect(self._show_about)

        menu.addSeparator()

        action_quit = menu.addAction("Beenden")
        action_quit.triggered.connect(self._on_quit)

        self.setContextMenu(menu)

    def _on_activated(self, reason):
        if reason in (QSystemTrayIcon.ActivationReason.Trigger, QSystemTrayIcon.ActivationReason.DoubleClick):
            self.show_settings()

    def _on_menu_toggle(self, checked: bool):
        self.config.enabled = checked
        self._update_icon()
        self.toggle_requested.emit(checked)

    def show_settings(self):
        if not self.settings_dialog:
            self.settings_dialog = BreezeSettingsDialog(self.config)
            self.settings_dialog.config_changed.connect(self._on_config_changed)
        self.settings_dialog.show()
        self.settings_dialog.raise_()
        self.settings_dialog.activateWindow()

    def _on_config_changed(self):
        self.action_toggle.setChecked(self.config.enabled)
        self._update_icon()

    def _show_about(self):
        QMessageBox.about(
            None,
            "Über Breeze Autoscroll",
            "<h3>Breeze Autoscroll für Kubuntu 26.04</h3>"
            "<p>Klassische Windows-Mausrad-Klick (Autoscroll) Funktion im modernen KDE Plasma 6 / Breeze Design.</p>"
            "<p><b>Features:</b></p>"
            "<ul>"
            "<li>Flüssiges 2D-Scrollen in allen Anwendungen</li>"
            "<li>Transzenter KDE Breeze Indikator mit Pfeilbeleuchtung</li>"
            "<li>Unterstützung für Wayland & X11 via Linux uinput</li>"
            "<li>Einstellbare Totzone und Geschwindigkeitskurve</li>"
            "</ul>"
        )

    def _on_quit(self):
        from PyQt6.QtWidgets import QApplication
        QApplication.quit()
