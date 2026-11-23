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
use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};

use config::ConfigManager;
use indicator::IndicatorHandle;
use input_manager::InputEngine;
use tray::QuitNotifier;
use window_detector::WindowTracker;

static SIGNAL_QUIT: AtomicBool = AtomicBool::new(false);
static SIGNAL_WAKE_WRITE_FD: AtomicI32 = AtomicI32::new(-1);

extern "C" fn handle_quit_signal(_sig: libc::c_int) {
    SIGNAL_QUIT.store(true, Ordering::SeqCst);
    let fd = SIGNAL_WAKE_WRITE_FD.load(Ordering::Acquire);
    if fd >= 0 {
        let byte = [b'Q'];
        unsafe {
            let _ = libc::write(fd, byte.as_ptr().cast::<libc::c_void>(), 1);
        }
    }
}

extern "C" fn handle_sigchld(_sig: libc::c_int) {
    let fd = SIGNAL_WAKE_WRITE_FD.load(Ordering::Acquire);
    if fd >= 0 {
        let byte = [b'C'];
        unsafe {
            let _ = libc::write(fd, byte.as_ptr().cast::<libc::c_void>(), 1);
        }
    }
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

fn create_signal_pipe() -> io::Result<(File, File)> {
    let mut fds = [-1i32; 2];
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    let read_end = unsafe { File::from_raw_fd(fds[0]) };
    let write_end = unsafe { File::from_raw_fd(fds[1]) };
    Ok((read_end, write_end))
}

fn install_sigaction(
    sig: libc::c_int,
    handler: extern "C" fn(libc::c_int),
    flags: libc::c_int,
) -> io::Result<()> {
    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = handler as *const () as usize;
        sa.sa_flags = flags;
        libc::sigemptyset(&mut sa.sa_mask);
        if libc::sigaction(sig, &sa, std::ptr::null_mut()) < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

fn install_signal_handlers() -> io::Result<()> {
    install_sigaction(libc::SIGINT, handle_quit_signal, libc::SA_RESTART)?;
    install_sigaction(libc::SIGTERM, handle_quit_signal, libc::SA_RESTART)?;
    install_sigaction(
        libc::SIGCHLD,
        handle_sigchld,
        libc::SA_RESTART | libc::SA_NOCLDSTOP,
    )?;
    Ok(())
}

#[derive(Debug, Default, PartialEq, Eq)]
struct WakeEvents {
    quit: bool,
    child_exited: bool,
}

fn drain_signal_pipe(read_fd: RawFd) -> WakeEvents {
    let mut events = WakeEvents::default();
    let mut buf = [0u8; 64];
    loop {
        let n = unsafe { libc::read(read_fd, buf.as_mut_ptr().cast::<libc::c_void>(), buf.len()) };
        if n < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            break;
        }
        if n == 0 {
            break;
        }
        for &b in &buf[..n as usize] {
            match b {
                b'Q' => events.quit = true,
                b'C' => events.child_exited = true,
                _ => {}
            }
        }
    }
    events
}

struct WakeFdResetGuard {
    tray_wake_fd: Arc<AtomicI32>,
}

impl Drop for WakeFdResetGuard {
    fn drop(&mut self) {
        SIGNAL_WAKE_WRITE_FD.store(-1, Ordering::Release);
        self.tray_wake_fd.store(-1, Ordering::Release);
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

    let config_mgr = match ConfigManager::try_new(None) {
        Ok(mgr) => mgr,
        Err(err) => {
            eprintln!("[Breeze Autoscroll] Konfigurationsfehler: {err}");
            return ExitCode::FAILURE;
        }
    };

    let (wake_read, wake_write) = match create_signal_pipe() {
        Ok(pair) => pair,
        Err(err) => {
            eprintln!("[Breeze Autoscroll] Konnte Signal-Pipe nicht anlegen: {err}");
            return ExitCode::FAILURE;
        }
    };

    let wake_write_raw = wake_write.as_raw_fd();
    let tray_wake_fd = Arc::new(AtomicI32::new(wake_write_raw));
    SIGNAL_QUIT.store(false, Ordering::SeqCst);
    SIGNAL_WAKE_WRITE_FD.store(wake_write_raw, Ordering::Release);

    // Ensure global/shared write FDs are cleared BEFORE `wake_write` is dropped on any exit path.
    let _wake_reset_guard = WakeFdResetGuard {
        tray_wake_fd: Arc::clone(&tray_wake_fd),
    };

    if let Err(err) = install_signal_handlers() {
        eprintln!("[Breeze Autoscroll] Signal-Handler Fehler: {err}");
        return ExitCode::FAILURE;
    }

    let window_tracker = WindowTracker::new();
    let quit_requested = Arc::new(AtomicBool::new(false));
    let quit_notifier = QuitNotifier::new(Arc::clone(&quit_requested), Arc::clone(&tray_wake_fd));

    let tray_service = match tray::start_tray_and_dbus_service(
        config_mgr.clone(),
        window_tracker.clone(),
        quit_notifier,
    ) {
        Ok(service) => service,
        Err(err) => {
            eprintln!("[Breeze Autoscroll] D-Bus Fehler: {err}");
            return ExitCode::FAILURE;
        }
    };

    // Start single persistent KWin Wayland window & pointer bridge + opt-in AT-SPI2 inspector
    window_tracker.start_kwin_bridge(config_mgr.clone());

    // Start translucent X11/XWayland Breeze indicator overlay thread (bounded channel + auto-reconnect)
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

    if open_settings {
        tray_service.open_settings();
    }

    if input_engine.is_none() {
        tray_service.reap_exited_children();
        indicator_handle.stop();
        window_tracker.stop_kwin_bridge();
        return ExitCode::FAILURE;
    }

    println!("[InputManager] Hintergrunddienst für Autoscroll erfolgreich gestartet.");

    // Pure event-driven wait (0 idle wakeups): wakes instantaneously on SIGINT, SIGTERM,
    // SIGCHLD (for immediate child reaping), or Tray "Beenden".
    let mut pfd = libc::pollfd {
        fd: wake_read.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };

    while !quit_requested.load(Ordering::SeqCst) && !SIGNAL_QUIT.load(Ordering::SeqCst) {
        pfd.revents = 0;
        let rc = unsafe { libc::poll(&mut pfd, 1, -1) };
        if rc < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            break;
        }
        if rc > 0 && (pfd.revents & libc::POLLIN) != 0 {
            let events = drain_signal_pipe(wake_read.as_raw_fd());
            if events.child_exited {
                tray_service.reap_exited_children();
            }
            if events.quit {
                break;
            }
        }
    }

    println!("\n[Breeze Autoscroll] Beende Anwendung...");
    if let Some(mut engine) = input_engine.take() {
        engine.stop();
    }
    indicator_handle.stop();
    window_tracker.stop_kwin_bridge();
    tray_service.reap_exited_children();
    drop(_wake_reset_guard);
    drop(wake_write);
    drop(wake_read);
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
            let config_mgr = match ConfigManager::try_new(None) {
                Ok(mgr) => mgr,
                Err(err) => {
                    eprintln!("[Settings UI] Konfigurationsfehler: {err}");
                    return ExitCode::FAILURE;
                }
            };
            match settings_ui::run_settings_window(config_mgr) {
                Ok(()) => ExitCode::SUCCESS,
                Err(err) => {
                    eprintln!("[Settings UI] Fehler: {err}");
                    ExitCode::FAILURE
                }
            }
        }
        CliAction::AboutWindow => {
            let config_mgr = match ConfigManager::try_new(None) {
                Ok(mgr) => mgr,
                Err(err) => {
                    eprintln!("[About UI] Konfigurationsfehler: {err}");
                    return ExitCode::FAILURE;
                }
            };
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

    #[test]
    fn signal_pipe_enforces_cloexec_nonblock_and_decodes_wake_events() {
        let (read_end, write_end) = create_signal_pipe().unwrap();
        for f in [&read_end, &write_end] {
            let fd_flags = unsafe { libc::fcntl(f.as_raw_fd(), libc::F_GETFD) };
            assert_ne!(fd_flags & libc::FD_CLOEXEC, 0);
            let fl_flags = unsafe { libc::fcntl(f.as_raw_fd(), libc::F_GETFL) };
            assert_ne!(fl_flags & libc::O_NONBLOCK, 0);
        }

        // Write SIGCHLD marker ('C')
        let c = [b'C'];
        unsafe {
            assert_eq!(
                libc::write(write_end.as_raw_fd(), c.as_ptr().cast::<libc::c_void>(), 1),
                1
            );
        }
        assert_eq!(
            drain_signal_pipe(read_end.as_raw_fd()),
            WakeEvents {
                quit: false,
                child_exited: true,
            }
        );

        // QuitNotifier writes 'Q' and sets requested flag
        let requested = Arc::new(AtomicBool::new(false));
        let notifier = QuitNotifier::new(
            Arc::clone(&requested),
            Arc::new(AtomicI32::new(write_end.as_raw_fd())),
        );
        notifier.request_quit();
        assert!(requested.load(Ordering::SeqCst));
        assert_eq!(
            drain_signal_pipe(read_end.as_raw_fd()),
            WakeEvents {
                quit: true,
                child_exited: false,
            }
        );
    }
}
