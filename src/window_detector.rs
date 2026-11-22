//! Active window, window-under-cursor, live pointer coordinates, and 3D/CAD context detection.
//!
//! Works across KDE Plasma 6 (Wayland + KWin Scripting / D-Bus) and X11 / XWayland (`x11rb`).

use std::env;
use std::fs;
use std::path::PathBuf;
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use x11rb::connection::Connection as X11Connection;
use x11rb::protocol::xproto::{AtomEnum, ConnectionExt as _, Window};
use x11rb::rust_connection::RustConnection;
use zbus::{Connection, Proxy};

use crate::config::ConfigData;

const KWIN_TRACKER_SCRIPT_NAME: &str = "breeze_autoscroll_tracker";
const KWIN_CURSOR_LIVE_SCRIPT_NAME: &str = "breeze_autoscroll_cursor_live";
const PROBE_TIMEOUT_MS: u64 = 80;

/// Metadata about an application window (including active browser tab title).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ActiveWindowInfo {
    pub app_class: String,
    pub app_name: String,
    pub desktop_file: String,
    pub title: String,
    pub updated_at_ms: u64,
}

/// Snapshot of pointer coordinates and target window captured right when `BTN_MIDDLE` is pressed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PointerProbeResult {
    /// Logical compositor pointer X (or X11 root X).
    pub cursor_x: i32,
    /// Logical compositor pointer Y (or X11 root Y).
    pub cursor_y: i32,
    /// Placement X in X11/XWayland root coordinates for the overlay window.
    pub overlay_x: i32,
    /// Placement Y in X11/XWayland root coordinates for the overlay window.
    pub overlay_y: i32,
    /// Window directly under the pointer (or active window fallback).
    pub window: Option<ActiveWindowInfo>,
    /// True if the pointer was clicked on a desktop wallpaper, dock, or panel.
    pub is_dock_or_desktop: bool,
    /// True when coordinates came directly from KWin's live compositor state.
    pub from_kwin: bool,
}

#[derive(Debug, Default)]
struct TrackerState {
    kwin_active: Option<ActiveWindowInfo>,
    kwin_cursor: Option<(i32, i32)>,
    kwin_virtual_size: Option<(i32, i32)>,
    probe_seq: u64,
    last_probe: Option<PointerProbeResult>,
}

struct X11Client {
    conn: RustConnection,
    root: Window,
    root_width: i32,
    root_height: i32,
}

impl X11Client {
    fn connect() -> Option<Self> {
        if env::var_os("DISPLAY").is_none() {
            return None;
        }
        let (conn, screen_num) = x11rb::connect(None).ok()?;
        let (root, root_width, root_height) = {
            let screen = conn.setup().roots.get(screen_num)?;
            (
                screen.root,
                screen.width_in_pixels as i32,
                screen.height_in_pixels as i32,
            )
        };
        Some(Self {
            conn,
            root,
            root_width,
            root_height,
        })
    }

    fn root_size(&self) -> (i32, i32) {
        (self.root_width, self.root_height)
    }

    fn query_pointer(&self) -> Result<Option<(i32, i32)>, ()> {
        let cookie = self.conn.query_pointer(self.root).map_err(|_| ())?;
        let reply = cookie.reply().map_err(|_| ())?;
        Ok(Some((reply.root_x as i32, reply.root_y as i32)))
    }

    fn query_active_window(&self) -> Result<Option<ActiveWindowInfo>, ()> {
        let Some(active_atom) = intern_atom(&self.conn, b"_NET_ACTIVE_WINDOW") else {
            return Ok(None);
        };
        let prop = self
            .conn
            .get_property(false, self.root, active_atom, AtomEnum::WINDOW, 0, 1)
            .map_err(|_| ())?
            .reply()
            .map_err(|_| ())?;
        let Some(win) = prop.value32().and_then(|mut it| it.next()) else {
            return Ok(None);
        };
        if win == 0 {
            return Ok(None);
        }

        let (app_name, app_class) = query_x11_wm_class(&self.conn, win).unwrap_or_default();
        let title = query_x11_window_title(&self.conn, win).unwrap_or_default();

        Ok(Some(ActiveWindowInfo {
            app_class,
            app_name,
            desktop_file: String::new(),
            title,
            updated_at_ms: now_epoch_ms(),
        }))
    }
}

enum BridgeCmd {
    EnsureTrackerInstalled,
    StartLiveCursorAndProbe,
    StopLiveCursor,
    Shutdown,
}

struct BridgeHandle {
    tx: Sender<BridgeCmd>,
    join: Option<JoinHandle<()>>,
}

