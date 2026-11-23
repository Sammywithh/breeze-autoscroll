//! Low-level Linux `evdev` mouse/keyboard listener, `uinput` virtual wheel emitter,
//! and 3D-orbit-protected autoscroll state machine.

use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::SyncSender;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::config::{ConfigData, ConfigManager};
use crate::window_detector::{WindowTracker, is_wayland_session};

// ============================================================================
// Linux Kernel evdev / uinput ABI constants (x86_64 / generic Linux)
// ============================================================================

const EV_SYN: u16 = 0x00;
const EV_KEY: u16 = 0x01;
const EV_REL: u16 = 0x02;

const SYN_REPORT: u16 = 0;

const KEY_ESC: u16 = 1;
const BTN_LEFT: u16 = 0x110;
const BTN_RIGHT: u16 = 0x111;
const BTN_MIDDLE: u16 = 0x112;
const BTN_SIDE: u16 = 0x113;
const BTN_EXTRA: u16 = 0x114;

const REL_X: u16 = 0x00;
const REL_Y: u16 = 0x01;
const REL_HWHEEL: u16 = 0x06;
const REL_WHEEL: u16 = 0x08;
const REL_WHEEL_HI_RES: u16 = 0x0b;
const REL_HWHEEL_HI_RES: u16 = 0x0c;

const BUS_USB: u16 = 0x03;
const UINPUT_MAX_NAME_SIZE: usize = 80;

const IOC_NRBITS: u32 = 8;
const IOC_TYPEBITS: u32 = 8;
const IOC_SIZEBITS: u32 = 14;
const IOC_NRSHIFT: u32 = 0;
const IOC_TYPESHIFT: u32 = IOC_NRSHIFT + IOC_NRBITS;
const IOC_SIZESHIFT: u32 = IOC_TYPESHIFT + IOC_TYPEBITS;
const IOC_DIRSHIFT: u32 = IOC_SIZESHIFT + IOC_SIZEBITS;
const IOC_NONE: u32 = 0;
const IOC_WRITE: u32 = 1;
const IOC_READ: u32 = 2;

const fn ioc(dir: u32, ty: u8, nr: u8, size: usize) -> libc::c_ulong {
    (((dir as libc::c_ulong) << IOC_DIRSHIFT)
        | ((ty as libc::c_ulong) << IOC_TYPESHIFT)
        | ((nr as libc::c_ulong) << IOC_NRSHIFT)
        | ((size as libc::c_ulong) << IOC_SIZESHIFT)) as libc::c_ulong
}

const fn io(ty: u8, nr: u8) -> libc::c_ulong {
    ioc(IOC_NONE, ty, nr, 0)
}

const fn iow<T>(ty: u8, nr: u8) -> libc::c_ulong {
    ioc(IOC_WRITE, ty, nr, std::mem::size_of::<T>())
}

const fn ior_len(ty: u8, nr: u8, len: usize) -> libc::c_ulong {
    ioc(IOC_READ, ty, nr, len)
}

const UI_DEV_CREATE: libc::c_ulong = io(b'U', 1);
const UI_DEV_DESTROY: libc::c_ulong = io(b'U', 2);
const UI_DEV_SETUP: libc::c_ulong = iow::<UInputSetup>(b'U', 3);
const UI_SET_EVBIT: libc::c_ulong = iow::<libc::c_int>(b'U', 100);
const UI_SET_KEYBIT: libc::c_ulong = iow::<libc::c_int>(b'U', 101);
const UI_SET_RELBIT: libc::c_ulong = iow::<libc::c_int>(b'U', 102);

const fn eviocgname(len: usize) -> libc::c_ulong {
    ior_len(b'E', 0x06, len)
}

const fn eviocgbit(ev: u8, len: usize) -> libc::c_ulong {
    ior_len(b'E', 0x20 + ev, len)
}

#[repr(C)]
#[derive(Clone, Copy)]
struct InputEvent {
    time: libc::timeval,
    type_: u16,
    code: u16,
    value: i32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct InputId {
    bustype: u16,
    vendor: u16,
    product: u16,
    version: u16,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct UInputSetup {
    id: InputId,
    name: [libc::c_char; UINPUT_MAX_NAME_SIZE],
    ff_effects_max: u32,
}

// ============================================================================
// UI / Overlay Events & Wheel Output Abstraction
// ============================================================================

/// Events emitted by the scroll engine for the translucent overlay indicator.
#[derive(Debug, Clone, PartialEq)]
pub enum IndicatorCommand {
    Show {
        anchor_x: i32,
        anchor_y: i32,
        deadzone: u32,
        theme: String,
        opacity: f32,
        allow_horizontal: bool,
    },
    UpdateOffset {
        delta_x: f32,
        delta_y: f32,
    },
    Hide,
    Shutdown,
}

/// Abstracts virtual wheel emission so the state machine is 100% unit-testable.
pub trait WheelEmitter {
    fn emit_wheel(&mut self, vertical: bool, hires_delta: i32, std_delta: i32);
}

struct UInputEmitter {
    file: File,
}

impl UInputEmitter {
    fn create() -> Result<Self, String> {
        let uinput_path = Path::new("/dev/uinput");
        if !uinput_path.exists() {
            return Err(
                "/dev/uinput nicht gefunden. Kernel-Modul uinput laden: sudo modprobe uinput"
                    .to_string(),
            );
        }

        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_CLOEXEC)
            .open(uinput_path)
            .map_err(|err| {
                if err.kind() == io::ErrorKind::PermissionDenied {
                    "Keine Schreibrechte auf /dev/uinput. Bitte 'install.sh' ausführen, um die gehärtete udev-Regel (70-uinput-autoscroll.rules mit TAG+=\"uaccess\") zu aktivieren.".to_string()
                } else {
                    format!("Fehler beim Öffnen von /dev/uinput: {err}")
                }
            })?;

        let fd = file.as_raw_fd();

        unsafe {
            // Minimal capability set recognized as a pointer/wheel device by libinput/KWin:
            // BTN_LEFT is required by udev input_id/libinput for ID_INPUT_MOUSE classification;
            // BTN_RIGHT, BTN_MIDDLE, BTN_SIDE, BTN_EXTRA are intentionally omitted.
            ioctl_check(fd, UI_SET_EVBIT, EV_KEY as libc::c_int, "UI_SET_EVBIT(EV_KEY)")?;
            ioctl_check(fd, UI_SET_EVBIT, EV_REL as libc::c_int, "UI_SET_EVBIT(EV_REL)")?;

            ioctl_check(
                fd,
                UI_SET_KEYBIT,
                BTN_LEFT as libc::c_int,
                "UI_SET_KEYBIT(BTN_LEFT)",
            )?;
            for rel in [
                REL_X,
                REL_Y,
                REL_WHEEL,
                REL_HWHEEL,
                REL_WHEEL_HI_RES,
                REL_HWHEEL_HI_RES,
            ] {
                ioctl_check(fd, UI_SET_RELBIT, rel as libc::c_int, "UI_SET_RELBIT")?;
            }

            let mut setup = UInputSetup {
                id: InputId {
                    bustype: BUS_USB,
                    vendor: 0x1d6b,
                    product: 0x0104,
                    version: 2,
                },
                name: [0; UINPUT_MAX_NAME_SIZE],
                ff_effects_max: 0,
            };
            let dev_name = b"Breeze Autoscroll Virtual Mouse";
            for (dst, &src) in setup.name.iter_mut().zip(dev_name.iter()) {
                *dst = src as libc::c_char;
            }

            if libc::ioctl(fd, UI_DEV_SETUP, &setup) < 0 {
                let err = io::Error::last_os_error();
                return Err(format!("UI_DEV_SETUP fehlgeschlagen: {err}"));
            }
            if libc::ioctl(fd, UI_DEV_CREATE) < 0 {
                let err = io::Error::last_os_error();
                return Err(format!("UI_DEV_CREATE fehlgeschlagen: {err}"));
            }
        }

        Ok(Self { file })
    }

    fn write_event(&self, type_: u16, code: u16, value: i32) -> io::Result<()> {
        let ev = InputEvent {
            time: libc::timeval {
                tv_sec: 0,
                tv_usec: 0,
            },
            type_,
            code,
            value,
        };
        let bytes = unsafe {
            std::slice::from_raw_parts(
                (&ev as *const InputEvent).cast::<u8>(),
                std::mem::size_of::<InputEvent>(),
            )
        };
        write_all_retry_eintr(self.file.as_raw_fd(), bytes)
    }
}

fn write_all_retry_eintr(fd: RawFd, mut buf: &[u8]) -> io::Result<()> {
    while !buf.is_empty() {
        let written = unsafe { libc::write(fd, buf.as_ptr().cast::<libc::c_void>(), buf.len()) };
        if written < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(err);
        }
        if written == 0 {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "uinput write returned 0 bytes",
            ));
        }
        buf = &buf[written as usize..];
    }
    Ok(())
}

