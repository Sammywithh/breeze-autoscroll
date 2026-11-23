#!/usr/bin/env bash
# ==============================================================================
# Deinstallation Script: Breeze Autoscroll for Kubuntu 26.04
# ==============================================================================

set -e

echo "=== Deinstallation von Breeze Autoscroll ==="
echo ""

# 1. Stop and disable systemd service
if systemctl --user is-active --quiet breeze-autoscroll 2>/dev/null; then
    echo "Stoppe Hintergrunddienst..."
    systemctl --user stop breeze-autoscroll 2>/dev/null || true
    systemctl --user disable breeze-autoscroll 2>/dev/null || true
fi

# 2. Kill running processes (Rust & legacy Python)
pkill -x "breeze-autoscro" 2>/dev/null || true
pkill -f "^([^[:space:]]*/)?breeze-autoscroll($| )" 2>/dev/null || true
pkill -f "breeze-autoscroll/src/main.py" 2>/dev/null || true

# 3. Remove installed files
echo "Entferne installierte Dateien..."
rm -f "${HOME}/.local/bin/breeze-autoscroll"
rm -f "${HOME}/.local/share/applications/breeze-autoscroll.desktop"
rm -f "${HOME}/.config/autostart/breeze-autoscroll.desktop"
rm -f "${HOME}/.config/systemd/user/breeze-autoscroll.service"
rm -rf "${HOME}/.local/share/breeze-autoscroll"
rm -rf "${HOME}/.cache/breeze-autoscroll"
if [ -n "${XDG_RUNTIME_DIR:-}" ]; then
    rm -rf "${XDG_RUNTIME_DIR}/breeze-autoscroll"
fi

# 4. Optional removal of udev rules (current 70- and legacy 99-)
if [ -f /etc/udev/rules.d/70-uinput-autoscroll.rules ] || [ -f /etc/udev/rules.d/99-uinput-autoscroll.rules ]; then
    if command -v sudo &>/dev/null && [ "$EUID" -ne 0 ]; then
        echo "Entferne udev-Regeln..."
        sudo rm -f /etc/udev/rules.d/70-uinput-autoscroll.rules /etc/udev/rules.d/99-uinput-autoscroll.rules 2>/dev/null || true
        sudo udevadm control --reload-rules 2>/dev/null || true
        sudo udevadm trigger --subsystem-match=misc --sysname-match=uinput 2>/dev/null || true
        sudo udevadm trigger --subsystem-match=input --property-match=ID_INPUT_MOUSE=1 2>/dev/null || true
    fi
fi

echo ""
echo "✓ Breeze Autoscroll wurde erfolgreich deinstalliert."
