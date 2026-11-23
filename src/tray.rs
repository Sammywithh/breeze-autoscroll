//! KDE Plasma 6 System Tray (`StatusNotifierItem` + `com.canonical.dbusmenu`)
//! and single-instance / KWin D-Bus service (`org.breeze.Autoscroll`).

use std::collections::HashMap;
use std::env;
use std::process::{Child, Command};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use zbus::fdo::DBusProxy;
use zbus::message::Header;
use zbus::object_server::SignalEmitter;
use zbus::wire::{OwnedObjectPath, OwnedVariant, Str, Structure, Variant};
use zbus::{Connection, ObjectServer, Proxy, connection, interface};

use crate::config::{ConfigManager, truncate_utf8_bytes};
use crate::window_detector::WindowTracker;

pub const SERVICE_NAME: &str = "org.breeze.Autoscroll";
pub const AUTOSCROLL_PATH: &str = "/Autoscroll";
pub const SNI_PATH: &str = "/StatusNotifierItem";
pub const MENU_PATH: &str = "/MenuBar";

const KWIN_BUS_NAME: &str = "org.kde.KWin";
const KWIN_OWNER_CACHE_TTL: Duration = Duration::from_secs(10);
const KWIN_MISMATCH_REFRESH_COOLDOWN: Duration = Duration::from_millis(200);
const KWIN_WARMUP_TIMEOUT_MS: u64 = 250;

const MAX_DBUS_META_BYTES: usize = 256;
const MAX_DBUS_TITLE_BYTES: usize = 1024;
const MAX_DBUS_COORD_BYTES: usize = 128;
const MAX_MENU_GROUP_IDS: usize = 64;
const MAX_MENU_PROP_NAME_BYTES: usize = 64;

pub type MenuItem = (i32, HashMap<String, OwnedVariant>, Vec<OwnedVariant>);

fn clamp_dbus_str(s: String, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        s
    } else {
        truncate_utf8_bytes(&s, max_bytes).to_string()
    }
}

/// Event-driven quit handle shared between the D-Bus tray menu and `main`'s `poll()` loop.
#[derive(Clone)]
pub struct QuitNotifier {
    requested: Arc<AtomicBool>,
    wake_write_fd: Arc<AtomicI32>,
}

impl QuitNotifier {
    pub fn new(requested: Arc<AtomicBool>, wake_write_fd: Arc<AtomicI32>) -> Self {
        Self {
            requested,
            wake_write_fd,
        }
    }