impl Drop for UInputEmitter {
    fn drop(&mut self) {
        unsafe {
            let _ = libc::ioctl(self.file.as_raw_fd(), UI_DEV_DESTROY);
        }
    }
}

impl WheelEmitter for UInputEmitter {
    fn emit_wheel(&mut self, vertical: bool, hires_delta: i32, std_delta: i32) {
        if hires_delta == 0 && std_delta == 0 {
            return;
        }
        let (std_code, hires_code) = if vertical {
            (REL_WHEEL, REL_WHEEL_HI_RES)
        } else {
            (REL_HWHEEL, REL_HWHEEL_HI_RES)
        };

        if std_delta != 0
            && let Err(err) = self.write_event(EV_REL, std_code, std_delta)
        {
            eprintln!("[InputManager] Fehler beim Schreiben von REL_WHEEL/HWHEEL: {err}");
            return;
        }
        if hires_delta != 0
            && let Err(err) = self.write_event(EV_REL, hires_code, hires_delta)
        {
            eprintln!("[InputManager] Fehler beim Schreiben von HI_RES Wheel: {err}");
            return;
        }
        if let Err(err) = self.write_event(EV_SYN, SYN_REPORT, 0) {
            eprintln!("[InputManager] Fehler beim Schreiben von SYN_REPORT: {err}");
        }
    }
}

unsafe fn ioctl_check(
    fd: RawFd,
    req: libc::c_ulong,
    arg: libc::c_int,
    label: &str,
) -> Result<(), String> {
    if unsafe { libc::ioctl(fd, req, arg) } < 0 {
        let err = io::Error::last_os_error();
        return Err(format!("{label} fehlgeschlagen: {err}"));
    }
    Ok(())
}

// ============================================================================
// Pure, Deterministic Scroll State Machine & 3D-Orbit Drag Protection
// ============================================================================

/// High-resolution wheel quantum emitted per smooth tick step (120 = 1 standard notch).
const HI_RES_STEP_QUANTUM: i32 = 15;
const HI_RES_PER_NOTCH: i32 = 120;

/// Maximum click duration in dedicated `"toggle"` mode if `protect_3d_drag` is enabled.
const TOGGLE_CLICK_MAX_MS: u64 = 300;
/// Ignore accidental physical wheel jitter immediately after BTN_MIDDLE press (wobbly wheel click).
const WHEEL_CANCEL_GRACE_MS: u64 = 150;
/// Maximum window in milliseconds after `BTN_MIDDLE` press during which an un-scrolled
/// tab close / page navigation (`captionChanged` / `windowActivated`) auto-cancels the anchor.
const CONTEXT_CHANGE_CANCEL_WINDOW_MS: u64 = 450;

#[derive(Debug, Clone, PartialEq)]
enum ScrollPhase {
    Idle,
    /// Short-click passthrough / optional 3D drag guard phase:
    /// - Releasing before `hold_delay_ms` inside deadzone passes through as a normal middle-click
    ///   (closing tabs, opening links/folders, primary-selection paste) in EVERY app.
    /// - Dragging outside the deadzone while held starts Halte-Modus immediately.
    /// - Holding stationary >= `hold_delay_ms` shows the anchor for Einrast-Modus.
    Pending {
        pressed_at: Instant,
        anchor_x: i32,
        anchor_y: i32,
        overlay_x: i32,
        overlay_y: i32,
        live_cursor_available: bool,
        using_absolute_cursor: bool,
        move_x: f32,
        move_y: f32,
    },
    Active {
        started_at: Instant,
        last_tick_at: Instant,
        anchor_x: i32,
        anchor_y: i32,
        delta_x: f32,
        delta_y: f32,
        button_held: bool,
        exited_deadzone_while_held: bool,
        has_scrolled: bool,
        live_cursor_available: bool,
        using_absolute_cursor: bool,
        accum_hires_x: f32,
        accum_hires_y: f32,
        pending_std_hires_x: i32,
        pending_std_hires_y: i32,
    },
}

/// Deterministic state machine for Breeze Autoscroll.
pub struct ScrollStateMachine {
    phase: ScrollPhase,
}

impl Default for ScrollStateMachine {
    fn default() -> Self {
        Self::new()
    }
}

impl ScrollStateMachine {
    pub fn new() -> Self {
        Self {
            phase: ScrollPhase::Idle,
        }
    }

    pub fn is_idle(&self) -> bool {
        matches!(self.phase, ScrollPhase::Idle)
    }

    pub fn is_pending(&self) -> bool {
        matches!(self.phase, ScrollPhase::Pending { .. })
    }

    pub fn is_active(&self) -> bool {
        matches!(self.phase, ScrollPhase::Active { .. })
    }

    pub fn uses_live_cursor(&self) -> bool {
        matches!(
            self.phase,
            ScrollPhase::Pending {
                live_cursor_available: true,
                ..
            } | ScrollPhase::Active {
                live_cursor_available: true,
                ..
            }
        )
    }

    /// Handles `BTN_MIDDLE` press.
    ///
    /// With system-wide `short_click_passthrough` (default), enters `Pending`:
    /// - Quick stationary release (< `hold_delay_ms`, inside deadzone) cancels cleanly so every
    ///   app receives its native middle-click (close tab, open link/folder, paste).
    /// - Pulling outside the deadzone while held starts Halte-Modus immediately.
    /// - Holding stationary >= `hold_delay_ms` displays the anchor for Einrast-Modus.
    pub fn on_middle_down<F>(
        &mut self,
        now: Instant,
        config: &ConfigData,
        cursor_pos: (i32, i32),
        overlay_pos: (i32, i32),
        live_cursor_available: bool,
        is_excluded_context: F,
    ) -> Option<IndicatorCommand>
    where
        F: FnOnce() -> Option<String>,
    {
        if !config.enabled {
            return self.force_stop();
        }

        // Pressing middle button while active always stops autoscroll (Einrast-Modus exit).
        if self.is_active() {
            return self.force_stop();
        }

        if self.is_pending() {
            return None;
        }

        // Exclude recognized 3D/CAD/Slicer apps, browser tabs, docks, and desktop background.
        if let Some(_matched) = is_excluded_context() {
            return None;
        }

        if config.protect_3d_drag || config.short_click_passthrough {
            self.phase = ScrollPhase::Pending {
                pressed_at: now,
                anchor_x: cursor_pos.0,
                anchor_y: cursor_pos.1,
                overlay_x: overlay_pos.0,
                overlay_y: overlay_pos.1,
                live_cursor_available,
                using_absolute_cursor: false,
                move_x: 0.0,
                move_y: 0.0,
            };
            return None;
        }

        self.phase = ScrollPhase::Active {
            started_at: now,
            last_tick_at: now,
            anchor_x: cursor_pos.0,
            anchor_y: cursor_pos.1,
            delta_x: 0.0,
            delta_y: 0.0,
            button_held: true,
            exited_deadzone_while_held: false,
            has_scrolled: false,
            live_cursor_available,
            using_absolute_cursor: false,
            accum_hires_x: 0.0,
            accum_hires_y: 0.0,
            pending_std_hires_x: 0,
            pending_std_hires_y: 0,
        };

        Some(IndicatorCommand::Show {
            anchor_x: overlay_pos.0,
            anchor_y: overlay_pos.1,
            deadzone: config.deadzone,
            theme: config.theme.clone(),
            opacity: config.indicator_opacity,
            allow_horizontal: config.allow_horizontal,
        })
    }

    /// Updates pointer offset from live compositor / X11 coordinates (`cur - anchor`),
    /// eliminating drift when returning to the anchor symbol.
    pub fn set_absolute_cursor(&mut self, cur_x: i32, cur_y: i32, config: &ConfigData) {
        match &mut self.phase {
            ScrollPhase::Idle => {}
            ScrollPhase::Pending {
                anchor_x,
                anchor_y,
                move_x,
                move_y,
                using_absolute_cursor,
                ..
            } => {
                *using_absolute_cursor = true;
                *move_x = (cur_x - *anchor_x) as f32;
                *move_y = (cur_y - *anchor_y) as f32;
                if config.protect_3d_drag && move_x.hypot(*move_y) >= config.drag_cancel_threshold_px
                {
                    self.phase = ScrollPhase::Idle;
                }
            }
            ScrollPhase::Active {
                anchor_x,
                anchor_y,
                delta_x,
                delta_y,
                button_held,
                exited_deadzone_while_held,
                using_absolute_cursor,
                ..
            } => {
                *using_absolute_cursor = true;
                *delta_x = (cur_x - *anchor_x) as f32;
                *delta_y = (cur_y - *anchor_y) as f32;
                if *button_held && is_outside_deadzone(*delta_x, *delta_y, config) {
                    *exited_deadzone_while_held = true;
                }
            }
        }
    }

