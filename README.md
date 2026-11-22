# Breeze Autoscroll für Kubuntu 26.04 (KDE Plasma 6) — Rust Edition

> **Klassische Windows-Mausrad-Klick (Autoscroll) Funktion in nativem Rust mit KDE Plasma 6 / Breeze Design & intelligentem 3D/CAD-Schutz.**

---

## 🌟 Übersicht

Unter Windows aktiviert ein Mittelklick (Drücken des Mausrades) auf einer freien Fläche einer scrollbaren Seite den Autoscroll-Modus (Automatisches Bildlaufen / Panning). **Breeze Autoscroll v2.3** setzt dieses Funktionsprinzip exakt und nativ in **Rust** für **Kubuntu 26.04 LTS (KDE Plasma 6)** um:

- 🎯 **Ankerpunkt und Vektorsteuerung (2-Wege & 4-Wege)**:
  - Beim Auslösen auf einer **nicht-interaktiven Stelle** der Seite erscheint an der exakten Klickposition (auch unter KDE Plasma 6 Wayland Multi-Monitor) ein feststehendes Ankersymbol mit Mittelpunkt und **2 Pfeilen** (Hoch/Runter) bzw. **4 Pfeilen** (wenn horizontales 2D-Scrollen aktiviert ist).
  - **Totzone (Stillstand)**: Solange der Mauszeiger direkt auf dem Ankersymbol innerhalb der Totzone bleibt, steht die Ansicht still.
  - **Richtung & Diagonal**: Bewegt man die Maus vom Anker weg, scrollt die Ansicht exakt in diese Richtung – bei aktiviertem 4-Wege-Anker stufenlos in alle Richtungen inkl. diagonal.
  - **Progressive Geschwindigkeit**: Kleine Abstände ermöglichen langsames, ruhiges Mitlesen; größere Abstände beschleunigen nicht-linear für schnelles Durchscrollen.
- 🌐 **Systemweiter Schutz interaktiver Mittelklicks (in ALLEN Linux-Apps)**:
  - **Kurze Mittelklicks systemweit durchreichen (Standard aktiv)**: Da Linux/Wayland, Electron (Obsidian, VS Code, Discord) und GTK4 (Nautilus) nicht universell melden, ob ein Mittelklick ein UI-Element ausgelöst hat, lässt Breeze Autoscroll kurze, stationäre Mittelklicks (`< 180 ms` innerhalb der Totzone) systemweit ohne Anker-Aufblitzen direkt an die jeweilige Anwendung durch – Tabs schließen, Links/Ordner im neuen Tab öffnen und Mittelklick-Einfügen funktionieren dadurch universell in **jeder** App.
  - **Zusätzlicher Mehrschicht-Schutz (Defense-in-Depth)**:
    - Generische **CSD-Headerbar-/Tab-Leisten-Erkennung** (`48 px` bei rahmenlosen Electron/GTK4-Apps wie Obsidian, Nautilus, VS Code) sowie Browser-Tableisten & Fenstertitelleisten via KWin.
    - **Hyperlink-Cursor-Erkennung** (`pointing_hand`) über die Linux DRM Hardware-Cursor-Plane (Wayland) bzw. XFixes (X11).
    - **AT-SPI2-Hit-Test** (`< 2 ms`) für native KDE/Qt- und unterstützte GTK-Anwendungen (z. B. Dolphin-Ordner, Tabs & Seitenleiste).
- 🔄 **Automatische Erkennung der zwei Bedienmodi**:
  - **Halte-Modus (sofortiges Drücken & Ziehen)**: Hält man das Mausrad gedrückt und zieht den Zeiger direkt aus der Totzone heraus, aktiviert sich Autoscroll **sofort** (ohne Wartezeit) und verschwindet beim Loslassen sofort wieder.
  - **Einrast-Modus (kurz gedrückt halten & loslassen)**: Hält man das Mausrad kurz (`~180 ms`, konfigurierbar) ruhig an einer Stelle und lässt es in der Totzone los, rastet der Anker dauerhaft ein. Beendet wird per beliebigem Mausklick (links, rechts oder mitte), physischem Mausrad oder `ESC`. (Optional lässt sich der kurze Durchreich-Schutz in den Einstellungen auch abschalten.)
