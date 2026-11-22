//! AT-SPI2 Accessibility hit-testing for native Linux desktop apps (Qt6/KDE Dolphin,
//! Konsole, Kate, Okular, GTK/Nautilus/Thunar, etc.).
//!
//! Runs on a dedicated background worker thread with a persistent D-Bus connection to the
//! AT-SPI2 accessibility bus so middle-click hit-testing has a strict bounded latency
//! (`<= 45 ms`) and never blocks the input engine or session D-Bus server.

use std::collections::HashMap;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use zbus::wire::{OwnedObjectPath, OwnedVariant, Variant};
use zbus::{Connection, Proxy, connection};

const ATSPI_QUERY_TIMEOUT_MS: u64 = 45;
const ATSPI_NULL_PATH: &str = "/org/a11y/atspi/null";
const ATSPI_ROOT_PATH: &str = "/org/a11y/atspi/accessible/root";
const ATSPI_COORD_SCREEN: u32 = 0;
const ATSPI_COORD_WINDOW: u32 = 1;

/// Result of an AT-SPI2 hit-test at the clicked coordinates inside an application window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AtSpiHitResult {
    /// Lowercase AT-SPI2 role name of the deepest element under the pointer.
    pub role: String,
    /// True if the element is interactive (tab, folder/file list item, button, menu, link, etc.).
    pub is_interactive: bool,
}

struct QueryRequest {
    pid: u32,
    window_title: String,
    rel_x: i32,
    rel_y: i32,
    screen_x: i32,
    screen_y: i32,
    win_w: i32,
    win_h: i32,
    reply_tx: Sender<Option<AtSpiHitResult>>,
}

enum WorkerCmd {
    Query(QueryRequest),
    Shutdown,
}

struct WorkerHandle {
    tx: Sender<WorkerCmd>,
    join: Option<JoinHandle<()>>,
}

/// Thread-safe handle to the background AT-SPI2 hit-testing worker.
#[derive(Clone, Default)]
pub struct AtSpiInspector {
    worker: Arc<Mutex<Option<WorkerHandle>>>,
}

impl AtSpiInspector {
    pub fn new() -> Self {
        Self::default()
    }

    /// Starts the persistent AT-SPI2 worker thread and warms up the accessibility bus connection.
    pub fn start(&self) {
        let Ok(mut guard) = self.worker.lock() else {
            return;
        };
        if guard.is_some() {
            return;
        }
        let (tx, rx) = mpsc::channel();
        let join = thread::Builder::new()
            .name("breeze-atspi-worker".to_string())
            .spawn(move || run_atspi_worker(rx))
            .ok();
        *guard = Some(WorkerHandle { tx, join });
    }

    /// Stops the AT-SPI2 worker thread and restores `org.a11y.Status.IsEnabled` if we enabled it.
    pub fn stop(&self) {
        let handle = if let Ok(mut guard) = self.worker.lock() {
            guard.take()
        } else {
            None
        };
        if let Some(mut h) = handle {
            let _ = h.tx.send(WorkerCmd::Shutdown);
            if let Some(j) = h.join.take() {
                let _ = j.join();
            }
        }
    }

    /// Queries the AT-SPI2 accessibility tree of `pid` at window-relative `(rel_x, rel_y)`
    /// (and screen `(screen_x, screen_y)`) with a strict bounded timeout.
    #[allow(clippy::too_many_arguments)]
    pub fn inspect_point(
        &self,
        pid: u32,
        window_title: &str,
        rel_x: i32,
        rel_y: i32,
        screen_x: i32,
        screen_y: i32,
        win_w: i32,
        win_h: i32,
    ) -> Option<AtSpiHitResult> {
        if pid == 0 || win_w <= 0 || win_h <= 0 {
            return None;
        }
        let tx = {
            let guard = self.worker.lock().ok()?;
            guard.as_ref()?.tx.clone()
        };
        let (reply_tx, reply_rx) = mpsc::channel();
        tx.send(WorkerCmd::Query(QueryRequest {
            pid,
            window_title: window_title.to_string(),
            rel_x,
            rel_y,
            screen_x,
            screen_y,
            win_w,
            win_h,
            reply_tx,
        }))
        .ok()?;
        reply_rx
            .recv_timeout(Duration::from_millis(ATSPI_QUERY_TIMEOUT_MS))
            .ok()
            .flatten()
    }
}

