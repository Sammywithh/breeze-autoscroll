//! KDE Plasma 6 System Tray (`StatusNotifierItem` + `com.canonical.dbusmenu`)
//! and single-instance / KWin D-Bus service (`org.breeze.Autoscroll`).

use std::collections::HashMap;
use std::env;
use std::process::{Child, Command};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use zbus::fdo::DBusProxy;
use zbus::object_server::SignalEmitter;
use zbus::wire::{OwnedObjectPath, OwnedVariant, Str, Structure, Variant};
use zbus::{Connection, ObjectServer, Proxy, connection, interface};

use crate::config::ConfigManager;
use crate::window_detector::WindowTracker;

pub const SERVICE_NAME: &str = "org.breeze.Autoscroll";
pub const AUTOSCROLL_PATH: &str = "/Autoscroll";
pub const SNI_PATH: &str = "/StatusNotifierItem";
pub const MENU_PATH: &str = "/MenuBar";

pub type MenuItem = (i32, HashMap<String, OwnedVariant>, Vec<OwnedVariant>);

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
    quit_requested: Arc<AtomicBool>,
    menu_revision: Arc<AtomicU32>,
    settings_child: Arc<Mutex<Option<Child>>>,
    about_child: Arc<Mutex<Option<Child>>>,
}

impl TraySharedState {
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
        emit_tray_refresh(server, &self.state).await;
    }

    #[zbus(name = "UpdateActiveWindow")]
    fn update_active_window(
        &self,
        app_class: String,
        app_name: String,
        desktop_file: String,
        title: String,
        cursor_x: String,
        cursor_y: String,
    ) {
        let cx = cursor_x.trim().parse::<i32>().ok();
        let cy = cursor_y.trim().parse::<i32>().ok();
        self.state
            .window_tracker
            .update_from_kwin(app_class, app_name, desktop_file, title, cx, cy);
    }
}

// ============================================================================
// org.kde.StatusNotifierItem Interface
// ============================================================================

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
        let enabled = self.state.config.snapshot().enabled;
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
    fn tool_tip(&self) -> (String, Vec<(i32, i32, Vec<u8>)>, String, String) {
        let cfg = self.state.config.snapshot();
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
        let cfg = self.state.config.snapshot();
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
        let cfg = self.state.config.snapshot();
        ids.into_iter()
            .map(|id| (id, build_menu_properties(id, &cfg)))
            .collect()
    }

    #[zbus(name = "GetProperty")]
    fn get_property(&self, id: i32, name: String) -> zbus::fdo::Result<OwnedVariant> {
        let cfg = self.state.config.snapshot();
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
                self.state.quit_requested.store(true, Ordering::SeqCst);
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
    let _ = zbus::block_on(async {
        let Ok(conn) = Connection::session().await else {
            return;
        };
        if let Ok(proxy) = Proxy::new(&conn, SERVICE_NAME, AUTOSCROLL_PATH, SERVICE_NAME).await {
            let _ = proxy.call::<_, _, ()>("ReloadConfig", &()).await;
        }
    });
}

/// Starts the session D-Bus service (`org.breeze.Autoscroll`, `StatusNotifierItem`, `dbusmenu`)
/// and registers with KDE Plasma's `StatusNotifierWatcher`.
pub fn start_tray_and_dbus_service(
    config: ConfigManager,
    window_tracker: WindowTracker,
    quit_requested: Arc<AtomicBool>,
) -> Result<Connection, String> {
    let shared = TraySharedState {
        config,
        window_tracker,
        quit_requested,
        menu_revision: Arc::new(AtomicU32::new(1)),
        settings_child: Arc::new(Mutex::new(None)),
        about_child: Arc::new(Mutex::new(None)),
    };

    let conn = zbus::block_on(async move {
        let control = AutoscrollControlIface {
            state: shared.clone(),
        };
        let sni = StatusNotifierItemIface {
            state: shared.clone(),
        };
        let menu = DbusMenuIface { state: shared };

        let conn = connection::Builder::session()
            .name(SERVICE_NAME)
            .serve_at(AUTOSCROLL_PATH, control)
            .serve_at(SNI_PATH, sni)
            .serve_at(MENU_PATH, menu)
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

    Ok(conn)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ConfigData;

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
}