/// Thread-safe active window, Wayland/X11 pointer probe, and live cursor position tracker.
#[derive(Clone)]
pub struct WindowTracker {
    sync: Arc<(Mutex<TrackerState>, Condvar)>,
    x11: Arc<Mutex<Option<X11Client>>>,
    bridge: Arc<Mutex<Option<BridgeHandle>>>,
}

impl Default for WindowTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl WindowTracker {
    pub fn new() -> Self {
        Self {
            sync: Arc::new((Mutex::new(TrackerState::default()), Condvar::new())),
            x11: Arc::new(Mutex::new(None)),
            bridge: Arc::new(Mutex::new(None)),
        }
    }

    /// Starts the persistent KWin D-Bus scripting bridge and installs the background window tracker.
    pub fn start_kwin_bridge(&self) {
        let Ok(mut guard) = self.bridge.lock() else {
            return;
        };
        if guard.is_some() {
            return;
        }

        let (tx, rx) = mpsc::channel();
        let join = thread::Builder::new()
            .name("breeze-kwin-bridge".to_string())
            .spawn(move || run_kwin_bridge_loop(rx))
            .ok();

        let _ = tx.send(BridgeCmd::EnsureTrackerInstalled);
        *guard = Some(BridgeHandle { tx, join });
    }

    /// Unloads ephemeral KWin scripts and shuts down the KWin bridge thread.
    pub fn stop_kwin_bridge(&self) {
        let handle = if let Ok(mut guard) = self.bridge.lock() {
            guard.take()
        } else {
            None
        };
        if let Some(mut h) = handle {
            let _ = h.tx.send(BridgeCmd::Shutdown);
            if let Some(j) = h.join.take() {
                let _ = j.join();
            }
        }
    }

    /// Updates cached active window and cursor information pushed from KWin via D-Bus.
    pub fn update_from_kwin(
        &self,
        app_class: String,
        app_name: String,
        desktop_file: String,
        title: String,
        cursor_x: Option<i32>,
        cursor_y: Option<i32>,
    ) {
        let (lock, _) = &*self.sync;
        if let Ok(mut guard) = lock.lock() {
            guard.kwin_active = Some(ActiveWindowInfo {
                app_class,
                app_name,
                desktop_file,
                title,
                updated_at_ms: now_epoch_ms(),
            });
            if let (Some(x), Some(y)) = (cursor_x, cursor_y) {
                guard.kwin_cursor = Some((x, y));
            }
        }
    }

    /// Receives live pointer probe data pushed by `kwin_cursor_live.js` via D-Bus and wakes waiters.
    #[allow(clippy::too_many_arguments)]
    pub fn report_pointer_probe(
        &self,
        cursor_x: i32,
        cursor_y: i32,
        virt_w: i32,
        virt_h: i32,
        app_class: String,
        app_name: String,
        desktop_file: String,
        title: String,
        is_dock_or_desktop: bool,
    ) {
        let x11_size = self.x11_root_size();
        let (overlay_x, overlay_y) = scale_kwin_to_x11(
            cursor_x,
            cursor_y,
            (virt_w > 0 && virt_h > 0).then_some((virt_w, virt_h)),
            x11_size,
        );

        let window = if app_class.is_empty()
            && app_name.is_empty()
            && desktop_file.is_empty()
            && title.is_empty()
        {
            None
        } else {
            Some(ActiveWindowInfo {
                app_class,
                app_name,
                desktop_file,
                title,
                updated_at_ms: now_epoch_ms(),
            })
        };

        let result = PointerProbeResult {
            cursor_x,
            cursor_y,
            overlay_x,
            overlay_y,
            window,
            is_dock_or_desktop,
            from_kwin: true,
        };

        let (lock, cvar) = &*self.sync;
        if let Ok(mut guard) = lock.lock() {
            guard.kwin_cursor = Some((cursor_x, cursor_y));
            if virt_w > 0 && virt_h > 0 {
                guard.kwin_virtual_size = Some((virt_w, virt_h));
            }
            guard.probe_seq = guard.probe_seq.wrapping_add(1);
            guard.last_probe = Some(result);
            cvar.notify_all();
        }
    }

    /// Updates live cursor coordinates streamed from `kwin_cursor_live.js` while autoscroll is active.
    pub fn update_cursor_pos(&self, cursor_x: i32, cursor_y: i32) {
        let (lock, _) = &*self.sync;
        if let Ok(mut guard) = lock.lock() {
            guard.kwin_cursor = Some((cursor_x, cursor_y));
        }
    }

