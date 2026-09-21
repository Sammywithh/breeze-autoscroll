"""
Breeze Autoscroll - Cursor Position Helper
Queries global mouse cursor coordinates on KDE Plasma 6 / Wayland / Xwayland.
Uses QCursor.pos() as primary source (works natively on Wayland/KDE Plasma),
with X11 XQueryPointer as a safe fallback.
"""

import ctypes
from ctypes import c_int, c_uint, c_ulong, c_void_p, c_char_p, byref, POINTER
import os
from typing import Optional, Tuple

from PyQt6.QtGui import QCursor, QGuiApplication

# Lazily configure libX11 with strict 64-bit pointer types
_libx11 = None
_x11_configured = False


def _ensure_x11():
    """Load and configure libX11 once, lazily."""
    global _libx11, _x11_configured
    if _x11_configured:
        return
    _x11_configured = True
    try:
        _libx11 = ctypes.cdll.LoadLibrary("libX11.so.6")
        _libx11.XOpenDisplay.restype = c_void_p
        _libx11.XOpenDisplay.argtypes = [c_char_p]
        _libx11.XDefaultRootWindow.restype = c_ulong
        _libx11.XDefaultRootWindow.argtypes = [c_void_p]
        _libx11.XCloseDisplay.restype = c_int
        _libx11.XCloseDisplay.argtypes = [c_void_p]
        _libx11.XQueryPointer.restype = c_int
        _libx11.XQueryPointer.argtypes = [
            c_void_p, c_ulong,
            POINTER(c_ulong), POINTER(c_ulong),
            POINTER(c_int), POINTER(c_int),
            POINTER(c_int), POINTER(c_int),
            POINTER(c_uint)
        ]
    except Exception:
        _libx11 = None


def _query_x11_cursor() -> Optional[Tuple[int, int]]:
    """Query cursor position via X11 XQueryPointer (fallback for X11/Xwayland)."""
    _ensure_x11()
    if _libx11 is None or "DISPLAY" not in os.environ:
        return None
    display = None
    try:
        display = _libx11.XOpenDisplay(None)
        if not display:
            return None
        root = _libx11.XDefaultRootWindow(display)
        root_ret = c_ulong()
        child_ret = c_ulong()
        root_x = c_int()
        root_y = c_int()
        win_x = c_int()
        win_y = c_int()
        mask = c_uint()
        res = _libx11.XQueryPointer(
            display, root,
            byref(root_ret), byref(child_ret),
            byref(root_x), byref(root_y),
            byref(win_x), byref(win_y),
            byref(mask)
        )
        if res != 0:
            return (int(root_x.value), int(root_y.value))
        return None
    except Exception:
        return None
    finally:
        if display is not None:
            try:
                _libx11.XCloseDisplay(display)
            except Exception:
                pass


def get_global_cursor_pos() -> Tuple[int, int]:
    """
    Returns (x, y) global screen coordinates of the mouse cursor.
    
    Priority:
    1. QCursor.pos() — works natively on KDE Plasma 6 Wayland and X11.
    2. X11 XQueryPointer — fallback for Xwayland/X11 sessions.
    3. Screen center — last resort.
    """
    # 1. Primary: Qt QCursor.pos() — reliable on native Wayland/KDE Plasma 6
    try:
        qpos = QCursor.pos()
        x, y = qpos.x(), qpos.y()
        # Sanity check: (0,0) might be valid but is suspicious on multi-monitor.
        # Accept it if there's a screen at that point.
        if x != 0 or y != 0:
            return (x, y)
        # (0,0) could be valid — check if any screen contains (0,0)
        screen = QGuiApplication.screenAt(qpos)
        if screen is not None:
            return (x, y)
    except Exception:
        pass

    # 2. Fallback: X11 XQueryPointer
    x11_pos = _query_x11_cursor()
    if x11_pos is not None:
        return x11_pos

    # 3. Last resort: primary screen center
    try:
        screen = QGuiApplication.primaryScreen()
        if screen:
            geom = screen.geometry()
            return (geom.center().x(), geom.center().y())
    except Exception:
        pass

    return (500, 500)