    /// Handles raw relative mouse movement (`REL_X`, `REL_Y`) from `evdev`.
    pub fn on_mouse_move(&mut self, dx: f32, dy: f32, config: &ConfigData) {
        match &mut self.phase {
            ScrollPhase::Idle => {}
            ScrollPhase::Pending {
                move_x,
                move_y,
                using_absolute_cursor,
                ..
            } => {
                if !*using_absolute_cursor {
                    *move_x += dx;
                    *move_y += dy;
                }

                if config.protect_3d_drag {
                    let dist = move_x.hypot(*move_y);
                    if dist >= config.drag_cancel_threshold_px {
                        self.phase = ScrollPhase::Idle;
                    }
                }
            }
            ScrollPhase::Active {
                delta_x,
                delta_y,
                button_held,
                exited_deadzone_while_held,
                using_absolute_cursor,
                ..
            } => {
                if !*using_absolute_cursor {
                    *delta_x += dx;
                    *delta_y += dy;
                    if *button_held && is_outside_deadzone(*delta_x, *delta_y, config) {
                        *exited_deadzone_while_held = true;
                    }
                }
            }
        }
    }

    /// Handles `BTN_MIDDLE` release.
    pub fn on_middle_up(
        &mut self,
        now: Instant,
        config: &ConfigData,
    ) -> Option<IndicatorCommand> {
        match &mut self.phase {
            ScrollPhase::Idle => None,
            ScrollPhase::Pending {
                pressed_at,
                anchor_x,
                anchor_y,
                overlay_x,
                overlay_y,
                live_cursor_available,
                move_x,
                move_y,
                ..
            } => {
                let held = now.saturating_duration_since(*pressed_at);
                let dist = move_x.hypot(*move_y);

                // When system-wide short-click passthrough is enabled, releasing while still
                // Pending (< hold_delay_ms and inside deadzone) ALWAYS passes through cleanly
                // as a normal middle-click (close tab, open link/folder, paste) in every app.
                if !config.short_click_passthrough
                    && config.mode == "toggle"
                    && held < Duration::from_millis(TOGGLE_CLICK_MAX_MS)
                    && dist < config.drag_cancel_threshold_px
                {
                    let ax = *anchor_x;
                    let ay = *anchor_y;
                    let ox = *overlay_x;
                    let oy = *overlay_y;
                    let live_ok = *live_cursor_available;
                    self.phase = ScrollPhase::Active {
                        started_at: now,
                        last_tick_at: now,
                        anchor_x: ax,
                        anchor_y: ay,
                        delta_x: 0.0,
                        delta_y: 0.0,
                        button_held: false,
                        exited_deadzone_while_held: false,
                        has_scrolled: false,
                        live_cursor_available: live_ok,
                        using_absolute_cursor: false,
                        accum_hires_x: 0.0,
                        accum_hires_y: 0.0,
                        pending_std_hires_x: 0,
                        pending_std_hires_y: 0,
                    };
                    return Some(IndicatorCommand::Show {
                        anchor_x: ox,
                        anchor_y: oy,
                        deadzone: config.deadzone,
                        theme: config.theme.clone(),
                        opacity: config.indicator_opacity,
                        allow_horizontal: config.allow_horizontal,
                    });
                }

                self.phase = ScrollPhase::Idle;
                None
            }
            ScrollPhase::Active {
                button_held,
                exited_deadzone_while_held,
                ..
            } => {
                if !*button_held {
                    // Already in Einrast-Modus; ignore stray release
                    return None;
                }
                *button_held = false;

                match config.mode.as_str() {
                    "drag" => {
                        self.phase = ScrollPhase::Idle;
                        Some(IndicatorCommand::Hide)
                    }
                    "toggle" => None,
                    _ => {
                        // Windows Hybrid ("smart"):
                        // Pulled outside deadzone while holding (Halte-Modus) -> stop on release
                        // Released inside deadzone (Einrast-Modus) -> stay latched
                        if *exited_deadzone_while_held {
                            self.phase = ScrollPhase::Idle;
                            Some(IndicatorCommand::Hide)
                        } else {
                            None
                        }
                    }
                }
            }
        }
    }

    /// Handles clicking any other mouse button or pressing ESC.
    pub fn on_cancel_input(&mut self) -> Option<IndicatorCommand> {
        self.force_stop()
    }

    /// Auto-cancels an un-scrolled anchor if the window title/tab or active window changed
    /// immediately after `BTN_MIDDLE` was pressed (e.g., closing an active tab or navigating).
    pub fn on_window_context_changed(
        &mut self,
        now: Instant,
        config: &ConfigData,
    ) -> Option<IndicatorCommand> {
        if !config.ignore_hyperlinks_and_tabs {
            return None;
        }
        match &self.phase {
            ScrollPhase::Idle => None,
            ScrollPhase::Pending { .. } => {
                self.phase = ScrollPhase::Idle;
                None
            }
            ScrollPhase::Active {
                started_at,
                has_scrolled,
                ..
            } => {
                if !*has_scrolled
                    && now.saturating_duration_since(*started_at)
                        <= Duration::from_millis(CONTEXT_CHANGE_CANCEL_WINDOW_MS)
                {
                    self.phase = ScrollPhase::Idle;
                    Some(IndicatorCommand::Hide)
                } else {
                    None
                }
            }
        }
    }

    /// Handles physical mouse wheel turns from hardware devices.
    pub fn on_physical_wheel(&mut self, now: Instant) -> Option<IndicatorCommand> {
        match &self.phase {
            ScrollPhase::Idle => None,
            ScrollPhase::Pending { pressed_at, .. } => {
                if now.saturating_duration_since(*pressed_at)
                    >= Duration::from_millis(WHEEL_CANCEL_GRACE_MS)
                {
                    self.phase = ScrollPhase::Idle;
                }
                None
            }
            ScrollPhase::Active { started_at, .. } => {
                if now.saturating_duration_since(*started_at)
                    >= Duration::from_millis(WHEEL_CANCEL_GRACE_MS)
                {
                    self.phase = ScrollPhase::Idle;
                    Some(IndicatorCommand::Hide)
                } else {
                    None
                }
            }
        }
    }

    /// Forces the state machine back to `Idle`.
    pub fn force_stop(&mut self) -> Option<IndicatorCommand> {
        match self.phase {
            ScrollPhase::Idle => None,
            ScrollPhase::Pending { .. } => {
                self.phase = ScrollPhase::Idle;
                None
            }
            ScrollPhase::Active { .. } => {
                self.phase = ScrollPhase::Idle;
                Some(IndicatorCommand::Hide)
            }
        }
    }

    /// Advances timers and emits smooth hi-res scroll wheel ticks using 2D vector physics.
    #[cfg(test)]
    pub fn tick<E: WheelEmitter>(
        &mut self,
        now: Instant,
        config: &ConfigData,
        emitter: &mut E,
    ) -> Vec<IndicatorCommand> {
        let mut commands = Vec::new();
        self.tick_into(now, config, emitter, &mut commands);
        commands
    }