    /// Probes the exact cursor position and target window under the pointer, and starts live
    /// KWin cursor streaming on Wayland sessions.
    pub fn probe_pointer_and_start_live_cursor(&self) -> PointerProbeResult {
        let bridge_tx = self
            .bridge
            .lock()
            .ok()
            .and_then(|g| g.as_ref().map(|h| h.tx.clone()));

        if let Some(tx) = bridge_tx {
            let baseline_seq = {
                let (lock, _) = &*self.sync;
                lock.lock().map(|g| g.probe_seq).unwrap_or(0)
            };

            if tx.send(BridgeCmd::StartLiveCursorAndProbe).is_ok() {
                let deadline = Instant::now() + Duration::from_millis(PROBE_TIMEOUT_MS);
                let (lock, cvar) = &*self.sync;
                let mut probed: Option<PointerProbeResult> = None;
                if let Ok(mut guard) = lock.lock() {
                    loop {
                        if guard.probe_seq != baseline_seq {
                            probed = guard.last_probe.clone();
                            break;
                        }
                        let remaining = deadline.saturating_duration_since(Instant::now());
                        if remaining.is_zero() {
                            break;
                        }
                        match cvar.wait_timeout(guard, remaining) {
                            Ok((next_guard, timeout_res)) => {
                                guard = next_guard;
                                if guard.probe_seq != baseline_seq {
                                    probed = guard.last_probe.clone();
                                    break;
                                }
                                if timeout_res.timed_out() {
                                    break;
                                }
                            }
                            Err(_) => break,
                        }
                    }
                }
                if let Some(probe) = probed {
                    return probe;
                }
            }
        }

        // Fallback path: pure X11 (when not on Wayland) or cached KWin state if KWin probe timed out
        if !is_wayland_session()
            && let Some((cx, cy)) = self.query_x11_cursor()
        {
            return PointerProbeResult {
                cursor_x: cx,
                cursor_y: cy,
                overlay_x: cx,
                overlay_y: cy,
                window: self.get_active_window(),
                is_dock_or_desktop: false,
                from_kwin: false,
            };
        }

        // On Wayland, never prefer frozen XWayland query_pointer over KWin cursor if KWin cursor exists
        let (kwin_pos, kwin_virt, active_win) = {
            let (lock, _) = &*self.sync;
            lock.lock()
                .map(|g| (g.kwin_cursor, g.kwin_virtual_size, g.kwin_active.clone()))
                .unwrap_or((None, None, None))
        };

        if let Some((cx, cy)) = kwin_pos {
            let x11_size = self.x11_root_size();
            let (ox, oy) = scale_kwin_to_x11(cx, cy, kwin_virt, x11_size);
            return PointerProbeResult {
                cursor_x: cx,
                cursor_y: cy,
                overlay_x: ox,
                overlay_y: oy,
                window: active_win.or_else(|| self.query_x11_active()),
                is_dock_or_desktop: false,
                from_kwin: false,
            };
        }

        if let Some((cx, cy)) = self.query_x11_cursor() {
            return PointerProbeResult {
                cursor_x: cx,
                cursor_y: cy,
                overlay_x: cx,
                overlay_y: cy,
                window: active_win.or_else(|| self.query_x11_active()),
                is_dock_or_desktop: false,
                from_kwin: false,
            };
        }

        PointerProbeResult {
            cursor_x: 500,
            cursor_y: 500,
            overlay_x: 500,
            overlay_y: 500,
            window: active_win,
            is_dock_or_desktop: false,
            from_kwin: false,
        }
    }

    /// Returns live cursor coordinates while autoscroll is active (zero-drift absolute positioning).
    pub fn get_live_cursor_pos(&self) -> Option<(i32, i32)> {
        let has_bridge = self
            .bridge
            .lock()
            .ok()
            .is_some_and(|g| g.is_some());

        if is_wayland_session() || has_bridge {
            let (lock, _) = &*self.sync;
            if let Ok(guard) = lock.lock()
                && let Some(pos) = guard.kwin_cursor
            {
                return Some(pos);
            }
        }

        if !is_wayland_session() {
            return self.query_x11_cursor();
        }
        None
    }

    /// Unloads the ephemeral KWin live-cursor script once autoscroll stops.
    pub fn stop_live_cursor(&self) {
        if let Ok(guard) = self.bridge.lock()
            && let Some(handle) = guard.as_ref()
        {
            let _ = handle.tx.send(BridgeCmd::StopLiveCursor);
        }
    }

    /// Returns the currently focused window, preferring live KWin Wayland state and
    /// falling back to X11 `_NET_ACTIVE_WINDOW`.
    pub fn get_active_window(&self) -> Option<ActiveWindowInfo> {
        {
            let (lock, _) = &*self.sync;
            if let Ok(guard) = lock.lock()
                && let Some(info) = guard.kwin_active.as_ref()
            {
                return Some(info.clone());
            }
        }
        self.query_x11_active()
    }