- 🛡️ **3D / CAD & Slicer Schutz (Onshape, Bambu Studio, etc.)**:
  - Erkennt automatisch das Fenster direkt unter dem Klick sowie aktive Browser-Tabs via KDE KWin & X11 und lässt natives 3D-Orbit/Pan in **Onshape**, **Bambu Studio**, **OrcaSlicer**, **PrusaSlicer**, **Cura**, **FreeCAD**, **Blender**, **KiCad**, **Tinkercad** und **Sketchfab** vollständig unberührt.
  - Optional lässt sich in den Einstellungen zusätzlich ein strenger Orbit-Schutz für nicht gelistete 3D-Websites aktivieren.
- 🎨 **KDE Plasma 6 Breeze Design & Hi-Res Smooth Scrolling**:
  - Klick-durchlässiges, 4× supersampled 32-Bit-ARGB-Overlay im Breeze-Dunkel- oder Breeze-Hell-Stil und natives System-Tray-Applet (`StatusNotifierItem`).
  - Feingranulare `REL_WHEEL_HI_RES` / `REL_HWHEEL_HI_RES` Events via Linux Kernel `uinput` mit automatischer Maus-Hotplug-Erkennung.

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
sudo apt install ./breeze-autoscroll_2.3.0_amd64.deb
```

---

## 📖 Bedienung

### 1. Autoscroll auslösen
- **Kurzer Mittelklick (`< 180 ms`)**: Wird systemweit als normaler Mittelklick durchgereicht (Tabs schließen, Links/Ordner im neuen Tab öffnen, Einfügen in jeder Anwendung).
- **Halte-Modus (sofort)**: Mausrad drücken, Zeiger direkt aus der Totzone ziehen und zum Beenden einfach wieder loslassen – startet ohne Verzögerung.
- **Einrast-Modus**: Mausrad kurz (`~180 ms`, bis der Anker erscheint) in der Totzone gedrückt halten und loslassen $\rightarrow$ Anker bleibt fixiert; Beenden durch Links-, Rechts- oder Mittelklick, physisches Mausrad oder `ESC`.
- **3D-Programme & CAD**: In erkannten 3D-Anwendungen und 3D-Browser-Tabs (z. B. Onshape, Bambu Studio) bleibt das Mittelklick-Drehen automatisch nativ erhalten.

---

## ⚙️ Einstellungen & Anpassung

Öffne die Einstellungen über das System-Tray-Symbol in der KDE-Leiste oder über das Terminal:

```bash
breeze-autoscroll --settings
```

### Optionen im Überblick:
- **Mausrad-Klick Verhalten**: *Windows-Automatik (Einrast- & Halte-Modus, Standard)*, *Immer Einrast-Modus (Toggle)* oder *Immer Halte-Modus (Drag)*.
- **Systemweiter Schutz für kurze Mittelklicks**: *Kurze Mittelklicks systemweit durchreichen* (Standard aktiv) + einstellbare Haltezeit für den Einrast-Modus (`100 ms`–`400 ms`, Standard `180 ms`).
- **Anker-Typ**: *4-Wege-Anker inkl. Diagonal* (wenn horizontales 2D-Scrollen aktiviert) oder *2-Wege-Anker* (nur Vertikal).
- **3D-Raum, CAD & Slicer Schutz**:
  - Automatische Deaktivierung in 3D-Apps & 3D-Websites + frei bearbeitbare Liste ausgeschlossener Apps und Fenstertitel-Keywords.
  - Optionaler strenger Orbit-Schutz bei sofortiger Bewegung für nicht gelistete 3D-Websites.
- **Geschwindigkeit & Totbereich**: Scroll-Geschwindigkeit (`20%`–`300%`) und Totzone (`4 px`–`30 px`).
- **Erscheinungsbild**: *KDE Breeze Dunkel* / *KDE Breeze Hell*.

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
