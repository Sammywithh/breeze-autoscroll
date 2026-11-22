//! Breeze Autoscroll - Main Entry Point
//!
//! Windows-style middle-click autoscroll for Kubuntu 26.04 (KDE Plasma 6 / Breeze)
//! with two-layer 3D/CAD orbit protection (Onshape, Bambu Studio, FreeCAD, Blender, etc.).

mod atspi_inspector;
mod config;
mod indicator;
mod input_manager;
mod settings_ui;
mod tray;
mod window_detector;

use std::env;
use std::process::{Command, ExitCode};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use config::ConfigManager;
use indicator::IndicatorHandle;
use input_manager::InputEngine;
use window_detector::WindowTracker;

static SIGNAL_QUIT: AtomicBool = AtomicBool::new(false);

extern "C" fn handle_unix_signal(_sig: libc::c_int) {
    SIGNAL_QUIT.store(true, Ordering::SeqCst);
}

#[derive(Debug, PartialEq, Eq)]
enum CliAction {
    RunDaemon { open_settings: bool },
    SettingsWindow,
    AboutWindow,
    Help,
    Version,
}

fn parse_cli_args<I, S>(args: I) -> Result<CliAction, String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut open_settings = false;
    let mut settings_window = false;
    let mut about_window = false;

    for arg in args.into_iter().skip(1) {
        match arg.as_ref() {
            "-s" | "--settings" => open_settings = true,
            "--settings-window" => settings_window = true,
            "--about" => about_window = true,
            "-h" | "--help" => return Ok(CliAction::Help),
            "-V" | "--version" => return Ok(CliAction::Version),
            other => {
                return Err(format!(
                    "Unbekannte Option: {other}\nNutze --help für verfügbare Optionen."
                ));
            }
        }
    }

    if settings_window {
        Ok(CliAction::SettingsWindow)
    } else if about_window {
        Ok(CliAction::AboutWindow)
    } else {
        Ok(CliAction::RunDaemon { open_settings })
    }
}

fn print_help() {
    println!(
        concat!(
            "Breeze Autoscroll v{} — Klassisches Mittelklick-Autoscroll für KDE Plasma 6\n\n",
            "VERWENDUNG:\n",
            "  breeze-autoscroll [OPTIONEN]\n\n",
            "OPTIONEN:\n",
            "  -s, --settings    Einstellungsdialog öffnen (aktiviert laufende Instanz)\n",
            "  -h, --help        Diese Hilfe anzeigen\n",
            "  -V, --version     Versionsnummer anzeigen"
        ),
        env!("CARGO_PKG_VERSION")
    );
}

fn install_signal_handlers() {
    unsafe {
        libc::signal(
            libc::SIGINT,
            handle_unix_signal as *const () as libc::sighandler_t,
        );
        libc::signal(
            libc::SIGTERM,
            handle_unix_signal as *const () as libc::sighandler_t,
        );
    }
}

fn run_daemon(open_settings: bool) -> ExitCode {
    if tray::try_activate_existing_instance(open_settings) {
        if open_settings {
            println!("[Breeze Autoscroll] Eine Instanz läuft bereits. Öffne Einstellungen...");
        } else {
            println!("[Breeze Autoscroll] Breeze Autoscroll läuft bereits im Hintergrund.");
        }
        return ExitCode::SUCCESS;
    }

    install_signal_handlers();

    let config_mgr = ConfigManager::new(None);
    let window_tracker = WindowTracker::new();
    let quit_requested = Arc::new(AtomicBool::new(false));

    let _dbus_conn = match tray::start_tray_and_dbus_service(
        config_mgr.clone(),
        window_tracker.clone(),
        Arc::clone(&quit_requested),
    ) {
        Ok(conn) => conn,
        Err(err) => {
            eprintln!("[Breeze Autoscroll] D-Bus Fehler: {err}");
            return ExitCode::FAILURE;
        }
    };

    // Start KWin Wayland window & pointer bridge
    window_tracker.start_kwin_bridge();

    // Start translucent X11/XWayland Breeze indicator overlay thread
    let (mut indicator_handle, indicator_tx) = IndicatorHandle::spawn();

    // Start evdev/uinput autoscroll engine
    let mut input_engine =
        match InputEngine::start(config_mgr.clone(), window_tracker.clone(), indicator_tx) {
            Ok(engine) => Some(engine),
            Err(err) => {
                eprintln!("[InputManager] {err}");
                None
            }
        };

    if open_settings && let Ok(exe) = env::current_exe() {
        let _ = Command::new(exe).arg("--settings-window").spawn();
    }

    if input_engine.is_none() {
        indicator_handle.stop();
        window_tracker.stop_kwin_bridge();
        return ExitCode::FAILURE;
    }

    println!("[InputManager] Hintergrunddienst für Autoscroll erfolgreich gestartet.");

    while !quit_requested.load(Ordering::SeqCst) && !SIGNAL_QUIT.load(Ordering::SeqCst) {
        thread::sleep(Duration::from_millis(100));
    }

    println!("\n[Breeze Autoscroll] Beende Anwendung...");
    if let Some(mut engine) = input_engine.take() {
        engine.stop();
    }
    indicator_handle.stop();
    window_tracker.stop_kwin_bridge();
    ExitCode::SUCCESS
}

fn main() -> ExitCode {
    let action = match parse_cli_args(env::args()) {
        Ok(action) => action,
        Err(err) => {
            eprintln!("{err}");
            return ExitCode::from(2);
        }
    };

    match action {
        CliAction::Help => {
            print_help();
            ExitCode::SUCCESS
        }
        CliAction::Version => {
            println!("breeze-autoscroll {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        CliAction::SettingsWindow => {
            let config_mgr = ConfigManager::new(None);
            match settings_ui::run_settings_window(config_mgr) {
                Ok(()) => ExitCode::SUCCESS,
                Err(err) => {
                    eprintln!("[Settings UI] Fehler: {err}");
                    ExitCode::FAILURE
                }
            }
        }
        CliAction::AboutWindow => {
            let config_mgr = ConfigManager::new(None);
            match settings_ui::run_about_window(config_mgr) {
                Ok(()) => ExitCode::SUCCESS,
                Err(err) => {
                    eprintln!("[About UI] Fehler: {err}");
                    ExitCode::FAILURE
                }
            }
        }
        CliAction::RunDaemon { open_settings } => run_daemon(open_settings),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_cli_flags_accurately() {
        assert_eq!(
            parse_cli_args(["breeze-autoscroll"]).unwrap(),
            CliAction::RunDaemon {
                open_settings: false
            }
        );
        assert_eq!(
            parse_cli_args(["breeze-autoscroll", "--settings"]).unwrap(),
            CliAction::RunDaemon {
                open_settings: true
            }
        );
        assert_eq!(
            parse_cli_args(["breeze-autoscroll", "-s"]).unwrap(),
            CliAction::RunDaemon {
                open_settings: true
            }
        );
        assert_eq!(
            parse_cli_args(["breeze-autoscroll", "--settings-window"]).unwrap(),
            CliAction::SettingsWindow
        );
        assert_eq!(
            parse_cli_args(["breeze-autoscroll", "--about"]).unwrap(),
            CliAction::AboutWindow
        );
        assert_eq!(
            parse_cli_args(["breeze-autoscroll", "--help"]).unwrap(),
            CliAction::Help
        );
        assert_eq!(
            parse_cli_args(["breeze-autoscroll", "--version"]).unwrap(),
            CliAction::Version
        );
        assert!(parse_cli_args(["breeze-autoscroll", "--unknown"]).is_err());
    }
}
