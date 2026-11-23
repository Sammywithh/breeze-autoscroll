//! Active window, window-under-cursor, live pointer coordinates, and 3D/CAD context detection.
//!
//! Works across KDE Plasma 6 (Wayland + KWin Scripting / D-Bus) and X11 / XWayland (`x11rb`).

use std::env;
use std::ffi::{CStr, OsString};
use std::fs::{File, OpenOptions};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::sync::mpsc::{self, RecvTimeoutError, SyncSender};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use x11rb::connection::Connection as X11Connection;
use x11rb::protocol::xfixes::ConnectionExt as _;
use x11rb::protocol::xproto::{AtomEnum, ConnectionExt as _, Window};
use x11rb::rust_connection::RustConnection;
use zbus::fdo::DBusProxy;
use zbus::{Connection, Proxy, connection};

use crate::atspi_inspector::AtSpiInspector;
use crate::config::{ConfigData, ConfigManager, write_file_atomic_secure};
use crate::tray::{BoundedBackoff, KWinOwnerVerifier, next_or_closed};

const KWIN_BUS_NAME: &str = "org.kde.KWin";
const KWIN_TRACKER_SCRIPT_NAME: &str = "breeze_autoscroll_tracker";
const KWIN_LEGACY_LIVE_SCRIPT_NAME: &str = "breeze_autoscroll_cursor_live";
const KWIN_SHORTCUT_PROBE_START: &str = "breeze_autoscroll_probe_start";
const KWIN_SHORTCUT_PROBE_STOP: &str = "breeze_autoscroll_probe_stop";
const KWIN_BRIDGE_DBUS_TIMEOUT_MS: u64 = 500;
const KWIN_LOAD_BACKOFF_MS: &[u64] = &[250, 500, 1_000, 2_000, 5_000];
const BRIDGE_CHANNEL_CAPACITY: usize = 32;
const PROBE_TIMEOUT_MS: u64 = 80;
const MAX_DRM_SLICE_LEN: usize = 4096;
/// Client-side headerbar / tab strip height for frameless CSD applications
/// (GTK4/Libadwaita, Electron apps like Obsidian/VS Code/Discord, etc.).
const CSD_HEADERBAR_HEIGHT_PX: i32 = 48;
/// Fallback top toolbar + tab bar height for tabbed file managers (Dolphin, Nautilus, etc.)
/// if AT-SPI2 hit-testing is unavailable.
const FILE_MANAGER_TOP_CHROME_PX: i32 = 86;

/// Metadata about an application window (including active browser tab title).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ActiveWindowInfo {
    pub app_class: String,
    pub app_name: String,
    pub desktop_file: String,
    pub title: String,
    pub updated_at_ms: u64,
}

/// Snapshot of pointer coordinates, target window geometry, and cursor shape captured
/// right when `BTN_MIDDLE` is pressed.
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
    /// True if the pointer was clicked on a desktop wallpaper, dock, panel, menu, or popup.
    pub is_dock_or_desktop: bool,
    /// Client area X of the window under the cursor (in logical compositor coordinates).
    pub win_x: i32,
    /// Client area Y of the window under the cursor (in logical compositor coordinates).
    pub win_y: i32,
    /// Client area width of the window under the cursor.
    pub win_w: i32,
    /// Client area height of the window under the cursor.
    pub win_h: i32,
    /// PID of the window directly under the cursor (for AT-SPI2 hit-testing).
    pub win_pid: u32,
    /// True when the target window is in fullscreen mode.
    pub is_fullscreen: bool,
    /// True when the target window uses client-side decorations (`noBorder == true`).
    pub no_border: bool,
    /// Detected cursor hotspot X (`~16` for `pointer`/`pointing_hand`, `~4` for `default`).
    pub hotspot_x: Option<i32>,
    /// Detected cursor hotspot Y (`~4` for `pointer`/`default`, `~15` for `text`).
    pub hotspot_y: Option<i32>,
    /// True when the cursor shape is a hyperlink/button pointer (`pointing_hand` / `pointer` / `hand2`).
    pub is_interactive_cursor: bool,
    /// Deepest AT-SPI2 role name under the pointer (e.g. `"list item"`, `"page tab"`, `"list"`).
    pub atspi_role: Option<String>,
    /// True when AT-SPI2 identified an interactive element (folder/file icon, tab, sidebar item, button).
    pub is_interactive_atspi: bool,
    /// True when coordinates came directly from KWin's live compositor state.
    pub from_kwin: bool,
}

/// Raw pointer probe payload pushed from KWin on the D-Bus dispatcher thread.
/// Enriched off the `zbus` thread (DRM/X11 cursor hotspot + AT-SPI2 hit-test) by the caller.
#[derive(Debug, Clone, Default)]
struct RawKWinProbe {
    cursor_x: i32,
    cursor_y: i32,
    virt_w: i32,
    virt_h: i32,
    window: Option<ActiveWindowInfo>,
    is_dock_or_desktop: bool,
    win_x: i32,
    win_y: i32,
    win_w: i32,
    win_h: i32,
    win_pid: u32,
    is_fullscreen: bool,
    no_border: bool,
    screen_x: i32,
    screen_y: i32,
    screen_scale: f64,
}

#[derive(Debug, Default)]
struct TrackerState {
    kwin_active: Option<ActiveWindowInfo>,
    kwin_cursor: Option<(i32, i32)>,
    kwin_virtual_size: Option<(i32, i32)>,
    probe_seq: u64,
    last_raw_probe: Option<RawKWinProbe>,
    #[cfg(feature = "debug-dbus")]
    last_probe: Option<PointerProbeResult>,
    window_context_changed: bool,
}

// ============================================================================
// Linux DRM Hardware Cursor Plane Inspector (Wayland Cursor Hotspot Detection)
// ============================================================================

const DRM_CLIENT_CAP_UNIVERSAL_PLANES: u64 = 1;
const DRM_CLIENT_CAP_ATOMIC: u64 = 3;
const DRM_MODE_OBJECT_PLANE: u32 = 0xeeeeeeee;
const DRM_PLANE_TYPE_CURSOR: u64 = 2;

#[repr(C)]
struct DrmModePlaneRes {
    count_planes: u32,
    planes: *mut u32,
}

#[repr(C)]
struct DrmModePlane {
    count_formats: u32,
    formats: *mut u32,
    plane_id: u32,
    crtc_id: u32,
    fb_id: u32,
    crtc_x: i32,
    crtc_y: i32,
    x: u32,
    y: u32,
    possible_crtcs: u32,
    gamma_size: u32,
}

#[repr(C)]
struct DrmModeObjectProperties {
    count_props: u32,
    props: *mut u32,
    prop_values: *mut u64,
}

#[repr(C)]
struct DrmModePropertyRes {
    prop_id: u32,
    flags: u32,
    name: [libc::c_char; 32],
    count_values: libc::c_int,
    values: *mut u64,
    count_enums: libc::c_int,
    enums: *mut libc::c_void,
    count_blobs: libc::c_int,
    blob_ids: *mut u32,
}

type FnDrmSetClientCap = unsafe extern "C" fn(libc::c_int, u64, u64) -> libc::c_int;
type FnDrmModeGetPlaneResources = unsafe extern "C" fn(libc::c_int) -> *mut DrmModePlaneRes;
type FnDrmModeFreePlaneResources = unsafe extern "C" fn(*mut DrmModePlaneRes);
type FnDrmModeGetPlane = unsafe extern "C" fn(libc::c_int, u32) -> *mut DrmModePlane;
type FnDrmModeFreePlane = unsafe extern "C" fn(*mut DrmModePlane);
type FnDrmModeObjectGetProperties =
    unsafe extern "C" fn(libc::c_int, u32, u32) -> *mut DrmModeObjectProperties;
type FnDrmModeFreeObjectProperties = unsafe extern "C" fn(*mut DrmModeObjectProperties);
type FnDrmModeGetProperty = unsafe extern "C" fn(libc::c_int, u32) -> *mut DrmModePropertyRes;
type FnDrmModeFreeProperty = unsafe extern "C" fn(*mut DrmModePropertyRes);

struct DlHandle(*mut libc::c_void);

unsafe impl Send for DlHandle {}

impl Drop for DlHandle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                libc::dlclose(self.0);
            }
            self.0 = std::ptr::null_mut();
        }
    }
}

/// Safely reads a bounded C string from a fixed-size DRM property name buffer (`[c_char; 32]`)
/// without relying on `CStr::from_ptr` (which would cause UB if no NUL byte is present).
fn drm_prop_name_from_bytes(raw: &[libc::c_char; 32]) -> Option<&str> {
    let bytes: &[u8; 32] = unsafe { &*(raw as *const [libc::c_char; 32]).cast::<[u8; 32]>() };
    let len = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    std::str::from_utf8(&bytes[..len]).ok()
}