    /// Checks whether a probe result (preferring the window directly under the cursor)
    /// belongs to an excluded 3D/CAD/Slicer context.
    pub fn check_probe_excluded(
        &self,
        probe: &PointerProbeResult,
        config: &ConfigData,
    ) -> Option<String> {
        if !config.auto_exclude_3d_apps || config.excluded_apps_and_titles.is_empty() {
            return None;
        }
        if let Some(win) = probe.window.as_ref()
            && let Some(matched) =
                match_excluded_3d_context(win, &config.excluded_apps_and_titles)
        {
            return Some(matched);
        }
        let active = self.get_active_window()?;
        match_excluded_3d_context(&active, &config.excluded_apps_and_titles)
    }

    /// Legacy cursor helper retained until Step 2 switches input_manager to pointer probing.
    #[allow(dead_code)]
    pub fn get_cursor_pos(&self) -> (i32, i32) {
        let probe = self.probe_pointer_and_start_live_cursor();
        (probe.overlay_x, probe.overlay_y)
    }

    /// Checks whether the currently active application or browser tab belongs to an
    /// excluded 3D/CAD/Slicer context.
    pub fn check_excluded_context(&self, config: &ConfigData) -> Option<String> {
        if !config.auto_exclude_3d_apps || config.excluded_apps_and_titles.is_empty() {
            return None;
        }
        let info = self.get_active_window()?;
        match_excluded_3d_context(&info, &config.excluded_apps_and_titles)
    }

    fn with_x11<T, F: Fn(&X11Client) -> Result<Option<T>, ()>>(&self, f: F) -> Option<T> {
        let Ok(mut guard) = self.x11.lock() else {
            return None;
        };
        if guard.is_none() {
            *guard = X11Client::connect();
        }
        if let Some(client) = guard.as_ref() {
            match f(client) {
                Ok(val) => return val,
                Err(()) => {
                    *guard = X11Client::connect();
                    if let Some(reconnected) = guard.as_ref() {
                        return f(reconnected).ok().flatten();
                    }
                }
            }
        }
        None
    }

    fn x11_root_size(&self) -> Option<(i32, i32)> {
        self.with_x11(|c| Ok(Some(c.root_size())))
    }

    fn query_x11_cursor(&self) -> Option<(i32, i32)> {
        self.with_x11(|c| c.query_pointer())
    }

    fn query_x11_active(&self) -> Option<ActiveWindowInfo> {
        self.with_x11(|c| c.query_active_window())
    }
}

/// Returns true when running inside a Wayland session.
pub fn is_wayland_session() -> bool {
    env::var_os("WAYLAND_DISPLAY").is_some_and(|v| !v.is_empty())
        || env::var("XDG_SESSION_TYPE")
            .map(|v| v.eq_ignore_ascii_case("wayland"))
            .unwrap_or(false)
}

/// Maps KWin logical virtual-desktop coordinates into X11/XWayland root coordinates.
pub fn scale_kwin_to_x11(
    cx: i32,
    cy: i32,
    virt_size: Option<(i32, i32)>,
    x11_size: Option<(i32, i32)>,
) -> (i32, i32) {
    let (Some((vw, vh)), Some((xw, xh))) = (virt_size, x11_size) else {
        return (cx, cy);
    };
    if vw <= 0 || vh <= 0 || xw <= 0 || xh <= 0 || (vw == xw && vh == xh) {
        return (cx, cy);
    }
    let ox = ((cx as f64) * (xw as f64) / (vw as f64)).round() as i32;
    let oy = ((cy as f64) * (xh as f64) / (vh as f64)).round() as i32;
    (ox, oy)
}

/// Checks whether any exclusion pattern matches `app_class`, `app_name`, `desktop_file`,
/// or `title` (using word-boundary matching in titles for short alphanumeric tokens like `cura`).
pub fn match_excluded_3d_context(info: &ActiveWindowInfo, patterns: &[String]) -> Option<String> {
    let app_fields = [
        info.app_class.to_ascii_lowercase(),
        info.app_name.to_ascii_lowercase(),
        info.desktop_file.to_ascii_lowercase(),
    ];
    let title_lower = info.title.to_ascii_lowercase();

    for raw_pattern in patterns {
        let trimmed = raw_pattern.trim();
        if trimmed.is_empty() {
            continue;
        }
        let pattern = trimmed.to_ascii_lowercase();

        if app_fields.iter().any(|field| field.contains(&pattern)) {
            return Some(trimmed.to_string());
        }

        let is_short_word =
            pattern.len() <= 4 && pattern.bytes().all(|b| b.is_ascii_alphanumeric());
        let title_matches = if is_short_word {
            contains_ascii_word(&title_lower, &pattern)
        } else {
            title_lower.contains(&pattern)
        };

        if title_matches {
            return Some(trimmed.to_string());
        }
    }

    None
}

