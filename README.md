# Breeze Autoscroll für Kubuntu 26.04 (KDE Plasma 6) — Rust Edition

> **Klassische Windows-Mausrad-Klick (Autoscroll) Funktion in nativem Rust mit KDE Plasma 6 / Breeze Design & intelligentem 3D/CAD-Schutz.**

---

## 🌟 Übersicht

Unter Windows ermöglicht ein Klick mit dem Mausrad (Mittelklick) das automatische Scrollen durch Bewegen der Maus. **Breeze Autoscroll v2.0** wurde vollständig von Python/PyQt6 nach **Rust** migriert und bringt diese Funktion blitzschnell, ressourcensparend und systemweit nach **Kubuntu 26.04 LTS (KDE Plasma 6)**:

- 🛡️ **Zweistufiger 3D / CAD & Slicer Schutz** *(Neu in v2.0)*:
  - **Stufe 1 — 3D-Orbit/Pan-Gestenerkennung**: Wenn du das Mausrad drückst und die Maus sofort zum Drehen oder Schwenken bewegst, bricht Autoscroll augenblicklich ab und lässt die native 3D-Kamera-Navigation unberührt.
  - **Stufe 2 — Automatische Fenster- & Browser-Tab-Erkennung**: Erkennt aktive Fensterklassen und Browser-Tab-Titel unter KDE Plasma 6 Wayland (KWin D-Bus) sowie X11 und deaktiviert Autoscroll automatisch u. a. in **Onshape**, **Bambu Studio**, **OrcaSlicer**, **PrusaSlicer**, **Cura**, **FreeCAD**, **Blender**, **KiCad**, **Tinkercad** und **Sketchfab** (in den Einstellungen frei anpassbar).
- 🎯 **Klick & Scroll mit Hi-Res Smooth Scrolling**: Sendet echte hochauflösende `REL_WHEEL_HI_RES` / `REL_HWHEEL_HI_RES` Events via Linux Kernel `uinput` für butterweiches, pixelgenaues 2D-Scrollen.
- 🎨 **KDE Plasma 6 Breeze Design**:
  - Klick-durchlässiges, 4× supersampled 32-Bit-ARGB-Overlay im Breeze-Dunkel- oder Breeze-Hell-Stil.
  - Natives KDE System-Tray-Applet (`StatusNotifierItem` + `dbusmenu`) und Breeze-Einstellungsdialog (`eframe`/`egui`).
- ⌨️ **Sofort-Abbruch**: Stoppt bei jedem Mausklick, beim Drehen des physischen Mausrads oder per `ESC`-Taste.
- 🔌 **Hotplug-Unterstützung**: Erkennt neu angeschlossene USB- und Bluetooth-Mäuse automatisch.

---

## 🚀 Schnellstart & Installation

Führe einfach das mitgelieferte Installationsskript aus (kompiliert mit Cargo oder nutzt das vorkompilierte Binary):

```bash
cd ~/KI-Agentic-Projects/breeze-autoscroll
./install.sh
```

Das Skript erledigt automatisch:
1. Kompilierung / Installation des nativen Rust-Binaries nach `~/.local/bin/breeze-autoscroll`
2. Einrichtung von Desktop-, Autostart- und Systemd-User-Service-Einträgen
3. Konfiguration der `uinput`-Berechtigungen via udev-Regel (`/etc/udev/rules.d/99-uinput-autoscroll.rules`)

Alternativ kannst du ein Debian-Paket (`.deb`) erstellen oder direkt installieren:

```bash
./build_deb.sh
sudo apt install ./breeze-autoscroll_2.0.0_amd64.deb
```

---

## 📖 Bedienung

### 1. Autoscroll auslösen
- **Mausrad kurz stationär drücken & halten (~180 ms)**: Zeigt den Breeze-Indikator am Klickpunkt an.
- **Kurzer Klick (< 180 ms)**: Bleibt im Hybrid-Modus ein normaler Mittelklick (Tab schließen, Link öffnen, Text einfügen).
- **Sofortiges Ziehen mit gedrücktem Mausrad**: Wird als 3D-Orbit/Pan-Drag erkannt — Autoscroll löst nicht aus.
- **Autoscroll beenden**:
  - Beliebigen Mausklick (Links-, Rechts- oder Mittelklick), physisches Mausrad drehen oder `ESC` drücken.

---

## ⚙️ Einstellungen & Anpassung

Öffne die Einstellungen über das System-Tray-Symbol in der KDE-Leiste oder über das Terminal:

```bash
breeze-autoscroll --settings
```

### Optionen im Überblick:
- **Mausrad-Klick Verhalten**: *Intelligenter Hybrid-Modus (Standard)*, *Klick zum Umschalten (Toggle)* oder *Gedrückt halten & Ziehen (Drag)*.
- **3D-Raum, CAD & Slicer Schutz**:
  - 3D-Orbit/Pan-Erkennung beim direkten Ziehen ein-/ausschalten.
  - Automatische Deaktivierung in 3D-Apps & 3D-Websites + frei bearbeitbare Liste ausgeschlossener Apps und Fenstertitel-Keywords.
- **Geschwindigkeit & Totbereich**: Scroll-Geschwindigkeit (`20%`–`300%`) und Totzone (`4 px`–`30 px`).
- **2D-Scrollen & Erscheinungsbild**: Horizontales Scrollen sowie *KDE Breeze Dunkel* / *KDE Breeze Hell*.

---

## 🛠️ Entwicklung, Tests & Verwaltung

| Aktion | Befehl |
|---|---|
| **Unit-Tests ausführen** | `cargo test --verbose` |
| **Release-Binary kompilieren** | `cargo build --release` |
| **Debian-Paket bauen** | `./build_deb.sh` |
| **Testlauf im Terminal** | `./test_autoscroll.sh` |
| **Einstellungen öffnen** | `breeze-autoscroll --settings` |
| **Hintergrunddienst aktivieren** | `systemctl --user enable --now breeze-autoscroll` |
| **Status prüfen** | `systemctl --user status breeze-autoscroll` |
| **Deinstallation** | `./uninstall.sh` |

---

## 🔒 Berechtigungen (`/dev/uinput`)

Damit der Dienst ohne Root-Rechte virtuelle Scroll-Events senden kann, muss dein Benutzer Zugriff auf `/dev/uinput` und die Eingabegeräte haben. Dies wird über `/etc/udev/rules.d/99-uinput-autoscroll.rules` und die Gruppe `input` automatisch eingerichtet. Nach erstmaliger Gruppenaufnahme einmal ab- und wieder anmelden.