/// Safely constructs a slice from a C pointer + length only when `ptr` is non-null and
/// `len` is within `1..=MAX_DRM_SLICE_LEN`.
unsafe fn safe_c_slice<'a, T>(ptr: *const T, len: usize) -> &'a [T] {
    if ptr.is_null() || len == 0 || len > MAX_DRM_SLICE_LEN {
        &[]
    } else {
        unsafe { std::slice::from_raw_parts(ptr, len) }
    }
}

struct CursorPlaneInfo {
    plane_id: u32,
    crtc_x_prop: u32,
    crtc_y_prop: u32,
}

struct DrmCursorInspector {
    _file: File,
    fd: RawFd,
    cursor_planes: Vec<CursorPlaneInfo>,
    get_plane: FnDrmModeGetPlane,
    free_plane: FnDrmModeFreePlane,
    get_obj_props: FnDrmModeObjectGetProperties,
    free_obj_props: FnDrmModeFreeObjectProperties,
    // Declared LAST so `_file` (DRM FD) is closed before `libdrm.so.2` is `dlclose`d.
    _dl_handle: DlHandle,
}

enum DrmState {
    Uninitialized,
    Unavailable,
    Ready(DrmCursorInspector),
}

impl DrmState {
    fn query_or_init_with<F, Q>(&mut self, open_fn: F, query_fn: Q) -> Option<(i32, i32)>
    where
        F: FnOnce() -> Option<DrmCursorInspector>,
        Q: FnOnce(&DrmCursorInspector) -> Option<(i32, i32)>,
    {
        match self {
            DrmState::Uninitialized => match open_fn() {
                Some(inspector) => {
                    let pos = query_fn(&inspector);
                    *self = DrmState::Ready(inspector);
                    pos
                }
                None => {
                    *self = DrmState::Unavailable;
                    None
                }
            },
            DrmState::Unavailable => None,
            DrmState::Ready(inspector) => query_fn(inspector),
        }
    }
}

unsafe impl Send for DrmCursorInspector {}

impl DrmCursorInspector {
    fn open() -> Option<Self> {
        unsafe {
            let raw_handle = libc::dlopen(c"libdrm.so.2".as_ptr(), libc::RTLD_LAZY);
            if raw_handle.is_null() {
                return None;
            }
            // RAII guard ensures `dlclose` on every early return path.
            let dl_handle = DlHandle(raw_handle);

            let set_cap: FnDrmSetClientCap =
                std::mem::transmute(load_sym(dl_handle.0, c"drmSetClientCap")?);
            let get_res: FnDrmModeGetPlaneResources =
                std::mem::transmute(load_sym(dl_handle.0, c"drmModeGetPlaneResources")?);
            let free_res: FnDrmModeFreePlaneResources =
                std::mem::transmute(load_sym(dl_handle.0, c"drmModeFreePlaneResources")?);
            let get_plane: FnDrmModeGetPlane =
                std::mem::transmute(load_sym(dl_handle.0, c"drmModeGetPlane")?);
            let free_plane: FnDrmModeFreePlane =
                std::mem::transmute(load_sym(dl_handle.0, c"drmModeFreePlane")?);
            let get_obj_props: FnDrmModeObjectGetProperties =
                std::mem::transmute(load_sym(dl_handle.0, c"drmModeObjectGetProperties")?);
            let free_obj_props: FnDrmModeFreeObjectProperties =
                std::mem::transmute(load_sym(dl_handle.0, c"drmModeFreeObjectProperties")?);
            let get_prop: FnDrmModeGetProperty =
                std::mem::transmute(load_sym(dl_handle.0, c"drmModeGetProperty")?);
            let free_prop: FnDrmModeFreeProperty =
                std::mem::transmute(load_sym(dl_handle.0, c"drmModeFreeProperty")?);

            for card_idx in 0..4 {
                let path = format!("/dev/dri/card{card_idx}");
                let Ok(file) = OpenOptions::new()
                    .read(true)
                    .custom_flags(libc::O_CLOEXEC)
                    .open(&path)
                else {
                    continue;
                };
                let fd = file.as_raw_fd();
                let _ = set_cap(fd, DRM_CLIENT_CAP_UNIVERSAL_PLANES, 1);
                let _ = set_cap(fd, DRM_CLIENT_CAP_ATOMIC, 1);

                let res_ptr = get_res(fd);
                if res_ptr.is_null() {
                    continue;
                }
                let count = (*res_ptr).count_planes as usize;
                let plane_ids = safe_c_slice((*res_ptr).planes, count).to_vec();
                free_res(res_ptr);

                let mut cursor_planes = Vec::new();
                for pid in plane_ids {
                    let props_ptr = get_obj_props(fd, pid, DRM_MODE_OBJECT_PLANE);
                    if props_ptr.is_null() {
                        continue;
                    }
                    let nprops = (*props_ptr).count_props as usize;
                    let prop_ids = safe_c_slice((*props_ptr).props, nprops);
                    let prop_vals = safe_c_slice((*props_ptr).prop_values, nprops);
                    let pair_count = prop_ids.len().min(prop_vals.len());

                    let mut plane_type = 0u64;
                    let mut crtc_x_prop = 0u32;
                    let mut crtc_y_prop = 0u32;

                    for i in 0..pair_count {
                        let p_ptr = get_prop(fd, prop_ids[i]);
                        if p_ptr.is_null() {
                            continue;
                        }
                        if let Some(name) = drm_prop_name_from_bytes(&(*p_ptr).name) {
                            match name {
                                "type" => plane_type = prop_vals[i],
                                "CRTC_X" => crtc_x_prop = prop_ids[i],
                                "CRTC_Y" => crtc_y_prop = prop_ids[i],
                                _ => {}
                            }
                        }
                        free_prop(p_ptr);
                    }
                    free_obj_props(props_ptr);

                    if plane_type == DRM_PLANE_TYPE_CURSOR && crtc_x_prop != 0 && crtc_y_prop != 0 {
                        cursor_planes.push(CursorPlaneInfo {
                            plane_id: pid,
                            crtc_x_prop,
                            crtc_y_prop,
                        });
                    }
                }

                if !cursor_planes.is_empty() {
                    return Some(Self {
                        _file: file,
                        fd,
                        cursor_planes,
                        get_plane,
                        free_plane,
                        get_obj_props,
                        free_obj_props,
                        _dl_handle: dl_handle,
                    });
                }
            }
            None
        }
    }

    /// Queries the active hardware cursor plane (`fb_id != 0`) for its CRTC-local `(CRTC_X, CRTC_Y)`.
    fn query_active_cursor_crtc_pos(&self) -> Option<(i32, i32)> {
        unsafe {
            for cp in &self.cursor_planes {
                let plane_ptr = (self.get_plane)(self.fd, cp.plane_id);
                if plane_ptr.is_null() {
                    continue;
                }
                let fb_id = (*plane_ptr).fb_id;
                let crtc_id = (*plane_ptr).crtc_id;
                (self.free_plane)(plane_ptr);

                if fb_id == 0 || crtc_id == 0 {
                    continue;
                }

                let props_ptr = (self.get_obj_props)(self.fd, cp.plane_id, DRM_MODE_OBJECT_PLANE);
                if props_ptr.is_null() {
                    continue;
                }
                let nprops = (*props_ptr).count_props as usize;
                let prop_ids = safe_c_slice((*props_ptr).props, nprops);
                let prop_vals = safe_c_slice((*props_ptr).prop_values, nprops);
                let pair_count = prop_ids.len().min(prop_vals.len());

                let mut crtc_x: Option<i32> = None;
                let mut crtc_y: Option<i32> = None;
                for i in 0..pair_count {
                    if prop_ids[i] == cp.crtc_x_prop {
                        crtc_x = Some(prop_vals[i] as u32 as i32);
                    } else if prop_ids[i] == cp.crtc_y_prop {
                        crtc_y = Some(prop_vals[i] as u32 as i32);
                    }
                }
                (self.free_obj_props)(props_ptr);

                if let (Some(cx), Some(cy)) = (crtc_x, crtc_y) {
                    return Some((cx, cy));
                }
            }
            None
        }
    }
}

unsafe fn load_sym(handle: *mut libc::c_void, name: &CStr) -> Option<*mut libc::c_void> {
    let ptr = unsafe { libc::dlsym(handle, name.as_ptr()) };
    (!ptr.is_null()).then_some(ptr)
}

type CursorHotspotInfo = ((i32, i32), bool);

struct X11Client {
    conn: RustConnection,
    root: Window,
    root_width: i32,
    root_height: i32,
}

