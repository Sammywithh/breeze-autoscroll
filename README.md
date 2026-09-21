# Breeze Autoscroll für Kubuntu 26.04 (KDE Plasma 6)

> **Klassische Windows-Mausrad-Klick (Autoscroll) Funktion im eleganten KDE Plasma 6 / Breeze Design.**

---

## 🌟 Übersicht

Unter Windows ermöglicht ein Klick mit dem Mausrad (Mittelklick) das automatische Scrollen durch Bewegen der Maus. Unter Linux fehlt diese Funktion standardmäßig im gesamten Desktop, da der Mittelklick historisch für das Einfügen von markiertem Text verwendet wird.

**Breeze Autoscroll** bringt diese klassische Windows-Funktion systemweit nach **Kubuntu 26.04 LTS (KDE Plasma 6)**:
- 🎯 **Klick & Scroll**: Klicke mit dem Mausrad in beliebigen Anwendungen (Dateimanager Dolphin, Browser, Texteditoren wie Kate, LibreOffice, PDF-Betrachter, Terminal etc.).
- 🎨 **Kubuntu 26.04 Breeze Design**:
  - Transluzenter Glas-Hintergrund im Breeze-Dunkel- oder Breeze-Hell-Stil.
  - Vektorbasierte Richtungsfeile mit dynamischer Breeze-Ozeanblau-Hervorhebung (`#3daee9`) in Scrollrichtung.
  - Subtiler Ankerpunkt und Richtungsvektor.
  - Weiche Ein- und Ausblend-Animationen.
- ⚡ **Flüssiges 2D-Scrollen**: Sowohl vertikal (oben/unten) als auch horizontal (links/rechts).
- 🖱️ **Universelle Kompatibilität**: Nutzt Linux Kernel `uinput` – funktioniert nahtlos unter **Wayland** (KWin) und **X11**.
- ⚙️ **KDE System-Tray Integration**: Komfortables Applet in der Kontrollleiste mit Einstellungsdialog.

---

## 🚀 Schnellstart & Installation

Führe einfach das mitgelieferte Installationsskript aus:

```bash
cd ~/KI-Agentic-Projects/breeze-autoscroll
./install.sh
```

Das Skript erledigt automatisch:
1. Installation benötigter Abhängigkeiten (`python3-pyqt6`, `python3-evdev`)
2. Erstellung des Starters in `~/.local/bin/breeze-autoscroll`
3. Einrichtung von Desktop- und Autostart-Einträgen
4. Konfiguration der `uinput`-Berechtigungen via udev-Regel

---

## 📖 Bedienung

### 1. Autoscroll auslösen
- **Mausrad drücken**: Ein Klick mit dem Mausrad in einem scrollbaren Fenster zeigt den Breeze-Indikator am Klickpunkt an.
- **Maus bewegen**: 
  - Maus nach oben $\rightarrow$ Seite scrollt nach oben.
  - Maus nach unten $\rightarrow$ Seite scrollt nach unten.
  - Maus nach links/rechts $\rightarrow$ Seite scrollt horizontal.
  - Je weiter die Maus vom Mittelpunkt entfernt ist, desto schneller scrollt es.
- **Autoscroll beenden**:
  - Im **Hybrid-Modus (Standard)**: Klicke kurz zum Aktivieren und beende mit einem beliebigen Mausklick (Links-, Rechts- oder Mittelklick). Wenn du das Mausrad gedrückt hältst und ziehst, stoppt das Scrollen sofort beim Loslassen.

---

## ⚙️ Einstellungen & Anpassung

Öffne die Einstellungen über das System-Tray-Symbol in der KDE-Leiste oder über das Terminal:

```bash
breeze-autoscroll --settings
```

### Anpassungsoptionen:
- **Verhaltensmodus**:
  - *Intelligenter Hybrid-Modus* (Standard & empfohlen)
  - *Klick zum Umschalten (Toggle)*
  - *Gedrückt halten & Ziehen (Drag)*
- **Geschwindigkeit & Kurve**: Anpassbar von 20% bis 300% mit sanfter Beschleunigungskurve.
- **Totbereich (Deadzone)**: Neutraler Radius (Standard: 12 px), um ungewolltes Zittern bei ruhender Hand zu vermeiden.
- **2D-Scrollen**: Horizontales Scrollen kann nach Wunsch aktiviert oder deaktiviert werden.
- **Erscheinungsbild**: Farbschema umschaltbar zwischen *KDE Breeze Dunkel* und *KDE Breeze Hell*.

---

## 🛠️ Verwaltung & Befehle

| Aktion | Befehl |
|---|---|
| **Manuell starten** | `breeze-autoscroll` |
| **Einstellungen öffnen** | `breeze-autoscroll --settings` |
| **Hintergrunddienst aktivieren** | `systemctl --user enable --now breeze-autoscroll` |
| **Hintergrunddienst stoppen** | `systemctl --user stop breeze-autoscroll` |
| **Status prüfen** | `systemctl --user status breeze-autoscroll` |
| **Testlauf im Terminal** | `./test_autoscroll.sh` |
| **Deinstallation** | `./uninstall.sh` |

---

## 💡 Alternative: Browser-spezifisches Autoscrolling

Falls du Autoscroll ausschließlich in Webbrowsern benötigst:

### Firefox
1. Öffne die Einstellungen $\rightarrow$ **Allgemein** $\rightarrow$ Abschnitt **Surfen**.
2. Setze ein Häkchen bei **„Automatischen Bildlauf aktivieren“** (oder in `about:config` die Option `general.autoScroll` auf `true` setzen).

### Google Chrome / Chromium / Brave
Starte den Browser mit folgendem Parameter:
```bash
google-chrome --enable-blink-features=MiddleClickAutoscroll
```

---

## 🔒 Berechtigungen (`/dev/uinput`)

Damit der Dienst ohne Root-Rechte virtuelle Scroll-Events senden kann, muss dein Benutzer Zugriff auf `/dev/uinput` haben. Dies wird vom Installationsskript über `/etc/udev/rules.d/99-uinput-autoscroll.rules` und die Gruppe `input` automatisch eingerichtet.

Falls nach der Installation Berechtigungsfehler auftreten, melde dich einmal kurz ab und wieder an, damit die Gruppenrechte (`input`) aktiv werden.
