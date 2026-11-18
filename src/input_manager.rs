//! Low-level Linux `evdev` mouse/keyboard listener, `uinput` virtual wheel emitter,
//! and 3D-orbit-protected autoscroll state machine.

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::config::{ConfigData, ConfigManager};
use crate::window_detector::WindowTracker;

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
            .open(uinput_path)
            .map_err(|err| {
                if err.kind() == io::ErrorKind::PermissionDenied {
                    "Keine Schreibrechte auf /dev/uinput. Bitte 'install.sh' ausführen oder Benutzer zur Gruppe 'input' hinzufügen.".to_string()
                } else {
                    format!("Fehler beim Öffnen von /dev/uinput: {err}")
                }
            })?;

        let fd = file.as_raw_fd();

        unsafe {
            ioctl_check(fd, UI_SET_EVBIT, EV_KEY as libc::c_int, "UI_SET_EVBIT(EV_KEY)")?;
            ioctl_check(fd, UI_SET_EVBIT, EV_REL as libc::c_int, "UI_SET_EVBIT(EV_REL)")?;

            for btn in [BTN_LEFT, BTN_RIGHT, BTN_MIDDLE, BTN_SIDE, BTN_EXTRA] {
                ioctl_check(fd, UI_SET_KEYBIT, btn as libc::c_int, "UI_SET_KEYBIT")?;
            }
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

    fn write_event(&self, type_: u16, code: u16, value: i32) {
        let ev = InputEvent {
            time: libc::timeval {
                tv_sec: 0,
                tv_usec: 0,
            },
            type_,
            code,
            value,
        };
        let ptr = (&ev as *const InputEvent).cast::<libc::c_void>();
        let size = std::mem::size_of::<InputEvent>();
        unsafe {
            let _ = libc::write(self.file.as_raw_fd(), ptr, size);
        }
    }
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

        if std_delta != 0 {
            self.write_event(EV_REL, std_code, std_delta);
        }
        if hires_delta != 0 {
            self.write_event(EV_REL, hires_code, hires_delta);
        }
        self.write_event(EV_SYN, SYN_REPORT, 0);
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

/// Distance moved while middle button is held before releasing to distinguish click-toggle vs drag.
const SMART_LATCH_MAX_DIST_PX: f32 = 20.0;
const SMART_LATCH_MAX_ACTIVE_MS: u64 = 350;
const TOGGLE_CLICK_MAX_MS: u64 = 300;
/// Ignore accidental physical wheel jitter immediately after BTN_MIDDLE press (wobbly wheel click).
const WHEEL_CANCEL_GRACE_MS: u64 = 150;

#[derive(Debug, Clone, PartialEq)]
enum ScrollPhase {
    Idle,
    /// Middle button was pressed on a non-excluded window; waiting to distinguish:
    /// - Quick middle click (< hold_delay_ms)
    /// - 3D Orbit/Pan drag (mouse moves >= drag_cancel_threshold_px while held)
    /// - Intentional stationary hold for Autoscroll (>= hold_delay_ms without drag)
    Pending {
        pressed_at: Instant,
        anchor_x: i32,
        anchor_y: i32,
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
        toggle_latched: bool,
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

    /// Handles `BTN_MIDDLE` press.
    ///
    /// Returns `Some(IndicatorCommand::Hide)` if an active toggle session was stopped.
    pub fn on_middle_down<F>(
        &mut self,
        now: Instant,
        config: &ConfigData,
        cursor_pos: (i32, i32),
        is_excluded_context: F,
    ) -> Option<IndicatorCommand>
    where
        F: FnOnce() -> Option<String>,
    {
        if !config.enabled {
            return self.force_stop();
        }

        // Pressing middle button while active always stops autoscroll.
        if self.is_active() {
            return self.force_stop();
        }

        if self.is_pending() {
            return None;
        }

        // Layer 2 protection: completely ignore middle-click in excluded 3D/CAD/Slicer windows
        // and excluded browser tabs (e.g. Onshape, Bambu Studio, OrcaSlicer, Blender, FreeCAD).
        if let Some(_matched) = is_excluded_context() {
            return None;
        }

        self.phase = ScrollPhase::Pending {
            pressed_at: now,
            anchor_x: cursor_pos.0,
            anchor_y: cursor_pos.1,
            move_x: 0.0,
            move_y: 0.0,
        };
        None
    }

    /// Handles raw relative mouse movement (`REL_X`, `REL_Y`).
    pub fn on_mouse_move(&mut self, dx: f32, dy: f32, config: &ConfigData) {
        match &mut self.phase {
            ScrollPhase::Idle => {}
            ScrollPhase::Pending { move_x, move_y, .. } => {
                *move_x += dx;
                *move_y += dy;

                // Layer 1 protection: if the user immediately drags while holding BTN_MIDDLE,
                // this is a native Middle-Mouse Drag (3D Orbit / Camera Pan / Canvas Drag),
                // NOT a stationary autoscroll hold! Abort pending immediately.
                if config.protect_3d_drag {
                    let dist = move_x.hypot(*move_y);
                    if dist >= config.drag_cancel_threshold_px {
                        self.phase = ScrollPhase::Idle;
                    }
                }
            }
            ScrollPhase::Active {
                delta_x, delta_y, ..
            } => {
                *delta_x += dx;
                *delta_y += dy;
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
                move_x,
                move_y,
            } => {
                let held = now.saturating_duration_since(*pressed_at);
                let dist = move_x.hypot(*move_y);

                // In dedicated "toggle" mode, a crisp stationary click (< 300ms, < drag threshold)
                // latches autoscroll open upon release.
                if config.mode == "toggle"
                    && held < Duration::from_millis(TOGGLE_CLICK_MAX_MS)
                    && dist < config.drag_cancel_threshold_px
                {
                    let ax = *anchor_x;
                    let ay = *anchor_y;
                    self.phase = ScrollPhase::Active {
                        started_at: now,
                        last_tick_at: now,
                        anchor_x: ax,
                        anchor_y: ay,
                        delta_x: 0.0,
                        delta_y: 0.0,
                        toggle_latched: true,
                        accum_hires_x: 0.0,
                        accum_hires_y: 0.0,
                        pending_std_hires_x: 0,
                        pending_std_hires_y: 0,
                    };
                    return Some(IndicatorCommand::Show {
                        anchor_x: ax,
                        anchor_y: ay,
                        deadzone: config.deadzone,
                        theme: config.theme.clone(),
                        opacity: config.indicator_opacity,
                    });
                }

                // In "smart" and "drag" modes, releasing before hold_delay_ms cancels pending
                // cleanly so normal middle-click (close tab, open link, paste) works naturally.
                self.phase = ScrollPhase::Idle;
                None
            }
            ScrollPhase::Active {
                started_at,
                delta_x,
                delta_y,
                toggle_latched,
                ..
            } => {
                if *toggle_latched {
                    // Already latched earlier; ignore stray release
                    return None;
                }

                match config.mode.as_str() {
                    "drag" => {
                        self.phase = ScrollPhase::Idle;
                        Some(IndicatorCommand::Hide)
                    }
                    "toggle" => {
                        *toggle_latched = true;
                        None
                    }
                    _ => {
                        // "smart" hybrid mode:
                        // Short hold after activation with minimal pointer displacement -> latch toggle
                        // Long hold or dragged far -> stop immediately on release
                        let active_dur = now.saturating_duration_since(*started_at);
                        let dist = delta_x.hypot(*delta_y);
                        if active_dur < Duration::from_millis(SMART_LATCH_MAX_ACTIVE_MS)
                            && dist < SMART_LATCH_MAX_DIST_PX
                        {
                            *toggle_latched = true;
                            None
                        } else {
                            self.phase = ScrollPhase::Idle;
                            Some(IndicatorCommand::Hide)
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

    /// Advances timers and emits smooth hi-res scroll wheel ticks.
    pub fn tick<E: WheelEmitter>(
        &mut self,
        now: Instant,
        config: &ConfigData,
        emitter: &mut E,
    ) -> Vec<IndicatorCommand> {
        let mut commands = Vec::new();

        if !config.enabled {
            if let Some(cmd) = self.force_stop() {
                commands.push(cmd);
            }
            return commands;
        }

        // Transition Pending -> Active once hold_delay_ms elapses without 3D-drag abort
        if let ScrollPhase::Pending {
            pressed_at,
            anchor_x,
            anchor_y,
            move_x,
            move_y,
        } = self.phase
        {
            let elapsed = now.saturating_duration_since(pressed_at);
            if elapsed >= Duration::from_millis(config.hold_delay_ms) {
                let dist = move_x.hypot(move_y);
                if config.protect_3d_drag && dist >= config.drag_cancel_threshold_px {
                    self.phase = ScrollPhase::Idle;
                    return commands;
                }

                self.phase = ScrollPhase::Active {
                    started_at: now,
                    last_tick_at: now,
                    anchor_x,
                    anchor_y,
                    // Always start delta at (0, 0) upon activation — never leak pre-activation jitter
                    delta_x: 0.0,
                    delta_y: 0.0,
                    toggle_latched: false,
                    accum_hires_x: 0.0,
                    accum_hires_y: 0.0,
                    pending_std_hires_x: 0,
                    pending_std_hires_y: 0,
                };
                commands.push(IndicatorCommand::Show {
                    anchor_x,
                    anchor_y,
                    deadzone: config.deadzone,
                    theme: config.theme.clone(),
                    opacity: config.indicator_opacity,
                });
            }
            return commands;
        }

        if let ScrollPhase::Active {
            last_tick_at,
            delta_x,
            delta_y,
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

            // ---- Vertical Scrolling ----
            let abs_dy = dy.abs();
            if abs_dy > deadzone {
                let mut rate_notches_per_sec = calc_scroll_rate(abs_dy - deadzone, speed_mult);
                if config.invert_vertical {
                    rate_notches_per_sec = -rate_notches_per_sec;
                }
                // Mouse DOWN (dy > 0) -> page scrolls DOWN -> REL_WHEEL < 0
                // Mouse UP   (dy < 0) -> page scrolls UP   -> REL_WHEEL > 0
                let signed_hires_rate = if dy < 0.0 {
                    rate_notches_per_sec * (HI_RES_PER_NOTCH as f32)
                } else {
                    -rate_notches_per_sec * (HI_RES_PER_NOTCH as f32)
                };

                *accum_hires_y += signed_hires_rate * dt;
                flush_axis_scroll(accum_hires_y, pending_std_hires_y, true, emitter);
            } else {
                *accum_hires_y = 0.0;
            }

            // ---- Horizontal Scrolling ----
            if config.allow_horizontal {
                let abs_dx = dx.abs();
                if abs_dx > deadzone {
                    let mut rate_notches_per_sec = calc_scroll_rate(abs_dx - deadzone, speed_mult);
                    if config.invert_horizontal {
                        rate_notches_per_sec = -rate_notches_per_sec;
                    }
                    let signed_hires_rate = if dx > 0.0 {
                        rate_notches_per_sec * (HI_RES_PER_NOTCH as f32)
                    } else {
                        -rate_notches_per_sec * (HI_RES_PER_NOTCH as f32)
                    };

                    *accum_hires_x += signed_hires_rate * dt;
                    flush_axis_scroll(accum_hires_x, pending_std_hires_x, false, emitter);
                } else {
                    *accum_hires_x = 0.0;
                }
            }
        }

        commands
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
/// Quadratic acceleration curve: gentle fine control near deadzone, rapid scan farther out.
pub fn calc_scroll_rate(effective_dist: f32, speed_mult: f32) -> f32 {
    if effective_dist <= 0.0 {
        return 0.0;
    }
    let rate = speed_mult * (0.25 * effective_dist + 0.004 * effective_dist * effective_dist);
    rate.min(80.0)
}

// ============================================================================
// evdev Device Discovery & Background Input Thread
// ============================================================================

struct EvdevDevice {
    path: PathBuf,
    file: File,
    is_mouse: bool,
    has_esc: bool,
}

fn find_input_devices() -> Vec<EvdevDevice> {
    let mut result = Vec::new();
    let Ok(entries) = fs::read_dir("/dev/input") else {
        return result;
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

    for path in paths {
        let Ok(file) = OpenOptions::new().read(true).open(&path) else {
            continue;
        };
        let fd = file.as_raw_fd();

        // Set non-blocking mode for select/poll-free drain reads
        unsafe {
            let flags = libc::fcntl(fd, libc::F_GETFL);
            if flags >= 0 {
                libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
            }
        }

        let name = read_device_name(fd);
        if name.contains("Breeze Autoscroll") {
            continue;
        }

        let is_mouse = device_is_mouse(fd);
        let has_esc = device_has_key(fd, KEY_ESC);
        if is_mouse || has_esc {
            result.push(EvdevDevice {
                path,
                file,
                is_mouse,
                has_esc,
            });
        }
    }

    result
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
    wake_write_fd: RawFd,
}

impl InputEngine {
    /// Starts the input engine background thread.
    pub fn start(
        config: ConfigManager,
        window_tracker: WindowTracker,
        indicator_tx: Sender<IndicatorCommand>,
    ) -> Result<Self, String> {
        let mut emitter = UInputEmitter::create()?;

        // Self-pipe to wake up `poll` immediately on shutdown
        let mut pipe_fds = [-1i32; 2];
        if unsafe { libc::pipe(pipe_fds.as_mut_ptr()) } < 0 {
            return Err("Konnte interne Steuer-Pipe nicht anlegen".to_string());
        }
        let wake_read_fd = pipe_fds[0];
        let wake_write_fd = pipe_fds[1];

        let running = Arc::new(AtomicBool::new(true));
        let running_clone = Arc::clone(&running);

        let handle = thread::Builder::new()
            .name("breeze-input-engine".to_string())
            .spawn(move || {
                // Keep wake_read_fd owned inside thread for automatic cleanup on exit
                let _wake_read_guard = unsafe { File::from_raw_fd(wake_read_fd) };
                run_engine_loop(
                    running_clone,
                    wake_read_fd,
                    &mut emitter,
                    config,
                    window_tracker,
                    indicator_tx,
                );
            })
            .map_err(|err| format!("Fehler beim Starten des Input-Threads: {err}"))?;

        Ok(Self {
            running,
            handle: Some(handle),
            wake_write_fd,
        })
    }

    /// Stops the background input engine cleanly.
    pub fn stop(&mut self) {
        self.running.store(false, Ordering::SeqCst);
        if self.wake_write_fd >= 0 {
            let byte = [1u8; 1];
            unsafe {
                let _ = libc::write(self.wake_write_fd, byte.as_ptr().cast(), 1);
                libc::close(self.wake_write_fd);
            }
            self.wake_write_fd = -1;
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

fn run_engine_loop(
    running: Arc<AtomicBool>,
    wake_read_fd: RawFd,
    emitter: &mut UInputEmitter,
    config_mgr: ConfigManager,
    window_tracker: WindowTracker,
    indicator_tx: Sender<IndicatorCommand>,
) {
    let mut state_machine = ScrollStateMachine::new();
    let mut devices = find_input_devices();
    let mut last_rescan = Instant::now();
    let mut needs_rescan = false;

    while running.load(Ordering::SeqCst) {
        let now = Instant::now();
        if needs_rescan || now.saturating_duration_since(last_rescan) >= Duration::from_secs(3) {
            refresh_devices(&mut devices);
            last_rescan = now;
            needs_rescan = false;
        }

        let cfg = config_mgr.snapshot();

        // Build pollfd list: wake pipe + all input fds
        let mut pollfds: Vec<libc::pollfd> = Vec::with_capacity(devices.len() + 1);
        pollfds.push(libc::pollfd {
            fd: wake_read_fd,
            events: libc::POLLIN,
            revents: 0,
        });
        for dev in &devices {
            pollfds.push(libc::pollfd {
                fd: dev.file.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            });
        }

        // Poll quickly (~16ms) while pending/active, sleep up to 500ms when idle
        let timeout_ms: libc::c_int = if state_machine.is_idle() {
            500
        } else {
            cfg.tick_interval_ms.clamp(8, 50) as libc::c_int
        };

        let rc = unsafe { libc::poll(pollfds.as_mut_ptr(), pollfds.len() as libc::nfds_t, timeout_ms) };
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

        let mut actions = Vec::new();
        if rc > 0 {
            if (pollfds[0].revents & libc::POLLIN) != 0 {
                break;
            }
            for (idx, pfd) in pollfds.iter().skip(1).enumerate() {
                if (pfd.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL)) != 0 {
                    needs_rescan = true;
                    continue;
                }
                if (pfd.revents & libc::POLLIN) != 0
                    && let Some(dev) = devices.get(idx)
                {
                    if !read_device_events(dev, &mut actions) {
                        needs_rescan = true;
                    }
                }
            }
        }

        let event_now = Instant::now();
        for action in actions {
            let cmd = match action {
                RawEvdevAction::MiddleDown => {
                    let cursor_pos = window_tracker.get_cursor_pos();
                    state_machine.on_middle_down(event_now, &cfg, cursor_pos, || {
                        window_tracker.check_excluded_context(&cfg)
                    })
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
                let _ = indicator_tx.send(c);
            }
        }

        for cmd in state_machine.tick(Instant::now(), &cfg, emitter) {
            let _ = indicator_tx.send(cmd);
        }
    }

    if let Some(cmd) = state_machine.force_stop() {
        let _ = indicator_tx.send(cmd);
    }
    let _ = indicator_tx.send(IndicatorCommand::Shutdown);
}

fn refresh_devices(devices: &mut Vec<EvdevDevice>) {
    let existing: HashMap<PathBuf, ()> = devices.iter().map(|d| (d.path.clone(), ())).collect();
    let discovered = find_input_devices();
    let mut changed = discovered.len() != existing.len();
    if !changed {
        for d in &discovered {
            if !existing.contains_key(&d.path) {
                changed = true;
                break;
            }
        }
    }
    if changed {
        *devices = discovered;
    }
}

fn read_device_events(dev: &EvdevDevice, out: &mut Vec<RawEvdevAction>) -> bool {
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
                } else if dev.is_mouse
                    && ev.value == 1
                    && matches!(ev.code, BTN_LEFT | BTN_RIGHT | BTN_SIDE | BTN_EXTRA)
                {
                    out.push(RawEvdevAction::CancelInput);
                } else if dev.has_esc && ev.value == 1 && ev.code == KEY_ESC {
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
    fn aborts_immediately_on_3d_orbit_drag_even_in_unlisted_app() {
        let mut sm = ScrollStateMachine::new();
        let cfg = ConfigData::default();
        let mut emitter = MockEmitter::default();
        let t0 = Instant::now();

        // Press BTN_MIDDLE on an unlisted website (Layer 2 allows it)
        assert!(sm.on_middle_down(t0, &cfg, (500, 400), || None).is_none());
        assert!(sm.is_pending());

        // Immediately drag 12px (simulating 3D camera orbit in an unknown WebGL app)
        sm.on_mouse_move(10.0, 7.0, &cfg);

        // Layer 1 protection must have aborted Pending -> Idle immediately!
        assert!(sm.is_idle());

        // Even after 500ms while holding BTN_MIDDLE, autoscroll MUST NOT activate
        let cmds = sm.tick(t0 + Duration::from_millis(500), &cfg, &mut emitter);
        assert!(cmds.is_empty());
        assert!(sm.is_idle());
        assert!(emitter.events.is_empty());

        // Releasing BTN_MIDDLE stays Idle cleanly
        assert!(
            sm.on_middle_up(t0 + Duration::from_millis(600), &cfg)
                .is_none()
        );
        assert!(sm.is_idle());
    }

    #[test]
    fn suppresses_activation_completely_in_onshape_and_bambu_studio() {
        let mut sm = ScrollStateMachine::new();
        let cfg = ConfigData::default();
        let mut emitter = MockEmitter::default();
        let t0 = Instant::now();

        // Layer 2 detects Onshape / Bambu Studio -> never even enters Pending!
        assert!(
            sm.on_middle_down(t0, &cfg, (500, 400), || Some("onshape".to_string()))
                .is_none()
        );
        assert!(sm.is_idle());

        // Even if user pauses stationary for 400ms and then moves, autoscroll stays disabled
        let cmds = sm.tick(t0 + Duration::from_millis(400), &cfg, &mut emitter);
        assert!(cmds.is_empty());
        sm.on_mouse_move(0.0, 50.0, &cfg);
        let cmds2 = sm.tick(t0 + Duration::from_millis(416), &cfg, &mut emitter);
        assert!(cmds2.is_empty());
        assert!(emitter.events.is_empty());
    }

    #[test]
    fn quick_middle_click_in_smart_mode_does_not_activate_autoscroll() {
        let mut sm = ScrollStateMachine::new();
        let cfg = ConfigData::default();
        let t0 = Instant::now();

        sm.on_middle_down(t0, &cfg, (300, 300), || None);
        assert!(sm.is_pending());

        // Release after 90ms (closing a browser tab)
        let res = sm.on_middle_up(t0 + Duration::from_millis(90), &cfg);
        assert!(res.is_none());
        assert!(sm.is_idle());
    }

    #[test]
    fn stationary_hold_activates_and_emits_smooth_hires_scroll() {
        let mut sm = ScrollStateMachine::new();
        let cfg = ConfigData::default();
        let mut emitter = MockEmitter::default();
        let t0 = Instant::now();

        sm.on_middle_down(t0, &cfg, (600, 500), || None);
        // Tiny sub-threshold jitter (2px < 5px) must NOT abort
        sm.on_mouse_move(1.5, -1.0, &cfg);
        assert!(sm.is_pending());

        // Hold stationary past hold_delay_ms (180ms)
        let cmds = sm.tick(t0 + Duration::from_millis(190), &cfg, &mut emitter);
        assert!(sm.is_active());
        assert_eq!(
            cmds,
            vec![IndicatorCommand::Show {
                anchor_x: 600,
                anchor_y: 500,
                deadzone: 12,
                theme: "dark".to_string(),
                opacity: 0.92,
            }]
        );

        // Move pointer 60px downward beyond deadzone (total dy = 60, effective = 48)
        sm.on_mouse_move(0.0, 60.0, &cfg);
        for step in 1..=10 {
            let t = t0 + Duration::from_millis(190 + step * 16);
            let _ = sm.tick(t, &cfg, &mut emitter);
        }

        // Expect downward (negative) hi-res scroll events emitted smoothly
        assert!(!emitter.events.is_empty());
        assert!(emitter.events.iter().all(|(vert, hires, _)| *vert && *hires < 0));

        // Release after dragging (> 350ms or > 20px) stops autoscroll
        let stop_cmd = sm.on_middle_up(t0 + Duration::from_millis(400), &cfg);
        assert_eq!(stop_cmd, Some(IndicatorCommand::Hide));
        assert!(sm.is_idle());
    }

    #[test]
    fn toggle_mode_latches_on_quick_click_and_cancels_on_esc_or_wheel() {
        let mut sm = ScrollStateMachine::new();
        let mut cfg = ConfigData::default();
        cfg.mode = "toggle".to_string();
        let emitter = MockEmitter::default();
        let t0 = Instant::now();

        sm.on_middle_down(t0, &cfg, (400, 400), || None);
        let show_cmd = sm.on_middle_up(t0 + Duration::from_millis(80), &cfg);
        assert!(matches!(show_cmd, Some(IndicatorCommand::Show { .. })));
        assert!(sm.is_active());

        // Spinning physical wheel within grace window (< 150ms) is ignored
        assert!(sm.on_physical_wheel(t0 + Duration::from_millis(100)).is_none());
        assert!(sm.is_active());

        // Spinning physical wheel after grace window stops autoscroll
        let hide_cmd = sm.on_physical_wheel(t0 + Duration::from_millis(300));
        assert_eq!(hide_cmd, Some(IndicatorCommand::Hide));
        assert!(sm.is_idle());

        // Reactivate and verify ESC / button click cancels
        sm.on_middle_down(t0 + Duration::from_millis(400), &cfg, (400, 400), || None);
        let _ = sm.on_middle_up(t0 + Duration::from_millis(460), &cfg);
        assert!(sm.is_active());
        assert_eq!(sm.on_cancel_input(), Some(IndicatorCommand::Hide));
        assert!(sm.is_idle());

        let _ = emitter.events;
    }

    #[test]
    fn scroll_rate_curve_scales_smoothly() {
        assert_eq!(calc_scroll_rate(0.0, 1.0), 0.0);
        let slow = calc_scroll_rate(5.0, 1.0);
        let med = calc_scroll_rate(30.0, 1.0);
        let fast = calc_scroll_rate(100.0, 1.0);
        assert!(slow > 1.0 && slow < 2.0);
        assert!(med > 10.0 && med < 12.0);
        assert!(fast > 60.0 && fast <= 80.0);
    }
}
