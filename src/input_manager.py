"""
Breeze Autoscroll - Input Manager & Virtual Mouse Engine
Handles mouse device monitoring (middle click detection and raw relative motion)
and emits virtual scroll wheel events using Linux uinput / evdev.

Key behaviors:
- Delayed activation: A short press-and-release without movement is treated as a
  normal middle click (e.g. closing a Firefox tab) and does NOT activate autoscroll.
- Gentle scroll curve: Speed starts very slow near the deadzone and increases
  gradually with distance from the anchor point.
- Thread-safe: evdev listener runs in background thread; all Qt/GUI work runs on
  the main thread via QueuedConnection signals.
"""

import math
import os
import sys
import threading
import time
from typing import List, Optional

from PyQt6.QtCore import QObject, QTimer, pyqtSignal, Qt

from config import Config
from cursor_helper import get_global_cursor_pos

# Check for evdev availability
try:
    import evdev
    from evdev import ecodes, InputDevice, UInput
    EVDEV_AVAILABLE = True
except ImportError:
    evdev = None
    ecodes = None
    InputDevice = None
    UInput = None
    EVDEV_AVAILABLE = False

# Minimum hold duration (ms) before autoscroll can activate.
# Clicks shorter than this are treated as normal middle-clicks (e.g. close tab).
ACTIVATION_DELAY_MS = 200

# Minimum movement (px) during a held press to force immediate activation
# even before ACTIVATION_DELAY_MS has elapsed.
ACTIVATION_MOVE_THRESHOLD = 6.0