fn contains_ascii_word(haystack_lower: &str, needle_lower: &str) -> bool {
    let h = haystack_lower.as_bytes();
    let n = needle_lower.as_bytes();
    if n.is_empty() || h.len() < n.len() {
        return false;
    }

    let mut start = 0;
    while let Some(rel_idx) = haystack_lower[start..].find(needle_lower) {
        let idx = start + rel_idx;
        let left_ok = idx == 0 || !h[idx - 1].is_ascii_alphanumeric();
        let end = idx + n.len();
        let right_ok = end == h.len() || !h[end].is_ascii_alphanumeric();
        if left_ok && right_ok {
            return true;
        }
        start = idx + 1;
    }
    false
}

fn intern_atom<C: X11Connection>(conn: &C, name: &[u8]) -> Option<u32> {
    conn.intern_atom(true, name)
        .ok()?
        .reply()
        .ok()
        .map(|r| r.atom)
        .filter(|&a| a != 0)
}

fn query_x11_wm_class<C: X11Connection>(conn: &C, win: Window) -> Option<(String, String)> {
    let reply = conn
        .get_property(false, win, AtomEnum::WM_CLASS, AtomEnum::STRING, 0, 256)
        .ok()?
        .reply()
        .ok()?;
    if reply.value.is_empty() {
        return None;
    }
    let parts: Vec<String> = reply
        .value
        .split(|&b| b == 0)
        .filter(|s| !s.is_empty())
        .map(|s| String::from_utf8_lossy(s).into_owned())
        .collect();
    let instance = parts.first().cloned().unwrap_or_default();
    let class = parts.get(1).cloned().unwrap_or_else(|| instance.clone());
    Some((instance, class))
}

fn query_x11_window_title<C: X11Connection>(conn: &C, win: Window) -> Option<String> {
    if let Some(utf8_atom) = intern_atom(conn, b"UTF8_STRING")
        && let Some(net_wm_name) = intern_atom(conn, b"_NET_WM_NAME")
        && let Ok(cookie) = conn.get_property(false, win, net_wm_name, utf8_atom, 0, 1024)
        && let Ok(reply) = cookie.reply()
        && !reply.value.is_empty()
    {
        return Some(String::from_utf8_lossy(&reply.value).into_owned());
    }

    let reply = conn
        .get_property(false, win, AtomEnum::WM_NAME, AtomEnum::STRING, 0, 1024)
        .ok()?
        .reply()
        .ok()?;
    if reply.value.is_empty() {
        return None;
    }
    Some(String::from_utf8_lossy(&reply.value).into_owned())
}

// ============================================================================
// Persistent KWin D-Bus Scripting Bridge
// ============================================================================

fn run_kwin_bridge_loop(rx: mpsc::Receiver<BridgeCmd>) {
    let tracker_path = kwin_script_path("kwin_tracker.js");
    let live_path = kwin_script_path("kwin_cursor_live.js");
    if let Some(parent) = tracker_path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let _ = fs::write(&tracker_path, KWIN_TRACKER_JS);
    let _ = fs::write(&live_path, KWIN_CURSOR_LIVE_JS);

    let tracker_str = tracker_path.to_string_lossy().into_owned();
    let live_str = live_path.to_string_lossy().into_owned();

    zbus::block_on(async move {
        let mut conn: Option<Connection> = None;

        while let Ok(cmd) = rx.recv() {
            match cmd {
                BridgeCmd::EnsureTrackerInstalled => {
                    let _ = ensure_loaded_and_run(
                        &mut conn,
                        &tracker_str,
                        KWIN_TRACKER_SCRIPT_NAME,
                    )
                    .await;
                }
                BridgeCmd::StartLiveCursorAndProbe => {
                    if ensure_loaded_and_run(
                        &mut conn,
                        &live_str,
                        KWIN_CURSOR_LIVE_SCRIPT_NAME,
                    )
                    .await
                    .is_err()
                    {
                        conn = None;
                        let _ = ensure_loaded_and_run(
                            &mut conn,
                            &live_str,
                            KWIN_CURSOR_LIVE_SCRIPT_NAME,
                        )
                        .await;
                    }
                }
                BridgeCmd::StopLiveCursor => {
                    let _ = unload_script(&mut conn, KWIN_CURSOR_LIVE_SCRIPT_NAME).await;
                }
                BridgeCmd::Shutdown => {
                    let _ = unload_script(&mut conn, KWIN_CURSOR_LIVE_SCRIPT_NAME).await;
                    let _ = unload_script(&mut conn, KWIN_TRACKER_SCRIPT_NAME).await;
                    break;
                }
            }
        }
    });
}