    pub fn request_quit(&self) {
        self.requested.store(true, Ordering::SeqCst);
        let fd = self.wake_write_fd.load(Ordering::Acquire);
        if fd >= 0 {
            let byte = [b'Q'];
            unsafe {
                let _ = libc::write(fd, byte.as_ptr().cast::<libc::c_void>(), 1);
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VerifyDecision {
    Allow,
    Deny,
    RefreshNeeded,
}

#[derive(Debug, Default)]
struct KWinOwnerCache {
    owner: Option<String>,
    refreshed_at: Option<Instant>,
    last_mismatch_refresh: Option<Instant>,
}

impl KWinOwnerCache {
    fn seed(&mut self, owner: Option<String>, now: Instant) {
        self.owner = owner;
        self.refreshed_at = Some(now);
    }

    fn check(&self, sender: &str, now: Instant) -> VerifyDecision {
        if sender.is_empty() {
            return VerifyDecision::Deny;
        }
        if self.owner.as_deref() == Some(sender)
            && self
                .refreshed_at
                .is_some_and(|t| now.saturating_duration_since(t) < KWIN_OWNER_CACHE_TTL)
        {
            return VerifyDecision::Allow;
        }
        if self.owner.as_deref() != Some(sender)
            && self
                .last_mismatch_refresh
                .is_some_and(|t| now.saturating_duration_since(t) < KWIN_MISMATCH_REFRESH_COOLDOWN)
        {
            return VerifyDecision::Deny;
        }
        VerifyDecision::RefreshNeeded
    }

    fn update_and_verify(&mut self, fresh_owner: Option<String>, sender: &str, now: Instant) -> bool {
        self.owner = fresh_owner;
        self.refreshed_at = Some(now);
        let allowed = !sender.is_empty() && self.owner.as_deref() == Some(sender);
        if !allowed {
            self.last_mismatch_refresh = Some(now);
        }
        allowed
    }
}

/// Verifies that incoming KWin callback D-Bus messages genuinely originate from the
/// current unique-name owner of `org.kde.KWin`.
#[derive(Clone, Default)]
struct KWinOwnerVerifier {
    cache: Arc<Mutex<KWinOwnerCache>>,
}

impl KWinOwnerVerifier {
    fn new() -> Self {
        Self::default()
    }

    async fn query_owner(conn: &Connection) -> Option<String> {
        let dbus = DBusProxy::new(conn).await.ok()?;
        let name = KWIN_BUS_NAME.try_into().ok()?;
        dbus.get_name_owner(name).await.ok().map(|u| u.to_string())
    }

    /// Pre-warms the cached `org.kde.KWin` owner at daemon startup using a short D-Bus timeout
    /// so the very first middle-click incurs zero extra lookup latency.
    async fn warm_up(&self) {
        let Ok(short_conn) = connection::Builder::session()
            .method_timeout(Duration::from_millis(KWIN_WARMUP_TIMEOUT_MS))
            .build()
            .await
        else {
            return;
        };
        if let Some(owner) = Self::query_owner(&short_conn).await
            && let Ok(mut guard) = self.cache.lock()
        {
            guard.seed(Some(owner), Instant::now());
        }
    }

    async fn verify_kwin_sender(
        &self,
        conn: &Connection,
        header: &Header<'_>,
    ) -> zbus::fdo::Result<()> {
        let sender = header
            .sender()
            .map(|s| s.as_str())
            .ok_or_else(|| zbus::fdo::Error::AccessDenied("Fehlender D-Bus-Sender".to_string()))?;

        let now = Instant::now();
        let decision = self
            .cache
            .lock()
            .map(|g| g.check(sender, now))
            .unwrap_or(VerifyDecision::RefreshNeeded);

        match decision {
            VerifyDecision::Allow => Ok(()),
            VerifyDecision::Deny => Err(zbus::fdo::Error::AccessDenied(
                "Nur org.kde.KWin darf diese Methode aufrufen".to_string(),
            )),
            VerifyDecision::RefreshNeeded => {
                let fresh_owner = Self::query_owner(conn).await;
                let allowed = self
                    .cache
                    .lock()
                    .map(|mut g| g.update_and_verify(fresh_owner, sender, Instant::now()))
                    .unwrap_or(false);
                if allowed {
                    Ok(())
                } else {
                    Err(zbus::fdo::Error::AccessDenied(
                        "Nur org.kde.KWin darf diese Methode aufrufen".to_string(),
                    ))
                }
            }
        }
    }
}

/// Renders a crisp 64x64 (or `size x size`) KDE Breeze tray icon in big-endian ARGB32
/// byte order (`[A, R, G, B]`), as required by `org.kde.StatusNotifierItem.IconPixmap`.
pub fn render_tray_icon_argb32_be(enabled: bool, size: usize) -> Vec<u8> {
    let mut out = vec![0u8; size * size * 4];
    let center = (size as f32) * 0.5;
    let r = (size as f32) * 0.42;
    let ss = 4usize;
    let inv_ss = 1.0 / (ss as f32);
    let total_samples = (ss * ss) as f32;

    let (bg, border, accent) = if enabled {
        (
            [35.0 / 255.0, 38.0 / 255.0, 41.0 / 255.0, 230.0 / 255.0],
            [61.0 / 255.0, 174.0 / 255.0, 233.0 / 255.0, 220.0 / 255.0],
            [61.0 / 255.0, 174.0 / 255.0, 233.0 / 255.0, 1.0],
        )
    } else {
        (
            [35.0 / 255.0, 38.0 / 255.0, 41.0 / 255.0, 160.0 / 255.0],
            [140.0 / 255.0, 145.0 / 255.0, 150.0 / 255.0, 140.0 / 255.0],
            [140.0 / 255.0, 145.0 / 255.0, 150.0 / 255.0, 180.0 / 255.0],
        )
    };

    let arrow_dist = r * 0.62;
    let arrow_size = (size as f32) * (5.0 / 64.0);
    let dot_radius = (size as f32) * (4.0 / 64.0);
    let stroke_half = (size as f32) * (1.25 / 64.0);

    for py in 0..size {
        for px in 0..size {
            let mut sum_r = 0.0f32;
            let mut sum_g = 0.0f32;
            let mut sum_b = 0.0f32;
            let mut sum_a = 0.0f32;

            for sy in 0..ss {
                for sx in 0..ss {
                    let x = (px as f32) + ((sx as f32) + 0.5) * inv_ss;
                    let y = (py as f32) + ((sy as f32) + 0.5) * inv_ss;
                    let dx = x - center;
                    let dy = y - center;
                    let dist = dx.hypot(dy);

                    let mut pixel = [0.0f32; 4];

                    if dist <= r {
                        pixel = over(bg, pixel);
                    }
                    if (dist - r).abs() <= stroke_half {
                        pixel = over(border, pixel);
                    }
                    if dist <= dot_radius {
                        pixel = over(accent, pixel);
                    }

                    // Four directional triangles at 0, 90, 180, 270 deg
                    for &angle in &[
                        0.0f32,
                        std::f32::consts::FRAC_PI_2,
                        std::f32::consts::PI,
                        -std::f32::consts::FRAC_PI_2,
                    ] {
                        let (sin_a, cos_a) = (-angle).sin_cos();
                        let rx = dx * cos_a - dy * sin_a;
                        let ry = dx * sin_a + dy * cos_a;
                        if point_in_triangle(
                            rx,
                            ry,
                            (0.0, -arrow_dist),
                            (-arrow_size, -arrow_dist + arrow_size * 1.2),
                            (arrow_size, -arrow_dist + arrow_size * 1.2),
                        ) {
                            pixel = over(accent, pixel);
                        }
                    }

                    sum_r += pixel[0] * pixel[3];
                    sum_g += pixel[1] * pixel[3];
                    sum_b += pixel[2] * pixel[3];
                    sum_a += pixel[3];
                }
            }

            let a = (sum_a / total_samples).clamp(0.0, 1.0);
            let (r_out, g_out, b_out) = if a > 1e-5 {
                (
                    (sum_r / sum_a).clamp(0.0, 1.0),
                    (sum_g / sum_a).clamp(0.0, 1.0),
                    (sum_b / sum_a).clamp(0.0, 1.0),
                )
            } else {
                (0.0, 0.0, 0.0)
            };

            let off = (py * size + px) * 4;
            out[off] = (a * 255.0).round() as u8;
            out[off + 1] = (r_out * 255.0).round() as u8;
            out[off + 2] = (g_out * 255.0).round() as u8;
            out[off + 3] = (b_out * 255.0).round() as u8;
        }
    }

    out
}

fn over(src: [f32; 4], dst: [f32; 4]) -> [f32; 4] {
    let out_a = src[3] + dst[3] * (1.0 - src[3]);
    if out_a <= 1e-6 {
        return [0.0; 4];
    }
    [
        (src[0] * src[3] + dst[0] * dst[3] * (1.0 - src[3])) / out_a,
        (src[1] * src[3] + dst[1] * dst[3] * (1.0 - src[3])) / out_a,
        (src[2] * src[3] + dst[2] * dst[3] * (1.0 - src[3])) / out_a,
        out_a,
    ]
}

fn point_in_triangle(
    px: f32,
    py: f32,
    a: (f32, f32),
    b: (f32, f32),
    c: (f32, f32),
) -> bool {
    let sign = |p1: (f32, f32), p2: (f32, f32), p3: (f32, f32)| -> f32 {
        (p1.0 - p3.0) * (p2.1 - p3.1) - (p2.0 - p3.0) * (p1.1 - p3.1)
    };
    let d1 = sign((px, py), a, b);
    let d2 = sign((px, py), b, c);
    let d3 = sign((px, py), c, a);
    let has_neg = (d1 < 0.0) || (d2 < 0.0) || (d3 < 0.0);
    let has_pos = (d1 > 0.0) || (d2 > 0.0) || (d3 > 0.0);
    !(has_neg && has_pos)
}

#[derive(Clone)]
struct TraySharedState {
    config: ConfigManager,
    window_tracker: WindowTracker,
    quit_notifier: QuitNotifier,
    kwin_verifier: KWinOwnerVerifier,
    menu_revision: Arc<AtomicU32>,
    settings_child: Arc<Mutex<Option<Child>>>,
    about_child: Arc<Mutex<Option<Child>>>,
}

impl TraySharedState {
    fn reap_slot(slot: &Arc<Mutex<Option<Child>>>) {
        let Ok(mut guard) = slot.lock() else {
            return;
        };
        if let Some(child) = guard.as_mut() {
            match child.try_wait() {
                Ok(Some(_)) | Err(_) => *guard = None,
                Ok(None) => {}
            }
        }
    }

    fn spawn_child_window(slot: &Arc<Mutex<Option<Child>>>, flag: &str) {
        let Ok(mut guard) = slot.lock() else {
            return;
        };
        if let Some(child) = guard.as_mut() {
            match child.try_wait() {
                Ok(None) => return,
                Ok(Some(_)) | Err(_) => *guard = None,
            }
        }
        let Ok(exe) = env::current_exe() else {
            return;
        };
        if let Ok(child) = Command::new(exe).arg(flag).spawn() {
            *guard = Some(child);
        }
    }

    fn open_settings(&self) {
        Self::spawn_child_window(&self.settings_child, "--settings-window");
    }

    fn open_about(&self) {
        Self::spawn_child_window(&self.about_child, "--about");
    }

    fn reap_exited_children(&self) {
        Self::reap_slot(&self.settings_child);
        Self::reap_slot(&self.about_child);
    }
}

/// Handle to the running session D-Bus & KDE System Tray service.
pub struct TrayService {
    _conn: Connection,
    state: TraySharedState,
}

impl TrayService {
    /// Opens (or focuses if already running) the tracked `--settings-window` child process.
    pub fn open_settings(&self) {
        self.state.open_settings();
    }

    /// Reaps any exited `--settings-window` or `--about` child processes so no zombies remain.
    pub fn reap_exited_children(&self) {
        self.state.reap_exited_children();
    }
}

async fn emit_tray_refresh(server: &ObjectServer, state: &TraySharedState) {
    if let Ok(sni_ref) = server
        .interface::<_, StatusNotifierItemIface>(SNI_PATH)
        .await
    {
        let emitter = sni_ref.signal_emitter();
        let _ = StatusNotifierItemIface::new_icon(emitter).await;
        let _ = StatusNotifierItemIface::new_tool_tip(emitter).await;
    }

    let rev = state.menu_revision.fetch_add(1, Ordering::SeqCst) + 1;
    if let Ok(menu_ref) = server.interface::<_, DbusMenuIface>(MENU_PATH).await {
        let emitter = menu_ref.signal_emitter();
        let _ = DbusMenuIface::layout_updated(emitter, rev, 0).await;
    }
}

// ============================================================================
// org.breeze.Autoscroll Control & KWin Tracker Receiver Interface
// ============================================================================

struct AutoscrollControlIface {
    state: TraySharedState,
}

#[interface(name = "org.breeze.Autoscroll")]
impl AutoscrollControlIface {
    #[zbus(name = "ShowSettings")]
    fn show_settings(&self) {
        self.state.open_settings();
    }

    #[zbus(name = "ReloadConfig")]
    async fn reload_config(&self, #[zbus(object_server)] server: &ObjectServer) {
        self.state.config.load();
        self.state.window_tracker.sync_atspi_config();
        emit_tray_refresh(server, &self.state).await;
    }

    #[allow(clippy::too_many_arguments)]
    #[zbus(name = "UpdateActiveWindow")]
    async fn update_active_window(
        &self,
        app_class: String,
        app_name: String,
        desktop_file: String,
        title: String,
        cursor_x: String,
        cursor_y: String,
        #[zbus(connection)] conn: &Connection,
        #[zbus(header)] header: Header<'_>,
    ) -> zbus::fdo::Result<()> {
        self.state
            .kwin_verifier
            .verify_kwin_sender(conn, &header)
            .await?;

        let app_class = clamp_dbus_str(app_class, MAX_DBUS_META_BYTES);
        let app_name = clamp_dbus_str(app_name, MAX_DBUS_META_BYTES);
        let desktop_file = clamp_dbus_str(desktop_file, MAX_DBUS_META_BYTES);
        let title = clamp_dbus_str(title, MAX_DBUS_TITLE_BYTES);
        let cursor_x = clamp_dbus_str(cursor_x, MAX_DBUS_COORD_BYTES);
        let cursor_y = clamp_dbus_str(cursor_y, MAX_DBUS_COORD_BYTES);

        let cx = cursor_x.trim().parse::<i32>().ok();
        let cy = cursor_y.trim().parse::<i32>().ok();
        self.state
            .window_tracker
            .update_from_kwin(app_class, app_name, desktop_file, title, cx, cy);
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    #[zbus(name = "ReportPointerProbe")]
    async fn report_pointer_probe(
        &self,
        coords: String,
        app_class: String,
        app_name: String,
        desktop_file: String,
        title: String,
        is_dock_or_desktop: String,
        win_geom: String,
        screen_geom: String,
        #[zbus(connection)] conn: &Connection,
        #[zbus(header)] header: Header<'_>,
    ) -> zbus::fdo::Result<()> {
        self.state
            .kwin_verifier
            .verify_kwin_sender(conn, &header)
            .await?;

        let coords = clamp_dbus_str(coords, MAX_DBUS_COORD_BYTES);
        let app_class = clamp_dbus_str(app_class, MAX_DBUS_META_BYTES);
        let app_name = clamp_dbus_str(app_name, MAX_DBUS_META_BYTES);
        let desktop_file = clamp_dbus_str(desktop_file, MAX_DBUS_META_BYTES);
        let title = clamp_dbus_str(title, MAX_DBUS_TITLE_BYTES);
        let is_dock_or_desktop = clamp_dbus_str(is_dock_or_desktop, MAX_DBUS_COORD_BYTES);
        let win_geom = clamp_dbus_str(win_geom, MAX_DBUS_COORD_BYTES);
        let screen_geom = clamp_dbus_str(screen_geom, MAX_DBUS_COORD_BYTES);

        let mut coord_parts = coords.split(',');
        let cx = coord_parts
            .next()
            .and_then(|s| s.trim().parse::<i32>().ok())
            .unwrap_or(0);
        let cy = coord_parts
            .next()
            .and_then(|s| s.trim().parse::<i32>().ok())
            .unwrap_or(0);
        let vw = coord_parts
            .next()
            .and_then(|s| s.trim().parse::<i32>().ok())
            .unwrap_or(0);
        let vh = coord_parts
            .next()
            .and_then(|s| s.trim().parse::<i32>().ok())
            .unwrap_or(0);
        let dock_or_desktop = is_dock_or_desktop.trim() == "1";

        let mut win_parts = win_geom.split(',');
        let wx = win_parts
            .next()
            .and_then(|s| s.trim().parse::<i32>().ok())
            .unwrap_or(0);
        let wy = win_parts
            .next()
            .and_then(|s| s.trim().parse::<i32>().ok())
            .unwrap_or(0);
        let ww = win_parts
            .next()
            .and_then(|s| s.trim().parse::<i32>().ok())
            .unwrap_or(0);
        let wh = win_parts
            .next()
            .and_then(|s| s.trim().parse::<i32>().ok())
            .unwrap_or(0);
        let fullscreen = win_parts.next().is_some_and(|s| s.trim() == "1");
        let borderless = win_parts.next().is_some_and(|s| s.trim() == "1");
        let w_pid = win_parts
            .next()
            .and_then(|s| s.trim().parse::<u32>().ok())
            .unwrap_or(0);

        let mut scr_parts = screen_geom.split(',');
        let sx = scr_parts
            .next()
            .and_then(|s| s.trim().parse::<i32>().ok())
            .unwrap_or(0);
        let sy = scr_parts
            .next()
            .and_then(|s| s.trim().parse::<i32>().ok())
            .unwrap_or(0);
        let s_scale = scr_parts
            .next()
            .and_then(|s| s.trim().parse::<f64>().ok())
            .filter(|v| v.is_finite())
            .unwrap_or(1.0);

        // Strictly non-blocking: AT-SPI2 / DRM / X11 enrichment happens off the zbus thread.
        self.state.window_tracker.report_pointer_probe(
            cx,
            cy,
            vw,
            vh,
            app_class,
            app_name,
            desktop_file,
            title,
            dock_or_desktop,
            wx,
            wy,
            ww,
            wh,
            w_pid,
            fullscreen,
            borderless,
            sx,
            sy,
            s_scale,
        );
        Ok(())
    }

    #[zbus(name = "UpdateCursorPos")]
    async fn update_cursor_pos(
        &self,
        cursor_x: String,
        cursor_y: String,
        #[zbus(connection)] conn: &Connection,
        #[zbus(header)] header: Header<'_>,
    ) -> zbus::fdo::Result<()> {
        self.state
            .kwin_verifier
            .verify_kwin_sender(conn, &header)
            .await?;

        let cursor_x = clamp_dbus_str(cursor_x, MAX_DBUS_COORD_BYTES);
        let cursor_y = clamp_dbus_str(cursor_y, MAX_DBUS_COORD_BYTES);
        if let (Ok(cx), Ok(cy)) = (
            cursor_x.trim().parse::<i32>(),
            cursor_y.trim().parse::<i32>(),
        ) {
            self.state.window_tracker.update_cursor_pos(cx, cy);
        }
        Ok(())
    }

    #[zbus(name = "NotifyWindowContextChanged")]
    async fn notify_window_context_changed(
        &self,
        #[zbus(connection)] conn: &Connection,
        #[zbus(header)] header: Header<'_>,
    ) -> zbus::fdo::Result<()> {
        self.state
            .kwin_verifier
            .verify_kwin_sender(conn, &header)
            .await?;

        self.state.window_tracker.notify_window_context_changed();
        Ok(())
    }
}

#[cfg(feature = "debug-dbus")]
struct AutoscrollDebugIface {
    state: TraySharedState,
}

#[cfg(feature = "debug-dbus")]
#[interface(name = "org.breeze.Autoscroll.Debug")]
impl AutoscrollDebugIface {
    #[zbus(name = "TriggerProbe")]
    fn trigger_probe(&self) {
        self.state.window_tracker.trigger_probe();
    }

    #[zbus(name = "GetLastProbe")]
    fn get_last_probe(&self) -> String {
        self.state.window_tracker.stop_live_cursor();
        let Some(probe) = self.state.window_tracker.last_probe_snapshot() else {
            return "none".to_string();
        };
        let cfg = self.state.config.current();
        let interactive_chrome =
            crate::window_detector::is_interactive_window_chrome(&probe, &cfg);
        let win_desc = probe
            .window
            .as_ref()
            .map(|w| format!("class='{}' title='{}'", w.app_class, w.title))
            .unwrap_or_else(|| "none".to_string());
        format!(
            "from_kwin={} cursor=({}, {}) overlay=({}, {}) win=({}, {}, {}x{}) pid={} hotspot=({:?}, {:?}) interactive_cursor={} interactive_chrome={} atspi_role={:?} interactive_atspi={} dock_or_desktop={} {}",
            probe.from_kwin,
            probe.cursor_x,
            probe.cursor_y,
            probe.overlay_x,
            probe.overlay_y,
            probe.win_x,
            probe.win_y,
            probe.win_w,
            probe.win_h,
            probe.win_pid,
            probe.hotspot_x,
            probe.hotspot_y,
            probe.is_interactive_cursor,
            interactive_chrome,
            probe.atspi_role,
            probe.is_interactive_atspi,
            probe.is_dock_or_desktop,
            win_desc
        )
    }
}

// ============================================================================
// org.kde.StatusNotifierItem Interface
// ============================================================================

type IconPixmap = (i32, i32, Vec<u8>);
type ToolTip = (String, Vec<IconPixmap>, String, String);

struct StatusNotifierItemIface {
    state: TraySharedState,
}

#[interface(name = "org.kde.StatusNotifierItem")]
impl StatusNotifierItemIface {
    #[zbus(property, name = "Category")]
    fn category(&self) -> String {
        "ApplicationStatus".to_string()
    }

    #[zbus(property, name = "Id")]
    fn id(&self) -> String {
        "breeze-autoscroll".to_string()
    }

    #[zbus(property, name = "Title")]
    fn title(&self) -> String {
        "Breeze Autoscroll".to_string()
    }

    #[zbus(property, name = "Status")]
    fn status(&self) -> String {
        "Active".to_string()
    }

    #[zbus(property, name = "WindowId")]
    fn window_id(&self) -> i32 {
        0
    }

    #[zbus(property, name = "ItemIsMenu")]
    fn item_is_menu(&self) -> bool {
        false
    }

    #[zbus(property, name = "IconName")]
    fn icon_name(&self) -> String {
        String::new()
    }

    #[zbus(property, name = "IconPixmap")]
    fn icon_pixmap(&self) -> Vec<(i32, i32, Vec<u8>)> {
        let enabled = self.state.config.current().enabled;
        vec![
            (32, 32, render_tray_icon_argb32_be(enabled, 32)),
            (64, 64, render_tray_icon_argb32_be(enabled, 64)),
        ]
    }

    #[zbus(property, name = "OverlayIconName")]
    fn overlay_icon_name(&self) -> String {
        String::new()
    }

    #[zbus(property, name = "OverlayIconPixmap")]
    fn overlay_icon_pixmap(&self) -> Vec<(i32, i32, Vec<u8>)> {
        Vec::new()
    }

    #[zbus(property, name = "AttentionIconName")]
    fn attention_icon_name(&self) -> String {
        String::new()
    }

    #[zbus(property, name = "AttentionIconPixmap")]
    fn attention_icon_pixmap(&self) -> Vec<(i32, i32, Vec<u8>)> {
        Vec::new()
    }

    #[zbus(property, name = "AttentionMovieName")]
    fn attention_movie_name(&self) -> String {
        String::new()
    }

    #[zbus(property, name = "ToolTip")]
    fn tool_tip(&self) -> ToolTip {
        let cfg = self.state.config.current();
        let status = if cfg.enabled {
            "Aktiviert"
        } else {
            "Deaktiviert"
        };
        let shield = if cfg.auto_exclude_3d_apps || cfg.protect_3d_drag {
            " • 3D/CAD-Schutz aktiv"
        } else {
            ""
        };
        (
            String::new(),
            Vec::new(),
            format!("Breeze Autoscroll ({status})"),
            format!("Mittelklick-Autoscroll ({status}{shield})"),
        )
    }

    #[zbus(property, name = "Menu")]
    fn menu(&self) -> zbus::fdo::Result<OwnedObjectPath> {
        OwnedObjectPath::try_from(MENU_PATH)
            .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))
    }

    #[zbus(name = "Activate")]
    fn activate(&self, _x: i32, _y: i32) {
        self.state.open_settings();
    }

    #[zbus(name = "SecondaryActivate")]
    async fn secondary_activate(
        &self,
        _x: i32,
        _y: i32,
        #[zbus(object_server)] server: &ObjectServer,
    ) {
        self.state.config.update(true, |cfg| {
            cfg.enabled = !cfg.enabled;
        });
        emit_tray_refresh(server, &self.state).await;
    }

    #[zbus(name = "ContextMenu")]
    fn context_menu(&self, _x: i32, _y: i32) {}

    #[zbus(name = "Scroll")]
    fn scroll(&self, _delta: i32, _orientation: String) {}

    #[zbus(name = "ProvideXdgActivationToken")]
    fn provide_xdg_activation_token(&self, _token: String) {}

    #[zbus(signal, name = "NewIcon")]
    async fn new_icon(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;

    #[zbus(signal, name = "NewToolTip")]
    async fn new_tool_tip(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;
}

// ============================================================================
// com.canonical.dbusmenu Interface
// ============================================================================

struct DbusMenuIface {
    state: TraySharedState,
}

const MENU_ID_ROOT: i32 = 0;
const MENU_ID_TOGGLE_ENABLED: i32 = 1;
const MENU_ID_TOGGLE_3D_SHIELD: i32 = 2;
const MENU_ID_SEP_1: i32 = 3;
const MENU_ID_SETTINGS: i32 = 4;
const MENU_ID_ABOUT: i32 = 5;
const MENU_ID_SEP_2: i32 = 6;
const MENU_ID_QUIT: i32 = 7;

fn ov_str(s: &str) -> OwnedVariant {
    OwnedVariant::from(Str::from(s.to_string()))
}

fn ov_i32(v: i32) -> OwnedVariant {
    OwnedVariant::from(v)
}

fn ov_bool(v: bool) -> OwnedVariant {
    OwnedVariant::from(v)
}

fn wrap_menu_item_as_variant(item: MenuItem) -> Option<OwnedVariant> {
    let structure = Structure::from(item);
    OwnedVariant::try_from(Variant::from(structure)).ok()
}

fn build_menu_properties(id: i32, config: &crate::config::ConfigData) -> HashMap<String, OwnedVariant> {
    let mut props = HashMap::new();
    match id {
        MENU_ID_ROOT => {
            props.insert("children-display".to_string(), ov_str("submenu"));
        }
        MENU_ID_TOGGLE_ENABLED => {
            props.insert("label".to_string(), ov_str("Autoscroll aktiviert"));
            props.insert("toggle-type".to_string(), ov_str("checkmark"));
            props.insert(
                "toggle-state".to_string(),
                ov_i32(if config.enabled { 1 } else { 0 }),
            );
            props.insert("enabled".to_string(), ov_bool(true));
            props.insert("visible".to_string(), ov_bool(true));
        }
        MENU_ID_TOGGLE_3D_SHIELD => {
            props.insert(
                "label".to_string(),
                ov_str("3D / CAD-Schutz (Onshape, Bambu Studio)"),
            );
            props.insert("toggle-type".to_string(), ov_str("checkmark"));
            props.insert(
                "toggle-state".to_string(),
                ov_i32(if config.auto_exclude_3d_apps { 1 } else { 0 }),
            );
            props.insert("enabled".to_string(), ov_bool(true));
            props.insert("visible".to_string(), ov_bool(true));
        }
        MENU_ID_SEP_1 | MENU_ID_SEP_2 => {
            props.insert("type".to_string(), ov_str("separator"));
            props.insert("visible".to_string(), ov_bool(true));
        }
        MENU_ID_SETTINGS => {
            props.insert("label".to_string(), ov_str("Einstellungen..."));
            props.insert("enabled".to_string(), ov_bool(true));
            props.insert("visible".to_string(), ov_bool(true));
        }
        MENU_ID_ABOUT => {
            props.insert("label".to_string(), ov_str("Über Breeze Autoscroll"));
            props.insert("enabled".to_string(), ov_bool(true));
            props.insert("visible".to_string(), ov_bool(true));
        }
        MENU_ID_QUIT => {
            props.insert("label".to_string(), ov_str("Beenden"));
            props.insert("enabled".to_string(), ov_bool(true));
            props.insert("visible".to_string(), ov_bool(true));
        }
        _ => {}
    }
    props
}

fn build_menu_layout(config: &crate::config::ConfigData) -> MenuItem {
    let child_ids = [
        MENU_ID_TOGGLE_ENABLED,
        MENU_ID_TOGGLE_3D_SHIELD,
        MENU_ID_SEP_1,
        MENU_ID_SETTINGS,
        MENU_ID_ABOUT,
        MENU_ID_SEP_2,
        MENU_ID_QUIT,
    ];
    let mut children = Vec::with_capacity(child_ids.len());
    for id in child_ids {
        let item: MenuItem = (id, build_menu_properties(id, config), Vec::new());
        if let Some(wrapped) = wrap_menu_item_as_variant(item) {
            children.push(wrapped);
        }
    }
    (MENU_ID_ROOT, build_menu_properties(MENU_ID_ROOT, config), children)
}

#[interface(name = "com.canonical.dbusmenu")]
impl DbusMenuIface {
    #[zbus(property, name = "Version")]
    fn version(&self) -> u32 {
        3
    }

    #[zbus(property, name = "Status")]
    fn status(&self) -> String {
        "normal".to_string()
    }

    #[zbus(name = "GetLayout")]
    fn get_layout(
        &self,
        parent_id: i32,
        _recursion_depth: i32,
        _property_names: Vec<String>,
    ) -> (u32, MenuItem) {
        let cfg = self.state.config.current();
        let rev = self.state.menu_revision.load(Ordering::SeqCst);
        if parent_id == MENU_ID_ROOT {
            (rev, build_menu_layout(&cfg))
        } else {
            (rev, (parent_id, build_menu_properties(parent_id, &cfg), Vec::new()))
        }
    }

    #[zbus(name = "GetGroupProperties")]
    fn get_group_properties(
        &self,
        ids: Vec<i32>,
        _property_names: Vec<String>,
    ) -> Vec<(i32, HashMap<String, OwnedVariant>)> {
        let cfg = self.state.config.current();
        ids.into_iter()
            .take(MAX_MENU_GROUP_IDS)
            .map(|id| (id, build_menu_properties(id, &cfg)))
            .collect()
    }

    #[zbus(name = "GetProperty")]
    fn get_property(&self, id: i32, name: String) -> zbus::fdo::Result<OwnedVariant> {
        let name = clamp_dbus_str(name, MAX_MENU_PROP_NAME_BYTES);
        let cfg = self.state.config.current();
        build_menu_properties(id, &cfg)
            .remove(&name)
            .ok_or_else(|| zbus::fdo::Error::Failed(format!("Unknown menu property: {name}")))
    }

    #[zbus(name = "Event")]
    async fn event(
        &self,
        id: i32,
        event_id: String,
        _data: OwnedVariant,
        _timestamp: u32,
        #[zbus(object_server)] server: &ObjectServer,
    ) {
        if event_id != "clicked" {
            return;
        }
        match id {
            MENU_ID_TOGGLE_ENABLED => {
                self.state.config.update(true, |cfg| {
                    cfg.enabled = !cfg.enabled;
                });
                emit_tray_refresh(server, &self.state).await;
            }
            MENU_ID_TOGGLE_3D_SHIELD => {
                self.state.config.update(true, |cfg| {
                    cfg.auto_exclude_3d_apps = !cfg.auto_exclude_3d_apps;
                });
                emit_tray_refresh(server, &self.state).await;
            }
            MENU_ID_SETTINGS => {
                self.state.open_settings();
            }
            MENU_ID_ABOUT => {
                self.state.open_about();
            }
            MENU_ID_QUIT => {
                self.state.quit_notifier.request_quit();
            }
            _ => {}
        }
    }

    #[zbus(name = "AboutToShow")]
    fn about_to_show(&self, _id: i32) -> bool {
        true
    }

    #[zbus(signal, name = "LayoutUpdated")]
    async fn layout_updated(
        emitter: &SignalEmitter<'_>,
        revision: u32,
        parent: i32,
    ) -> zbus::Result<()>;
}

// ============================================================================
// Session Bus Registration & IPC Client Helpers
// ============================================================================

/// Checks whether another `org.breeze.Autoscroll` daemon already owns the D-Bus name.
/// If `open_settings` is true, asks the running daemon to open the Settings window.
pub fn try_activate_existing_instance(open_settings: bool) -> bool {
    zbus::block_on(async {
        let Ok(conn) = Connection::session().await else {
            return false;
        };
        let Ok(dbus) = DBusProxy::new(&conn).await else {
            return false;
        };
        let Ok(bus_name) = SERVICE_NAME.try_into() else {
            return false;
        };
        let Ok(true) = dbus.name_has_owner(bus_name).await else {
            return false;
        };

        if open_settings
            && let Ok(proxy) =
                Proxy::new(&conn, SERVICE_NAME, AUTOSCROLL_PATH, SERVICE_NAME).await
        {
            let _ = proxy.call::<_, _, ()>("ShowSettings", &()).await;
        }
        true
    })
}

/// Notifies a running background daemon to reload `config.json` and refresh the tray icon.
pub fn notify_daemon_reload_config() {
    zbus::block_on(async {
        let Ok(conn) = Connection::session().await else {
            return;
        };
        if let Ok(proxy) = Proxy::new(&conn, SERVICE_NAME, AUTOSCROLL_PATH, SERVICE_NAME).await {
            let _ = proxy.call::<_, _, ()>("ReloadConfig", &()).await;
        }
    });
}

/// Starts the session D-Bus service (`org.breeze.Autoscroll`, `StatusNotifierItem`, `dbusmenu`),
/// pre-warms `KWinOwnerVerifier`, and registers with KDE Plasma's `StatusNotifierWatcher`.
pub fn start_tray_and_dbus_service(
    config: ConfigManager,
    window_tracker: WindowTracker,
    quit_notifier: QuitNotifier,
) -> Result<TrayService, String> {
    let kwin_verifier = KWinOwnerVerifier::new();
    let shared = TraySharedState {
        config,
        window_tracker,
        quit_notifier,
        kwin_verifier: kwin_verifier.clone(),
        menu_revision: Arc::new(AtomicU32::new(1)),
        settings_child: Arc::new(Mutex::new(None)),
        about_child: Arc::new(Mutex::new(None)),
    };

    let shared_for_handle = shared.clone();
    let conn = zbus::block_on(async move {
        // Pre-warm org.kde.KWin unique owner before first middle-click (short 250ms timeout)
        kwin_verifier.warm_up().await;

        let control = AutoscrollControlIface {
            state: shared.clone(),
        };
        let sni = StatusNotifierItemIface {
            state: shared.clone(),
        };
        let menu = DbusMenuIface {
            state: shared.clone(),
        };

        let builder = connection::Builder::session()
            .name(SERVICE_NAME)
            .serve_at(AUTOSCROLL_PATH, control)
            .serve_at(SNI_PATH, sni)
            .serve_at(MENU_PATH, menu);

        #[cfg(feature = "debug-dbus")]
        let builder = builder.serve_at(AUTOSCROLL_PATH, AutoscrollDebugIface { state: shared });

        let conn = builder
            .build()
            .await
            .map_err(|e| format!("D-Bus Session-Verbindung fehlgeschlagen: {e}"))?;

        if let Ok(watcher) = Proxy::new(
            &conn,
            "org.kde.StatusNotifierWatcher",
            "/StatusNotifierWatcher",
            "org.kde.StatusNotifierWatcher",
        )
        .await
        {
            let _ = watcher
                .call::<_, _, ()>("RegisterStatusNotifierItem", &(SERVICE_NAME))
                .await;
        }

        Ok::<Connection, String>(conn)
    })?;

    Ok(TrayService {
        _conn: conn,
        state: shared_for_handle,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ConfigData;
    use zbus::object_server::Interface;

    #[test]
    fn tray_icon_argb32_renders_expected_dimensions_and_colors() {
        let active = render_tray_icon_argb32_be(true, 64);
        let inactive = render_tray_icon_argb32_be(false, 64);
        assert_eq!(active.len(), 64 * 64 * 4);
        assert_eq!(inactive.len(), 64 * 64 * 4);
        assert_ne!(active, inactive);

        // Corner pixel (0, 0) must be transparent (A = 0 in big-endian ARGB)
        assert_eq!(active[0], 0);
        // Center pixel (32, 32) must have Breeze blue dot (alpha = 255, B > R)
        let center_off = (32 * 64 + 32) * 4;
        assert_eq!(active[center_off], 255);
        assert!(active[center_off + 3] > active[center_off + 1]);
    }

    #[test]
    fn dbusmenu_layout_contains_all_actions_and_3d_shield_toggle() {
        let cfg = ConfigData::default();
        let (root_id, _props, children) = build_menu_layout(&cfg);
        assert_eq!(root_id, 0);
        assert_eq!(children.len(), 7);

        let toggle_props = build_menu_properties(MENU_ID_TOGGLE_ENABLED, &cfg);
        assert_eq!(
            i32::try_from(toggle_props.get("toggle-state").unwrap()).unwrap(),
            1
        );

        let shield_props = build_menu_properties(MENU_ID_TOGGLE_3D_SHIELD, &cfg);
        assert_eq!(
            i32::try_from(shield_props.get("toggle-state").unwrap()).unwrap(),
            1
        );
    }

    #[test]
    fn kwin_owner_cache_verifies_sender_rate_limits_spoofing_and_recovers_on_restart() {
        let mut cache = KWinOwnerCache::default();
        let t0 = Instant::now();

        // Warm-up seeds KWin owner -> immediate zero-lookup Allow
        cache.seed(Some(":1.42".to_string()), t0);
        assert_eq!(cache.check(":1.42", t0), VerifyDecision::Allow);
        assert_eq!(cache.check("", t0), VerifyDecision::Deny);

        // Spoofed sender triggers one refresh, then is rate-limited inside cooldown window
        assert_eq!(cache.check(":1.99", t0), VerifyDecision::RefreshNeeded);
        assert!(!cache.update_and_verify(Some(":1.42".to_string()), ":1.99", t0));
        assert_eq!(
            cache.check(":1.99", t0 + Duration::from_millis(50)),
            VerifyDecision::Deny
        );

        // After cooldown, if KWin restarted with a new unique name (:1.105), refresh accepts it
        let t1 = t0 + Duration::from_millis(250);
        assert_eq!(cache.check(":1.105", t1), VerifyDecision::RefreshNeeded);
        assert!(cache.update_and_verify(Some(":1.105".to_string()), ":1.105", t1));
        assert_eq!(cache.check(":1.105", t1), VerifyDecision::Allow);

        // Expired TTL forces re-verification
        assert_eq!(
            cache.check(":1.105", t1 + KWIN_OWNER_CACHE_TTL + Duration::from_secs(1)),
            VerifyDecision::RefreshNeeded
        );
    }

    #[test]
    fn clamp_dbus_str_truncates_utf8_cleanly() {
        let ascii = clamp_dbus_str("abcdef".to_string(), 4);
        assert_eq!(ascii, "abcd");

        // Multi-byte UTF-8 ('ä' = 2 bytes, '€' = 3 bytes) never panics or splits char
        let utf8 = clamp_dbus_str("a€b".to_string(), 3);
        assert_eq!(utf8, "a");
        let utf8_exact = clamp_dbus_str("a€b".to_string(), 4);
        assert_eq!(utf8_exact, "a€");
    }

    #[test]
    fn debug_probe_methods_follow_feature_flag_in_introspection() {
        let tmp_dir = env::temp_dir().join(format!(
            "breeze_tray_introspect_{}",
            std::process::id()
        ));
        let mut xml = String::new();
        let shared = TraySharedState {
            config: ConfigManager::try_new(Some(tmp_dir.clone())).unwrap(),
            window_tracker: WindowTracker::new(),
            quit_notifier: QuitNotifier::new(
                Arc::new(AtomicBool::new(false)),
                Arc::new(AtomicI32::new(-1)),
            ),
            kwin_verifier: KWinOwnerVerifier::new(),
            menu_revision: Arc::new(AtomicU32::new(1)),
            settings_child: Arc::new(Mutex::new(None)),
            about_child: Arc::new(Mutex::new(None)),
        };
        let iface = AutoscrollControlIface {
            state: shared.clone(),
        };
        iface.introspect_to_writer(&mut xml, 0);
        assert!(!xml.contains("GetLastProbe"));
        assert!(!xml.contains("TriggerProbe"));

        #[cfg(feature = "debug-dbus")]
        {
            let mut debug_xml = String::new();
            let debug_iface = AutoscrollDebugIface { state: shared };
            debug_iface.introspect_to_writer(&mut debug_xml, 0);
            assert!(debug_xml.contains("GetLastProbe"));
            assert!(debug_xml.contains("TriggerProbe"));
        }

        let _ = std::fs::remove_dir_all(&tmp_dir);
    }

    #[test]
    fn reap_slot_clears_finished_child_without_leaving_zombie() {
        let slot: Arc<Mutex<Option<Child>>> = Arc::new(Mutex::new(None));
        let child = Command::new("/bin/true").spawn().unwrap();
        *slot.lock().unwrap() = Some(child);

        // Wait briefly for /bin/true to exit, then reap via TraySharedState::reap_slot
        for _ in 0..50 {
            TraySharedState::reap_slot(&slot);
            if slot.lock().unwrap().is_none() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(slot.lock().unwrap().is_none());
    }
}