/// Returns true when an AT-SPI2 role represents an interactive UI control or clickable item
/// (tabs, folder/file list items, sidebar items, buttons, menus, links, inputs, scrollbars).
///
/// Empty scrollable containers (`list`, `tree`, `table` with no item under pointer, `scroll pane`,
/// `viewport`, `panel`, `filler`, `frame`, `text`, `terminal`, `document frame`) return `false`.
pub fn is_interactive_atspi_role(role: &str) -> bool {
    let r = role.trim().to_ascii_lowercase();
    matches!(
        r.as_str(),
        "page tab"
            | "page tab list"
            | "list item"
            | "tree item"
            | "table cell"
            | "table row"
            | "table column header"
            | "table row header"
            | "icon"
            | "push button"
            | "toggle button"
            | "menu button"
            | "radio button"
            | "check box"
            | "combo box"
            | "spin button"
            | "slider"
            | "scroll bar"
            | "dial"
            | "split button"
            | "menu"
            | "menu bar"
            | "menu item"
            | "check menu item"
            | "radio menu item"
            | "tool bar"
            | "link"
            | "hyperlink"
            | "entry"
            | "password text"
            | "autocomplete"
    )
}

/// Returns true when `(px, py)` lies inside the rectangle `(rx, ry, rw, rh)` with `margin` tolerance.
pub fn rect_contains_point(
    rect: (i32, i32, i32, i32),
    px: i32,
    py: i32,
    margin: i32,
) -> bool {
    let (rx, ry, rw, rh) = rect;
    if rw <= 0 || rh <= 0 {
        return false;
    }
    px >= rx - margin && px <= rx + rw + margin && py >= ry - margin && py <= ry + rh + margin
}

fn is_item_container_role(role: &str) -> bool {
    matches!(
        role,
        "list" | "tree" | "tree table" | "table" | "page tab list"
    )
}

fn run_atspi_worker(rx: Receiver<WorkerCmd>) {
    zbus::block_on(async move {
        let mut session_conn = Connection::session().await.ok();
        let mut enabled_by_us = false;
        let mut a11y_conn: Option<Connection> = None;
        let mut pid_cache: HashMap<u32, String> = HashMap::new();

        if let Some(ref sconn) = session_conn {
            enabled_by_us = ensure_atspi_enabled(sconn).await;
            a11y_conn = connect_atspi_bus(sconn).await;
        }

        while let Ok(cmd) = rx.recv() {
            match cmd {
                WorkerCmd::Query(req) => {
                    if a11y_conn.is_none() {
                        if session_conn.is_none() {
                            session_conn = Connection::session().await.ok();
                        }
                        if let Some(ref sconn) = session_conn {
                            if !enabled_by_us {
                                enabled_by_us = ensure_atspi_enabled(sconn).await;
                            }
                            a11y_conn = connect_atspi_bus(sconn).await;
                        }
                    }

                    let result = if let Some(ref conn) = a11y_conn {
                        query_atspi_point(conn, &mut pid_cache, &req).await
                    } else {
                        None
                    };
                    let _ = req.reply_tx.send(result);
                }
                WorkerCmd::Shutdown => break,
            }
        }

        // Cleanly restore org.a11y.Status.IsEnabled if we enabled it at startup
        if enabled_by_us
            && let Some(ref sconn) = session_conn
        {
            let _ = set_a11y_enabled(sconn, false).await;
        }
    });
}

async fn ensure_atspi_enabled(session_conn: &Connection) -> bool {
    let Ok(proxy) = Proxy::new(
        session_conn,
        "org.a11y.Bus",
        "/org/a11y/bus",
        "org.freedesktop.DBus.Properties",
    )
    .await
    else {
        return false;
    };

    let already_enabled = proxy
        .call::<_, _, OwnedVariant>("Get", &("org.a11y.Status", "IsEnabled"))
        .await
        .ok()
        .and_then(|v| bool::try_from(&v).ok())
        .unwrap_or(false);

    if already_enabled {
        return false;
    }

    set_a11y_enabled(session_conn, true).await
}

