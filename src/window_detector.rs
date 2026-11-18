//! Active window, browser tab title, cursor position, and 3D/CAD context detection.
//!
//! Works across KDE Plasma 6 (Wayland + KWin Scripting / D-Bus) and X11 / XWayland (`x11rb`).

use std::env;
use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};

use x11rb::connection::Connection as X11Connection;
use x11rb::protocol::xproto::{AtomEnum, ConnectionExt as _, Window};
use zbus::{Connection, Proxy};

use crate::config::ConfigData;

const KWIN_SCRIPT_NAME: &str = "breeze_autoscroll_tracker";

/// Metadata about the currently focused application window (including active browser tab title).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ActiveWindowInfo {
    pub app_class: String,
    pub app_name: String,
    pub desktop_file: String,
    pub title: String,
    pub updated_at_ms: u64,
}

#[derive(Debug, Default)]
struct TrackerState {
    kwin_active: Option<ActiveWindowInfo>,
    kwin_cursor: Option<(i32, i32)>,
}

/// Thread-safe active window and cursor position tracker.
#[derive(Clone, Default)]
pub struct WindowTracker {
    state: Arc<RwLock<TrackerState>>,
}

impl WindowTracker {
    pub fn new() -> Self {
        Self::default()
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
        if let Ok(mut guard) = self.state.write() {
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

    /// Returns the currently focused window, preferring live KWin Wayland state and
    /// falling back to X11 `_NET_ACTIVE_WINDOW`.
    pub fn get_active_window(&self) -> Option<ActiveWindowInfo> {
        if let Ok(guard) = self.state.read()
            && let Some(info) = guard.kwin_active.as_ref()
        {
            return Some(info.clone());
        }

        query_x11_active_window()
    }

    /// Returns the global mouse cursor coordinates `(x, y)`.
    ///
    /// Uses X11/XWayland `QueryPointer` first, then the latest KWin cursor position,
    /// then a safe default fallback.
    pub fn get_cursor_pos(&self) -> (i32, i32) {
        if let Some(pos) = query_x11_cursor_pos() {
            return pos;
        }
        if let Ok(guard) = self.state.read()
            && let Some(pos) = guard.kwin_cursor
        {
            return pos;
        }
        (500, 500)
    }

    /// Checks whether the currently active application or browser tab belongs to an
    /// excluded 3D/CAD/Slicer context. Returns `Some(matched_keyword)` if excluded.
    pub fn check_excluded_context(&self, config: &ConfigData) -> Option<String> {
        if !config.auto_exclude_3d_apps || config.excluded_apps_and_titles.is_empty() {
            return None;
        }
        let info = self.get_active_window()?;
        match_excluded_3d_context(&info, &config.excluded_apps_and_titles)
    }
}

/// Checks whether any exclusion pattern matches `app_class`, `app_name`, `desktop_file`,
/// or `title` (case-insensitive substring match).
pub fn match_excluded_3d_context(info: &ActiveWindowInfo, patterns: &[String]) -> Option<String> {
    let fields = [
        info.app_class.to_ascii_lowercase(),
        info.app_name.to_ascii_lowercase(),
        info.desktop_file.to_ascii_lowercase(),
        info.title.to_ascii_lowercase(),
    ];

    for raw_pattern in patterns {
        let pattern = raw_pattern.trim().to_ascii_lowercase();
        if pattern.is_empty() {
            continue;
        }
        if fields.iter().any(|field| field.contains(&pattern)) {
            return Some(raw_pattern.trim().to_string());
        }
    }

    None
}

/// Queries the global pointer coordinates via X11/XWayland `QueryPointer`.
pub fn query_x11_cursor_pos() -> Option<(i32, i32)> {
    if env::var_os("DISPLAY").is_none() {
        return None;
    }
    let (conn, screen_num) = x11rb::connect(None).ok()?;
    let screen = conn.setup().roots.get(screen_num)?;
    let reply = conn.query_pointer(screen.root).ok()?.reply().ok()?;
    Some((reply.root_x as i32, reply.root_y as i32))
}

/// Queries the active X11 window (`_NET_ACTIVE_WINDOW` -> `WM_CLASS`, `_NET_WM_NAME`).
pub fn query_x11_active_window() -> Option<ActiveWindowInfo> {
    if env::var_os("DISPLAY").is_none() {
        return None;
    }
    let (conn, screen_num) = x11rb::connect(None).ok()?;
    let screen = conn.setup().roots.get(screen_num)?;

    let active_atom = intern_atom(&conn, b"_NET_ACTIVE_WINDOW")?;
    let prop = conn
        .get_property(false, screen.root, active_atom, AtomEnum::WINDOW, 0, 1)
        .ok()?
        .reply()
        .ok()?;
    let win: Window = prop.value32()?.next()?;
    if win == 0 {
        return None;
    }

    let (app_name, app_class) = query_x11_wm_class(&conn, win).unwrap_or_default();
    let title = query_x11_window_title(&conn, win).unwrap_or_default();

    Some(ActiveWindowInfo {
        app_class,
        app_name,
        desktop_file: String::new(),
        title,
        updated_at_ms: now_epoch_ms(),
    })
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

/// Loads and starts the ephemeral KDE Plasma 6 KWin tracker script so active window
/// class and browser tab titles are reported via D-Bus on Wayland & X11.
pub fn install_and_start_kwin_tracker() {
    let script_path = kwin_tracker_script_path();
    if let Some(parent) = script_path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Err(err) = fs::write(&script_path, KWIN_TRACKER_JS) {
        eprintln!("[WindowDetector] Konnte KWin-Tracker-Skript nicht schreiben: {err}");
        return;
    }

    let path_str = script_path.to_string_lossy().into_owned();
    let _ = zbus::block_on(async move {
        let Ok(conn) = Connection::session().await else {
            return;
        };
        let Ok(scripting) = Proxy::new(
            &conn,
            "org.kde.KWin",
            "/Scripting",
            "org.kde.kwin.Scripting",
        )
        .await
        else {
            return;
        };

        // Unload any stale script instance from a previous run
        let _ = scripting
            .call::<_, _, bool>("unloadScript", &(KWIN_SCRIPT_NAME))
            .await;

        let Ok(script_id) = scripting
            .call::<_, _, i32>("loadScript", &(path_str.as_str(), KWIN_SCRIPT_NAME))
            .await
        else {
            return;
        };
        if script_id < 0 {
            return;
        }

        let script_obj = format!("/Scripting/Script{script_id}");
        if let Ok(script_proxy) =
            Proxy::new(&conn, "org.kde.KWin", script_obj, "org.kde.kwin.Script").await
        {
            let _ = script_proxy.call::<_, _, ()>("run", &()).await;
        } else {
            let _ = scripting.call::<_, _, ()>("start", &()).await;
        }
    });
}

/// Unloads the ephemeral KWin tracker script on clean shutdown.
pub fn stop_kwin_tracker() {
    let _ = zbus::block_on(async {
        let Ok(conn) = Connection::session().await else {
            return;
        };
        if let Ok(scripting) = Proxy::new(
            &conn,
            "org.kde.KWin",
            "/Scripting",
            "org.kde.kwin.Scripting",
        )
        .await
        {
            let _ = scripting
                .call::<_, _, bool>("unloadScript", &(KWIN_SCRIPT_NAME))
                .await;
        }
    });
}

fn kwin_tracker_script_path() -> PathBuf {
    if let Some(cache) = env::var_os("XDG_CACHE_HOME")
        && !cache.is_empty()
    {
        return PathBuf::from(cache)
            .join("breeze-autoscroll")
            .join("kwin_tracker.js");
    }
    let home = env::var_os("HOME").unwrap_or_else(|| "/tmp".into());
    PathBuf::from(home).join(".cache/breeze-autoscroll/kwin_tracker.js")
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
    fn allows_normal_apps_and_standard_browser_tabs() {
        let patterns = default_excluded_apps_and_titles();

        let dolphin = ActiveWindowInfo {
            app_class: "org.kde.dolphin".to_string(),
            app_name: "dolphin".to_string(),
            desktop_file: "org.kde.dolphin".to_string(),
            title: "Dokumente — Dolphin".to_string(),
            updated_at_ms: 0,
        };
        assert_eq!(match_excluded_3d_context(&dolphin, &patterns), None);

        let firefox_docs = ActiveWindowInfo {
            app_class: "firefox_firefox".to_string(),
            app_name: "firefox".to_string(),
            desktop_file: "firefox_firefox".to_string(),
            title: "std::sync::RwLock - Rust — Mozilla Firefox".to_string(),
            updated_at_ms: 0,
        };
        assert_eq!(match_excluded_3d_context(&firefox_docs, &patterns), None);
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
}