impl X11Client {
    fn connect() -> Option<Self> {
        env::var_os("DISPLAY")?;
        let (conn, screen_num) = x11rb::connect(None).ok()?;
        let _ = conn
            .xfixes_query_version(5, 0)
            .ok()
            .and_then(|c| c.reply().ok());
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

    fn query_cursor_hotspot_and_interactive(&self) -> Result<Option<CursorHotspotInfo>, ()> {
        let cookie = self
            .conn
            .xfixes_get_cursor_image_and_name()
            .map_err(|_| ())?;
        let reply = cookie.reply().map_err(|_| ())?;
        let hx = reply.xhot as i32;
        let hy = reply.yhot as i32;
        let name = String::from_utf8_lossy(&reply.name);
        let interactive = is_pointer_hand_name(&name) || is_pointer_hand_hotspot(hx, hy);
        Ok(Some(((hx, hy), interactive)))
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
    ReinstallTracker,
    KWinOwnerChanged(Option<String>),
    StartLiveCursorAndProbe,
    StopLiveCursor,
    Shutdown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KWinOwnerAction {
    MarkAbsent,
    KeepInstalled,
    ReloadScript,
}

fn decide_kwin_owner_action(
    tracker_installed: bool,
    loaded_owner: Option<&str>,
    new_owner: Option<&str>,
) -> KWinOwnerAction {
    let Some(owner) = new_owner.filter(|s| !s.is_empty()) else {
        return KWinOwnerAction::MarkAbsent;
    };
    if tracker_installed && loaded_owner == Some(owner) {
        KWinOwnerAction::KeepInstalled
    } else {
        KWinOwnerAction::ReloadScript
    }
}

struct BridgeHandle {
    tx: SyncSender<BridgeCmd>,
    join: Option<JoinHandle<()>>,
    watch_state: Arc<Mutex<(bool, Option<Connection>)>>,
    watch_join: Option<JoinHandle<()>>,
}

/// Thread-safe active window, Wayland/X11 pointer probe, and live cursor position tracker.
#[derive(Clone)]
pub struct WindowTracker {
    sync: Arc<(Mutex<TrackerState>, Condvar)>,
    x11: Arc<Mutex<Option<X11Client>>>,
    drm: Arc<Mutex<DrmState>>,
    atspi: AtSpiInspector,
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
            drm: Arc::new(Mutex::new(DrmState::Uninitialized)),
            atspi: AtSpiInspector::new(),
            bridge: Arc::new(Mutex::new(None)),
        }
    }

    /// Starts the persistent KWin D-Bus scripting bridge, `NameOwnerChanged` watcher for
    /// `org.kde.KWin`, and opt-in AT-SPI2 inspector.
    pub(crate) fn start_kwin_bridge(
        &self,
        config: ConfigManager,
        kwin_verifier: Option<KWinOwnerVerifier>,
    ) {
        self.atspi.start(config);

        let Ok(mut guard) = self.bridge.lock() else {
            return;
        };
        if guard.is_some() {
            return;
        }

        let (tx, rx) = mpsc::sync_channel(BRIDGE_CHANNEL_CAPACITY);
        let bridge_verifier = kwin_verifier.clone();
        let join = thread::Builder::new()
            .name("breeze-kwin-bridge".to_string())
            .spawn(move || run_kwin_bridge_loop(rx, bridge_verifier))
            .ok();

        let watch_state: Arc<Mutex<(bool, Option<Connection>)>> =
            Arc::new(Mutex::new((false, None)));
        let watch_state_clone = Arc::clone(&watch_state);
        let watch_tx = tx.clone();
        let watch_join = thread::Builder::new()
            .name("breeze-kwin-watch".to_string())
            .spawn(move || run_kwin_owner_watcher(watch_tx, watch_state_clone, kwin_verifier))
            .ok();

        let _ = tx.try_send(BridgeCmd::EnsureTrackerInstalled);
        *guard = Some(BridgeHandle {
            tx,
            join,
            watch_state,
            watch_join,
        });
    }

    /// Notifies the AT-SPI2 worker to re-read runtime configuration (e.g. `auto_enable_atspi`).
    pub fn sync_atspi_config(&self) {
        self.atspi.sync_config();
    }

    /// Unloads the persistent KWin script and shuts down the KWin bridge, watcher & AT-SPI2 worker threads.
    pub fn stop_kwin_bridge(&self) {
        self.atspi.stop();

        let handle = if let Ok(mut guard) = self.bridge.lock() {
            guard.take()
        } else {
            None
        };
        if let Some(mut h) = handle {
            let active_watch_conn = if let Ok(mut guard) = h.watch_state.lock() {
                guard.0 = true;
                guard.1.take()
            } else {
                None
            };
            if let Some(conn) = active_watch_conn {
                let _ = zbus::block_on(conn.close());
            }

            if h.tx.try_send(BridgeCmd::Shutdown).is_err() {
                let _ = h.tx.send(BridgeCmd::Shutdown);
            }
            if let Some(j) = h.watch_join.take() {
                let _ = j.join();
            }
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

    /// Stores raw live pointer probe data pushed by the persistent KWin script via D-Bus
    /// and immediately wakes waiters.
    ///
    /// Strictly non-blocking: performs NO DRM, X11, or AT-SPI2 I/O on the `zbus` dispatcher thread.
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
        win_x: i32,
        win_y: i32,
        win_w: i32,
        win_h: i32,
        win_pid: u32,
        is_fullscreen: bool,
        no_border: bool,
        screen_x: i32,
        screen_y: i32,
        screen_scale: f64,
    ) {
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

        let raw = RawKWinProbe {
            cursor_x,
            cursor_y,
            virt_w,
            virt_h,
            window,
            is_dock_or_desktop,
            win_x,
            win_y,
            win_w,
            win_h,
            win_pid,
            is_fullscreen,
            no_border,
            screen_x,
            screen_y,
            screen_scale,
        };

        let (lock, cvar) = &*self.sync;
        if let Ok(mut guard) = lock.lock() {
            guard.kwin_cursor = Some((cursor_x, cursor_y));
            if virt_w > 0 && virt_h > 0 {
                guard.kwin_virtual_size = Some((virt_w, virt_h));
            }
            guard.window_context_changed = false;
            guard.probe_seq = guard.probe_seq.wrapping_add(1);
            guard.last_raw_probe = Some(raw);
            cvar.notify_all();
        }
    }

    /// Enriches a raw KWin probe with X11 overlay scaling, DRM/X11 cursor hotspot detection,
    /// and optional AT-SPI2 hit-testing. Runs on the caller (`InputEngine`) thread, never on `zbus`.
    fn enrich_raw_probe(&self, raw: RawKWinProbe, inspect_atspi: bool) -> PointerProbeResult {
        let x11_size = self.x11_root_size();
        let (overlay_x, overlay_y) = scale_kwin_to_x11(
            raw.cursor_x,
            raw.cursor_y,
            (raw.virt_w > 0 && raw.virt_h > 0).then_some((raw.virt_w, raw.virt_h)),
            x11_size,
        );

        let scale = if raw.screen_scale.is_finite() && raw.screen_scale > 0.1 {
            raw.screen_scale
        } else {
            1.0
        };
        let (hotspot_x, hotspot_y, is_interactive_cursor) =
            if let Some((crtc_x, crtc_y)) = self.query_drm_cursor_crtc_pos() {
                let local_x = ((raw.cursor_x - raw.screen_x) as f64 * scale).round() as i32;
                let local_y = ((raw.cursor_y - raw.screen_y) as f64 * scale).round() as i32;
                let hx = local_x - crtc_x;
                let hy = local_y - crtc_y;
                (Some(hx), Some(hy), is_pointer_hand_hotspot(hx, hy))
            } else if let Some(((hx, hy), interactive)) = self.query_x11_cursor_hotspot() {
                (Some(hx), Some(hy), interactive)
            } else {
                (None, None, false)
            };

        let is_browser = raw.window.as_ref().is_some_and(is_browser_window);
        let in_client_rect = raw.win_w > 0
            && raw.win_h > 0
            && raw.cursor_x >= raw.win_x
            && raw.cursor_x < raw.win_x + raw.win_w
            && raw.cursor_y >= raw.win_y
            && raw.cursor_y < raw.win_y + raw.win_h;

        let (atspi_role, is_interactive_atspi) = if inspect_atspi
            && !raw.is_dock_or_desktop
            && !is_interactive_cursor
            && !is_browser
            && raw.win_pid > 0
            && in_client_rect
        {
            let rel_x = raw.cursor_x - raw.win_x;
            let rel_y = raw.cursor_y - raw.win_y;
            let title_ref = raw.window.as_ref().map(|w| w.title.as_str()).unwrap_or("");
            match self.atspi.inspect_point(
                raw.win_pid,
                title_ref,
                rel_x,
                rel_y,
                raw.cursor_x,
                raw.cursor_y,
                raw.win_w,
                raw.win_h,
            ) {
                Some(hit) => (Some(hit.role), hit.is_interactive),
                None => (None, false),
            }
        } else {
            (None, false)
        };

        let result = PointerProbeResult {
            cursor_x: raw.cursor_x,
            cursor_y: raw.cursor_y,
            overlay_x,
            overlay_y,
            window: raw.window,
            is_dock_or_desktop: raw.is_dock_or_desktop,
            win_x: raw.win_x,
            win_y: raw.win_y,
            win_w: raw.win_w,
            win_h: raw.win_h,
            win_pid: raw.win_pid,
            is_fullscreen: raw.is_fullscreen,
            no_border: raw.no_border,
            hotspot_x,
            hotspot_y,
            is_interactive_cursor,
            atspi_role,
            is_interactive_atspi,
            from_kwin: true,
        };

        #[cfg(feature = "debug-dbus")]
        {
            let (lock, _) = &*self.sync;
            if let Ok(mut guard) = lock.lock() {
                guard.last_probe = Some(result.clone());
            }
        }

        result
    }

    /// Updates live cursor coordinates streamed from the persistent KWin script while active.
    pub fn update_cursor_pos(&self, cursor_x: i32, cursor_y: i32) {
        let (lock, _) = &*self.sync;
        if let Ok(mut guard) = lock.lock() {
            guard.kwin_cursor = Some((cursor_x, cursor_y));
        }
    }

    /// Records that the target window's title/tab or active focus changed right after a middle-click.
    pub fn notify_window_context_changed(&self) {
        let (lock, _) = &*self.sync;
        if let Ok(mut guard) = lock.lock() {
            guard.window_context_changed = true;
        }
    }

    /// Atomically reads and clears the window context changed flag.
    pub fn take_window_context_changed(&self) -> bool {
        let (lock, _) = &*self.sync;
        if let Ok(mut guard) = lock.lock() {
            let changed = guard.window_context_changed;
            guard.window_context_changed = false;
            changed
        } else {
            false
        }
    }

    /// Probes the exact cursor position and target window under the pointer, starts live
    /// KWin cursor streaming on Wayland sessions, and enriches the probe off the `zbus` thread.
    pub fn probe_pointer_and_start_live_cursor(&self, inspect_atspi: bool) -> PointerProbeResult {
        let bridge_tx = self
            .bridge
            .lock()
            .ok()
            .and_then(|g| g.as_ref().map(|h| h.tx.clone()));

        if let Some(tx) = bridge_tx {
            let baseline_seq = {
                let (lock, _) = &*self.sync;
                lock.lock()
                    .map(|mut g| {
                        g.window_context_changed = false;
                        g.probe_seq
                    })
                    .unwrap_or(0)
            };

            if tx.try_send(BridgeCmd::StartLiveCursorAndProbe).is_ok() {
                let deadline = Instant::now() + Duration::from_millis(PROBE_TIMEOUT_MS);
                let (lock, cvar) = &*self.sync;
                let mut raw_probed: Option<RawKWinProbe> = None;
                if let Ok(mut guard) = lock.lock() {
                    loop {
                        if guard.probe_seq != baseline_seq {
                            raw_probed = guard.last_raw_probe.clone();
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
                                    raw_probed = guard.last_raw_probe.clone();
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
                if let Some(raw) = raw_probed {
                    return self.enrich_raw_probe(raw, inspect_atspi);
                }
                // KWin may have restarted and dropped our persistent script; schedule self-heal.
                let _ = tx.try_send(BridgeCmd::ReinstallTracker);
            }
        }

        let (x11_hs, x11_interactive) = self
            .query_x11_cursor_hotspot()
            .map(|((hx, hy), inter)| ((Some(hx), Some(hy)), inter))
            .unwrap_or(((None, None), false));

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
                hotspot_x: x11_hs.0,
                hotspot_y: x11_hs.1,
                is_interactive_cursor: x11_interactive,
                from_kwin: false,
                ..Default::default()
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
                ..Default::default()
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
                hotspot_x: x11_hs.0,
                hotspot_y: x11_hs.1,
                is_interactive_cursor: x11_interactive,
                from_kwin: false,
                ..Default::default()
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
            ..Default::default()
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

    /// Triggers an asynchronous KWin pointer probe without blocking the caller's D-Bus thread.
    #[cfg(feature = "debug-dbus")]
    pub fn trigger_probe(&self) {
        if let Ok(guard) = self.bridge.lock()
            && let Some(handle) = guard.as_ref()
        {
            let _ = handle.tx.try_send(BridgeCmd::StartLiveCursorAndProbe);
        }
    }

    /// Returns the most recent KWin pointer probe result, if any.
    #[cfg(feature = "debug-dbus")]
    pub fn last_probe_snapshot(&self) -> Option<PointerProbeResult> {
        let (lock, _) = &*self.sync;
        lock.lock().ok().and_then(|g| g.last_probe.clone())
    }

    /// Stops live cursor streaming in the persistent KWin script once autoscroll stops.
    pub fn stop_live_cursor(&self) {
        if let Ok(guard) = self.bridge.lock()
            && let Some(handle) = guard.as_ref()
        {
            let _ = handle.tx.try_send(BridgeCmd::StopLiveCursor);
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

    /// Checks whether a probe result falls on interactive window chrome (server-side titlebar,
    /// browser tab bar / address bar, or CSD headerbar), over a hyperlink/button pointer cursor,
    /// or over an interactive AT-SPI2 UI control/item (Dolphin folders/files, tabs, sidebar items).
    pub fn is_interactive_probe(
        &self,
        probe: &PointerProbeResult,
        config: &ConfigData,
    ) -> bool {
        if !config.ignore_hyperlinks_and_tabs {
            return false;
        }
        is_interactive_window_chrome(probe, config)
            || probe.is_interactive_cursor
            || probe.is_interactive_atspi
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

    /// Checks whether the currently active application or browser tab belongs to an
    /// excluded 3D/CAD/Slicer context.
    #[cfg(test)]
    pub fn check_excluded_context(&self, config: &ConfigData) -> Option<String> {
        if !config.auto_exclude_3d_apps || config.excluded_apps_and_titles.is_empty() {
            return None;
        }
        let info = self.get_active_window()?;
        match_excluded_3d_context(&info, &config.excluded_apps_and_titles)
    }

    fn query_drm_cursor_crtc_pos(&self) -> Option<(i32, i32)> {
        let Ok(mut guard) = self.drm.lock() else {
            return None;
        };
        guard.query_or_init_with(DrmCursorInspector::open, |inspector| {
            inspector.query_active_cursor_crtc_pos()
        })
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

    fn query_x11_cursor_hotspot(&self) -> Option<((i32, i32), bool)> {
        self.with_x11(|c| c.query_cursor_hotspot_and_interactive())
    }

    fn query_x11_active(&self) -> Option<ActiveWindowInfo> {
        self.with_x11(|c| c.query_active_window())
    }
}

/// Returns true when the cursor hotspot `(hx, hy)` matches the pointing-hand (`pointer` /
/// `pointing_hand` / `hand2`) cursor used for hyperlinks (`<a href>`) and interactive buttons.
///
/// Across cursor sizes `12px..72px`:
/// - `default` (`left_ptr`): `hx == hy` (e.g. `(4, 4)` at 24px)
/// - `text` (`ibeam`): `hx ≈ hy` (e.g. `(16, 15)` at 24px)
/// - `pointer` (`pointing_hand`): `hx ≈ 4 * hy` (e.g. `(16, 4)` at 24px, `(24, 6)` at 36px, `(33, 8)` at 48px)
pub fn is_pointer_hand_hotspot(hx: i32, hy: i32) -> bool {
    (9..=56).contains(&hx) && (1..=15).contains(&hy) && hx >= 2 * hy + 4
}

/// Returns true when an X11/XFixes cursor name identifies a link/button hand pointer.
pub fn is_pointer_hand_name(name: &str) -> bool {
    let trimmed = name.trim().to_ascii_lowercase();
    matches!(
        trimmed.as_str(),
        "pointer" | "pointing_hand" | "hand2" | "hand1" | "hand"
    )
}

/// Returns true when the window metadata identifies a web browser where the top area
/// of the client rectangle contains the tab strip and navigation/address bar.
pub fn is_browser_window(info: &ActiveWindowInfo) -> bool {
    const BROWSER_TOKENS: &[&str] = &[
        "firefox",
        "librewolf",
        "waterfox",
        "floorp",
        "zen",
        "navigator",
        "tor browser",
        "brave",
        "chrome",
        "chromium",
        "msedge",
        "microsoft-edge",
        "vivaldi",
        "opera",
        "epiphany",
        "falkon",
        "thorium",
    ];
    let fields = [
        info.app_class.to_ascii_lowercase(),
        info.app_name.to_ascii_lowercase(),
        info.desktop_file.to_ascii_lowercase(),
    ];
    BROWSER_TOKENS
        .iter()
        .any(|token| fields.iter().any(|f| f.contains(token)))
}

/// Returns true when the window metadata identifies a tabbed file manager (Dolphin, Nautilus,
/// Thunar, Nemo, PCManFM, Caja, Krusader) where the top area contains the location bar & tab strip.
pub fn is_file_manager_window(info: &ActiveWindowInfo) -> bool {
    const FM_TOKENS: &[&str] = &[
        "dolphin",
        "nautilus",
        "org.gnome.files",
        "nemo",
        "thunar",
        "pcmanfm",
        "caja",
        "krusader",
    ];
    let fields = [
        info.app_class.to_ascii_lowercase(),
        info.app_name.to_ascii_lowercase(),
        info.desktop_file.to_ascii_lowercase(),
    ];
    FM_TOKENS
        .iter()
        .any(|token| fields.iter().any(|f| f.contains(token)))
}

/// Returns true when `probe` was clicked on a window titlebar, frame decoration,
/// browser tab bar / address bar, tabbed file manager toolbar/tab bar fallback, or CSD headerbar.
pub fn is_interactive_window_chrome(
    probe: &PointerProbeResult,
    config: &ConfigData,
) -> bool {
    if !config.ignore_hyperlinks_and_tabs {
        return false;
    }
    if probe.win_w <= 0 || probe.win_h <= 0 {
        return false;
    }

    // 1. Click outside the window's clientGeometry (e.g., server-side KWin titlebar or border)
    if probe.cursor_x < probe.win_x
        || probe.cursor_x >= probe.win_x + probe.win_w
        || probe.cursor_y < probe.win_y
        || probe.cursor_y >= probe.win_y + probe.win_h
    {
        return true;
    }

    if probe.is_fullscreen {
        return false;
    }

    let rel_y = probe.cursor_y - probe.win_y;

    // 2. Web browser top chrome (Tab strip + Navigation/Address bar)
    if probe.window.as_ref().is_some_and(is_browser_window) {
        let top_chrome = config.browser_top_chrome_px as i32;
        if rel_y >= 0 && rel_y < top_chrome {
            return true;
        }
    } else if probe.atspi_role.is_none()
        && probe.window.as_ref().is_some_and(is_file_manager_window)
        && (0..FILE_MANAGER_TOP_CHROME_PX).contains(&rel_y)
    {
        // 3. Fallback toolbar + tab bar guard for file managers when AT-SPI2 is unavailable
        return true;
    } else if probe.no_border && (0..CSD_HEADERBAR_HEIGHT_PX).contains(&rel_y) {
        // 4. Frameless CSD application window headerbar
        return true;
    }

    false
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
// Persistent KWin D-Bus Scripting Bridge (Single Persistent Script + kglobalaccel)
// ============================================================================

fn run_kwin_owner_watcher(
    tx: SyncSender<BridgeCmd>,
    watch_state: Arc<Mutex<(bool, Option<Connection>)>>,
    kwin_verifier: Option<KWinOwnerVerifier>,
) {
    zbus::block_on(async move {
        if watch_state.lock().map(|g| g.0).unwrap_or(true) {
            return;
        }

        let Ok(conn) = connection::Builder::session()
            .method_timeout(Duration::from_millis(KWIN_BRIDGE_DBUS_TIMEOUT_MS))
            .build()
            .await
        else {
            return;
        };

        // Atomically re-check shutdown before publishing `conn` so a fast `stop_kwin_bridge()`
        // during startup can never race or leave the watcher hanging.
        let aborted = match watch_state.lock() {
            Ok(mut guard) if !guard.0 => {
                guard.1 = Some(conn.clone());
                false
            }
            _ => true,
        };
        if aborted {
            let _ = conn.close().await;
            return;
        }

        let owner_stream = match DBusProxy::new(&conn).await {
            Ok(dbus) => dbus
                .receive_name_owner_changed_with_args(&[(0, KWIN_BUS_NAME)])
                .await
                .ok(),
            Err(_) => None,
        };

        if let Some(mut stream) = owner_stream {
            while !watch_state.lock().map(|g| g.0).unwrap_or(true) {
                let Some(sig) = next_or_closed(&mut stream, &conn).await else {
                    break;
                };
                if watch_state.lock().map(|g| g.0).unwrap_or(true) {
                    break;
                }
                if let Ok(args) = sig.args() {
                    if args.name().as_str() != KWIN_BUS_NAME {
                        continue;
                    }
                    let new_owner = args
                        .new_owner()
                        .as_ref()
                        .map(|u| u.to_string())
                        .filter(|s| !s.is_empty());

                    if let Some(ref verifier) = kwin_verifier {
                        verifier.update_owner_from_signal(new_owner.clone());
                    }
                    let _ = tx.try_send(BridgeCmd::KWinOwnerChanged(new_owner));
                }
            }
        }

        if let Ok(mut guard) = watch_state.lock() {
            guard.1 = None;
        }
    });
}

async fn query_kwin_unique_owner(conn: &Connection) -> Option<String> {
    let dbus = DBusProxy::new(conn).await.ok()?;
    let name = KWIN_BUS_NAME.try_into().ok()?;
    dbus.get_name_owner(name)
        .await
        .ok()
        .map(|u| u.to_string())
        .filter(|s| !s.is_empty())
}

async fn try_install_tracker(
    conn_slot: &mut Option<Connection>,
    tracker_str: &str,
    kwin_verifier: Option<&KWinOwnerVerifier>,
    unload_legacy: bool,
) -> Result<Option<String>, ()> {
    if unload_legacy {
        let _ = unload_script(conn_slot, KWIN_LEGACY_LIVE_SCRIPT_NAME).await;
    }
    ensure_loaded_and_run(conn_slot, tracker_str, KWIN_TRACKER_SCRIPT_NAME).await?;
    let owner = if let Some(conn) = conn_slot.as_ref() {
        query_kwin_unique_owner(conn).await
    } else {
        None
    };
    if let (Some(verifier), Some(owner_id)) = (kwin_verifier, owner.clone()) {
        verifier.update_owner_from_signal(Some(owner_id));
    }
    Ok(owner)
}

fn run_kwin_bridge_loop(
    rx: mpsc::Receiver<BridgeCmd>,
    kwin_verifier: Option<KWinOwnerVerifier>,
) {
    let tracker_path = match kwin_script_path("kwin_tracker.js") {
        Ok(p) => p,
        Err(err) => {
            eprintln!("[WindowDetector] KWin-Skript-Verzeichnis nicht verfügbar (fail closed): {err}");
            return;
        }
    };
    if let Err(err) =
        write_file_atomic_secure(&tracker_path, KWIN_PERSISTENT_BRIDGE_JS.as_bytes(), true)
    {
        eprintln!("[WindowDetector] Konnte KWin-Skript nicht sicher schreiben: {err}");
        return;
    }

    let tracker_str = tracker_path.to_string_lossy().into_owned();

    zbus::block_on(async move {
        let mut conn: Option<Connection> = None;
        let mut tracker_installed = false;
        let mut kwin_known_absent = false;
        let mut loaded_kwin_owner: Option<String> = None;
        let mut retry_backoff = BoundedBackoff::new(KWIN_LOAD_BACKOFF_MS);
        let mut next_retry_at: Option<Instant> = None;

        loop {
            let next_cmd = if let Some(deadline) = next_retry_at {
                let remaining = deadline.saturating_duration_since(Instant::now());
                match rx.recv_timeout(remaining) {
                    Ok(cmd) => Some(cmd),
                    Err(RecvTimeoutError::Timeout) => {
                        next_retry_at = None;
                        if !tracker_installed && !kwin_known_absent {
                            match try_install_tracker(
                                &mut conn,
                                &tracker_str,
                                kwin_verifier.as_ref(),
                                true,
                            )
                            .await
                            {
                                Ok(owner) => {
                                    tracker_installed = true;
                                    loaded_kwin_owner = owner;
                                    retry_backoff.reset();
                                }
                                Err(()) => {
                                    conn = None;
                                    loaded_kwin_owner = None;
                                    next_retry_at =
                                        retry_backoff.next_delay().map(|d| Instant::now() + d);
                                }
                            }
                        }
                        continue;
                    }
                    Err(RecvTimeoutError::Disconnected) => break,
                }
            } else {
                match rx.recv() {
                    Ok(cmd) => Some(cmd),
                    Err(_) => break,
                }
            };

            let Some(cmd) = next_cmd else {
                continue;
            };

            match cmd {
                BridgeCmd::EnsureTrackerInstalled => {
                    retry_backoff.reset();
                    next_retry_at = None;
                    match try_install_tracker(
                        &mut conn,
                        &tracker_str,
                        kwin_verifier.as_ref(),
                        true,
                    )
                    .await
                    {
                        Ok(owner) => {
                            tracker_installed = true;
                            kwin_known_absent = false;
                            loaded_kwin_owner = owner;
                        }
                        Err(()) => {
                            conn = None;
                            tracker_installed = false;
                            loaded_kwin_owner = None;
                            next_retry_at =
                                retry_backoff.next_delay().map(|d| Instant::now() + d);
                        }
                    }
                }
                BridgeCmd::ReinstallTracker => {
                    conn = None;
                    tracker_installed = false;
                    loaded_kwin_owner = None;
                    retry_backoff.reset();
                    next_retry_at = None;
                    match try_install_tracker(
                        &mut conn,
                        &tracker_str,
                        kwin_verifier.as_ref(),
                        true,
                    )
                    .await
                    {
                        Ok(owner) => {
                            tracker_installed = true;
                            kwin_known_absent = false;
                            loaded_kwin_owner = owner;
                        }
                        Err(()) => {
                            conn = None;
                            next_retry_at =
                                retry_backoff.next_delay().map(|d| Instant::now() + d);
                        }
                    }
                }
                BridgeCmd::KWinOwnerChanged(new_owner) => {
                    if let Some(ref verifier) = kwin_verifier {
                        verifier.update_owner_from_signal(new_owner.clone());
                    }
                    match decide_kwin_owner_action(
                        tracker_installed,
                        loaded_kwin_owner.as_deref(),
                        new_owner.as_deref(),
                    ) {
                        KWinOwnerAction::MarkAbsent => {
                            tracker_installed = false;
                            kwin_known_absent = true;
                            loaded_kwin_owner = None;
                            next_retry_at = None;
                            conn = None;
                        }
                        KWinOwnerAction::KeepInstalled => {
                            kwin_known_absent = false;
                            next_retry_at = None;
                            retry_backoff.reset();
                        }
                        KWinOwnerAction::ReloadScript => {
                            kwin_known_absent = false;
                            tracker_installed = false;
                            loaded_kwin_owner = None;
                            conn = None;
                            retry_backoff.reset();
                            next_retry_at = None;
                            match try_install_tracker(
                                &mut conn,
                                &tracker_str,
                                kwin_verifier.as_ref(),
                                true,
                            )
                            .await
                            {
                                Ok(owner) => {
                                    tracker_installed = true;
                                    loaded_kwin_owner = owner.or(new_owner);
                                }
                                Err(()) => {
                                    conn = None;
                                    next_retry_at =
                                        retry_backoff.next_delay().map(|d| Instant::now() + d);
                                }
                            }
                        }
                    }
                }
                BridgeCmd::StartLiveCursorAndProbe => {
                    if !tracker_installed {
                        match try_install_tracker(
                            &mut conn,
                            &tracker_str,
                            kwin_verifier.as_ref(),
                            false,
                        )
                        .await
                        {
                            Ok(owner) => {
                                tracker_installed = true;
                                kwin_known_absent = false;
                                loaded_kwin_owner = owner;
                                next_retry_at = None;
                                retry_backoff.reset();
                            }
                            Err(()) => {
                                conn = None;
                                loaded_kwin_owner = None;
                            }
                        }
                    }
                    if tracker_installed
                        && invoke_kwin_shortcut(&mut conn, KWIN_SHORTCUT_PROBE_START)
                            .await
                            .is_err()
                    {
                        conn = None;
                        match try_install_tracker(
                            &mut conn,
                            &tracker_str,
                            kwin_verifier.as_ref(),
                            false,
                        )
                        .await
                        {
                            Ok(owner) => {
                                tracker_installed = true;
                                kwin_known_absent = false;
                                loaded_kwin_owner = owner;
                                let _ =
                                    invoke_kwin_shortcut(&mut conn, KWIN_SHORTCUT_PROBE_START)
                                        .await;
                            }
                            Err(()) => {
                                tracker_installed = false;
                                loaded_kwin_owner = None;
                            }
                        }
                    }
                }
                BridgeCmd::StopLiveCursor => {
                    if tracker_installed && !kwin_known_absent {
                        let _ = invoke_kwin_shortcut(&mut conn, KWIN_SHORTCUT_PROBE_STOP).await;
                    }
                }
                BridgeCmd::Shutdown => {
                    if tracker_installed && !kwin_known_absent {
                        let _ = invoke_kwin_shortcut(&mut conn, KWIN_SHORTCUT_PROBE_STOP).await;
                        // Unload with the 500ms-timeout D-Bus connection; do not stack extra
                        // timeouts if KWin has already exited or stopped responding during logout.
                        if unload_script(&mut conn, KWIN_TRACKER_SCRIPT_NAME)
                            .await
                            .is_ok()
                        {
                            let _ = unload_script(&mut conn, KWIN_LEGACY_LIVE_SCRIPT_NAME).await;
                        }
                    }
                    if let Some(c) = conn.take() {
                        let _ = c.close().await;
                    }
                    break;
                }
            }
        }
    });
}

async fn ensure_session_conn(slot: &mut Option<Connection>) -> Result<&Connection, ()> {
    if slot.is_none() {
        *slot = connection::Builder::session()
            .method_timeout(Duration::from_millis(KWIN_BRIDGE_DBUS_TIMEOUT_MS))
            .build()
            .await
            .ok();
    }
    slot.as_ref().ok_or(())
}

async fn invoke_kwin_shortcut(
    conn_slot: &mut Option<Connection>,
    shortcut_name: &str,
) -> Result<(), ()> {
    let conn = ensure_session_conn(conn_slot).await?;
    let accel = Proxy::new(
        conn,
        "org.kde.kglobalaccel",
        "/component/kwin",
        "org.kde.kglobalaccel.Component",
    )
    .await
    .map_err(|_| ())?;
    accel
        .call::<_, _, ()>("invokeShortcut", &(shortcut_name,))
        .await
        .map_err(|_| ())
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
    scripting
        .call::<_, _, bool>("unloadScript", &(plugin_name))
        .await
        .map_err(|_| ())?;
    Ok(())
}

/// Resolves the secure runtime directory for KWin JS scripts:
/// prefers `$XDG_RUNTIME_DIR/breeze-autoscroll`, falls back only to `$HOME/.cache/breeze-autoscroll`,
/// and fails closed if neither is set (never uses `/tmp`).
fn kwin_script_dir_from(
    xdg_runtime_dir: Option<OsString>,
    home: Option<OsString>,
) -> Result<PathBuf, String> {
    if let Some(runtime_dir) = xdg_runtime_dir
        && !runtime_dir.is_empty()
    {
        let p = PathBuf::from(runtime_dir);
        if p.is_absolute() {
            return Ok(p.join("breeze-autoscroll"));
        }
    }
    if let Some(h) = home
        && !h.is_empty()
    {
        let p = PathBuf::from(h);
        if p.is_absolute() {
            return Ok(p.join(".cache/breeze-autoscroll"));
        }
    }
    Err(
        "Weder XDG_RUNTIME_DIR noch HOME ist auf einen absoluten Pfad gesetzt; kein sicherer KWin-Skript-Pfad verfügbar."
            .to_string(),
    )
}

fn kwin_script_path(file_name: &str) -> Result<PathBuf, String> {
    Ok(kwin_script_dir_from(env::var_os("XDG_RUNTIME_DIR"), env::var_os("HOME"))?.join(file_name))
}

fn now_epoch_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

const KWIN_PERSISTENT_BRIDGE_JS: &str = r#"
// Breeze Autoscroll - Single Persistent KDE Plasma 6 Window Tracker, Pointer Probe & Live Cursor Bridge
// Loaded once per session and controlled via kglobalaccel without loadScript/unloadScript churn or signal leaks.
(function () {
    var trackedWin = null;
    var trackedHandler = null;

    var liveActive = false;
    var probeWin = null;
    var probeCaptionHandler = null;
    var cursorHandler = null;
    var lastX = -999999;
    var lastY = -999999;
    var lastSentMs = 0;

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

    function notifyContextChanged() {
        if (!liveActive) {
            return;
        }
        try {
            callDBus(
                "org.breeze.Autoscroll",
                "/Autoscroll",
                "org.breeze.Autoscroll",
                "NotifyWindowContextChanged"
            );
        } catch (e) {}
    }

    function stopLiveCursor() {
        liveActive = false;
        if (cursorHandler && workspace.cursorPosChanged) {
            try {
                workspace.cursorPosChanged.disconnect(cursorHandler);
            } catch (e) {}
        }
        cursorHandler = null;
        if (probeWin && probeCaptionHandler && probeWin.captionChanged) {
            try {
                probeWin.captionChanged.disconnect(probeCaptionHandler);
            } catch (e) {}
        }
        probeWin = null;
        probeCaptionHandler = null;
    }

    function startProbeAndLiveCursor() {
        // Always disconnect any previous probe/cursor handlers first to avoid signal leaks
        stopLiveCursor();
        liveActive = true;

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
        var wx = 0;
        var wy = 0;
        var ww = 0;
        var wh = 0;
        var wPid = 0;
        var isFullScreen = "0";
        var noBorder = "0";

        if (win) {
            cls = win.resourceClass ? String(win.resourceClass) : "";
            name = win.resourceName ? String(win.resourceName) : "";
            desktop = win.desktopFileName ? String(win.desktopFileName) : "";
            caption = win.caption ? String(win.caption) : "";
            if (
                win.dock ||
                win.desktopWindow ||
                win.isDock ||
                win.isDesktop ||
                win.toolbar ||
                win.menu ||
                win.dropdownMenu ||
                win.popupMenu ||
                win.comboBox ||
                win.tooltip ||
                win.notification ||
                win.criticalNotification ||
                win.appletPopup ||
                win.onScreenDisplay ||
                win.popupWindow ||
                win.specialWindow ||
                win.splash
            ) {
                isDockOrDesktop = "1";
            } else if ((cls === "plasmashell" || name === "plasmashell") && !win.normalWindow && !win.dialog) {
                isDockOrDesktop = "1";
            }

            var cg = win.clientGeometry || win.frameGeometry || win.bufferGeometry;
            if (cg) {
                wx = Math.round(cg.x);
                wy = Math.round(cg.y);
                ww = Math.round(cg.width);
                wh = Math.round(cg.height);
            }
            if (win.fullScreen) {
                isFullScreen = "1";
            }
            if (win.noBorder) {
                noBorder = "1";
            }
            if (win.pid && win.pid > 0) {
                wPid = Math.round(win.pid);
            }
            if (win.captionChanged) {
                probeWin = win;
                probeCaptionHandler = notifyContextChanged;
                win.captionChanged.connect(probeCaptionHandler);
            }
        }

        var sx = 0;
        var sy = 0;
        var sScale = "1";
        try {
            var scr = null;
            if (p && typeof workspace.screenAt === "function") {
                scr = workspace.screenAt(p);
            }
            if (!scr && win && win.output) {
                scr = win.output;
            }
            if (scr) {
                if (scr.geometry) {
                    sx = Math.round(scr.geometry.x);
                    sy = Math.round(scr.geometry.y);
                }
                if (scr.devicePixelRatio) {
                    sScale = String(scr.devicePixelRatio);
                }
            }
        } catch (e) {}

        lastX = cx;
        lastY = cy;
        lastSentMs = Date.now();

        if (workspace.cursorPosChanged) {
            cursorHandler = function () {
                if (!liveActive) return;
                var cur = workspace.cursorPos;
                if (!cur) return;
                var nx = Math.round(cur.x);
                var ny = Math.round(cur.y);
                var now = Date.now();
                if (now - lastSentMs >= 10 && (nx !== lastX || ny !== lastY)) {
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
            };
            workspace.cursorPosChanged.connect(cursorHandler);
        }

        var coords = String(cx) + "," + String(cy) + "," + String(vw) + "," + String(vh);
        var winGeom = String(wx) + "," + String(wy) + "," + String(ww) + "," + String(wh) + "," + isFullScreen + "," + noBorder + "," + String(wPid);
        var screenGeom = String(sx) + "," + String(sy) + "," + sScale;

        callDBus(
            "org.breeze.Autoscroll",
            "/Autoscroll",
            "org.breeze.Autoscroll",
            "ReportPointerProbe",
            coords,
            cls, name, desktop, caption, isDockOrDesktop,
            winGeom, screenGeom
        );
    }

    workspace.windowActivated.connect(function (win) {
        bindWindow(win);
        if (liveActive) {
            notifyContextChanged();
        }
    });

    if (typeof registerShortcut === "function") {
        registerShortcut(
            "breeze_autoscroll_probe_start",
            "Breeze Autoscroll Probe Start",
            "",
            startProbeAndLiveCursor
        );
        registerShortcut(
            "breeze_autoscroll_probe_stop",
            "Breeze Autoscroll Probe Stop",
            "",
            stopLiveCursor
        );
    }

    bindWindow(workspace.activeWindow);
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
            ..Default::default()
        };

        assert_eq!(
            tracker.check_probe_excluded(&probe, &cfg),
            Some("onshape".to_string())
        );
    }

    #[test]
    fn distinguishes_pointer_hand_hotspot_from_default_and_text_cursors() {
        // Hyperlink / interactive button hand cursor (pointer / pointing_hand / hand2) across sizes
        assert!(is_pointer_hand_hotspot(12, 3)); // 18px
        assert!(is_pointer_hand_hotspot(16, 4)); // 24px (Breeze default)
        assert!(is_pointer_hand_hotspot(17, 4)); // 24px with 1px rounding
        assert!(is_pointer_hand_hotspot(24, 6)); // 36px
        assert!(is_pointer_hand_hotspot(33, 8)); // 48px

        // Standard arrow (default / left_ptr)
        assert!(!is_pointer_hand_hotspot(4, 4)); // 24px
        assert!(!is_pointer_hand_hotspot(6, 6)); // 36px
        assert!(!is_pointer_hand_hotspot(8, 8)); // 48px

        // Text / I-Beam (text / xterm) and Openhand / Grab
        assert!(!is_pointer_hand_hotspot(16, 15)); // 24px text
        assert!(!is_pointer_hand_hotspot(17, 16)); // 24px text/crosshair
        assert!(!is_pointer_hand_hotspot(16, 16)); // 24px grab
        assert!(!is_pointer_hand_hotspot(24, 23)); // 36px text
    }

    #[test]
    fn detects_firefox_and_brave_tab_bar_and_hyperlink_probes() {
        let tracker = WindowTracker::new();
        let mut cfg = ConfigData::default();

        let firefox_win = ActiveWindowInfo {
            app_class: "firefox_firefox".to_string(),
            app_name: "Navigator".to_string(),
            desktop_file: "firefox_firefox".to_string(),
            title: "Rust Programming Language — Mozilla Firefox".to_string(),
            updated_at_ms: 0,
        };

        // 1. Middle-clicking a Firefox tab (y = 142 within window at y = 120 -> rel_y = 22 < 95)
        let tab_probe = PointerProbeResult {
            cursor_x: 2200,
            cursor_y: 142,
            overlay_x: 2200,
            overlay_y: 142,
            window: Some(firefox_win.clone()),
            win_x: 1920,
            win_y: 120,
            win_w: 1920,
            win_h: 1080,
            is_fullscreen: false,
            no_border: true,
            is_interactive_cursor: false,
            from_kwin: true,
            ..Default::default()
        };
        assert!(is_interactive_window_chrome(&tab_probe, &cfg));
        assert!(tracker.is_interactive_probe(&tab_probe, &cfg));

        // 2. Middle-clicking a hyperlink inside the page (rel_y = 400, is_interactive_cursor = true)
        let link_probe = PointerProbeResult {
            cursor_x: 2450,
            cursor_y: 520,
            overlay_x: 2450,
            overlay_y: 520,
            window: Some(firefox_win.clone()),
            win_x: 1920,
            win_y: 120,
            win_w: 1920,
            win_h: 1080,
            is_fullscreen: false,
            no_border: true,
            hotspot_x: Some(16),
            hotspot_y: Some(4),
            is_interactive_cursor: true,
            from_kwin: true,
            ..Default::default()
        };
        assert!(!is_interactive_window_chrome(&link_probe, &cfg));
        assert!(tracker.is_interactive_probe(&link_probe, &cfg));

        // 3. Middle-clicking a non-interactive area of the page (rel_y = 400, default cursor)
        let page_probe = PointerProbeResult {
            cursor_x: 2450,
            cursor_y: 520,
            overlay_x: 2450,
            overlay_y: 520,
            window: Some(firefox_win),
            win_x: 1920,
            win_y: 120,
            win_w: 1920,
            win_h: 1080,
            is_fullscreen: false,
            no_border: true,
            hotspot_x: Some(4),
            hotspot_y: Some(4),
            is_interactive_cursor: false,
            from_kwin: true,
            ..Default::default()
        };
        assert!(!is_interactive_window_chrome(&page_probe, &cfg));
        assert!(!tracker.is_interactive_probe(&page_probe, &cfg));

        // 4. Disabling ignore_hyperlinks_and_tabs allows all probes
        cfg.ignore_hyperlinks_and_tabs = false;
        assert!(!tracker.is_interactive_probe(&tab_probe, &cfg));
        assert!(!tracker.is_interactive_probe(&link_probe, &cfg));

        // 5. Window context change notification lifecycle
        assert!(!tracker.take_window_context_changed());
        tracker.notify_window_context_changed();
        assert!(tracker.take_window_context_changed());
        assert!(!tracker.take_window_context_changed());
    }

    #[test]
    fn detects_dolphin_folder_tab_places_via_atspi_and_allows_empty_space() {
        let tracker = WindowTracker::new();
        let cfg = ConfigData::default();

        let dolphin_win = ActiveWindowInfo {
            app_class: "org.kde.dolphin".to_string(),
            app_name: "dolphin".to_string(),
            desktop_file: "org.kde.dolphin".to_string(),
            title: "Antigravity-x64 — Dolphin".to_string(),
            updated_at_ms: 0,
        };
        assert!(is_file_manager_window(&dolphin_win));

        // 1. Middle-clicking a folder icon (`resources`) in Dolphin -> AT-SPI2 reports `list item`
        let folder_probe = PointerProbeResult {
            cursor_x: 2320,
            cursor_y: 250,
            overlay_x: 2320,
            overlay_y: 250,
            window: Some(dolphin_win.clone()),
            win_x: 1920,
            win_y: 120,
            win_w: 1227,
            win_h: 632,
            win_pid: 39175,
            atspi_role: Some("list item".to_string()),
            is_interactive_atspi: true,
            from_kwin: true,
            ..Default::default()
        };
        assert!(tracker.is_interactive_probe(&folder_probe, &cfg));

        // 2. Middle-clicking empty space in Dolphin file view -> AT-SPI2 reports `list` (no item)
        let empty_space_probe = PointerProbeResult {
            cursor_x: 2820,
            cursor_y: 620,
            overlay_x: 2820,
            overlay_y: 620,
            window: Some(dolphin_win.clone()),
            win_x: 1920,
            win_y: 120,
            win_w: 1227,
            win_h: 632,
            win_pid: 39175,
            atspi_role: Some("list".to_string()),
            is_interactive_atspi: false,
            from_kwin: true,
            ..Default::default()
        };
        assert!(!tracker.is_interactive_probe(&empty_space_probe, &cfg));

        // 3. Fallback when AT-SPI2 is unavailable: Dolphin's tab bar (rel_y = 60 < 86) is still protected
        let tab_fallback_probe = PointerProbeResult {
            cursor_x: 2170,
            cursor_y: 180,
            overlay_x: 2170,
            overlay_y: 180,
            window: Some(dolphin_win),
            win_x: 1920,
            win_y: 120,
            win_w: 1227,
            win_h: 632,
            atspi_role: None,
            is_interactive_atspi: false,
            from_kwin: true,
            ..Default::default()
        };
        assert!(is_interactive_window_chrome(&tab_fallback_probe, &cfg));
        assert!(tracker.is_interactive_probe(&tab_fallback_probe, &cfg));

        // 4. Generic frameless CSD app (e.g. Obsidian, Nautilus, VS Code, any Electron/GTK4 app)
        // protects top 48px tab/header bar without needing app-specific names
        let generic_csd_tab_probe = PointerProbeResult {
            cursor_x: 2200,
            cursor_y: 162, // rel_y = 42 < 48
            overlay_x: 2200,
            overlay_y: 162,
            window: Some(ActiveWindowInfo {
                app_class: "arbitrary-csd-app".to_string(),
                app_name: "arbitrary-csd-app".to_string(),
                desktop_file: "arbitrary-csd-app".to_string(),
                title: "Willkommen".to_string(),
                updated_at_ms: 0,
            }),
            win_x: 1920,
            win_y: 120,
            win_w: 1400,
            win_h: 900,
            no_border: true,
            from_kwin: true,
            ..Default::default()
        };
        assert!(is_interactive_window_chrome(&generic_csd_tab_probe, &cfg));
        assert!(tracker.is_interactive_probe(&generic_csd_tab_probe, &cfg));
    }

    #[test]
    fn kwin_script_dir_prefers_xdg_runtime_and_fails_closed_without_tmp() {
        assert!(kwin_script_dir_from(None, None).is_err());
        assert!(kwin_script_dir_from(Some(OsString::new()), Some(OsString::new())).is_err());
        assert!(kwin_script_dir_from(Some("relative".into()), None).is_err());
        assert_eq!(
            kwin_script_dir_from(Some("/run/user/1000".into()), Some("/home/alice".into()))
                .unwrap(),
            PathBuf::from("/run/user/1000/breeze-autoscroll")
        );
        assert_eq!(
            kwin_script_dir_from(None, Some("/home/alice".into())).unwrap(),
            PathBuf::from("/home/alice/.cache/breeze-autoscroll")
        );
    }

    #[test]
    fn drm_prop_name_and_safe_c_slice_guard_against_missing_nul_and_null_ptrs() {
        // Normal NUL-terminated property name
        let mut normal = [0 as libc::c_char; 32];
        for (dst, &b) in normal.iter_mut().zip(b"CRTC_X".iter()) {
            *dst = b as libc::c_char;
        }
        assert_eq!(drm_prop_name_from_bytes(&normal), Some("CRTC_X"));

        // Completely full 32-byte buffer WITHOUT NUL byte (would UB with CStr::from_ptr!)
        let no_nul = [b'A' as libc::c_char; 32];
        assert_eq!(
            drm_prop_name_from_bytes(&no_nul),
            Some("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA")
        );

        // Invalid UTF-8 returns None safely
        let mut bad_utf8 = [0 as libc::c_char; 32];
        bad_utf8[0] = 0xffu8 as libc::c_char;
        assert_eq!(drm_prop_name_from_bytes(&bad_utf8), None);

        // Null pointer or zero/excessive count always returns empty slice without UB
        unsafe {
            assert!(safe_c_slice::<u32>(std::ptr::null(), 10).is_empty());
            let val = 42u32;
            assert!(safe_c_slice(&val as *const u32, 0).is_empty());
            assert!(safe_c_slice(&val as *const u32, MAX_DRM_SLICE_LEN + 1).is_empty());
            assert_eq!(safe_c_slice(&val as *const u32, 1), &[42]);
        }
    }

    #[test]
    fn drm_state_caches_failed_initialization_to_avoid_repeated_dlopen() {
        let mut state = DrmState::Uninitialized;
        let mut open_calls = 0usize;

        let first = state.query_or_init_with(
            || {
                open_calls += 1;
                None
            },
            |_| Some((10, 20)),
        );
        assert_eq!(first, None);
        assert_eq!(open_calls, 1);
        assert!(matches!(state, DrmState::Unavailable));

        // Subsequent queries MUST NOT invoke open_fn again
        let second = state.query_or_init_with(
            || {
                open_calls += 1;
                None
            },
            |_| Some((10, 20)),
        );
        assert_eq!(second, None);
        assert_eq!(open_calls, 1);
    }

    #[test]
    fn persistent_kwin_script_disconnects_signals_and_registers_shortcuts() {
        assert!(KWIN_PERSISTENT_BRIDGE_JS.contains("breeze_autoscroll_probe_start"));
        assert!(KWIN_PERSISTENT_BRIDGE_JS.contains("breeze_autoscroll_probe_stop"));
        assert!(KWIN_PERSISTENT_BRIDGE_JS.contains("cursorPosChanged.disconnect"));
        assert!(KWIN_PERSISTENT_BRIDGE_JS.contains("captionChanged.disconnect"));
    }

    #[test]
    fn kwin_load_backoff_and_owner_change_decisions_handle_login_and_restarts() {
        let mut backoff = BoundedBackoff::new(KWIN_LOAD_BACKOFF_MS);
        let delays: Vec<u64> = std::iter::from_fn(|| backoff.next_delay())
            .map(|d| d.as_millis() as u64)
            .collect();
        assert_eq!(delays, vec![250, 500, 1_000, 2_000, 5_000]);
        assert!(backoff.is_exhausted());

        // KWin exited (empty or None owner) -> mark absent
        assert_eq!(
            decide_kwin_owner_action(true, Some(":1.42"), None),
            KWinOwnerAction::MarkAbsent
        );
        assert_eq!(
            decide_kwin_owner_action(true, Some(":1.42"), Some("")),
            KWinOwnerAction::MarkAbsent
        );

        // Same owner while already installed -> avoid redundant reload
        assert_eq!(
            decide_kwin_owner_action(true, Some(":1.42"), Some(":1.42")),
            KWinOwnerAction::KeepInstalled
        );

        // New KWin owner after compositor restart -> reload persistent script
        assert_eq!(
            decide_kwin_owner_action(true, Some(":1.42"), Some(":1.99")),
            KWinOwnerAction::ReloadScript
        );

        // Late KWin startup when script was not yet installed -> load script
        assert_eq!(
            decide_kwin_owner_action(false, None, Some(":1.15")),
            KWinOwnerAction::ReloadScript
        );
    }
}
