#!/usr/bin/env bash
# ==============================================================================
# Installation Script: Breeze Autoscroll (Rust) for Kubuntu 26.04 (KDE Plasma 6)
# ==============================================================================

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BIN_DIR="${HOME}/.local/bin"
DESKTOP_DIR="${HOME}/.local/share/applications"
AUTOSTART_DIR="${HOME}/.config/autostart"
SYSTEMD_USER_DIR="${HOME}/.config/systemd/user"
SHARE_DIR="${HOME}/.local/share/breeze-autoscroll"
TARGET_BIN="${BIN_DIR}/breeze-autoscroll"

echo "============================================================"
echo "  Breeze Autoscroll v2.3 (Rust) - Installation für Kubuntu 26.04"
echo "============================================================"
echo ""

# 1. Ensure required directories exist
mkdir -p "${BIN_DIR}" "${DESKTOP_DIR}" "${AUTOSTART_DIR}" "${SYSTEMD_USER_DIR}"

# 2. Build or locate native Rust binary
echo "[1/5] Bereite natives Rust-Binary vor..."
SOURCE_BIN=""
if command -v cargo &>/dev/null && [ -f "${SCRIPT_DIR}/Cargo.toml" ]; then
    echo "Kompiliere Release-Binary mit Cargo..."
    (cd "${SCRIPT_DIR}" && cargo build --release --locked)
    SOURCE_BIN="${SCRIPT_DIR}/target/release/breeze-autoscroll"
    cp "${SOURCE_BIN}" "${SCRIPT_DIR}/breeze-autoscroll"
    chmod +x "${SCRIPT_DIR}/breeze-autoscroll"
elif [ -x "${SCRIPT_DIR}/breeze-autoscroll" ]; then
    SOURCE_BIN="${SCRIPT_DIR}/breeze-autoscroll"
    echo "✓ Verwende vorkompiliertes Binary: ${SOURCE_BIN}"
else
    echo "Fehler: Weder 'cargo' noch vorkompiliertes './breeze-autoscroll' gefunden." >&2
    exit 1
fi

# 3. Stop any running instance (legacy Python or older Rust binary)
echo "[2/5] Stoppe eventuell laufende alte Prozesse..."
systemctl --user stop breeze-autoscroll.service 2>/dev/null || true
pkill -f "breeze-autoscroll/src/main.py" 2>/dev/null || true
pkill -x "breeze-autoscro" 2>/dev/null || true
pkill -f "^([^[:space:]]*/)?breeze-autoscroll($| )" 2>/dev/null || true
sleep 0.2
rm -rf "${SHARE_DIR}"

# Install native binary
echo "[3/5] Installiere Binary nach ${TARGET_BIN}..."
install -m 755 "${SOURCE_BIN}" "${TARGET_BIN}"

# 4. Install Desktop Starter & Autostart
echo "[4/5] Erstelle Anwendungs- und Autostart-Einträge..."
sed "s|^Exec=.*|Exec=${TARGET_BIN}|" "${SCRIPT_DIR}/breeze-autoscroll.desktop" > "${DESKTOP_DIR}/breeze-autoscroll.desktop"
cp "${DESKTOP_DIR}/breeze-autoscroll.desktop" "${AUTOSTART_DIR}/breeze-autoscroll.desktop"
chmod +x "${DESKTOP_DIR}/breeze-autoscroll.desktop"

# Install Systemd User Service
cp "${SCRIPT_DIR}/breeze-autoscroll.service" "${SYSTEMD_USER_DIR}/breeze-autoscroll.service"
systemctl --user daemon-reload 2>/dev/null || true

# 5. Configure uinput Permissions (udev)
echo "[5/5] Konfiguriere Eingabeberechtigungen (uinput / input-Gruppe)..."
if [ -f "${SCRIPT_DIR}/udev/99-uinput-autoscroll.rules" ]; then
    if cmp -s "${SCRIPT_DIR}/udev/99-uinput-autoscroll.rules" /etc/udev/rules.d/99-uinput-autoscroll.rules 2>/dev/null \
        && id -nG "${USER}" 2>/dev/null | grep -qw "input"; then
        echo "✓ udev-Regel und 'input'-Gruppenrechte sind bereits eingerichtet."
    elif command -v sudo &>/dev/null && [ "$EUID" -ne 0 ] && { [ -t 0 ] || sudo -n true 2>/dev/null; }; then
        echo "Richte udev-Regel für /dev/uinput ein..."
        sudo cp "${SCRIPT_DIR}/udev/99-uinput-autoscroll.rules" /etc/udev/rules.d/ 2>/dev/null || true
        sudo udevadm control --reload-rules 2>/dev/null || true
        sudo udevadm trigger 2>/dev/null || true
        sudo usermod -a -G input "${USER}" 2>/dev/null || true
        sudo modprobe uinput 2>/dev/null || true
    else
        echo "Hinweis zur Berechtigung:"
        echo "  Um ohne Root-Rechte auf /dev/uinput zuzugreifen, führe einmalig aus:"
        echo "  sudo cp ${SCRIPT_DIR}/udev/99-uinput-autoscroll.rules /etc/udev/rules.d/"
        echo "  sudo usermod -a -G input \${USER}"
        echo "  sudo udevadm control --reload-rules && sudo udevadm trigger"
    fi
fi

echo ""
echo "============================================================"
echo "  ✓ Installation / Aktualisierung erfolgreich abgeschlossen!"
echo "============================================================"
echo ""
echo "Verwaltung:"
echo " • Starten:              ${TARGET_BIN}"
echo " • Einstellungen:        ${TARGET_BIN} --settings"
echo " • Systemd-Dienst aktiv: systemctl --user enable --now breeze-autoscroll"
echo ""