    /// Allocation-free variant of `tick` that writes emitted indicator commands into `commands`.
    pub fn tick_into<E: WheelEmitter>(
        &mut self,
        now: Instant,
        config: &ConfigData,
        emitter: &mut E,
        commands: &mut Vec<IndicatorCommand>,
    ) {
        commands.clear();

        if !config.enabled {
            if let Some(cmd) = self.force_stop() {
                commands.push(cmd);
            }
            return;
        }

        // System-wide short-click protection (`short_click_passthrough`) and optional `protect_3d_drag`:
        // 1. Pulling outside the deadzone while holding BTN_MIDDLE -> instant Halte-Modus activation.
        // 2. Holding stationary >= `hold_delay_ms` -> anchor appears for Einrast-Modus (or Halte-Modus).
        if let ScrollPhase::Pending {
            pressed_at,
            anchor_x,
            anchor_y,
            overlay_x,
            overlay_y,
            live_cursor_available,
            using_absolute_cursor,
            move_x,
            move_y,
        } = self.phase
        {
            let elapsed = now.saturating_duration_since(pressed_at);
            let outside_deadzone = is_outside_deadzone(move_x, move_y, config);
            let early_drag_activate = !config.protect_3d_drag && outside_deadzone;
            let hold_elapsed = elapsed >= Duration::from_millis(config.hold_delay_ms);

            if early_drag_activate || hold_elapsed {
                let dist = move_x.hypot(move_y);
                if config.protect_3d_drag && dist >= config.drag_cancel_threshold_px {
                    self.phase = ScrollPhase::Idle;
                    return;
                }

                self.phase = ScrollPhase::Active {
                    started_at: now,
                    last_tick_at: now,
                    anchor_x,
                    anchor_y,
                    delta_x: move_x,
                    delta_y: move_y,
                    button_held: true,
                    exited_deadzone_while_held: outside_deadzone,
                    has_scrolled: false,
                    live_cursor_available,
                    using_absolute_cursor,
                    accum_hires_x: 0.0,
                    accum_hires_y: 0.0,
                    pending_std_hires_x: 0,
                    pending_std_hires_y: 0,
                };
                commands.push(IndicatorCommand::Show {
                    anchor_x: overlay_x,
                    anchor_y: overlay_y,
                    deadzone: config.deadzone,
                    theme: config.theme.clone(),
                    opacity: config.indicator_opacity,
                    allow_horizontal: config.allow_horizontal,
                });
                if !early_drag_activate {
                    return;
                }
            } else {
                return;
            }
        }

        if let ScrollPhase::Active {
            last_tick_at,
            delta_x,
            delta_y,
            has_scrolled,
            accum_hires_x,
            accum_hires_y,
            pending_std_hires_x,
            pending_std_hires_y,
            ..
        } = &mut self.phase
        {
            let dt = now
                .saturating_duration_since(*last_tick_at)
                .as_secs_f32()
                .clamp(0.001, 0.05);
            *last_tick_at = now;

            let dx = *delta_x;
            let dy = *delta_y;
            commands.push(IndicatorCommand::UpdateOffset {
                delta_x: dx,
                delta_y: dy,
            });

            let deadzone = config.deadzone as f32;
            let speed_mult = config.scroll_speed;
            let curve_exp = config.scroll_curve_exponent;

            if config.allow_horizontal {
                // True 2D circular deadzone + unit-vector directional decomposition
                let dist = dx.hypot(dy);
                if dist > deadzone {
                    *has_scrolled = true;
                    let total_rate = calc_scroll_rate(dist - deadzone, speed_mult, curve_exp);
                    let ux = dx / dist;
                    let uy = dy / dist;

                    let mut rate_y = total_rate * (-uy);
                    if config.invert_vertical {
                        rate_y = -rate_y;
                    }
                    *accum_hires_y += rate_y * (HI_RES_PER_NOTCH as f32) * dt;
                    flush_axis_scroll(accum_hires_y, pending_std_hires_y, true, emitter);

                    let mut rate_x = total_rate * ux;
                    if config.invert_horizontal {
                        rate_x = -rate_x;
                    }
                    *accum_hires_x += rate_x * (HI_RES_PER_NOTCH as f32) * dt;
                    flush_axis_scroll(accum_hires_x, pending_std_hires_x, false, emitter);
                } else {
                    *accum_hires_x = 0.0;
                    *accum_hires_y = 0.0;
                }
            } else {
                // Vertical-only autoscroll
                *accum_hires_x = 0.0;
                let abs_dy = dy.abs();
                if abs_dy > deadzone {
                    *has_scrolled = true;
                    let mut rate_y = calc_scroll_rate(abs_dy - deadzone, speed_mult, curve_exp);
                    if config.invert_vertical {
                        rate_y = -rate_y;
                    }
                    let signed_hires_rate = if dy < 0.0 {
                        rate_y * (HI_RES_PER_NOTCH as f32)
                    } else {
                        -rate_y * (HI_RES_PER_NOTCH as f32)
                    };
                    *accum_hires_y += signed_hires_rate * dt;
                    flush_axis_scroll(accum_hires_y, pending_std_hires_y, true, emitter);
                } else {
                    *accum_hires_y = 0.0;
                }
            }
        }
    }
}

fn is_outside_deadzone(dx: f32, dy: f32, config: &ConfigData) -> bool {
    let dz = config.deadzone as f32;
    if config.allow_horizontal {
        dx.hypot(dy) > dz
    } else {
        dy.abs() > dz
    }
}

fn flush_axis_scroll<E: WheelEmitter>(
    accum_hires: &mut f32,
    pending_std_hires: &mut i32,
    vertical: bool,
    emitter: &mut E,
) {
    let quantum = HI_RES_STEP_QUANTUM as f32;
    let mut hires_to_emit = 0i32;

    while *accum_hires >= quantum {
        *accum_hires -= quantum;
        hires_to_emit += HI_RES_STEP_QUANTUM;
    }
    while *accum_hires <= -quantum {
        *accum_hires += quantum;
        hires_to_emit -= HI_RES_STEP_QUANTUM;
    }

    if hires_to_emit == 0 {
        return;
    }

    *pending_std_hires += hires_to_emit;
    let mut std_to_emit = 0i32;
    while *pending_std_hires >= HI_RES_PER_NOTCH {
        *pending_std_hires -= HI_RES_PER_NOTCH;
        std_to_emit += 1;
    }
    while *pending_std_hires <= -HI_RES_PER_NOTCH {
        *pending_std_hires += HI_RES_PER_NOTCH;
        std_to_emit -= 1;
    }

    emitter.emit_wheel(vertical, hires_to_emit, std_to_emit);
}

/// Calculates scroll rate (wheel notches per second) from distance beyond deadzone.
///
/// Progressive non-linear curve: gentle fine reading near deadzone, rapid scrolling farther away.
pub fn calc_scroll_rate(effective_dist: f32, speed_mult: f32, curve_exponent: f32) -> f32 {
    if effective_dist <= 0.0 {
        return 0.0;
    }
    let exp = curve_exponent.clamp(1.0, 2.5) + 0.7;
    let rate = speed_mult * (0.18 * effective_dist + 0.004 * effective_dist.powf(exp));
    rate.min(100.0)
}

// ============================================================================
// evdev Device Discovery, inotify Hotplug & Background Input Thread
// ============================================================================

struct EvdevDevice {
    path: PathBuf,
    file: File,
    is_mouse: bool,
    has_esc: bool,
}

fn list_event_paths() -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir("/dev/input") else {
        return Vec::new();
    };
    let mut paths: Vec<PathBuf> = entries
        .filter_map(|e| e.ok().map(|entry| entry.path()))
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|s| s.starts_with("event"))
        })
        .collect();
    paths.sort();
    paths
}

fn open_evdev_nonblock(path: &Path) -> Option<File> {
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)
        .ok()
}

/// Discovers and opens ONLY mouse devices (`device_is_mouse`).
/// Keyboard devices are never opened during Idle/Pending.
fn find_mouse_devices() -> Vec<EvdevDevice> {
    let mut result = Vec::new();
    for path in list_event_paths() {
        let Some(file) = open_evdev_nonblock(&path) else {
            continue;
        };
        let fd = file.as_raw_fd();

        let name = read_device_name(fd);
        if name.contains("Breeze Autoscroll") {
            continue;
        }

        if device_is_mouse(fd) {
            let has_esc = device_has_key(fd, KEY_ESC);
            result.push(EvdevDevice {
                path,
                file,
                is_mouse: true,
                has_esc,
            });
        }
    }
    result
}

/// Incrementally refreshes open mouse devices without closing/reopening unchanged FDs.
fn refresh_mouse_devices(devices: &mut Vec<EvdevDevice>, bad_paths: &HashSet<PathBuf>) {
    let current_paths = list_event_paths();
    let present: HashSet<&Path> = current_paths.iter().map(PathBuf::as_path).collect();

    // Retain already-open healthy devices that still exist
    devices.retain(|dev| {
        present.contains(dev.path.as_path()) && !bad_paths.contains(&dev.path)
    });

    let already_open: HashSet<PathBuf> = devices.iter().map(|d| d.path.clone()).collect();
    for path in current_paths {
        if already_open.contains(&path) {
            continue;
        }
        let Some(file) = open_evdev_nonblock(&path) else {
            continue;
        };
        let fd = file.as_raw_fd();
        let name = read_device_name(fd);
        if name.contains("Breeze Autoscroll") {
            continue;
        }
        if device_is_mouse(fd) {
            let has_esc = device_has_key(fd, KEY_ESC);
            devices.push(EvdevDevice {
                path,
                file,
                is_mouse: true,
                has_esc,
            });
        }
    }
}

