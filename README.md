# Breeze Autoscroll

Windows-Mittelklick-Autoscroll in Rust für Kubuntu 26.04 (KDE Plasma 6 / Wayland und X11) mit 3D-/CAD-Schutz.

> **Hinweis zur Entstehung (Vibecoding):** Dieses Programm ist durch Vibecoding (KI-gestützte Entwicklung) entstanden. Die Codebasis wurde KI-generiert und anschließend strukturiert auditiert, gehärtet sowie per Unit-Tests und Live-Prüfungen getestet. Die Nutzung erfolgt dennoch auf eigene Verantwortung; vor dem Einsatz auf produktiven Systemen sollte der Code nach eigenem Ermessen geprüft werden.

## Funktionen

Breeze Autoscroll bildet das klassische Windows-Autoscroll-Verhalten (Mausrad-Klick zum Panning) nativ unter Linux nach:

- **Zwei Bedienmodi (Einrast- und Halte-Modus):**
  - **Halte-Modus:** Mausrad drücken und den Zeiger direkt aus der Totzone ziehen. Das Scrollen startet unmittelbar und endet beim Loslassen.
  - **Einrast-Modus:** Mausrad kurz innerhalb der Totzone gedrückt halten (Standard: `180 ms`, konfigurierbar `100 ms` bis `400 ms`) und loslassen. Der Anker bleibt fixiert, bis abgebrochen wird.
  - In den Einstellungen lassen sich neben dem Standardmodus `smart` (Automatik) auch reine `toggle`- oder `drag`-Modi wählen.
- **2D-Hi-Res-Scrollen über `uinput`:** Feingranulare `REL_WHEEL_HI_RES`- und `REL_HWHEEL_HI_RES`-Events mit progressiver Geschwindigkeitskurve, konfigurierbarer Totzone (`4 px` bis `30 px`) und optionalem 4-Wege-Anker (inkl. Diagonal-Scrollen) oder rein vertikalem 2-Wege-Anker.
- **Systemweiter Kurz-Klick-Schutz:** Kurze, stationäre Mittelklicks unterhalb der Haltezeit (`< 180 ms` innerhalb der Totzone) werden unverändert an die jeweilige Anwendung durchgereicht. Tab-Schließen, Links/Ordner im neuen Tab öffnen und Mittelklick-Einfügen bleiben systemweit erhalten (z. B. in Firefox, Obsidian, VS Code, Dolphin und Nautilus).
- **Zusätzliche Erkennung interaktiver UI-Elemente:**
  - KWin-gestützte Erkennung von Fenstertitelleisten, Browser-Tableisten (konfigurierbar, Standard `95 px`), Dateimanager-Kopfzeilen (`86 px` Fallback) und rahmenlosen CSD-Headerbars (`48 px`).
  - Hyperlink-Cursor-Erkennung (`pointing_hand`) über die Linux-DRM-Hardware-Cursor-Plane (Wayland) bzw. XFixes (X11).
  - Optionaler AT-SPI2-Hit-Test für native Qt-/GTK-Anwendungen (z. B. Dolphin-Ordner, Tabs und Seitenleisten).
- **3D-, CAD- und Slicer-Schutz:** Erkennt das Fenster direkt unter dem Zeiger sowie aktive Browser-Tabs über KWin und X11 und deaktiviert Autoscroll automatisch in 3D-Programmen und 3D-Webanwendungen (u. a. Onshape, Bambu Studio, OrcaSlicer, PrusaSlicer, Cura, FreeCAD, Blender, KiCad, Tinkercad, Sketchfab, Godot, Unity, Unreal). Die Ausschlussliste ist frei konfigurierbar; zusätzlich gibt es einen optionalen strengen Orbit-Schutz (`protect_3d_drag`).
- **KDE-Plasma-6-Integration:** Klick-durchlässiger 32-Bit-ARGB-Overlay-Indikator (4x Supersampling, Breeze Dunkel/Hell), natives System-Tray-Applet (`org.kde.StatusNotifierItem` + `com.canonical.dbusmenu`) und ein eigenständiger Einstellungsdialog (`eframe`/`egui`).

## Voraussetzungen und Installation

### Voraussetzungen

