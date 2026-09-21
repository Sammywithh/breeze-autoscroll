"""
Breeze Autoscroll - Configuration Manager
Handles user preferences, file persistence, and default values.
"""

import json
import os
from pathlib import Path
from typing import Any, Dict


DEFAULT_CONFIG: Dict[str, Any] = {
    "enabled": True,
    "mode": "smart",  # "smart", "toggle", "drag"
    "deadzone": 12,  # Radius in pixels where no scrolling happens
    "scroll_speed": 1.0,  # Speed multiplier (0.2 to 3.0)
    "scroll_curve_exponent": 1.3,  # Exponential curve for fine control vs fast scroll
    "allow_horizontal": True,  # Enable 2D autoscrolling (both X and Y)
    "invert_vertical": False,
    "invert_horizontal": False,
    "theme": "dark",  # "dark", "light", "breeze_blue"
    "indicator_size": 56,  # Diameter in pixels
    "indicator_opacity": 0.90,  # Window opacity
    "autostart": True,
    "tick_interval_ms": 16,  # 60 Hz scroll update loop
}


class Config:
    def __init__(self, config_dir: Path = None):
        if config_dir is None:
            self.config_dir = Path.home() / ".config" / "breeze-autoscroll"
        else:
            self.config_dir = Path(config_dir)
        
        self.config_file = self.config_dir / "config.json"
        self._data: Dict[str, Any] = dict(DEFAULT_CONFIG)
        self.load()

    def load(self) -> None:
        if self.config_file.exists():
            try:
                with open(self.config_file, "r", encoding="utf-8") as f:
                    loaded = json.load(f)
                    self._data.update(loaded)
            except Exception as e:
                print(f"[Config] Warnung beim Laden der Konfiguration: {e}")
        else:
            self.save()

    def save(self) -> None:
        try:
            self.config_dir.mkdir(parents=True, exist_ok=True)
            with open(self.config_file, "w", encoding="utf-8") as f:
                json.dump(self._data, f, indent=4, ensure_ascii=False)
        except Exception as e:
            print(f"[Config] Fehler beim Speichern der Konfiguration: {e}")

    def get(self, key: str, default: Any = None) -> Any:
        return self._data.get(key, default if default is not None else DEFAULT_CONFIG.get(key))

    def set(self, key: str, value: Any, auto_save: bool = True) -> None:
        self._data[key] = value
        if auto_save:
            self.save()

    def reset_defaults(self) -> None:
        self._data = dict(DEFAULT_CONFIG)
        self.save()

    @property
    def enabled(self) -> bool:
        return bool(self.get("enabled", True))

    @enabled.setter
    def enabled(self, val: bool) -> None:
        self.set("enabled", val)

    @property
    def deadzone(self) -> int:
        return int(self.get("deadzone", 12))

    @property
    def scroll_speed(self) -> float:
        return float(self.get("scroll_speed", 1.0))

    @property
    def mode(self) -> str:
        return str(self.get("mode", "smart"))

    @property
    def allow_horizontal(self) -> bool:
        return bool(self.get("allow_horizontal", True))

    @property
    def theme(self) -> str:
        return str(self.get("theme", "dark"))

    @property
    def indicator_size(self) -> int:
        return int(self.get("indicator_size", 56))

    @property
    def indicator_opacity(self) -> float:
        return float(self.get("indicator_opacity", 0.90))

    @property
    def tick_interval_ms(self) -> int:
        return int(self.get("tick_interval_ms", 16))