async fn set_a11y_enabled(session_conn: &Connection, enabled: bool) -> bool {
    let Ok(proxy) = Proxy::new(
        session_conn,
        "org.a11y.Bus",
        "/org/a11y/bus",
        "org.freedesktop.DBus.Properties",
    )
    .await
    else {
        return false;
    };

    proxy
        .call::<_, _, ()>(
            "Set",
            &("org.a11y.Status", "IsEnabled", Variant::from(enabled)),
        )
        .await
        .is_ok()
}

async fn connect_atspi_bus(session_conn: &Connection) -> Option<Connection> {
    let proxy = Proxy::new(session_conn, "org.a11y.Bus", "/org/a11y/bus", "org.a11y.Bus")
        .await
        .ok()?;
    let addr: String = proxy.call("GetAddress", &()).await.ok()?;
    if addr.trim().is_empty() {
        return None;
    }
    connection::Builder::address(addr.as_str())
        .build()
        .await
        .ok()
}

async fn query_atspi_point(
    conn: &Connection,
    pid_cache: &mut HashMap<u32, String>,
    req: &QueryRequest,
) -> Option<AtSpiHitResult> {
    let bus_name = resolve_pid_bus_name(conn, pid_cache, req.pid).await?;
    let frame_path = match find_target_frame(conn, &bus_name, req).await {
        Some(path) => path,
        None => {
            // Cached bus name may belong to a restarted process; refresh once
            pid_cache.remove(&req.pid);
            let refreshed = resolve_pid_bus_name(conn, pid_cache, req.pid).await?;
            find_target_frame(conn, &refreshed, req).await?
        }
    };

    let bus_name = pid_cache.get(&req.pid)?.clone();
    hit_test_tree(conn, &bus_name, &frame_path, req).await
}

async fn resolve_pid_bus_name(
    conn: &Connection,
    pid_cache: &mut HashMap<u32, String>,
    target_pid: u32,
) -> Option<String> {
    if let Some(cached) = pid_cache.get(&target_pid) {
        return Some(cached.clone());
    }

    let reg = Proxy::new(
        conn,
        "org.a11y.atspi.Registry",
        ATSPI_ROOT_PATH,
        "org.a11y.atspi.Accessible",
    )
    .await
    .ok()?;
    let children: Vec<(String, OwnedObjectPath)> = reg.call("GetChildren", &()).await.ok()?;

    let dbus_proxy = Proxy::new(
        conn,
        "org.freedesktop.DBus",
        "/org/freedesktop/DBus",
        "org.freedesktop.DBus",
    )
    .await
    .ok()?;

    pid_cache.clear();
    for (sender, _) in children {
        if sender.is_empty() {
            continue;
        }
        if let Ok(pid) = dbus_proxy
            .call::<_, _, u32>("GetConnectionUnixProcessID", &(&sender,))
            .await
        {
            pid_cache.insert(pid, sender);
        }
    }

    pid_cache.get(&target_pid).cloned()
}

async fn find_target_frame(
    conn: &Connection,
    bus_name: &str,
    req: &QueryRequest,
) -> Option<String> {
    let root = Proxy::new(
        conn,
        bus_name,
        ATSPI_ROOT_PATH,
        "org.a11y.atspi.Accessible",
    )
    .await
    .ok()?;
    let children: Vec<(String, OwnedObjectPath)> = root.call("GetChildren", &()).await.ok()?;
    let valid_paths: Vec<String> = children
        .into_iter()
        .map(|(_, p)| p.as_str().to_string())
        .filter(|p| !p.is_empty() && p != ATSPI_NULL_PATH)
        .collect();

    if valid_paths.is_empty() {
        return None;
    }
    if valid_paths.len() == 1 {
        return Some(valid_paths[0].clone());
    }

    // Multiple top-level windows for the same PID: match by title first, then geometry
    let title_lower = req.window_title.to_ascii_lowercase();
    let mut best_geom_match: Option<(i32, String)> = None;

    for path in &valid_paths {
        if !title_lower.is_empty()
            && let Some(name) = get_accessible_name(conn, bus_name, path).await
        {
            let name_lower = name.to_ascii_lowercase();
            if name_lower == title_lower
                || (!name_lower.is_empty() && title_lower.contains(&name_lower))
            {
                return Some(path.clone());
            }
        }
        if let Some((_, _, ew, eh)) = get_extents(conn, bus_name, path, ATSPI_COORD_WINDOW).await {
            let diff = (ew - req.win_w).abs() + (eh - req.win_h).abs();
            if best_geom_match
                .as_ref()
                .is_none_or(|(best_diff, _)| diff < *best_diff)
            {
                best_geom_match = Some((diff, path.clone()));
            }
        }
    }

    best_geom_match
        .map(|(_, p)| p)
        .or_else(|| valid_paths.into_iter().next())
}