async fn ensure_session_conn(slot: &mut Option<Connection>) -> Result<&Connection, ()> {
    if slot.is_none() {
        *slot = Connection::session().await.ok();
    }
    slot.as_ref().ok_or(())
}

async fn ensure_loaded_and_run(
    conn_slot: &mut Option<Connection>,
    script_path: &str,
    plugin_name: &str,
) -> Result<(), ()> {
    let conn = ensure_session_conn(conn_slot).await?;
    let scripting = Proxy::new(
        conn,
        "org.kde.KWin",
        "/Scripting",
        "org.kde.kwin.Scripting",
    )
    .await
    .map_err(|_| ())?;

    let _ = scripting
        .call::<_, _, bool>("unloadScript", &(plugin_name))
        .await;

    let script_id = scripting
        .call::<_, _, i32>("loadScript", &(script_path, plugin_name))
        .await
        .map_err(|_| ())?;
    if script_id < 0 {
        return Err(());
    }

    let script_obj = format!("/Scripting/Script{script_id}");
    if let Ok(script_proxy) =
        Proxy::new(conn, "org.kde.KWin", script_obj, "org.kde.kwin.Script").await
    {
        let _ = script_proxy.call::<_, _, ()>("run", &()).await;
    } else {
        let _ = scripting.call::<_, _, ()>("start", &()).await;
    }
    Ok(())
}

async fn unload_script(conn_slot: &mut Option<Connection>, plugin_name: &str) -> Result<(), ()> {
    let conn = ensure_session_conn(conn_slot).await?;
    let scripting = Proxy::new(
        conn,
        "org.kde.KWin",
        "/Scripting",
        "org.kde.kwin.Scripting",
    )
    .await
    .map_err(|_| ())?;
    let _ = scripting
        .call::<_, _, bool>("unloadScript", &(plugin_name))
        .await;
    Ok(())
}

fn kwin_script_path(file_name: &str) -> PathBuf {
    if let Some(cache) = env::var_os("XDG_CACHE_HOME")
        && !cache.is_empty()
    {
        return PathBuf::from(cache)
            .join("breeze-autoscroll")
            .join(file_name);
    }
    let home = env::var_os("HOME").unwrap_or_else(|| "/tmp".into());
    PathBuf::from(home)
        .join(".cache/breeze-autoscroll")
        .join(file_name)
}

fn now_epoch_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

const KWIN_TRACKER_JS: &str = r#"
// Breeze Autoscroll - Ephemeral KDE Plasma 6 Active Window & Browser Tab Tracker
(function () {
    var trackedWin = null;
    var trackedHandler = null;

    function sendWindowInfo(win) {
        var cls = "";
        var name = "";
        var desktop = "";
        var caption = "";
        if (win) {
            cls = win.resourceClass ? String(win.resourceClass) : "";
            name = win.resourceName ? String(win.resourceName) : "";
            desktop = win.desktopFileName ? String(win.desktopFileName) : "";
            caption = win.caption ? String(win.caption) : "";
        }
        var cx = "0";
        var cy = "0";
        if (workspace.cursorPos) {
            cx = String(Math.round(workspace.cursorPos.x));
            cy = String(Math.round(workspace.cursorPos.y));
        }
        callDBus(
            "org.breeze.Autoscroll",
            "/Autoscroll",
            "org.breeze.Autoscroll",
            "UpdateActiveWindow",
            cls, name, desktop, caption, cx, cy
        );
    }

    function bindWindow(win) {
        if (trackedWin && trackedHandler && trackedWin.captionChanged) {
            try {
                trackedWin.captionChanged.disconnect(trackedHandler);
            } catch (e) {}
        }
        trackedWin = win;
        trackedHandler = null;

        sendWindowInfo(win);

        if (win && win.captionChanged) {
            trackedHandler = function () {
                if (workspace.activeWindow === win) {
                    sendWindowInfo(win);
                }
            };
            win.captionChanged.connect(trackedHandler);
        }
    }

    workspace.windowActivated.connect(function (win) {
        bindWindow(win);
    });

    bindWindow(workspace.activeWindow);
})();
"#;