- Kubuntu 26.04 / KDE Plasma 6 (Wayland oder X11)
- Rust-Toolchain mit `cargo` (MSRV laut `Cargo.toml`: Rust `1.95`, Edition `2024`)
- Laufzeitbibliotheken: `libc6`, `libgl1`, `libx11-6`, `libxcb1`, `libwayland-client0`, `libdbus-1-3`
- Für den `.deb`-Bau zusätzlich `dpkg-deb`

### Installation aus dem Quellcode (Standard)

`install.sh` baut das Release-Binary stets deterministisch aus dem Quellcode (`cargo build --release --locked`):

```bash
./install.sh
```

Das Installationsskript führt folgende Schritte aus:

1. Deterministischer Release-Build nach `target/release/breeze-autoscroll` und Installation nach `~/.local/bin/breeze-autoscroll`.
2. Installation des Anwendungs-Starters nach `~/.local/share/applications/breeze-autoscroll.desktop` (`0644`).
3. Installation der Systemd-User-Unit nach `${XDG_CONFIG_HOME:-~/.config}/systemd/user/breeze-autoscroll.service` (`0644`), Ausführung von `systemctl --user daemon-reload` und `systemctl --user enable breeze-autoscroll.service` (bewusst ohne `--now`) sowie Entfernen eventuell vorhandener Legacy-Autostart-Dateien unter `~/.config/autostart/breeze-autoscroll.desktop`.
4. Einrichtung der Least-Privilege-udev-Regel `/etc/udev/rules.d/70-uinput-autoscroll.rules`, Laden des `uinput`-Kernelmoduls und gezieltes `udevadm trigger` auf `uinput` und `ID_INPUT_MOUSE=1`.

Da `install.sh` den Dienst bewusst nur für künftige Anmeldungen aktiviert (`enable` ohne `--now`), muss der Hintergrunddienst nach der Erstinstallation einmalig gestartet werden:

```bash
systemctl --user start breeze-autoscroll.service
```

### Alternative: Debian-Paket (`.deb`) bauen und installieren

```bash
./build_deb.sh
sudo apt install ./breeze-autoscroll_2.3.0_amd64.deb
```

Das `.deb`-Paket installiert `/usr/bin/breeze-autoscroll`, `/usr/share/applications/breeze-autoscroll.desktop`, `/usr/lib/systemd/user/breeze-autoscroll.service`, `/etc/udev/rules.d/70-uinput-autoscroll.rules` und `/etc/modules-load.d/breeze-autoscroll-uinput.conf`. Anschließend für den eigenen Benutzer aktivieren und starten:

```bash
systemctl --user daemon-reload
systemctl --user enable --now breeze-autoscroll.service
```

### Migration von älteren Versionen (`input`-Gruppe entfernen)

Falls der Benutzer durch eine ältere Version noch Mitglied der Gruppe `input` ist, sollte diese Mitgliedschaft wieder entfernt werden (danach einmal ab- und wieder anmelden):

```bash
sudo gpasswd -d "$USER" input
```

## Bedienung, CLI-Optionen und Autostart

### CLI-Optionen

| Option | Beschreibung |
|---|---|
| `breeze-autoscroll` | Startet den Hintergrunddienst (prüft auf bereits laufende Instanz über `org.breeze.Autoscroll`). |
| `breeze-autoscroll -s` / `--settings` | Öffnet den Einstellungsdialog (aktiviert die laufende Instanz über D-Bus oder startet sie). |
| `breeze-autoscroll -h` / `--help` | Zeigt die Hilfe an. |
| `breeze-autoscroll -V` / `--version` | Gibt die Versionsnummer (`2.3.0`) aus. |

Die Benutzerkonfiguration liegt unter `$XDG_CONFIG_HOME/breeze-autoscroll/config.json` (Fallback: `~/.config/breeze-autoscroll/config.json`).

### Abbruch im Einrast-Modus und Hinweis zu `ESC`

Im aktiven Einrast-Modus beenden Linksklick, Rechtsklick, Mittelklick oder eine Drehung am physischen Mausrad das Autoscroll sofort. Unter reinem `uaccess` hat der Dienst absichtlich keinen Lesezugriff auf Tastaturen, weshalb `ESC` im Standardbetrieb nicht über `evdev` abgefangen wird. `ESC` als zusätzliche Abbruchtaste greift nur dann, wenn der Benutzer explizit Tastatur-Lesezugriff besitzt (z. B. durch manuelle `input`-Gruppenmitgliedschaft); Tastatur-Deskriptoren werden selbst in diesem Fall erst während eines aktiven Autoscrolls geöffnet und beim Beenden sofort wieder geschlossen.

