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

# 5. Configure uinput & Mouse Permissions (udev uaccess - Least Privilege)
echo "[5/5] Konfiguriere Least-Privilege Eingabeberechtigungen (udev uaccess)..."
UDEV_RULE_SRC="${SCRIPT_DIR}/udev/70-uinput-autoscroll.rules"
UDEV_RULE_DST="/etc/udev/rules.d/70-uinput-autoscroll.rules"
LEGACY_UDEV_RULE="/etc/udev/rules.d/99-uinput-autoscroll.rules"
if [ -f "${UDEV_RULE_SRC}" ]; then
    if cmp -s "${UDEV_RULE_SRC}" "${UDEV_RULE_DST}" 2>/dev/null \
        && [ ! -e "${LEGACY_UDEV_RULE}" ]; then
        echo "✓ Gehärtete udev-Regel (uaccess) ist bereits eingerichtet."
    elif command -v sudo &>/dev/null && [ "$EUID" -ne 0 ] && { [ -t 0 ] || sudo -n true 2>/dev/null; }; then
        echo "Richte gehärtete udev-Regel (TAG+=\"uaccess\") für /dev/uinput und Mausgeräte ein..."
        sudo rm -f "${LEGACY_UDEV_RULE}" 2>/dev/null || true
        sudo install -m 644 "${UDEV_RULE_SRC}" "${UDEV_RULE_DST}" 2>/dev/null || true
        sudo modprobe uinput 2>/dev/null || true
        sudo udevadm control --reload-rules 2>/dev/null || true
        sudo udevadm trigger --subsystem-match=misc --sysname-match=uinput 2>/dev/null || true
        sudo udevadm trigger --subsystem-match=input --property-match=ID_INPUT_MOUSE=1 2>/dev/null || true
    else
        echo "Hinweis zur Least-Privilege-Berechtigung (kein automatisches 'input'-Gruppen-Opt-in):"
        echo "  Führe einmalig aus, um der aktiven lokalen Desktop-Sitzung Zugriff via uaccess zu geben:"
        echo "  sudo rm -f ${LEGACY_UDEV_RULE}"
        echo "  sudo install -m 644 ${UDEV_RULE_SRC} ${UDEV_RULE_DST}"
        echo "  sudo modprobe uinput"
        echo "  sudo udevadm control --reload-rules"
        echo "  sudo udevadm trigger --subsystem-match=misc --sysname-match=uinput"
        echo "  sudo udevadm trigger --subsystem-match=input --property-match=ID_INPUT_MOUSE=1"
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