class InputManager(QObject):
    """
    Manages low-level input detection and virtual scroll wheel emulation.
    """

    # Public Qt Signals for GUI and main app
    autoscroll_started = pyqtSignal(int, int)  # anchor_x, anchor_y
    autoscroll_stopped = pyqtSignal()
    offset_changed = pyqtSignal(float, float)  # delta_x, delta_y
    error_occurred = pyqtSignal(str)

    # Internal Signals to marshal events from background evdev thread to Qt main thread
    _sig_middle_down = pyqtSignal()
    _sig_middle_up = pyqtSignal()
    _sig_cancel_click = pyqtSignal()

    def __init__(self, config: Config, parent=None):
        super().__init__(parent)
        self.config = config
        self._lock = threading.Lock()

        # --- State machine ---
        # Phase 1: "pending" — middle button is held but activation delay hasn't elapsed
        # Phase 2: "active"  — autoscroll is active and emitting scroll events
        # Phase 3: "idle"    — nothing happening
        self.is_pending = False   # Waiting for activation delay
        self.is_active = False    # Autoscroll is running
        self.is_toggle_mode = False
        self.press_time = 0.0
        self.anchor_x = 0
        self.anchor_y = 0

        # Real-time relative pixel offsets accumulated from evdev hardware events
        self.curr_delta_x = 0.0
        self.curr_delta_y = 0.0

        # Accumulated movement since button down (for activation threshold)
        self.pending_move_x = 0.0
        self.pending_move_y = 0.0

        # Sub-pixel scroll accumulators
        self.accum_y = 0.0
        self.accum_x = 0.0
        self.last_tick_time = 0.0

        # Background listener thread
        self.listener_thread: Optional[threading.Thread] = None
        self.running = False

        # Virtual uinput device
        self.uinput_device = None

        # Activation delay timer (fires once after ACTIVATION_DELAY_MS)
        self.activation_timer = QTimer(self)
        self.activation_timer.setSingleShot(True)
        self.activation_timer.setInterval(ACTIVATION_DELAY_MS)
        self.activation_timer.timeout.connect(self._on_activation_timeout)

        # High-frequency scroll physics timer (runs in Qt main thread at 60Hz)
        self.scroll_timer = QTimer(self)
        self.scroll_timer.setInterval(self.config.tick_interval_ms)
        self.scroll_timer.timeout.connect(self._on_scroll_tick)

        # Connect internal worker signals with QueuedConnection to main thread
        self._sig_middle_down.connect(self._on_main_middle_down, Qt.ConnectionType.QueuedConnection)
        self._sig_middle_up.connect(self._on_main_middle_up, Qt.ConnectionType.QueuedConnection)
        self._sig_cancel_click.connect(self._on_main_cancel_click, Qt.ConnectionType.QueuedConnection)

    def start(self) -> bool:
        """Initializes virtual mouse and starts background evdev listener."""
        if not EVDEV_AVAILABLE:
            msg = "Python-Modul 'evdev' nicht gefunden. Bitte installieren mit: sudo apt install python3-evdev"
            print(f"[InputManager] {msg}")
            self.error_occurred.emit(msg)
            return False

        if not self._setup_uinput():
            return False

        self.running = True
        self.listener_thread = threading.Thread(target=self._evdev_listener_loop, daemon=True)
        self.listener_thread.start()
        print("[InputManager] Hintergrunddienst für Autoscroll erfolgreich gestartet.")
        return True

    def stop(self):
        """Stops background listeners and virtual device."""
        self.running = False
        self.stop_autoscroll()
        for t in (self.scroll_timer, self.activation_timer):
            if t.isActive():
                t.stop()
        if self.uinput_device:
            try:
                self.uinput_device.close()
            except Exception:
                pass
            self.uinput_device = None

    def _setup_uinput(self) -> bool:
        """Creates virtual mouse device with standard pointer and wheel capabilities."""
        if not os.path.exists("/dev/uinput"):
            msg = "/dev/uinput nicht gefunden. Kernel-Modul uinput laden: sudo modprobe uinput"
            self.error_occurred.emit(msg)
            return False

        if not os.access("/dev/uinput", os.W_OK):
            msg = "Keine Schreibrechte auf /dev/uinput. Bitte 'install.sh' ausführen oder Benutzer zur Gruppe 'input' hinzufügen."
            self.error_occurred.emit(msg)
            return False

        try:
            capabilities = {
                ecodes.EV_KEY: [
                    ecodes.BTN_LEFT, ecodes.BTN_RIGHT, ecodes.BTN_MIDDLE,
                    ecodes.BTN_SIDE, ecodes.BTN_EXTRA,
                ],
                ecodes.EV_REL: [
                    ecodes.REL_X, ecodes.REL_Y,
                    ecodes.REL_WHEEL, ecodes.REL_HWHEEL,
                    ecodes.REL_WHEEL_HI_RES, ecodes.REL_HWHEEL_HI_RES,
                ]
            }
            self.uinput_device = UInput(
                capabilities,
                name="Breeze Autoscroll Virtual Mouse",
                bustype=ecodes.BUS_USB,
                version=1
            )
            print("[InputManager] Virtuelles Eingabegerät erfolgreich registriert.")
            return True
        except Exception as e:
            msg = f"Fehler beim Erstellen des virtuellen Eingabegeräts: {e}"
            self.error_occurred.emit(msg)
            return False

    def _find_mouse_devices(self) -> List[InputDevice]:
        """Finds all real mouse input devices (excludes our virtual device)."""
        devices = []
        for path in evdev.list_devices():
            try:
                dev = InputDevice(path)
                caps = dev.capabilities()
                if ecodes.EV_REL in caps and ecodes.EV_KEY in caps:
                    rel_caps = caps[ecodes.EV_REL]
                    key_caps = caps[ecodes.EV_KEY]
                    if ecodes.REL_X in rel_caps and ecodes.REL_Y in rel_caps:
                        if "Breeze Autoscroll" not in dev.name:
                            devices.append(dev)
            except Exception:
                continue
        return devices

    def _evdev_listener_loop(self):
        """Monitors evdev mice for Middle Click and movement in background thread."""
        import select

        while self.running:
            devices = self._find_mouse_devices()
            if not devices:
                time.sleep(1.0)
                continue

            dev_map = {dev.fd: dev for dev in devices}
            try:
                while self.running:
                    r, _, _ = select.select(dev_map.keys(), [], [], 1.0)
                    for fd in r:
                        dev = dev_map.get(fd)
                        if not dev:
                            continue
                        try:
                            for event in dev.read():
                                self._handle_raw_event(event)
                        except (OSError, IOError):
                            break
            except Exception as e:
                print(f"[InputManager] Listener Loop Exception: {e}")
                time.sleep(1.0)

    def _handle_raw_event(self, event):
        """Processes raw evdev events from the hardware mouse (background thread)."""
        if not self.config.enabled:
            return

        # Track relative movement during pending or active autoscroll
        if event.type == ecodes.EV_REL:
            if self.is_active or self.is_pending:
                with self._lock:
                    if event.code == ecodes.REL_X:
                        self.curr_delta_x += float(event.value)
                        if self.is_pending:
                            self.pending_move_x += float(event.value)
                    elif event.code == ecodes.REL_Y:
                        self.curr_delta_y += float(event.value)
                        if self.is_pending:
                            self.pending_move_y += float(event.value)

        elif event.type == ecodes.EV_KEY:
            if event.code == ecodes.BTN_MIDDLE:
                if event.value == 1:   # Down
                    self._sig_middle_down.emit()
                elif event.value == 0:  # Up
                    self._sig_middle_up.emit()
            elif event.code in (ecodes.BTN_LEFT, ecodes.BTN_RIGHT, ecodes.BTN_SIDE, ecodes.BTN_EXTRA):
                if event.value == 1 and (self.is_active or self.is_pending):
                    self._sig_cancel_click.emit()

    # ========================================================================
    # Main GUI Thread Handlers
    # ========================================================================

    def _on_main_middle_down(self):
        """Middle button pressed — enter pending state or toggle off."""
        if self.is_active and self.is_toggle_mode:
            self._request_stop()
            return

        if self.is_active or self.is_pending:
            return

        # Capture cursor position NOW (on main thread, Qt can access Wayland cursor)
        cur_x, cur_y = get_global_cursor_pos()
        self.anchor_x = cur_x
        self.anchor_y = cur_y

        with self._lock:
            self.curr_delta_x = 0.0
            self.curr_delta_y = 0.0
            self.pending_move_x = 0.0
            self.pending_move_y = 0.0

        self.accum_x = 0.0
        self.accum_y = 0.0
        self.press_time = time.time()
        self.is_pending = True
        self.is_toggle_mode = False

        # Start activation delay timer
        self.activation_timer.start()

    def _on_activation_timeout(self):
        """Activation delay elapsed — if still pending, activate autoscroll."""
        if not self.is_pending:
            return
        self._activate_autoscroll()

    def _activate_autoscroll(self):
        """Transitions from pending to fully active autoscroll."""
        self.is_pending = False
        self.is_active = True
        self.last_tick_time = time.time()

        # Show indicator and start scroll physics
        self.autoscroll_started.emit(self.anchor_x, self.anchor_y)
        if not self.scroll_timer.isActive():
            self.scroll_timer.start()

    def _on_main_middle_up(self):
        """Middle button released."""
        # Case 1: Still in pending state (short click) — cancel, treat as normal click
        if self.is_pending:
            self.is_pending = False
            self.activation_timer.stop()
            # Do NOT activate autoscroll — this was just a regular middle click
            return

        if not self.is_active:
            return

        duration = time.time() - self.press_time
        with self._lock:
            dist = math.hypot(self.curr_delta_x, self.curr_delta_y)
        mode = self.config.mode

        if mode == "drag":
            self._request_stop()
        elif mode == "toggle":
            self.is_toggle_mode = True
        else:
            # "smart" hybrid:
            # Just activated (short time in active state) with little movement → toggle
            # Held and dragged → stop on release
            if duration < 0.45 and dist < 20.0:
                self.is_toggle_mode = True
            else:
                self._request_stop()

    def _on_main_cancel_click(self):
        """Any other mouse button pressed — cancel pending or active autoscroll."""
        if self.is_pending:
            self.is_pending = False
            self.activation_timer.stop()
            return
        if self.is_active:
            self._request_stop()

    def _request_stop(self):
        """Stop autoscroll completely."""
        self.is_active = False
        self.is_pending = False
        self.is_toggle_mode = False
        self.activation_timer.stop()
        if self.scroll_timer.isActive():
            self.scroll_timer.stop()
        self.autoscroll_stopped.emit()

    def stop_autoscroll(self):
        """Public API to stop autoscroll."""
        self._request_stop()

    # ========================================================================
    # Scroll Physics (60Hz on Main Thread)
    # ========================================================================

    def _on_scroll_tick(self):
        """Calculates scroll velocity from cursor displacement and emits wheel events."""
        if not self.is_active or not self.uinput_device:
            return

        # Check if pending movement exceeded activation threshold (early activation)
        if self.is_pending:
            with self._lock:
                pm = math.hypot(self.pending_move_x, self.pending_move_y)
            if pm > ACTIVATION_MOVE_THRESHOLD:
                self.activation_timer.stop()
                self._activate_autoscroll()
            return

        with self._lock:
            dx = self.curr_delta_x
            dy = self.curr_delta_y

        # Update overlay indicator
        self.offset_changed.emit(dx, dy)

        now = time.time()
        dt = max(0.001, min(0.05, now - self.last_tick_time))
        self.last_tick_time = now

        deadzone = float(self.config.deadzone)
        speed_mult = float(self.config.scroll_speed)

        # ---- Vertical Scrolling ----
        abs_dy = abs(dy)
        if abs_dy > deadzone:
            rate_y = self._calc_scroll_rate(abs_dy - deadzone, speed_mult)

            if self.config.get("invert_vertical", False):
                rate_y = -rate_y

            # Mouse DOWN (dy > 0) → page scrolls DOWN → REL_WHEEL = -1
            # Mouse UP   (dy < 0) → page scrolls UP   → REL_WHEEL = +1
            if dy < 0:
                self.accum_y += rate_y * dt
            else:
                self.accum_y -= rate_y * dt

            while self.accum_y >= 1.0:
                self._emit_wheel(ecodes.REL_WHEEL, ecodes.REL_WHEEL_HI_RES, 1)
                self.accum_y -= 1.0
            while self.accum_y <= -1.0:
                self._emit_wheel(ecodes.REL_WHEEL, ecodes.REL_WHEEL_HI_RES, -1)
                self.accum_y += 1.0

        # ---- Horizontal Scrolling ----
        if self.config.allow_horizontal:
            abs_dx = abs(dx)
            if abs_dx > deadzone:
                rate_x = self._calc_scroll_rate(abs_dx - deadzone, speed_mult)

                if self.config.get("invert_horizontal", False):
                    rate_x = -rate_x

                if dx > 0:
                    self.accum_x += rate_x * dt
                else:
                    self.accum_x -= rate_x * dt

                while self.accum_x >= 1.0:
                    self._emit_wheel(ecodes.REL_HWHEEL, ecodes.REL_HWHEEL_HI_RES, 1)
                    self.accum_x -= 1.0
                while self.accum_x <= -1.0:
                    self._emit_wheel(ecodes.REL_HWHEEL, ecodes.REL_HWHEEL_HI_RES, -1)
                    self.accum_x += 1.0

    @staticmethod
    def _calc_scroll_rate(effective_dist: float, speed_mult: float) -> float:
        """
        Calculates scroll rate (wheel steps per second) from distance beyond deadzone.
        
        Designed to feel natural like Windows autoscroll:
        - 1-10 px beyond deadzone:  ~1-3 lines/sec  (very slow, precise reading)
        - 10-30 px:                 ~3-8 lines/sec  (comfortable browsing)
        - 30-60 px:                 ~8-18 lines/sec (faster scanning)
        - 60-120 px:                ~18-40 lines/sec (rapid scrolling)
        - 120+ px:                  ~40-80 lines/sec (maximum speed)
        """
        # Quadratic curve: starts gentle, ramps up progressively
        # rate = speed_mult * (0.25 * d + 0.004 * d^2)
        # At d=5:  ~1.35 steps/s
        # At d=15: ~4.65 steps/s
        # At d=30: ~11.1 steps/s
        # At d=60: ~29.4 steps/s
        # At d=100: ~65 steps/s
        rate = speed_mult * (0.25 * effective_dist + 0.004 * effective_dist * effective_dist)
        return min(rate, 80.0)  # Cap at ~80 steps/sec

    def _emit_wheel(self, rel_code: int, hires_code: int, value: int):
        """Emits both standard and high-resolution wheel events via uinput."""
        try:
            self.uinput_device.write(ecodes.EV_REL, rel_code, value)
            self.uinput_device.write(ecodes.EV_REL, hires_code, value * 120)
            self.uinput_device.syn()
        except Exception as e:
            print(f"[InputManager] Fehler beim Senden des Scroll-Events: {e}")