### Eindeutige Single-Autostart-Regelung

Um Doppelstarts beim Login auszuschließen, dient `systemd --user` (`breeze-autoscroll.service`) als alleiniger Standard-Autostart-Mechanismus:

- Erkennt Breeze Autoscroll eine installierte User-Unit unter `$XDG_CONFIG_HOME/systemd/user/breeze-autoscroll.service`, `~/.config/systemd/user/breeze-autoscroll.service`, `/etc/systemd/user/breeze-autoscroll.service` oder `/usr/lib/systemd/user/breeze-autoscroll.service`, schaltet die Autostart-Option in den Einstellungen ausschließlich über `systemctl --user enable breeze-autoscroll.service` bzw. `systemctl --user disable breeze-autoscroll.service` (ohne `--now`, sodass der laufende Dienst nicht beendet wird) und entfernt eine eventuell vorhandene `~/.config/autostart/breeze-autoscroll.desktop`.
- Nur wenn keine Systemd-User-Unit existiert, greift ein Fallback über `~/.config/autostart/breeze-autoscroll.desktop` (atomar geschrieben via `write_file_atomic_secure(..., false)`, ohne `#!/usr/bin/env xdg-open`-Shebang und mit Freedesktop-konformem `Exec`-Escaping).
- Beim Öffnen des Einstellungsdialogs wird der reale Systemzustand (`systemctl --user is-enabled breeze-autoscroll.service` bzw. Vorhandensein der Fallback-`.desktop`-Datei) ausgelesen, ohne unnötig sofort auf die Platte zu schreiben.

## Sicherheitsarchitektur

### Eingabe- und Geräteberechtigungen

- **Least-Privilege via `udev` und `uaccess`:** `udev/70-uinput-autoscroll.rules` läuft mit Priorität `70` (nach `60-input-id.rules` und vor `73-seat-late.rules`) und vergibt `TAG+="uaccess"` ausschließlich für die aktive lokale Desktop-Sitzung:
  - `/dev/uinput` erhält `OWNER="root", GROUP="root", MODE="0660", TAG+="uaccess", OPTIONS+="static_node=uinput"`. Die Gruppen-Bits `0660` stellen sicher, dass die POSIX-ACL-Maske (`mask::rw-`) für den `uaccess`-Eintrag wirksam bleibt, ohne eine globale Benutzergruppe freizuschalten.
  - Eingabegeräte erhalten `TAG+="uaccess"` ausschließlich für reine Maus-Nodes (`SUBSYSTEM=="input", KERNEL=="event[0-9]*", ENV{ID_INPUT_MOUSE}=="1", ENV{ID_INPUT_KEYBOARD}!="1"`).
- **Keine `input`-Gruppe und keine Tastatur-FDs im Leerlauf:** Weder `install.sh` noch `build_deb.sh` fügen Benutzer zur Gruppe `input` hinzu. Im Leerlauf hält der Prozess keine Tastatur-Dateideskriptoren offen.
- **Minimale `uinput`-Capabilities:** Das virtuelle Scroll-Gerät registriert nur `EV_KEY` (`BTN_LEFT` für die `libinput`/`udev`-Erkennung als Zeigegerät; keine weiteren Tasten) sowie `EV_REL` (`REL_X`, `REL_Y`, `REL_WHEEL`, `REL_HWHEEL`, `REL_WHEEL_HI_RES`, `REL_HWHEEL_HI_RES`).
- **Ereignisgesteuertes Hotplug:** Änderungen in `/dev/input` werden über `inotify_init1(IN_NONBLOCK | IN_CLOEXEC)` statt periodischem Polling erkannt.

### D-Bus-, KWin- und Wayland-Isolation