/// Opens keyboard devices (`KEY_ESC`) strictly on-demand while autoscroll is `Active`.
/// Gracefully skips unreadable nodes (e.g. `EACCES` under least-privilege `uaccess` rules).
fn find_esc_keyboard_devices(mouse_devices: &[EvdevDevice]) -> Vec<EvdevDevice> {
    let already_open: HashSet<&Path> = mouse_devices.iter().map(|d| d.path.as_path()).collect();
    let mut result = Vec::new();
    for path in list_event_paths() {
        if already_open.contains(path.as_path()) {
            continue;
        }
        let Some(file) = open_evdev_nonblock(&path) else {
            continue;
        };
        let fd = file.as_raw_fd();
        let name = read_device_name(fd);
        if name.contains("Breeze Autoscroll") {
            continue;
        }
        if device_has_key(fd, KEY_ESC) {
            result.push(EvdevDevice {
                path,
                file,
                is_mouse: false,
                has_esc: true,
            });
        }
    }
    result
}

struct InotifyWatcher {
    file: File,
}

impl InotifyWatcher {
    fn new_dev_input() -> Option<Self> {
        let fd = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
        if fd < 0 {
            return None;
        }
        let file = unsafe { File::from_raw_fd(fd) };
        let mask = libc::IN_CREATE | libc::IN_DELETE | libc::IN_ATTRIB;
        let wd = unsafe { libc::inotify_add_watch(file.as_raw_fd(), c"/dev/input".as_ptr(), mask) };
        if wd < 0 {
            return None;
        }
        Some(Self { file })
    }

    fn as_raw_fd(&self) -> RawFd {
        self.file.as_raw_fd()
    }

    /// Drains all pending inotify events and returns true when an `event*` node changed
    /// (or the inotify queue overflowed).
    fn drain_and_check_rescan(&self) -> bool {
        let mut buf = [0u8; 4096];
        let mut should_rescan = false;
        loop {
            let n = unsafe {
                libc::read(
                    self.file.as_raw_fd(),
                    buf.as_mut_ptr().cast::<libc::c_void>(),
                    buf.len(),
                )
            };
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
            if parse_inotify_buffer(&buf[..n as usize]) {
                should_rescan = true;
            }
        }
        should_rescan
    }
}

/// Parses raw `inotify_event` bytes and returns true if any event refers to an `event*`
/// device node or signals `IN_Q_OVERFLOW`.
fn parse_inotify_buffer(bytes: &[u8]) -> bool {
    let hdr_size = std::mem::size_of::<libc::inotify_event>();
    let mut offset = 0usize;
    let mut rescan = false;

    while offset + hdr_size <= bytes.len() {
        let ev = unsafe {
            std::ptr::read_unaligned(bytes[offset..].as_ptr().cast::<libc::inotify_event>())
        };
        let name_len = ev.len as usize;
        let step = hdr_size.saturating_add(name_len);
        if step == 0 || offset + step > bytes.len() {
            rescan = true;
            break;
        }

        if (ev.mask & libc::IN_Q_OVERFLOW) != 0 {
            rescan = true;
        } else if name_len > 0 {
            let name_bytes = &bytes[offset + hdr_size..offset + step];
            let nul_pos = name_bytes
                .iter()
                .position(|&b| b == 0)
                .unwrap_or(name_bytes.len());
            if let Ok(name) = std::str::from_utf8(&name_bytes[..nul_pos])
                && name.starts_with("event")
            {
                rescan = true;
            }
        }

        offset += step;
    }

    rescan
}

