#!/usr/bin/env bash
# ==============================================================================
# Build Script: Erstellt ein AIO Debian Package (.deb) für Breeze Autoscroll
# ==============================================================================

set -e

VERSION="1.0.0"
PKG_NAME="breeze-autoscroll"
ARCH="all"
BUILD_DIR="${PKG_NAME}_${VERSION}_${ARCH}"

echo "============================================================"
echo "  Breeze Autoscroll - DEB Package Builder"
echo "============================================================"
echo "Erstelle Paketstruktur in ${BUILD_DIR}..."

# Verzeichnisstruktur erstellen
mkdir -p "${BUILD_DIR}/DEBIAN"
mkdir -p "${BUILD_DIR}/opt/${PKG_NAME}"
mkdir -p "${BUILD_DIR}/usr/bin"
mkdir -p "${BUILD_DIR}/usr/share/applications"
mkdir -p "${BUILD_DIR}/etc/udev/rules.d"
mkdir -p "${BUILD_DIR}/usr/lib/systemd/user"

# Sourcen kopieren (Code)
cp -r src "${BUILD_DIR}/opt/${PKG_NAME}/"
if [ -d "assets" ]; then
    cp -r assets "${BUILD_DIR}/opt/${PKG_NAME}/"
fi

# Launcher Script erstellen (/usr/bin/breeze-autoscroll)
cat << 'EOF' > "${BUILD_DIR}/usr/bin/${PKG_NAME}"
#!/usr/bin/env bash
# Startskript für das systemweit installierte Breeze Autoscroll
exec /usr/bin/python3 /opt/breeze-autoscroll/src/main.py "$@"
EOF
chmod 755 "${BUILD_DIR}/usr/bin/${PKG_NAME}"

# Desktop File für das Startmenü/Autostart
cp breeze-autoscroll.desktop "${BUILD_DIR}/usr/share/applications/"
chmod 644 "${BUILD_DIR}/usr/share/applications/breeze-autoscroll.desktop"

# Systemd User Service (angepasst für systemweiten Pfad /usr/bin)
sed -e 's|%h/.local/bin/breeze-autoscroll|/usr/bin/breeze-autoscroll|g' breeze-autoscroll.service > "${BUILD_DIR}/usr/lib/systemd/user/breeze-autoscroll.service"
chmod 644 "${BUILD_DIR}/usr/lib/systemd/user/breeze-autoscroll.service"

# Udev Rules für evdev/uinput
cp udev/99-uinput-autoscroll.rules "${BUILD_DIR}/etc/udev/rules.d/"
chmod 644 "${BUILD_DIR}/etc/udev/rules.d/99-uinput-autoscroll.rules"

# DEBIAN/control - Paketmetadaten und Abhängigkeiten
cat << EOF > "${BUILD_DIR}/DEBIAN/control"
Package: ${PKG_NAME}
Version: ${VERSION}
Section: utils
Priority: optional
Architecture: ${ARCH}
Depends: python3, python3-pyqt6, python3-evdev
Maintainer: KI Agent
Description: Windows-style middle-click autoscroll for Kubuntu 26.04 (KDE Plasma 6).
 Bringt die klassische Autoscroll-Funktion unter Linux (X11 & Wayland).
 Enthält ein natives Qt6 / Breeze Overlay.
EOF

# DEBIAN/postinst - Post-Installations-Skript
cat << 'EOF' > "${BUILD_DIR}/DEBIAN/postinst"
#!/usr/bin/env bash
set -e

if [ "$1" = "configure" ]; then
    echo "Lade udev-Regeln neu..."
    udevadm control --reload-rules || true
    udevadm trigger || true
    
    echo "Stelle sicher, dass Kernel-Modul uinput geladen ist..."
    modprobe uinput || true
    echo "uinput" > /etc/modules-load.d/breeze-autoscroll-uinput.conf
    
    echo "Füge Standard-Nutzer zur Gruppe 'input' hinzu..."
    for user in $(awk -F: '$3 >= 1000 && $3 < 60000 {print $1}' /etc/passwd); do
        usermod -a -G input "$user" 2>/dev/null || true
    done
    
    echo "======================================================================="
    echo "Breeze Autoscroll wurde erfolgreich installiert!"
    echo "HINWEIS: Da Nutzergruppen geändert wurden (input), müssen Sie sich"
    echo "         einmal aus- und wieder einloggen (Relogin) oder den PC neu starten,"
    echo "         damit das Programm Ihre Mauseingaben lesen darf."
    echo "======================================================================="
fi
exit 0
EOF
chmod 755 "${BUILD_DIR}/DEBIAN/postinst"

# DEBIAN/postrm - Post-Removal-Skript
cat << 'EOF' > "${BUILD_DIR}/DEBIAN/postrm"
#!/usr/bin/env bash
set -e

if [ "$1" = "remove" ] || [ "$1" = "purge" ]; then
    rm -f /etc/modules-load.d/breeze-autoscroll-uinput.conf
    udevadm control --reload-rules || true
    
    # Reload systemd user daemon for all users to catch removed service
    systemctl --global daemon-reload || true
fi
exit 0
EOF
chmod 755 "${BUILD_DIR}/DEBIAN/postrm"

# Build package
echo "Baue .deb Paket..."
chmod -R 755 "${BUILD_DIR}/DEBIAN"
chmod -R 755 "${BUILD_DIR}/opt"
chmod -R 755 "${BUILD_DIR}/usr"
chmod -R 755 "${BUILD_DIR}/etc"
dpkg-deb --root-owner-group --build "${BUILD_DIR}"

echo "Räume temporäre Ordner auf..."
rm -rf "${BUILD_DIR}"

echo "============================================================"
echo "  ✓ Fertig! Das Paket '${BUILD_DIR}.deb' wurde erfolgreich erstellt."
echo "  Installation lokal testen:"
echo "    sudo apt install ./${BUILD_DIR}.deb"
echo "============================================================"