- **KWin-Sender-Verifikation (`KWinOwnerVerifier`):** Alle KWin-Callback-Methoden auf `org.breeze.Autoscroll` (`UpdateActiveWindow`, `ReportPointerProbe`, `UpdateCursorPos`, `NotifyWindowContextChanged`) prüfen den D-Bus-Absender gegen den aktuellen Unique-Owner von `org.kde.KWin`. Fremde lokale Clients erhalten `org.freedesktop.DBus.Error.AccessDenied`. Eingehende Strings und Listen sind in der Länge begrenzt (u. a. 256 Bytes für Metadaten, 1024 Bytes für Fenstertitel, 64 IDs für `GetGroupProperties`, 128 Einträge für Ausschlusslisten).
- **Debug-Schnittstelle nicht im Standard-Build:** Die Diagnosemethoden `GetLastProbe` und `TriggerProbe` sind im Standard-Build nicht vorhanden und werden nur bei explizitem Build mit `--features debug-dbus` unter dem separaten Interface `org.breeze.Autoscroll.Debug` exportiert.
- **Einzelnes persistentes KWin-Skript:** Die KWin-Bridge schreibt genau ein persistentes Skript (`breeze_autoscroll_tracker`) atomar mit `O_CREAT | O_EXCL | O_NOFOLLOW | O_CLOEXEC` (Verzeichnis `0700`, Datei `0600`) nach `$XDG_RUNTIME_DIR/breeze-autoscroll/kwin_tracker.js` (Fallback ausschließlich `$HOME/.cache/breeze-autoscroll/`, kein `/tmp`-Fallback). Live-Probes werden über `org.kde.kglobalaccel` gesteuert, ohne wiederholtes `loadScript`/`unloadScript`.
- **AT-SPI2 strikt Opt-in:** `auto_enable_atspi` ist standardmäßig `false`; Barrierefreiheit wird nicht ungefragt sitzungsweit aktiviert. Der AT-SPI2-Worker läuft entkoppelt vom `zbus`-Dispatcher mit begrenztem PID-Cache (maximal 256 Einträge, 60 s TTL) und harten Timeouts (`35 ms` pro Call, `45 ms` Gesamtbudget).

### Systemd-Hardening, Prozess- und Build-Hygiene

- **Systemd-Unit (`breeze-autoscroll.service`):**
  - `[Unit]`: `After=graphical-session.target plasma-workspace.target`, `PartOf=graphical-session.target`, `StartLimitIntervalSec=30`, `StartLimitBurst=5`.
  - `[Service]`: `NoNewPrivileges=yes`, `RestrictAddressFamilies=AF_UNIX`, `LockPersonality=yes`, `RestrictRealtime=yes`, `RestrictSUIDSGID=yes`, `MemoryHigh=128M`, `MemoryMax=512M`, `TasksMax=96`, `LimitNOFILE=512`, `TimeoutStopSec=5`.
  - Bewusst **kein** `PrivateTmp=yes`, damit `/tmp/.X11-unix` für das XWayland-Overlay erreichbar bleibt. `MemoryMax=512M` und `TasksMax=96` berücksichtigen das Kindprozess-Einstellungsfenster (`eframe`/`glow`) in derselben Cgroup.
- **Login- und Restart-Resilienz:** Sowohl die `StatusNotifierWatcher`-Registrierung (`src/tray.rs`) als auch das Laden des persistenten KWin-Skripts (`src/window_detector.rs`) arbeiten mit begrenztem Backoff-Retry (`250 ms`, `500 ms`, `1 s`, `2 s`, `5 s`) und beobachten `NameOwnerChanged` für `org.kde.StatusNotifierWatcher` bzw. `org.kde.KWin`. `plasmashell`- und KWin-Neustarts werden automatisch überstanden; beim KWin-Shutdown verhindert ein 500-ms-Timeout Blockaden beim Logout/Reboot.
- **Prozess- und Dateisystem-Hygiene:** Signalbehandlung über `sigaction` und `pipe2(O_CLOEXEC | O_NONBLOCK)` (0 Idle-Wakeups, unmittelbares Reaping von `SIGCHLD`-Kindprozessen ohne Zombies). `config.json` wird atomar mit `0600` und `O_NOFOLLOW` geschrieben (`write_file_atomic_secure`).
- **Build- und Skript-Hygiene:** `[profile.release]` aktiviert `overflow-checks = true`, `x11rb` wird ohne `allow-unsafe-code` eingebunden, alle Shell-Skripte nutzen `set -euo pipefail`, `pkill -u "$(id -u)"` und deterministische `--locked`-Builds. Das Git-Repository enthält keine vorkompilierten Binaries oder `.deb`-Artefakte.

## Lokale Sicherheitsprüfung und Fehlerbehebung

### Verifikationsbefehle

