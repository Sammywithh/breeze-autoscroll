"""
Breeze Autoscroll - Origin Indicator Overlay Widget
Kubuntu 26.04 / KDE Plasma 6 Breeze design language.
Renders a compact, translucent, animated compass-style anchor widget at the middle-click point.
"""

import math
from typing import Tuple
from PyQt6.QtCore import Qt, QPoint, QPointF, QRectF, QPropertyAnimation, QEasingCurve, pyqtProperty
from PyQt6.QtGui import (
    QPainter, QColor, QBrush, QPen, QPainterPath, QRadialGradient, QLinearGradient, QPolygonF
)
from PyQt6.QtWidgets import QWidget


class BreezeIndicatorWidget(QWidget):
    """
    Compact floating transparent overlay widget displaying the autoscroll anchor point
    in modern KDE Breeze / Kubuntu style (~32px diameter).
    """

    def __init__(self, parent=None):
        super().__init__(parent)
        
        # ToolTip / BypassWindowManagerHint ensures exact pixel placement without window manager decorations
        self.setWindowFlags(
            Qt.WindowType.ToolTip |
            Qt.WindowType.FramelessWindowHint |
            Qt.WindowType.WindowStaysOnTopHint |
            Qt.WindowType.WindowTransparentForInput |
            Qt.WindowType.BypassWindowManagerHint
        )
        self.setAttribute(Qt.WidgetAttribute.WA_TranslucentBackground, True)
        self.setAttribute(Qt.WidgetAttribute.WA_ShowWithoutActivating, True)

        # Compact dimensions (32px inner circle + 6px glow margin = 44x44px widget)
        self.base_diameter = 32
        self.deadzone_radius = 8
        self.margin = 6
        total_size = self.base_diameter + (self.margin * 2)
        self.setFixedSize(total_size, total_size)
        
        # Anchor position in screen coordinates
        self.anchor_x = 0
        self.anchor_y = 0
        
        # Current mouse offset from anchor (delta_x, delta_y)
        self.delta_x = 0.0
        self.delta_y = 0.0
        self.is_active = False

        # Visual Theme (Breeze Dark default)
        self.theme_mode = "dark"
        self._opacity = 0.0

        # Fade Animation
        self.fade_anim = QPropertyAnimation(self, b"anim_opacity")
        self.fade_anim.setDuration(120)
        self.fade_anim.setEasingCurve(QEasingCurve.Type.OutCubic)

    def get_anim_opacity(self) -> float:
        return self._opacity

    def set_anim_opacity(self, val: float):
        self._opacity = val
        self.setWindowOpacity(val)
        self.update()

    anim_opacity = pyqtProperty(float, get_anim_opacity, set_anim_opacity)

    def show_at(self, screen_x: int, screen_y: int, deadzone: int = 8, theme: str = "dark"):
        """Displays the compact indicator centered exactly at the given screen coordinates."""
        self.anchor_x = screen_x
        self.anchor_y = screen_y
        self.deadzone_radius = max(4, min(14, deadzone))
        self.theme_mode = theme
        self.delta_x = 0.0
        self.delta_y = 0.0
        self.is_active = True

        # Center widget precisely over the anchor point
        half_w = self.width() // 2
        half_h = self.height() // 2
        self.move(screen_x - half_w, screen_y - half_h)
        self.show()

        # Fade in
        self.fade_anim.stop()
        self.fade_anim.setStartValue(self._opacity)
        self.fade_anim.setEndValue(0.95)
        self.fade_anim.start()

    def update_offset(self, delta_x: float, delta_y: float):
        """Updates current relative cursor position to animate arrows and vector."""
        self.delta_x = delta_x
        self.delta_y = delta_y
        self.update()

    def hide_indicator(self):
        """Fades out and hides the indicator."""
        self.is_active = False
        self.fade_anim.stop()
        self.fade_anim.setStartValue(self._opacity)
        self.fade_anim.setEndValue(0.0)
        self.fade_anim.finished.connect(self._on_fade_finished)
        self.fade_anim.start()

    def _on_fade_finished(self):
        if not self.is_active:
            try:
                self.fade_anim.finished.disconnect(self._on_fade_finished)
            except TypeError:
                pass
            self.hide()

    def paintEvent(self, event):
        painter = QPainter(self)
        painter.setRenderHint(QPainter.RenderHint.Antialiasing, True)
        painter.setRenderHint(QPainter.RenderHint.SmoothPixmapTransform, True)

        center_x = self.width() / 2.0
        center_y = self.height() / 2.0
        radius = self.base_diameter / 2.0  # 16px

        # Colors based on KDE Plasma 6 Breeze Theme
        if self.theme_mode == "light":
            bg_color = QColor(240, 242, 245, 235)
            border_color = QColor(190, 195, 200, 220)
            glow_color = QColor(61, 174, 233, 150)
            arrow_inactive = QColor(80, 85, 90, 190)
            arrow_active = QColor(29, 153, 243, 255)
            center_dot_color = QColor(29, 153, 243, 255)
            deadzone_color = QColor(160, 165, 170, 70)
        else:  # dark (default Kubuntu Breeze)
            bg_color = QColor(30, 34, 38, 235)
            border_color = QColor(255, 255, 255, 45)
            glow_color = QColor(61, 174, 233, 140)
            arrow_inactive = QColor(240, 240, 240, 180)
            arrow_active = QColor(61, 174, 233, 255)
            center_dot_color = QColor(61, 174, 233, 255)
            deadzone_color = QColor(255, 255, 255, 30)

        # 1. Subtle Outer Glow
        glow_gradient = QRadialGradient(center_x, center_y, radius + 5)
        glow_gradient.setColorAt(0.65, glow_color)
        glow_gradient.setColorAt(1.0, QColor(61, 174, 233, 0))
        painter.setPen(Qt.PenStyle.NoPen)
        painter.setBrush(QBrush(glow_gradient))
        painter.drawEllipse(QPointF(center_x, center_y), radius + 5, radius + 5)

        # 2. Main Glass Circle (Kubuntu Breeze Disc)
        painter.setPen(QPen(border_color, 1.2))
        painter.setBrush(QBrush(bg_color))
        painter.drawEllipse(QPointF(center_x, center_y), radius, radius)

        # 3. Deadzone Circle (Dashed subtle inner guide)
        deadzone_px = min(radius - 5, max(4.0, self.deadzone_radius * 0.7))
        deadzone_pen = QPen(deadzone_color, 0.8, Qt.PenStyle.DotLine)
        painter.setPen(deadzone_pen)
        painter.setBrush(Qt.BrushStyle.NoBrush)
        painter.drawEllipse(QPointF(center_x, center_y), deadzone_px, deadzone_px)

        # Distance calculation
        dist = math.hypot(self.delta_x, self.delta_y)
        is_scrolling = dist > self.deadzone_radius

        # 4. Center Anchor Point (Breeze Blue Dot with White Core)
        painter.setPen(Qt.PenStyle.NoPen)
        painter.setBrush(QBrush(center_dot_color))
        painter.drawEllipse(QPointF(center_x, center_y), 2.8, 2.8)
        
        painter.setBrush(QBrush(QColor(255, 255, 255, 240)))
        painter.drawEllipse(QPointF(center_x, center_y), 1.0, 1.0)

        # 5. Directional Arrow Calculation
        up_active = self.delta_y < -self.deadzone_radius
        down_active = self.delta_y > self.deadzone_radius
        left_active = self.delta_x < -self.deadzone_radius
        right_active = self.delta_x > self.deadzone_radius

        # Arrow geometry
        arrow_offset = radius * 0.64
        arrow_size = 3.6

        # Draw North Arrow (Up)
        self._draw_arrow(
            painter,
            tip=QPointF(center_x, center_y - arrow_offset),
            direction=0,
            size=arrow_size,
            active=up_active,
            active_color=arrow_active,
            inactive_color=arrow_inactive
        )

        # Draw South Arrow (Down)
        self._draw_arrow(
            painter,
            tip=QPointF(center_x, center_y + arrow_offset),
            direction=180,
            size=arrow_size,
            active=down_active,
            active_color=arrow_active,
            inactive_color=arrow_inactive
        )

        # Draw West Arrow (Left)
        self._draw_arrow(
            painter,
            tip=QPointF(center_x - arrow_offset, center_y),
            direction=270,
            size=arrow_size,
            active=left_active,
            active_color=arrow_active,
            inactive_color=arrow_inactive
        )

        # Draw East Arrow (Right)
        self._draw_arrow(
            painter,
            tip=QPointF(center_x + arrow_offset, center_y),
            direction=90,
            size=arrow_size,
            active=right_active,
            active_color=arrow_active,
            inactive_color=arrow_inactive
        )

    def _draw_arrow(
        self,
        painter: QPainter,
        tip: QPointF,
        direction: int,
        size: float,
        active: bool,
        active_color: QColor,
        inactive_color: QColor
    ):
        """Draws a crisp triangular Breeze vector arrow with optional glowing pulse."""
        painter.save()
        painter.translate(tip)
        painter.rotate(direction)

        poly = QPolygonF([
            QPointF(0, -size * 0.75),     # Tip
            QPointF(-size, size * 0.65),  # Bottom Left
            QPointF(0, size * 0.25),      # Inner Notch
            QPointF(size, size * 0.65),   # Bottom Right
        ])

        if active:
            # Subtle glow outline
            glow_pen = QPen(QColor(61, 174, 233, 140), 2.2, Qt.PenStyle.SolidLine, Qt.PenCapStyle.RoundCap, Qt.PenJoinStyle.RoundJoin)
            painter.setPen(glow_pen)
            painter.setBrush(Qt.BrushStyle.NoBrush)
            painter.drawPolygon(poly)

            # Solid glowing arrow
            painter.setPen(QPen(QColor(255, 255, 255, 230), 0.6))
            painter.setBrush(QBrush(active_color))
        else:
            painter.setPen(Qt.PenStyle.NoPen)
            painter.setBrush(QBrush(inactive_color))

        painter.drawPolygon(poly)
        painter.restore()
