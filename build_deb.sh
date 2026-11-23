#!/usr/bin/env bash
# ==============================================================================
# Build Script: Erstellt ein Debian Package (.deb) für Breeze Autoscroll (Rust)
# ==============================================================================

set -euo pipefail
umask 022

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "${SCRIPT_DIR}"

VERSION="2.3.0"
PKG_NAME="breeze-autoscroll"
ARCH="$(dpkg --print-architecture 2>/dev/null || echo amd64)"
BUILD_DIR="${PKG_NAME}_${VERSION}_${ARCH}"

echo "============================================================"
echo "  Breeze Autoscroll v${VERSION} - DEB Package Builder (${ARCH})"
echo "============================================================"

# Build release binary deterministically from source (no unverified prebuilt fallback)
if ! command -v cargo &>/dev/null; then
    echo "Fehler: 'cargo' (Rust Toolchain) wurde nicht gefunden. Bitte installiere Rust/Cargo für einen deterministischen Build." >&2
    exit 1
fi
if [ ! -f "Cargo.toml" ]; then
    echo "Fehler: 'Cargo.toml' im Repository-Verzeichnis nicht gefunden." >&2
    exit 1
fi
echo "Kompiliere Release-Binary deterministisch (--locked)..."
cargo build --release --locked
BIN_PATH="target/release/${PKG_NAME}"

echo "Erstelle Paketstruktur in ${BUILD_DIR}..."
rm -rf "${BUILD_DIR}"
mkdir -p "${BUILD_DIR}/DEBIAN"
mkdir -p "${BUILD_DIR}/usr/bin"
mkdir -p "${BUILD_DIR}/usr/share/applications"
mkdir -p "${BUILD_DIR}/etc/udev/rules.d"
mkdir -p "${BUILD_DIR}/usr/lib/systemd/user"

# Natives Binary installieren
install -m 755 "${BIN_PATH}" "${BUILD_DIR}/usr/bin/${PKG_NAME}"

# Desktop File für das Startmenü/Autostart
sed "s|^Exec=.*|Exec=/usr/bin/${PKG_NAME}|" breeze-autoscroll.desktop > "${BUILD_DIR}/usr/share/applications/breeze-autoscroll.desktop"
chmod 644 "${BUILD_DIR}/usr/share/applications/breeze-autoscroll.desktop"

# Systemd User Service (angepasst für systemweiten Pfad /usr/bin)
sed -e "s|%h/.local/bin/breeze-autoscroll|/usr/bin/${PKG_NAME}|g" breeze-autoscroll.service > "${BUILD_DIR}/usr/lib/systemd/user/breeze-autoscroll.service"
chmod 644 "${BUILD_DIR}/usr/lib/systemd/user/breeze-autoscroll.service"

# Udev Rules für evdev/uinput (Least Privilege via uaccess, Priorität 70 vor 73-seat-late.rules)
cp udev/70-uinput-autoscroll.rules "${BUILD_DIR}/etc/udev/rules.d/"
chmod 644 "${BUILD_DIR}/etc/udev/rules.d/70-uinput-autoscroll.rules"

# DEBIAN/control - Paketmetadaten und Abhängigkeiten
cat << EOF > "${BUILD_DIR}/DEBIAN/control"
Package: ${PKG_NAME}
Version: ${VERSION}
Section: utils
Priority: optional
Architecture: ${ARCH}
Depends: libc6, libgl1, libx11-6, libxcb1, libwayland-client0, libdbus-1-3
Maintainer: Sammywithh
Description: Windows-style middle-click autoscroll for Kubuntu 26.04 (KDE Plasma 6).
 Native Rust implementation bringing classic middle-click autoscroll to Linux
 (Wayland & X11) via uinput, including two-layer 3D/CAD orbit protection for
 Onshape, Bambu Studio, FreeCAD, Blender, and a native KDE Breeze UI.
EOF

# DEBIAN/postinst - Post-Installations-Skript (Least Privilege: kein globales usermod -aG input)
cat << 'EOF' > "${BUILD_DIR}/DEBIAN/postinst"
#!/usr/bin/env bash
set -euo pipefail
umask 022

if [ "${1:-}" = "configure" ]; then
    echo "Entferne veraltete udev-Regel (falls vorhanden)..."
    rm -f /etc/udev/rules.d/99-uinput-autoscroll.rules || true

    echo "Stelle sicher, dass Kernel-Modul uinput geladen ist..."
    modprobe uinput || true
    echo "uinput" > /etc/modules-load.d/breeze-autoscroll-uinput.conf

    echo "Lade gehärtete udev-Regeln (uaccess) neu..."
    udevadm control --reload-rules || true
    udevadm trigger --subsystem-match=misc --sysname-match=uinput || true
    udevadm trigger --subsystem-match=input --property-match=ID_INPUT_MOUSE=1 || true

    echo "======================================================================="
    echo "Breeze Autoscroll wurde erfolgreich installiert (Least-Privilege uaccess)!"
    echo "Die aktive lokale Desktop-Sitzung erhält automatisch Zugriff auf"
    echo "/dev/uinput und reine Maus-Geräte (ohne globale 'input'-Gruppe)."
    echo ""
    echo "Optionaler Hinweis: Falls für spezielle Eingabegeräte oder ESC-Abbruch"
    echo "über evdev explizit Tastatur-Lesezugriff gewünscht ist, kann ein"
    echo "einzelner Nutzer manuell hinzugefügt werden:"
    echo "  sudo usermod -aG input <benutzername>"
    echo "======================================================================="
fi
exit 0
EOF
chmod 755 "${BUILD_DIR}/DEBIAN/postinst"

# DEBIAN/postrm - Post-Removal-Skript
cat << 'EOF' > "${BUILD_DIR}/DEBIAN/postrm"
#!/usr/bin/env bash
set -euo pipefail
umask 022

if [ "${1:-}" = "remove" ] || [ "${1:-}" = "purge" ]; then
    rm -f /etc/modules-load.d/breeze-autoscroll-uinput.conf
    rm -f /etc/udev/rules.d/99-uinput-autoscroll.rules || true
    udevadm control --reload-rules || true
    udevadm trigger --subsystem-match=misc --sysname-match=uinput || true
    udevadm trigger --subsystem-match=input --property-match=ID_INPUT_MOUSE=1 || true
fi
exit 0
EOF
chmod 755 "${BUILD_DIR}/DEBIAN/postrm"

# Build package
echo "Baue .deb Paket..."
chmod 755 "${BUILD_DIR}/DEBIAN"
dpkg-deb --root-owner-group --build "${BUILD_DIR}"

echo "Räume temporäre Ordner auf..."
rm -rf "${BUILD_DIR}"

echo "============================================================"
echo "  ✓ Fertig! Das Paket '${BUILD_DIR}.deb' wurde erfolgreich erstellt."
echo "  Installation lokal testen:"
echo "    sudo apt install ./${BUILD_DIR}.deb"
echo "============================================================"