1. **ACL auf `/dev/uinput` prüfen** (erwartet `user:<benutzer>:rw-` und `mask::rw-` bei `root:root`):
   ```bash
   getfacl /dev/uinput
   ```
2. **Gruppenmitgliedschaft prüfen** (`input` sollte nicht enthalten sein):
   ```bash
   id -nG "$USER"
   ```
3. **Offene Eingabe-Dateideskriptoren des Daemons prüfen** (erwartet `/dev/uinput` und Maus-`event*`, keine Tastaturen im Leerlauf):
   ```bash
   lsof -p "$(pgrep -u "$(id -u)" -x breeze-autoscro)" 2>/dev/null | grep -E '/dev/(uinput|input/)'
   ```
4. **Rechte des KWin-Skript-Verzeichnisses prüfen** (erwartet `drwx------` / `0700` und `-rw-------` / `0600`):
   ```bash
   ls -la "$XDG_RUNTIME_DIR/breeze-autoscroll"
   ```
5. **D-Bus-Zugriffskontrolle (`KWinOwnerVerifier`) testen** (erwartet `org.freedesktop.DBus.Error.AccessDenied`):
   ```bash
   gdbus call --session \
     --dest org.breeze.Autoscroll \
     --object-path /Autoscroll \
     --method org.breeze.Autoscroll.UpdateActiveWindow \
     "test" "test" "test" "test" "0" "0"
   ```
6. **Systemd-Dienststatus und Logs prüfen:**
   ```bash
   systemctl --user status breeze-autoscroll.service
   journalctl --user -u breeze-autoscroll.service -n 50 --no-pager
   ```

### Fehlerbehebung bei Kombi-Mäusen (`ID_INPUT_MOUSE=1` und `ID_INPUT_KEYBOARD=1`)

Manche Gaming- oder Makro-Mäuse melden auf demselben `/dev/input/event*`-Node sowohl `ID_INPUT_MOUSE=1` als auch `ID_INPUT_KEYBOARD=1`. Da `udev/70-uinput-autoscroll.rules` aus Sicherheitsgründen gezielt `ENV{ID_INPUT_KEYBOARD}!="1"` verlangt, erhält ein solcher Kombi-Node standardmäßig kein `uaccess` und der Dienst erkennt die Maus nicht.

Mit folgendem Befehl lassen sich die Eigenschaften aller `event*`-Nodes prüfen:

```bash
for dev in /dev/input/event*; do
  echo "=== $dev ==="
  udevadm info --query=property --name="$dev" | grep -E '^(ID_MODEL|ID_VENDOR_ID|ID_MODEL_ID|ID_INPUT_MOUSE|ID_INPUT_KEYBOARD)='
done
```

Wenn die verwendete Maus tatsächlich beide Flags auf demselben Node setzt, kann entweder eine gezielte lokale udev-Regel unter `/etc/udev/rules.d/71-breeze-autoscroll-mouse.rules` für genau diese Vendor-/Product-ID (`ATTRS{idVendor}=="..."`, `ATTRS{idProduct}=="..."`, `TAG+="uaccess"`) angelegt werden, oder (weniger restriktiv) der eigene Benutzer manuell zur Gruppe `input` hinzugefügt werden (`sudo usermod -aG input "$USER"`, anschließend neu anmelden).

## Entwicklung, Tests und Deinstallation

### Tests, Linting und Build

```bash
# Unit-Tests (Standard und mit optionalem Debug-D-Bus-Interface)
cargo test --locked
cargo test --locked --features debug-dbus

# Clippy ohne Warnungen
cargo clippy --all-targets -- -D warnings
cargo clippy --all-targets --features debug-dbus -- -D warnings

# Release-Build
cargo build --release --locked

# Kombinierter Test- und Vordergrundlauf im Terminal
./test_autoscroll.sh
```

### Deinstallation

```bash
./uninstall.sh
```

`uninstall.sh` stoppt und deaktiviert `breeze-autoscroll.service`, beendet laufende Instanzen des aktuellen Benutzers, entfernt Binary, Desktop-Datei, Systemd-Unit sowie Runtime-/Cache-Verzeichnisse und löscht optional die udev-Regel unter `/etc/udev/rules.d/`. Bei Installation über das `.deb`-Paket erfolgt die Entfernung regulär über `sudo apt remove breeze-autoscroll`.

## Lizenz

`GPL-3.0-or-later` (siehe `Cargo.toml`).