fn create_cloexec_nonblock_pipe() -> io::Result<(File, File)> {
    let mut pipe_fds = [-1i32; 2];
    if unsafe { libc::pipe2(pipe_fds.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    let read_file = unsafe { File::from_raw_fd(pipe_fds[0]) };
    let write_file = unsafe { File::from_raw_fd(pipe_fds[1]) };
    Ok((read_file, write_file))
}

fn read_device_name(fd: RawFd) -> String {
    let mut buf = [0u8; 256];
    let rc = unsafe { libc::ioctl(fd, eviocgname(buf.len()), buf.as_mut_ptr()) };
    if rc <= 0 {
        return String::new();
    }
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    String::from_utf8_lossy(&buf[..end]).into_owned()
}

fn test_bit(bits: &[u8], bit: usize) -> bool {
    let byte_idx = bit / 8;
    let bit_idx = bit % 8;
    bits.get(byte_idx)
        .is_some_and(|&byte| (byte & (1 << bit_idx)) != 0)
}

fn device_is_mouse(fd: RawFd) -> bool {
    let mut ev_bits = [0u8; 4];
    if unsafe { libc::ioctl(fd, eviocgbit(0, ev_bits.len()), ev_bits.as_mut_ptr()) } < 0 {
        return false;
    }
    if !test_bit(&ev_bits, EV_REL as usize) || !test_bit(&ev_bits, EV_KEY as usize) {
        return false;
    }

    let mut rel_bits = [0u8; 4];
    if unsafe { libc::ioctl(fd, eviocgbit(EV_REL as u8, rel_bits.len()), rel_bits.as_mut_ptr()) }
        < 0
    {
        return false;
    }
    let has_xy = test_bit(&rel_bits, REL_X as usize) && test_bit(&rel_bits, REL_Y as usize);
    has_xy && device_has_key(fd, BTN_MIDDLE)
}

fn device_has_key(fd: RawFd, key_code: u16) -> bool {
    let mut key_bits = [0u8; 96]; // Covers up to KEY_MAX (0x2ff = 767 bits = 96 bytes)
    if unsafe { libc::ioctl(fd, eviocgbit(EV_KEY as u8, key_bits.len()), key_bits.as_mut_ptr()) }
        < 0
    {
        return false;
    }
    test_bit(&key_bits, key_code as usize)
}

/// Manages the background `evdev` reader, `uinput` scroll emission, and indicator command channel.
pub struct InputEngine {
    running: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
    wake_write: Option<File>,
}

impl InputEngine {
    /// Starts the input engine background thread.
    pub fn start(
        config: ConfigManager,
        window_tracker: WindowTracker,
        indicator_tx: SyncSender<IndicatorCommand>,
    ) -> Result<Self, String> {
        let mut emitter = UInputEmitter::create()?;

        // Self-pipe with O_CLOEXEC | O_NONBLOCK to wake up `poll` immediately on shutdown
        let (wake_read, wake_write) = create_cloexec_nonblock_pipe()
            .map_err(|err| format!("Konnte interne Steuer-Pipe nicht anlegen: {err}"))?;

        let running = Arc::new(AtomicBool::new(true));
        let running_clone = Arc::clone(&running);

        let handle = thread::Builder::new()
            .name("breeze-input-engine".to_string())
            .spawn(move || {
                let wake_read_fd = wake_read.as_raw_fd();
                run_engine_loop(
                    running_clone,
                    wake_read_fd,
                    &mut emitter,
                    config,
                    window_tracker,
                    indicator_tx,
                );
                drop(wake_read);
            })
            .map_err(|err| format!("Fehler beim Starten des Input-Threads: {err}"))?;

        Ok(Self {
            running,
            handle: Some(handle),
            wake_write: Some(wake_write),
        })
    }

    /// Stops the background input engine cleanly.
    pub fn stop(&mut self) {
        self.running.store(false, Ordering::SeqCst);
        if let Some(wake_write) = self.wake_write.take() {
            let byte = [1u8; 1];
            let _ = write_all_retry_eintr(wake_write.as_raw_fd(), &byte);
            drop(wake_write);
        }
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for InputEngine {
    fn drop(&mut self) {
        self.stop();
    }
}

#[derive(Debug)]
enum RawEvdevAction {
    MiddleDown,
    MiddleUp,
    CancelInput,
    PhysicalWheel,
    MouseMove(f32, f32),
}

fn send_indicator_cmd(tx: &SyncSender<IndicatorCommand>, cmd: IndicatorCommand) {
    let _ = tx.try_send(cmd);
}

fn run_engine_loop(
    running: Arc<AtomicBool>,
    wake_read_fd: RawFd,
    emitter: &mut UInputEmitter,
    config_mgr: ConfigManager,
    window_tracker: WindowTracker,
    indicator_tx: SyncSender<IndicatorCommand>,
) {
    let mut state_machine = ScrollStateMachine::new();
    let mut mouse_devices = find_mouse_devices();
    let mut esc_keyboards: Vec<EvdevDevice> = Vec::new();
    let inotify = InotifyWatcher::new_dev_input();
    let mut last_fallback_rescan = Instant::now();
    let mut needs_rescan = false;
    let mut bad_paths: HashSet<PathBuf> = HashSet::new();

    // Pre-allocated, reused loop buffers (zero per-event / idle heap allocations)
    let mut pollfds: Vec<libc::pollfd> = Vec::with_capacity(16);
    let mut actions: Vec<RawEvdevAction> = Vec::with_capacity(32);
    let mut cmd_buf: Vec<IndicatorCommand> = Vec::with_capacity(4);

    while running.load(Ordering::SeqCst) {
        if needs_rescan
            || (inotify.is_none()
                && Instant::now().saturating_duration_since(last_fallback_rescan)
                    >= Duration::from_secs(3))
        {
            refresh_mouse_devices(&mut mouse_devices, &bad_paths);
            bad_paths.clear();
            if state_machine.is_active() {
                esc_keyboards = find_esc_keyboard_devices(&mouse_devices);
            } else {
                esc_keyboards.clear();
            }
            last_fallback_rescan = Instant::now();
            needs_rescan = false;
        }

        let cfg = config_mgr.current();

        // Build pollfd list: [wake_pipe, optional inotify, mouse_devices..., active_esc_keyboards...]
        pollfds.clear();
        pollfds.push(libc::pollfd {
            fd: wake_read_fd,
            events: libc::POLLIN,
            revents: 0,
        });
        let has_inotify = if let Some(ref watcher) = inotify {
            pollfds.push(libc::pollfd {
                fd: watcher.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            });
            true
        } else {
            false
        };
        let mouse_start_idx = pollfds.len();
        for dev in &mouse_devices {
            pollfds.push(libc::pollfd {
                fd: dev.file.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            });
        }
        let kbd_start_idx = pollfds.len();
        for dev in &esc_keyboards {
            pollfds.push(libc::pollfd {
                fd: dev.file.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            });
        }

        // Pure event-driven sleep (-1) in Idle when inotify is active; ~16ms tick when Pending/Active
        let timeout_ms: libc::c_int = if state_machine.is_idle() {
            if has_inotify { -1 } else { 3000 }
        } else {
            cfg.tick_interval_ms.clamp(8, 50) as libc::c_int
        };

        let rc =
            unsafe { libc::poll(pollfds.as_mut_ptr(), pollfds.len() as libc::nfds_t, timeout_ms) };
        if !running.load(Ordering::SeqCst) {
            break;
        }
        if rc < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            thread::sleep(Duration::from_millis(50));
            continue;
        }

        actions.clear();
        let listen_esc = state_machine.is_active();
        if rc > 0 {
            if (pollfds[0].revents & libc::POLLIN) != 0 {
                break;
            }
            if has_inotify
                && (pollfds[1].revents & libc::POLLIN) != 0
                && let Some(ref watcher) = inotify
                && watcher.drain_and_check_rescan()
            {
                needs_rescan = true;
            }
            for (idx, pfd) in pollfds[mouse_start_idx..kbd_start_idx].iter().enumerate() {
                if (pfd.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL)) != 0 {
                    if let Some(dev) = mouse_devices.get(idx) {
                        bad_paths.insert(dev.path.clone());
                    }
                    needs_rescan = true;
                    continue;
                }
                if (pfd.revents & libc::POLLIN) != 0
                    && let Some(dev) = mouse_devices.get(idx)
                    && !read_device_events(dev, listen_esc, &mut actions)
                {
                    bad_paths.insert(dev.path.clone());
                    needs_rescan = true;
                }
            }
            for (idx, pfd) in pollfds[kbd_start_idx..].iter().enumerate() {
                if (pfd.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL)) != 0 {
                    needs_rescan = true;
                    continue;
                }
                if (pfd.revents & libc::POLLIN) != 0
                    && let Some(dev) = esc_keyboards.get(idx)
                    && !read_device_events(dev, listen_esc, &mut actions)
                {
                    needs_rescan = true;
                }
            }
        }

        let was_active_before = state_machine.is_active();
        let event_now = Instant::now();
        for action in actions.drain(..) {
            let was_not_idle = !state_machine.is_idle();
            let cmd = match action {
                RawEvdevAction::MiddleDown => {
                    if !state_machine.is_idle() {
                        state_machine.on_middle_down(
                            event_now,
                            &cfg,
                            (0, 0),
                            (0, 0),
                            false,
                            || None,
                        )
                    } else {
                        let probe = window_tracker
                            .probe_pointer_and_start_live_cursor(cfg.ignore_hyperlinks_and_tabs);
                        if probe.is_dock_or_desktop
                            || window_tracker.is_interactive_probe(&probe, &cfg)
                            || window_tracker.check_probe_excluded(&probe, &cfg).is_some()
                        {
                            window_tracker.stop_live_cursor();
                            None
                        } else {
                            let live_ok = probe.from_kwin || !is_wayland_session();
                            state_machine.on_middle_down(
                                event_now,
                                &cfg,
                                (probe.cursor_x, probe.cursor_y),
                                (probe.overlay_x, probe.overlay_y),
                                live_ok,
                                || None,
                            )
                        }
                    }
                }
                RawEvdevAction::MiddleUp => state_machine.on_middle_up(event_now, &cfg),
                RawEvdevAction::CancelInput => state_machine.on_cancel_input(),
                RawEvdevAction::PhysicalWheel => state_machine.on_physical_wheel(event_now),
                RawEvdevAction::MouseMove(dx, dy) => {
                    state_machine.on_mouse_move(dx, dy, &cfg);
                    None
                }
            };
            if let Some(c) = cmd {
                send_indicator_cmd(&indicator_tx, c);
            }
            if was_not_idle && state_machine.is_idle() {
                window_tracker.stop_live_cursor();
            }
        }

        if !state_machine.is_idle() && window_tracker.take_window_context_changed() {
            let was_not_idle = !state_machine.is_idle();
            if let Some(cmd) = state_machine.on_window_context_changed(Instant::now(), &cfg) {
                send_indicator_cmd(&indicator_tx, cmd);
            }
            if was_not_idle && state_machine.is_idle() {
                window_tracker.stop_live_cursor();
            }
        }

        if !state_machine.is_idle()
            && state_machine.uses_live_cursor()
            && let Some((cx, cy)) = window_tracker.get_live_cursor_pos()
        {
            let was_not_idle = !state_machine.is_idle();
            state_machine.set_absolute_cursor(cx, cy, &cfg);
            if was_not_idle && state_machine.is_idle() {
                window_tracker.stop_live_cursor();
            }
        }

        let was_not_idle = !state_machine.is_idle();
        state_machine.tick_into(Instant::now(), &cfg, emitter, &mut cmd_buf);
        for cmd in cmd_buf.drain(..) {
            send_indicator_cmd(&indicator_tx, cmd);
        }
        if was_not_idle && state_machine.is_idle() {
            window_tracker.stop_live_cursor();
        }

        // On-demand keyboard FD lifecycle: open ONLY on entering Active, close immediately on exit
        let is_active_now = state_machine.is_active();
        if !was_active_before && is_active_now {
            esc_keyboards = find_esc_keyboard_devices(&mouse_devices);
        } else if was_active_before && !is_active_now {
            esc_keyboards.clear();
        }
    }

    esc_keyboards.clear();
    if let Some(cmd) = state_machine.force_stop() {
        window_tracker.stop_live_cursor();
        send_indicator_cmd(&indicator_tx, cmd);
    }
    send_indicator_cmd(&indicator_tx, IndicatorCommand::Shutdown);
}

fn read_device_events(dev: &EvdevDevice, listen_esc: bool, out: &mut Vec<RawEvdevAction>) -> bool {
    let mut buf = [InputEvent {
        time: libc::timeval {
            tv_sec: 0,
            tv_usec: 0,
        },
        type_: 0,
        code: 0,
        value: 0,
    }; 64];
    let ev_size = std::mem::size_of::<InputEvent>();

    loop {
        let bytes = unsafe {
            libc::read(
                dev.file.as_raw_fd(),
                buf.as_mut_ptr().cast::<libc::c_void>(),
                buf.len() * ev_size,
            )
        };
        if bytes < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::WouldBlock {
                return true;
            }
            if err.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return false;
        }
        if bytes == 0 {
            return false;
        }

        let count = (bytes as usize) / ev_size;
        for ev in &buf[..count] {
            if dev.is_mouse && ev.type_ == EV_REL {
                match ev.code {
                    REL_X => out.push(RawEvdevAction::MouseMove(ev.value as f32, 0.0)),
                    REL_Y => out.push(RawEvdevAction::MouseMove(0.0, ev.value as f32)),
                    REL_WHEEL | REL_HWHEEL if ev.value != 0 => {
                        out.push(RawEvdevAction::PhysicalWheel);
                    }
                    _ => {}
                }
            } else if ev.type_ == EV_KEY && (ev.value == 1 || ev.value == 0) {
                if dev.is_mouse && ev.code == BTN_MIDDLE {
                    if ev.value == 1 {
                        out.push(RawEvdevAction::MiddleDown);
                    } else if ev.value == 0 {
                        out.push(RawEvdevAction::MiddleUp);
                    }
                } else if (dev.is_mouse
                    && ev.value == 1
                    && matches!(ev.code, BTN_LEFT | BTN_RIGHT | BTN_SIDE | BTN_EXTRA))
                    || (listen_esc && dev.has_esc && ev.value == 1 && ev.code == KEY_ESC)
                {
                    out.push(RawEvdevAction::CancelInput);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct MockEmitter {
        events: Vec<(bool, i32, i32)>,
    }

    impl WheelEmitter for MockEmitter {
        fn emit_wheel(&mut self, vertical: bool, hires_delta: i32, std_delta: i32) {
            self.events.push((vertical, hires_delta, std_delta));
        }
    }

    #[test]
    fn system_wide_short_click_passes_through_without_showing_anchor() {
        let mut sm = ScrollStateMachine::new();
        let cfg = ConfigData::default();
        let mut emitter = MockEmitter::default();
        let t0 = Instant::now();

        // Short stationary middle-click (e.g. closing an Obsidian tab or opening a Nautilus folder)
        assert!(
            sm.on_middle_down(t0, &cfg, (1200, 300), (1200, 300), true, || None)
                .is_none()
        );
        assert!(sm.is_pending());

        // Minor hand jiggle (7px < deadzone 12px) during 90ms click must NOT activate autoscroll
        sm.set_absolute_cursor(1205, 305, &cfg);
        assert!(
            sm.tick(t0 + Duration::from_millis(50), &cfg, &mut emitter)
                .is_empty()
        );
        assert!(sm.is_pending());

        // Release at 90ms (< 180ms hold_delay_ms) -> returns cleanly to Idle, zero anchor shown
        assert!(
            sm.on_middle_up(t0 + Duration::from_millis(90), &cfg)
                .is_none()
        );
        assert!(sm.is_idle());
        assert!(emitter.events.is_empty());
    }

    #[test]
    fn einrast_modus_latches_after_brief_hold_and_release_inside_deadzone() {
        let mut sm = ScrollStateMachine::new();
        let cfg = ConfigData::default();
        let mut emitter = MockEmitter::default();
        let t0 = Instant::now();

        // Press BTN_MIDDLE -> enters Pending first (protects quick clicks system-wide)
        assert!(
            sm.on_middle_down(t0, &cfg, (2500, 600), (2500, 600), true, || None)
                .is_none()
        );
        assert!(sm.is_pending());

        // Hold stationary for hold_delay_ms (180ms) -> anchor appears at exact click coordinates
        let cmds = sm.tick(t0 + Duration::from_millis(180), &cfg, &mut emitter);
        assert_eq!(
            cmds,
            vec![IndicatorCommand::Show {
                anchor_x: 2500,
                anchor_y: 600,
                deadzone: 12,
                theme: "dark".to_string(),
                opacity: 0.92,
                allow_horizontal: true,
            }]
        );
        assert!(sm.is_active());

        // Small jitter inside deadzone while releasing at 220ms -> latches Einrast-Modus
        sm.set_absolute_cursor(2503, 602, &cfg);
        assert!(
            sm.on_middle_up(t0 + Duration::from_millis(220), &cfg)
                .is_none()
        );
        assert!(sm.is_active());

        // Move mouse down 60px -> scrolls smoothly downward
        sm.set_absolute_cursor(2500, 660, &cfg);
        for step in 1..=10 {
            let _ = sm.tick(
                t0 + Duration::from_millis(220 + step * 16),
                &cfg,
                &mut emitter,
            );
        }
        assert!(!emitter.events.is_empty());
        assert!(emitter.events.iter().all(|(v, h, _)| *v && *h < 0));

        // Return pointer onto anchor deadzone -> zero drift, scrolling stops completely
        emitter.events.clear();
        sm.set_absolute_cursor(2501, 601, &cfg);
        let _ = sm.tick(t0 + Duration::from_millis(400), &cfg, &mut emitter);
        assert!(emitter.events.is_empty());

        // Any mouse click (e.g. left click / CancelInput) or ESC stops Einrast-Modus
        assert_eq!(sm.on_cancel_input(), Some(IndicatorCommand::Hide));
        assert!(sm.is_idle());
    }

    #[test]
    fn windows_halte_modus_activates_immediately_on_drag_and_stops_on_release() {
        let mut sm = ScrollStateMachine::new();
        let cfg = ConfigData::default();
        let mut emitter = MockEmitter::default();
        let t0 = Instant::now();

        assert!(
            sm.on_middle_down(t0, &cfg, (1000, 500), (1000, 500), true, || None)
                .is_none()
        );
        assert!(sm.is_pending());

        // Pull mouse directly out of deadzone after just 16ms (well before 180ms hold_delay_ms!)
        sm.set_absolute_cursor(1000, 570, &cfg);
        let first_cmds = sm.tick(t0 + Duration::from_millis(16), &cfg, &mut emitter);
        assert!(
            first_cmds
                .iter()
                .any(|c| matches!(c, IndicatorCommand::Show { .. }))
        );
        assert!(sm.is_active());

        for step in 2..=6 {
            let _ = sm.tick(t0 + Duration::from_millis(step * 16), &cfg, &mut emitter);
        }
        assert!(!emitter.events.is_empty());
        assert!(emitter.events.iter().all(|(v, h, _)| *v && *h < 0));

        // Release BTN_MIDDLE -> anchor disappears immediately (Halte-Modus)
        let stop = sm.on_middle_up(t0 + Duration::from_millis(120), &cfg);
        assert_eq!(stop, Some(IndicatorCommand::Hide));
        assert!(sm.is_idle());
    }

    #[test]
    fn instant_tap_to_latch_works_when_short_click_passthrough_disabled() {
        let mut sm = ScrollStateMachine::new();
        let cfg = ConfigData {
            short_click_passthrough: false,
            ..Default::default()
        };
        let t0 = Instant::now();

        let show = sm.on_middle_down(t0, &cfg, (2500, 600), (2500, 600), true, || None);
        assert!(matches!(show, Some(IndicatorCommand::Show { .. })));
        assert!(sm.is_active());
        assert!(
            sm.on_middle_up(t0 + Duration::from_millis(60), &cfg)
                .is_none()
        );
        assert!(sm.is_active());
    }

    #[test]
    fn diagonal_2d_scrolling_respects_circular_deadzone_and_1d_blocks_horizontal() {
        let mut sm = ScrollStateMachine::new();
        let mut cfg = ConfigData {
            deadzone: 12,
            allow_horizontal: true,
            ..Default::default()
        };
        let mut emitter = MockEmitter::default();
        let t0 = Instant::now();

        let _ = sm.on_middle_down(t0, &cfg, (500, 500), (500, 500), true, || None);
        // Pulling diagonally outside circular deadzone activates immediately and emits both axes
        sm.set_absolute_cursor(550, 550, &cfg);
        for step in 1..=10 {
            let _ = sm.tick(t0 + Duration::from_millis(step * 16), &cfg, &mut emitter);
        }
        assert!(emitter.events.iter().any(|(v, h, _)| *v && *h < 0));
        assert!(emitter.events.iter().any(|(v, h, _)| !*v && *h > 0));

        // Now disable horizontal scrolling: moving sideways only must NOT scroll
        emitter.events.clear();
        cfg.allow_horizontal = false;
        sm.set_absolute_cursor(600, 500, &cfg);
        for step in 11..=15 {
            let _ = sm.tick(t0 + Duration::from_millis(step * 16), &cfg, &mut emitter);
        }
        assert!(emitter.events.is_empty());
    }

    #[test]
    fn suppresses_activation_completely_in_onshape_and_bambu_studio() {
        let mut sm = ScrollStateMachine::new();
        let cfg = ConfigData::default();
        let mut emitter = MockEmitter::default();
        let t0 = Instant::now();

        assert!(
            sm.on_middle_down(
                t0,
                &cfg,
                (500, 400),
                (500, 400),
                true,
                || Some("onshape".to_string())
            )
            .is_none()
        );
        assert!(sm.is_idle());

        let cmds = sm.tick(t0 + Duration::from_millis(400), &cfg, &mut emitter);
        assert!(cmds.is_empty());
        assert!(emitter.events.is_empty());
    }

    #[test]
    fn optional_protect_3d_drag_aborts_immediate_movement_when_enabled() {
        let mut sm = ScrollStateMachine::new();
        let cfg = ConfigData {
            protect_3d_drag: true,
            ..Default::default()
        };
        let mut emitter = MockEmitter::default();
        let t0 = Instant::now();

        assert!(
            sm.on_middle_down(t0, &cfg, (500, 400), (500, 400), false, || None)
                .is_none()
        );
        assert!(sm.is_pending());

        sm.on_mouse_move(10.0, 7.0, &cfg);
        assert!(sm.is_idle());

        let cmds = sm.tick(t0 + Duration::from_millis(500), &cfg, &mut emitter);
        assert!(cmds.is_empty());
        assert!(emitter.events.is_empty());
    }

    #[test]
    fn physical_wheel_and_second_middle_click_stop_active_autoscroll() {
        let mut sm = ScrollStateMachine::new();
        let cfg = ConfigData::default();
        let mut emitter = MockEmitter::default();
        let t0 = Instant::now();

        let _ = sm.on_middle_down(t0, &cfg, (400, 400), (400, 400), true, || None);
        let cmds = sm.tick(t0 + Duration::from_millis(180), &cfg, &mut emitter);
        assert!(cmds.iter().any(|c| matches!(c, IndicatorCommand::Show { .. })));
        assert!(
            sm.on_middle_up(t0 + Duration::from_millis(220), &cfg)
                .is_none()
        );
        assert!(sm.is_active());

        // Spinning physical wheel within grace window (< 150ms after activation at t0+180ms) is ignored
        assert!(sm.on_physical_wheel(t0 + Duration::from_millis(260)).is_none());
        assert!(sm.is_active());

        // Spinning physical wheel after grace window stops autoscroll
        let hide_cmd = sm.on_physical_wheel(t0 + Duration::from_millis(400));
        assert_eq!(hide_cmd, Some(IndicatorCommand::Hide));
        assert!(sm.is_idle());

        // Reactivate and verify second middle click stops Einrast-Modus
        let t1 = t0 + Duration::from_millis(500);
        let _ = sm.on_middle_down(t1, &cfg, (400, 400), (400, 400), true, || None);
        let _ = sm.tick(t1 + Duration::from_millis(180), &cfg, &mut emitter);
        let _ = sm.on_middle_up(t1 + Duration::from_millis(220), &cfg);
        assert!(sm.is_active());
        assert_eq!(
            sm.on_middle_down(
                t1 + Duration::from_millis(320),
                &cfg,
                (400, 400),
                (400, 400),
                true,
                || None,
            ),
            Some(IndicatorCommand::Hide)
        );
        assert!(sm.is_idle());
    }

    #[test]
    fn auto_cancels_unscrolled_anchor_on_tab_close_or_title_change_but_preserves_active_scroll() {
        let mut sm = ScrollStateMachine::new();
        let cfg = ConfigData::default();
        let mut emitter = MockEmitter::default();
        let t0 = Instant::now();

        // 1. Anchor activates after brief hold, then tab/title changes before any scroll -> auto-cancels
        let _ = sm.on_middle_down(t0, &cfg, (800, 300), (800, 300), true, || None);
        let _ = sm.tick(t0 + Duration::from_millis(180), &cfg, &mut emitter);
        let _ = sm.on_middle_up(t0 + Duration::from_millis(210), &cfg);
        assert!(sm.is_active());

        let cancel = sm.on_window_context_changed(t0 + Duration::from_millis(240), &cfg);
        assert_eq!(cancel, Some(IndicatorCommand::Hide));
        assert!(sm.is_idle());

        // 2. Anchor starts via drag and user actually scrolls -> subsequent title updates do NOT cancel
        let _ = sm.on_middle_down(t0, &cfg, (800, 300), (800, 300), true, || None);
        sm.set_absolute_cursor(800, 360, &cfg);
        let _ = sm.tick(t0 + Duration::from_millis(20), &cfg, &mut emitter);
        assert!(
            sm.on_window_context_changed(t0 + Duration::from_millis(100), &cfg)
                .is_none()
        );
        assert!(sm.is_active());
    }

    #[test]
    fn scroll_rate_curve_scales_progressively() {
        assert_eq!(calc_scroll_rate(0.0, 1.0, 1.3), 0.0);
        let slow = calc_scroll_rate(5.0, 1.0, 1.3);
        let med = calc_scroll_rate(30.0, 1.0, 1.3);
        let fast = calc_scroll_rate(120.0, 1.0, 1.3);
        assert!(slow > 0.5 && slow < 1.5);
        assert!(med > slow * 5.0);
        assert!(fast > 50.0 && fast <= 100.0);
    }

    #[test]
    fn wake_pipe_enforces_cloexec_and_nonblock_and_writes_cleanly() {
        let (read_f, write_f) = create_cloexec_nonblock_pipe().unwrap();
        for f in [&read_f, &write_f] {
            let fd_flags = unsafe { libc::fcntl(f.as_raw_fd(), libc::F_GETFD) };
            assert!(fd_flags >= 0);
            assert_ne!(fd_flags & libc::FD_CLOEXEC, 0);

            let fl_flags = unsafe { libc::fcntl(f.as_raw_fd(), libc::F_GETFL) };
            assert!(fl_flags >= 0);
            assert_ne!(fl_flags & libc::O_NONBLOCK, 0);
        }

        let payload = b"breeze-ev";
        write_all_retry_eintr(write_f.as_raw_fd(), payload).unwrap();
        let mut out = [0u8; 9];
        let n = unsafe {
            libc::read(
                read_f.as_raw_fd(),
                out.as_mut_ptr().cast::<libc::c_void>(),
                out.len(),
            )
        };
        assert_eq!(n, payload.len() as isize);
        assert_eq!(&out, payload);

        // Closed read end causes write error instead of silent ignore
        drop(read_f);
        assert!(write_all_retry_eintr(write_f.as_raw_fd(), b"x").is_err());
    }

    #[test]
    fn parse_inotify_buffer_filters_event_nodes_and_handles_overflow() {
        fn build_event(mask: u32, name: &[u8]) -> Vec<u8> {
            let mut padded_name = name.to_vec();
            if !padded_name.is_empty() {
                padded_name.push(0);
                while !padded_name.len().is_multiple_of(4) {
                    padded_name.push(0);
                }
            }
            let ev = libc::inotify_event {
                wd: 1,
                mask,
                cookie: 0,
                len: padded_name.len() as u32,
            };
            let hdr = unsafe {
                std::slice::from_raw_parts(
                    (&ev as *const libc::inotify_event).cast::<u8>(),
                    std::mem::size_of::<libc::inotify_event>(),
                )
            };
            let mut buf = Vec::with_capacity(hdr.len() + padded_name.len());
            buf.extend_from_slice(hdr);
            buf.extend_from_slice(&padded_name);
            buf
        }

        // Non-event files (mice, js0) do not trigger rescan
        let mut ignored = build_event(libc::IN_CREATE, b"mice");
        ignored.extend_from_slice(&build_event(libc::IN_ATTRIB, b"js0"));
        assert!(!parse_inotify_buffer(&ignored));

        // event* device creation triggers rescan
        let created = build_event(libc::IN_CREATE, b"event12");
        assert!(parse_inotify_buffer(&created));

        // Queue overflow always triggers rescan
        let overflow = build_event(libc::IN_Q_OVERFLOW, b"");
        assert!(parse_inotify_buffer(&overflow));
    }

    #[test]
    fn tick_into_reuses_buffer_across_ticks() {
        let mut sm = ScrollStateMachine::new();
        let cfg = ConfigData::default();
        let mut emitter = MockEmitter::default();
        let t0 = Instant::now();

        let _ = sm.on_middle_down(t0, &cfg, (500, 500), (500, 500), true, || None);
        let mut buf = Vec::with_capacity(8);
        sm.tick_into(t0 + Duration::from_millis(180), &cfg, &mut emitter, &mut buf);
        assert_eq!(buf.len(), 1);
        assert!(matches!(buf[0], IndicatorCommand::Show { .. }));

        // Subsequent tick clears previous commands and writes only the new UpdateOffset
        sm.tick_into(t0 + Duration::from_millis(196), &cfg, &mut emitter, &mut buf);
        assert_eq!(buf.len(), 1);
        assert!(matches!(buf[0], IndicatorCommand::UpdateOffset { .. }));
    }
}