async fn hit_test_tree(
    conn: &Connection,
    bus_name: &str,
    frame_path: &str,
    req: &QueryRequest,
) -> Option<AtSpiHitResult> {
    let mut curr_path = frame_path.to_string();
    let mut last_role: Option<String> = None;

    for _depth in 0..8 {
        let mut next_path = get_accessible_at_point(
            conn,
            bus_name,
            &curr_path,
            req.rel_x,
            req.rel_y,
            ATSPI_COORD_WINDOW,
        )
        .await;

        // If WINDOW-relative hit-test returned null on an item container (e.g. Qt6 Dolphin
        // KItemListViewAccessible), query with container-local coordinates and verify the
        // returned item's bounding box contains the clicked pointer.
        if next_path.is_none()
            && last_role
                .as_deref()
                .is_some_and(is_item_container_role)
            && let Some((lx, ly, lw, lh)) =
                get_extents(conn, bus_name, &curr_path, ATSPI_COORD_WINDOW).await
            && lw > 0
            && lh > 0
        {
            let local_x = req.rel_x - lx;
            let local_y = req.rel_y - ly;
            if let Some(candidate) = get_accessible_at_point(
                conn,
                bus_name,
                &curr_path,
                local_x,
                local_y,
                ATSPI_COORD_SCREEN,
            )
            .await
                && candidate != curr_path
                && let Some(ext) =
                    get_extents(conn, bus_name, &candidate, ATSPI_COORD_WINDOW).await
                && rect_contains_point(ext, req.rel_x, req.rel_y, 4)
            {
                next_path = Some(candidate);
            }
        }

        // Also try SCREEN coordinates on the frame root if WINDOW coords returned null (GTK/X11)
        if next_path.is_none() && curr_path == frame_path {
            if let Some(candidate) = get_accessible_at_point(
                conn,
                bus_name,
                &curr_path,
                req.screen_x,
                req.screen_y,
                ATSPI_COORD_SCREEN,
            )
            .await
                && candidate != curr_path
            {
                next_path = Some(candidate);
            }
        }

        let Some(child_path) = next_path else {
            break;
        };
        if child_path == curr_path {
            break;
        }

        let role = get_role_name(conn, bus_name, &child_path)
            .await
            .unwrap_or_default()
            .to_ascii_lowercase();

        if is_interactive_atspi_role(&role) {
            return Some(AtSpiHitResult {
                role,
                is_interactive: true,
            });
        }

        last_role = Some(role);
        curr_path = child_path;
    }

    last_role.map(|role| AtSpiHitResult {
        is_interactive: is_interactive_atspi_role(&role),
        role,
    })
}

async fn get_accessible_at_point(
    conn: &Connection,
    bus_name: &str,
    path: &str,
    x: i32,
    y: i32,
    coord_type: u32,
) -> Option<String> {
    let proxy = Proxy::new(conn, bus_name, path, "org.a11y.atspi.Component")
        .await
        .ok()?;
    let reply = proxy
        .call_method("GetAccessibleAtPoint", &(x, y, coord_type))
        .await
        .ok()?;
    let (_, child_path): (String, OwnedObjectPath) = reply.body().deserialize().ok()?;
    let p = child_path.as_str();
    if p.is_empty() || p == ATSPI_NULL_PATH {
        None
    } else {
        Some(p.to_string())
    }
}