const KWIN_CURSOR_LIVE_JS: &str = r#"
// Breeze Autoscroll - On-Demand Pointer Probe & Live Cursor Tracker
(function () {
    var lastX = -999999;
    var lastY = -999999;
    var lastSentMs = 0;

    function pickWindowUnderCursor(p) {
        try {
            if (p && typeof workspace.windowAt === "function") {
                var list = workspace.windowAt(p, 4);
                if (list && list.length > 0) {
                    for (var i = 0; i < list.length; i++) {
                        var w = list[i];
                        if (!w) continue;
                        var wCls = w.resourceClass ? String(w.resourceClass) : "";
                        if (wCls === "breeze-autoscroll" || w.tooltip) {
                            continue;
                        }
                        return w;
                    }
                }
            }
        } catch (e) {}
        return workspace.activeWindow;
    }

    function sendProbe() {
        var p = workspace.cursorPos;
        var cx = p ? Math.round(p.x) : 0;
        var cy = p ? Math.round(p.y) : 0;
        var vs = workspace.virtualScreenSize;
        var vw = vs ? Math.round(vs.width) : 0;
        var vh = vs ? Math.round(vs.height) : 0;

        var win = pickWindowUnderCursor(p);
        var cls = "";
        var name = "";
        var desktop = "";
        var caption = "";
        var isDockOrDesktop = "0";

        if (win) {
            cls = win.resourceClass ? String(win.resourceClass) : "";
            name = win.resourceName ? String(win.resourceName) : "";
            desktop = win.desktopFileName ? String(win.desktopFileName) : "";
            caption = win.caption ? String(win.caption) : "";
            if (win.dock || win.desktopWindow || win.isDock || win.isDesktop) {
                isDockOrDesktop = "1";
            } else if ((cls === "plasmashell" || name === "plasmashell") && !win.normalWindow && !win.dialog) {
                isDockOrDesktop = "1";
            }
        }

        lastX = cx;
        lastY = cy;
        lastSentMs = Date.now();

        callDBus(
            "org.breeze.Autoscroll",
            "/Autoscroll",
            "org.breeze.Autoscroll",
            "ReportPointerProbe",
            String(cx), String(cy),
            String(vw), String(vh),
            cls, name, desktop, caption, isDockOrDesktop
        );
    }

    if (workspace.cursorPosChanged) {
        workspace.cursorPosChanged.connect(function () {
            var p = workspace.cursorPos;
            if (!p) return;
            var nx = Math.round(p.x);
            var ny = Math.round(p.y);
            var now = Date.now();
            if (now - lastSentMs >= 8 || Math.abs(nx - lastX) + Math.abs(ny - lastY) >= 2) {
                lastX = nx;
                lastY = ny;
                lastSentMs = now;
                callDBus(
                    "org.breeze.Autoscroll",
                    "/Autoscroll",
                    "org.breeze.Autoscroll",
                    "UpdateCursorPos",
                    String(nx), String(ny)
                );
            }
        });
    }

    sendProbe();
})();
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::default_excluded_apps_and_titles;

    #[test]
    fn excludes_onshape_browser_tab_in_firefox_and_chrome() {
        let patterns = default_excluded_apps_and_titles();

        let firefox_onshape = ActiveWindowInfo {
            app_class: "firefox_firefox".to_string(),
            app_name: "firefox".to_string(),
            desktop_file: "firefox_firefox".to_string(),
            title: "Part Studio 1 - Bracket | Onshape — Mozilla Firefox".to_string(),
            updated_at_ms: 0,
        };
        assert_eq!(
            match_excluded_3d_context(&firefox_onshape, &patterns),
            Some("onshape".to_string())
        );

        let chrome_onshape = ActiveWindowInfo {
            app_class: "google-chrome".to_string(),
            app_name: "google-chrome".to_string(),
            desktop_file: "google-chrome".to_string(),
            title: "Assembly 1 - Onshape - Google Chrome".to_string(),
            updated_at_ms: 0,
        };
        assert_eq!(
            match_excluded_3d_context(&chrome_onshape, &patterns),
            Some("onshape".to_string())
        );
    }

    #[test]
    fn excludes_bambu_studio_and_other_3d_cad_and_slicers() {
        let patterns = default_excluded_apps_and_titles();

        let bambu = ActiveWindowInfo {
            app_class: "BambuStudio".to_string(),
            app_name: "bambu-studio".to_string(),
            desktop_file: "com.bambulab.BambuStudio".to_string(),
            title: "plate_1.3mf - Bambu Studio".to_string(),
            updated_at_ms: 0,
        };
        assert!(match_excluded_3d_context(&bambu, &patterns).is_some());

        let orca = ActiveWindowInfo {
            app_class: "OrcaSlicer".to_string(),
            app_name: "orca-slicer".to_string(),
            desktop_file: "OrcaSlicer".to_string(),
            title: "Calibration Cube - OrcaSlicer".to_string(),
            updated_at_ms: 0,
        };
        assert!(match_excluded_3d_context(&orca, &patterns).is_some());

        let freecad = ActiveWindowInfo {
            app_class: "org.freecad.FreeCAD".to_string(),
            app_name: "freecad".to_string(),
            desktop_file: "org.freecad.FreeCAD".to_string(),
            title: "FreeCAD 1.0".to_string(),
            updated_at_ms: 0,
        };
        assert!(match_excluded_3d_context(&freecad, &patterns).is_some());

        let blender = ActiveWindowInfo {
            app_class: "blender".to_string(),
            app_name: "blender".to_string(),
            desktop_file: "blender".to_string(),
            title: "Blender*".to_string(),
            updated_at_ms: 0,
        };
        assert!(match_excluded_3d_context(&blender, &patterns).is_some());
    }

    #[test]
    fn allows_normal_apps_and_avoids_short_word_false_positives() {
        let patterns = default_excluded_apps_and_titles();

        let dolphin = ActiveWindowInfo {
            app_class: "org.kde.dolphin".to_string(),
            app_name: "dolphin".to_string(),
            desktop_file: "org.kde.dolphin".to_string(),
            title: "Dokumente — Dolphin".to_string(),
            updated_at_ms: 0,
        };
        assert_eq!(match_excluded_3d_context(&dolphin, &patterns), None);

        // "Accurate" contains "cura", which must NOT match short token "cura" in title
        let firefox_accurate = ActiveWindowInfo {
            app_class: "firefox_firefox".to_string(),
            app_name: "firefox".to_string(),
            desktop_file: "firefox_firefox".to_string(),
            title: "Accurate Weather Forecast — Mozilla Firefox".to_string(),
            updated_at_ms: 0,
        };
        assert_eq!(match_excluded_3d_context(&firefox_accurate, &patterns), None);

        // Actual "UltiMaker Cura" title MUST match "cura"
        let cura_title = ActiveWindowInfo {
            app_class: "firefox_firefox".to_string(),
            app_name: "firefox".to_string(),
            desktop_file: "firefox_firefox".to_string(),
            title: "UltiMaker Cura Documentation — Mozilla Firefox".to_string(),
            updated_at_ms: 0,
        };
        assert_eq!(
            match_excluded_3d_context(&cura_title, &patterns),
            Some("cura".to_string())
        );
    }

    #[test]
    fn scales_kwin_coordinates_to_x11_accurately() {
        // 1:1 desktop (like 5760x1200)
        assert_eq!(
            scale_kwin_to_x11(2600, 640, Some((5760, 1200)), Some((5760, 1200))),
            (2600, 640)
        );
        // 2x HiDPI XWayland scaling (2880x600 logical -> 5760x1200 physical)
        assert_eq!(
            scale_kwin_to_x11(1000, 300, Some((2880, 600)), Some((5760, 1200))),
            (2000, 600)
        );
        // Missing sizes pass through untouched
        assert_eq!(scale_kwin_to_x11(1234, 567, None, None), (1234, 567));
    }

    #[test]
    fn window_tracker_respects_toggle_and_kwin_updates() {
        let tracker = WindowTracker::new();
        let mut cfg = ConfigData::default();

        tracker.update_from_kwin(
            "firefox_firefox".to_string(),
            "firefox".to_string(),
            "firefox_firefox".to_string(),
            "My Bracket - Onshape — Mozilla Firefox".to_string(),
            Some(1200),
            Some(640),
        );

        assert_eq!(
            tracker.check_excluded_context(&cfg),
            Some("onshape".to_string())
        );

        cfg.auto_exclude_3d_apps = false;
        assert_eq!(tracker.check_excluded_context(&cfg), None);
    }

    #[test]
    fn check_probe_excluded_inspects_window_under_cursor() {
        let tracker = WindowTracker::new();
        let cfg = ConfigData::default();

        // Active window is Dolphin (not excluded), but pointer clicked over an unfocused Onshape tab!
        tracker.update_from_kwin(
            "org.kde.dolphin".to_string(),
            "dolphin".to_string(),
            "org.kde.dolphin".to_string(),
            "Home — Dolphin".to_string(),
            Some(200),
            Some(200),
        );

        let probe = PointerProbeResult {
            cursor_x: 2400,
            cursor_y: 500,
            overlay_x: 2400,
            overlay_y: 500,
            window: Some(ActiveWindowInfo {
                app_class: "firefox_firefox".to_string(),
                app_name: "firefox".to_string(),
                desktop_file: "firefox_firefox".to_string(),
                title: "Part Studio 1 - Onshape — Mozilla Firefox".to_string(),
                updated_at_ms: 0,
            }),
            is_dock_or_desktop: false,
            from_kwin: true,
        };

        assert_eq!(
            tracker.check_probe_excluded(&probe, &cfg),
            Some("onshape".to_string())
        );
    }
}
