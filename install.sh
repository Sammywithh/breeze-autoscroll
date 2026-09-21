#!/usr/bin/env bash
# ==============================================================================
# Installation Script: Breeze Autoscroll for Kubuntu 26.04 (KDE Plasma 6)
# ==============================================================================

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BIN_DIR="${HOME}/.local/bin"
DESKTOP_DIR="${HOME}/.local/share/applications"
AUTOSTART_DIR="${HOME}/.config/autostart"
SYSTEMD_USER_DIR="${HOME}/.config/systemd/user"
SHARE_DIR="${HOME}/.local/share/breeze-autoscroll"

echo "============================================================"
echo "  Breeze Autoscroll - Installation für Kubuntu 26.04"
echo "============================================================"
echo ""

# 1. Ensure required directories exist
mkdir -p "${BIN_DIR}" "${DESKTOP_DIR}" "${AUTOSTART_DIR}" "${SYSTEMD_USER_DIR}" "${SHARE_DIR}"

# 2. Check & Install Python Dependencies
echo "[1/5] Prüfe Python-Abhängigkeiten..."
MISSING_PKGS=""
if ! python3 -c "import PyQt6" 2>/dev/null; then
    MISSING_PKGS="${MISSING_PKGS} python3-pyqt6"
fi
if ! python3 -c "import evdev" 2>/dev/null; then
    MISSING_PKGS="${MISSING_PKGS} python3-evdev"
fi

if [ -n "${MISSING_PKGS}" ]; then
    echo "Folgende Pakete werden benötigt:${MISSING_PKGS}"
    if command -v sudo &>/dev/null && [ "$EUID" -ne 0 ]; then
        echo "Installiere Pakete mit apt..."
        sudo apt-get update -qq && sudo apt-get install -y ${MISSING_PKGS}
    else
        echo "HINWEIS: Bitte führe folgenden Befehl aus, um die Pakete zu installieren:"
        echo "  sudo apt-get install -y ${MISSING_PKGS}"
    fi
else
    echo "✓ Alle Python-Abhängigkeiten (PyQt6, evdev) sind vorhanden."
fi

# 3. Install Executable Launcher
echo "[2/5] Installiere Starter in ${BIN_DIR}/breeze-autoscroll..."
cat << 'EOF' > "${BIN_DIR}/breeze-autoscroll"
#!/usr/bin/env bash
PROJECT_DIR="${HOME}/KI-Agentic-Projects/breeze-autoscroll/src"
SHARE_DIR="${HOME}/.local/share/breeze-autoscroll"

if [ -d "${PROJECT_DIR}" ]; then
    exec /usr/bin/python3 "${PROJECT_DIR}/main.py" "$@"
elif [ -d "${SHARE_DIR}" ]; then
    exec /usr/bin/python3 "${SHARE_DIR}/main.py" "$@"
else
    echo "Fehler: Breeze Autoscroll Source-Dateien nicht gefunden." >&2
    exit 1
fi
EOF
chmod +x "${BIN_DIR}/breeze-autoscroll"

# Copy source to share directory as permanent install
cp -r "${SCRIPT_DIR}/src"/* "${SHARE_DIR}/"

# 4. Install Desktop Starter & Autostart
echo "[3/5] Erstelle Anwendungs- und Autostart-Einträge..."
cp "${SCRIPT_DIR}/breeze-autoscroll.desktop" "${DESKTOP_DIR}/breeze-autoscroll.desktop"
cp "${SCRIPT_DIR}/breeze-autoscroll.desktop" "${AUTOSTART_DIR}/breeze-autoscroll.desktop"
chmod +x "${DESKTOP_DIR}/breeze-autoscroll.desktop"

# 5. Install Systemd User Service
echo "[4/5] Richte Systemd-Dienst ein..."
cp "${SCRIPT_DIR}/breeze-autoscroll.service" "${SYSTEMD_USER_DIR}/breeze-autoscroll.service"

# 6. Configure uinput Permissions (udev)
echo "[5/5] Konfiguriere Eingabeberechtigungen (uinput / input-Gruppe)..."
if [ -f "${SCRIPT_DIR}/udev/99-uinput-autoscroll.rules" ]; then
    if command -v sudo &>/dev/null && [ "$EUID" -ne 0 ]; then
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

# 7. Restart any running process
pkill -f "breeze-autoscroll/src/main.py" 2>/dev/null || true

echo ""
echo "============================================================"
echo "  ✓ Installation / Aktualisierung erfolgreich abgeschlossen!"
echo "============================================================"
echo ""
echo "Verwaltung:"
echo " • Starten:              ~/.local/bin/breeze-autoscroll"
echo " • Einstellungen:        ~/.local/bin/breeze-autoscroll --settings"
echo " • Systemd-Dienst aktiv: systemctl --user enable --now breeze-autoscroll"
echo ""