async fn get_extents(
    conn: &Connection,
    bus_name: &str,
    path: &str,
    coord_type: u32,
) -> Option<(i32, i32, i32, i32)> {
    let proxy = Proxy::new(conn, bus_name, path, "org.a11y.atspi.Component")
        .await
        .ok()?;
    let reply = proxy.call_method("GetExtents", &(coord_type,)).await.ok()?;
    reply.body().deserialize().ok()
}

async fn get_role_name(conn: &Connection, bus_name: &str, path: &str) -> Option<String> {
    let proxy = Proxy::new(conn, bus_name, path, "org.a11y.atspi.Accessible")
        .await
        .ok()?;
    proxy.call::<_, _, String>("GetRoleName", &()).await.ok()
}

async fn get_accessible_name(conn: &Connection, bus_name: &str, path: &str) -> Option<String> {
    let proxy = Proxy::new(conn, bus_name, path, "org.freedesktop.DBus.Properties")
        .await
        .ok()?;
    let val: OwnedVariant = proxy
        .call("Get", &("org.a11y.atspi.Accessible", "Name"))
        .await
        .ok()?;
    String::try_from(val).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_dolphin_and_desktop_interactive_vs_scrollable_roles() {
        // Dolphin folders/files, tabs, places sidebar, toolbar buttons, menus, links
        for role in [
            "page tab",
            "page tab list",
            "list item",
            "tree item",
            "table cell",
            "push button",
            "toggle button",
            "menu button",
            "menu item",
            "tool bar",
            "link",
            "hyperlink",
            "entry",
            "scroll bar",
            "slider",
        ] {
            assert!(
                is_interactive_atspi_role(role),
                "expected '{role}' to be interactive"
            );
        }

        // Empty file-view canvas (`list` with no child), panels, text editors, terminals
        for role in [
            "list",
            "tree",
            "table",
            "scroll pane",
            "viewport",
            "panel",
            "filler",
            "frame",
            "layered pane",
            "splitter",
            "document frame",
            "text",
            "terminal",
            "label",
            "paragraph",
        ] {
            assert!(
                !is_interactive_atspi_role(role),
                "expected '{role}' to be non-interactive scrollable area"
            );
        }
    }

    #[test]
    fn rect_contains_point_checks_bounds_and_margin() {
        // Dolphin folder `resources` at (361, 93, 125x88)
        let folder = (361, 93, 125, 88);
        assert!(rect_contains_point(folder, 400, 130, 4)); // inside folder
        assert!(rect_contains_point(folder, 361, 93, 4)); // top-left corner
        assert!(rect_contains_point(folder, 486, 181, 4)); // bottom-right corner
        assert!(!rect_contains_point(folder, 351, 130, 4)); // gap between locales & resources
        assert!(!rect_contains_point(folder, 900, 500, 4)); // empty space bottom-right
        assert!(!rect_contains_point((0, 0, 0, 0), 0, 0, 4)); // hidden zero-size widget
    }

    #[test]
    #[ignore = "live desktop smoke test against a running Dolphin instance"]
    fn live_dolphin_atspi_hit_test_smoke() {
        let pid_out = std::process::Command::new("pgrep")
            .args(["-n", "-x", "dolphin"])
            .output()
            .expect("pgrep dolphin");
        let pid_str = String::from_utf8_lossy(&pid_out.stdout);
        let pid: u32 = pid_str.trim().parse().expect("dolphin must be running");

        let inspector = AtSpiInspector::new();
        inspector.start();
        std::thread::sleep(Duration::from_millis(200));

        let cases = [
            ("Toolbar (250, 24)", 250, 24),
            ("Tab (250, 60)", 250, 60),
            ("Places (100, 100)", 100, 100),
            ("Folder resources (400, 130)", 400, 130),
            ("Gap between icons (351, 130)", 351, 130),
            ("Empty space (900, 500)", 900, 500),
        ];

        for (label, rx, ry) in cases {
            let t0 = std::time::Instant::now();
            let hit = inspector.inspect_point(pid, "Dolphin", rx, ry, 1920 + rx, 120 + ry, 1227, 632);
            let elapsed = t0.elapsed().as_secs_f64() * 1000.0;
            println!("{label:30} -> {hit:?} ({elapsed:.2} ms)");
        }

        inspector.stop();
    }
}
