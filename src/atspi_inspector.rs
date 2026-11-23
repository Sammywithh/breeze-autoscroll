//! AT-SPI2 Accessibility hit-testing for native Linux desktop apps (Qt6/KDE Dolphin,
//! Konsole, Kate, Okular, GTK/Nautilus/Thunar, etc.).
//!
//! Runs on a dedicated background worker thread with:
//! - Opt-in global enablement (`auto_enable_atspi`, default `false`): by default only queries
//!   AT-SPI2 when accessibility is already active in the desktop session, reacting dynamically
//!   to runtime config changes.
//! - Bounded `PidCache` (capacity + TTL) so stale PIDs cannot accumulate unboundedly.
//! - Real per-call async D-Bus timeouts (`connection::Builder::method_timeout`) plus a strict
//!   per-request total deadline so hung applications can never wedge the worker thread.
//! - Bounded `sync_channel` queues with non-blocking `try_send`.

use std::collections::HashMap;
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use zbus::wire::{OwnedObjectPath, OwnedVariant, Variant};
use zbus::{Connection, Proxy, connection};

use crate::config::ConfigManager;

const ATSPI_QUERY_TIMEOUT_MS: u64 = 45;
const ATSPI_CALL_TIMEOUT_MS: u64 = 35;
const ATSPI_SESSION_TIMEOUT_MS: u64 = 150;
const ATSPI_STATUS_RECHECK_INTERVAL: Duration = Duration::from_secs(2);
const MAX_PID_CACHE_ENTRIES: usize = 256;
const PID_CACHE_TTL: Duration = Duration::from_secs(60);
const MAX_REGISTRY_CHILDREN_SCAN: usize = 128;
const WORKER_CHANNEL_CAPACITY: usize = 8;

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

/// Bounded PID -> AT-SPI2 bus-name cache with capacity limit and entry TTL.
#[derive(Debug)]
struct PidCache {
    entries: HashMap<u32, (String, Instant)>,
    max_entries: usize,
    ttl: Duration,
}

impl PidCache {
    fn new(max_entries: usize, ttl: Duration) -> Self {
        Self {
            entries: HashMap::with_capacity(max_entries.min(64)),
            max_entries: max_entries.max(1),
            ttl,
        }
    }

    fn get(&mut self, pid: u32, now: Instant) -> Option<String> {
        let is_expired = self
            .entries
            .get(&pid)
            .is_some_and(|(_, inserted_at)| now.saturating_duration_since(*inserted_at) >= self.ttl);
        if is_expired {
            self.entries.remove(&pid);
            return None;
        }
        self.entries.get(&pid).map(|(name, _)| name.clone())
    }

    fn insert(&mut self, pid: u32, bus_name: String, now: Instant) {
        self.purge_expired(now);
        if !self.entries.contains_key(&pid)
            && self.entries.len() >= self.max_entries
            && let Some(oldest_pid) = self
                .entries
                .iter()
                .min_by_key(|(_, (_, ts))| *ts)
                .map(|(&k, _)| k)
        {
            self.entries.remove(&oldest_pid);
        }
        self.entries.insert(pid, (bus_name, now));
    }

    fn remove(&mut self, pid: u32) {
        self.entries.remove(&pid);
    }

    fn clear(&mut self) {
        self.entries.clear();
    }

    fn purge_expired(&mut self, now: Instant) {
        let ttl = self.ttl;
        self.entries
            .retain(|_, (_, inserted_at)| now.saturating_duration_since(*inserted_at) < ttl);
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.entries.len()
    }
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
    reply_tx: SyncSender<Option<AtSpiHitResult>>,
}

enum WorkerCmd {
    Query(QueryRequest),
    SyncConfig,
    Shutdown,
}

struct WorkerHandle {
    tx: SyncSender<WorkerCmd>,
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

    /// Starts the persistent AT-SPI2 worker thread observing live `ConfigManager` opt-in state.
    pub fn start(&self, config: ConfigManager) {
        let Ok(mut guard) = self.worker.lock() else {
            return;
        };
        if guard.is_some() {
            return;
        }
        let (tx, rx) = mpsc::sync_channel(WORKER_CHANNEL_CAPACITY);
        let join = thread::Builder::new()
            .name("breeze-atspi-worker".to_string())
            .spawn(move || run_atspi_worker(rx, config))
            .ok();
        *guard = Some(WorkerHandle { tx, join });
    }

    /// Prompts the worker to re-evaluate `config.current().auto_enable_atspi` immediately
    /// (e.g. after `ReloadConfig`).
    pub fn sync_config(&self) {
        if let Ok(guard) = self.worker.lock()
            && let Some(handle) = guard.as_ref()
        {
            let _ = handle.tx.try_send(WorkerCmd::SyncConfig);
        }
    }

    /// Stops the AT-SPI2 worker thread and restores `org.a11y.Status.IsEnabled` if we enabled it.
    pub fn stop(&self) {
        let handle = if let Ok(mut guard) = self.worker.lock() {
            guard.take()
        } else {
            None
        };
        if let Some(mut h) = handle {
            if h.tx.try_send(WorkerCmd::Shutdown).is_err() {
                let _ = h.tx.send(WorkerCmd::Shutdown);
            }
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
        let (reply_tx, reply_rx) = mpsc::sync_channel(1);
        tx.try_send(WorkerCmd::Query(QueryRequest {
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

async fn connect_session_with_timeout() -> Option<Connection> {
    connection::Builder::session()
        .method_timeout(Duration::from_millis(ATSPI_SESSION_TIMEOUT_MS))
        .build()
        .await
        .ok()
}

struct WorkerState {
    session_conn: Option<Connection>,
    a11y_conn: Option<Connection>,
    enabled_by_us: bool,
    last_auto_enable: bool,
    last_status_check: Option<Instant>,
    pid_cache: PidCache,
}

impl WorkerState {
    fn new(initial_auto_enable: bool) -> Self {
        Self {
            session_conn: None,
            a11y_conn: None,
            enabled_by_us: false,
            last_auto_enable: initial_auto_enable,
            last_status_check: None,
            pid_cache: PidCache::new(MAX_PID_CACHE_ENTRIES, PID_CACHE_TTL),
        }
    }

    /// Synchronizes AT-SPI2 bus state with current `auto_enable_atspi` opt-in setting and
    /// session accessibility status. Never enables `org.a11y.Status.IsEnabled` unless
    /// `auto_enable` is explicitly `true`.
    async fn reconcile_connection(&mut self, auto_enable: bool, force_recheck: bool) {
        let opt_in_changed = auto_enable != self.last_auto_enable;
        self.last_auto_enable = auto_enable;

        // If user revoked opt-in at runtime and we were the ones who enabled AT-SPI2, turn it off cleanly.
        if !auto_enable && self.enabled_by_us {
            if self.session_conn.is_none() {
                self.session_conn = connect_session_with_timeout().await;
            }
            if let Some(ref sconn) = self.session_conn {
                let _ = set_a11y_enabled(sconn, false).await;
            }
            self.enabled_by_us = false;
            self.a11y_conn = None;
            self.pid_cache.clear();
            self.last_status_check = Some(Instant::now());
            return;
        }

        if self.a11y_conn.is_some() {
            return;
        }

        let now = Instant::now();
        if !force_recheck
            && !opt_in_changed
            && self
                .last_status_check
                .is_some_and(|ts| now.saturating_duration_since(ts) < ATSPI_STATUS_RECHECK_INTERVAL)
        {
            return;
        }
        self.last_status_check = Some(now);

        if self.session_conn.is_none() {
            self.session_conn = connect_session_with_timeout().await;
        }
        let Some(ref sconn) = self.session_conn else {
            return;
        };

        let is_enabled = match get_a11y_enabled(sconn).await {
            Some(true) => true,
            Some(false) if auto_enable && set_a11y_enabled(sconn, true).await => {
                self.enabled_by_us = true;
                true
            }
            _ => false,
        };

        if is_enabled {
            self.a11y_conn = connect_atspi_bus(sconn).await;
        }
    }
}

fn run_atspi_worker(rx: Receiver<WorkerCmd>, config: ConfigManager) {
    zbus::block_on(async move {
        let initial_opt_in = config.current().auto_enable_atspi;
        let mut state = WorkerState::new(initial_opt_in);
        state.reconcile_connection(initial_opt_in, true).await;

        while let Ok(cmd) = rx.recv() {
            match cmd {
                WorkerCmd::SyncConfig => {
                    let opt_in = config.current().auto_enable_atspi;
                    state.reconcile_connection(opt_in, true).await;
                }
                WorkerCmd::Query(req) => {
                    let deadline = Instant::now() + Duration::from_millis(ATSPI_QUERY_TIMEOUT_MS);
                    let opt_in = config.current().auto_enable_atspi;
                    state.reconcile_connection(opt_in, false).await;

                    let result = if Instant::now() < deadline
                        && let Some(ref conn) = state.a11y_conn
                    {
                        query_atspi_point(conn, &mut state.pid_cache, &req, deadline).await
                    } else {
                        None
                    };
                    let _ = req.reply_tx.try_send(result);
                }
                WorkerCmd::Shutdown => break,
            }
        }

        // Cleanly restore org.a11y.Status.IsEnabled ONLY if we explicitly enabled it under opt-in
        if state.enabled_by_us
            && let Some(ref sconn) = state.session_conn
        {
            let _ = set_a11y_enabled(sconn, false).await;
        }
    });
}

async fn get_a11y_enabled(session_conn: &Connection) -> Option<bool> {
    let proxy = Proxy::new(
        session_conn,
        "org.a11y.Bus",
        "/org/a11y/bus",
        "org.freedesktop.DBus.Properties",
    )
    .await
    .ok()?;

    proxy
        .call::<_, _, OwnedVariant>("Get", &("org.a11y.Status", "IsEnabled"))
        .await
        .ok()
        .and_then(|v| bool::try_from(&v).ok())
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
        .method_timeout(Duration::from_millis(ATSPI_CALL_TIMEOUT_MS))
        .build()
        .await
        .ok()
}

async fn query_atspi_point(
    conn: &Connection,
    pid_cache: &mut PidCache,
    req: &QueryRequest,
    deadline: Instant,
) -> Option<AtSpiHitResult> {
    if Instant::now() >= deadline {
        return None;
    }
    let bus_name = resolve_pid_bus_name(conn, pid_cache, req.pid, deadline).await?;
    let (active_bus_name, frame_path) =
        match find_target_frame(conn, &bus_name, req, deadline).await {
            Some(path) => (bus_name, path),
            None => {
                if Instant::now() >= deadline {
                    return None;
                }
                // Cached bus name may belong to a restarted process; refresh once
                pid_cache.remove(req.pid);
                let refreshed = resolve_pid_bus_name(conn, pid_cache, req.pid, deadline).await?;
                let path = find_target_frame(conn, &refreshed, req, deadline).await?;
                (refreshed, path)
            }
        };

    hit_test_tree(conn, &active_bus_name, &frame_path, req, deadline).await
}

async fn resolve_pid_bus_name(
    conn: &Connection,
    pid_cache: &mut PidCache,
    target_pid: u32,
    deadline: Instant,
) -> Option<String> {
    let now = Instant::now();
    if let Some(cached) = pid_cache.get(target_pid, now) {
        return Some(cached);
    }
    if now >= deadline {
        return None;
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

    let mut found: Option<String> = None;
    for (sender, _) in children.into_iter().take(MAX_REGISTRY_CHILDREN_SCAN) {
        if Instant::now() >= deadline {
            break;
        }
        if sender.is_empty() {
            continue;
        }
        if let Ok(pid) = dbus_proxy
            .call::<_, _, u32>("GetConnectionUnixProcessID", &(&sender,))
            .await
        {
            let ts = Instant::now();
            if pid == target_pid {
                found = Some(sender.clone());
            }
            pid_cache.insert(pid, sender, ts);
            if found.is_some() {
                break;
            }
        }
    }

    found.or_else(|| pid_cache.get(target_pid, Instant::now()))
}

async fn find_target_frame(
    conn: &Connection,
    bus_name: &str,
    req: &QueryRequest,
    deadline: Instant,
) -> Option<String> {
    if Instant::now() >= deadline {
        return None;
    }
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
        .take(32)
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
        if Instant::now() >= deadline {
            break;
        }
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
        if Instant::now() >= deadline {
            break;
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
    deadline: Instant,
) -> Option<AtSpiHitResult> {
    let mut curr_path = frame_path.to_string();
    let mut last_role: Option<String> = None;

    for _depth in 0..8 {
        if Instant::now() >= deadline {
            break;
        }
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
            && Instant::now() < deadline
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
            if Instant::now() < deadline
                && let Some(candidate) = get_accessible_at_point(
                    conn,
                    bus_name,
                    &curr_path,
                    local_x,
                    local_y,
                    ATSPI_COORD_SCREEN,
                )
                .await
                && candidate != curr_path
                && Instant::now() < deadline
                && let Some(ext) =
                    get_extents(conn, bus_name, &candidate, ATSPI_COORD_WINDOW).await
                && rect_contains_point(ext, req.rel_x, req.rel_y, 4)
            {
                next_path = Some(candidate);
            }
        }

        // Also try SCREEN coordinates on the frame root if WINDOW coords returned null (GTK/X11)
        if next_path.is_none()
            && Instant::now() < deadline
            && curr_path == frame_path
            && let Some(candidate) = get_accessible_at_point(
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

        let Some(child_path) = next_path else {
            break;
        };
        if child_path == curr_path || Instant::now() >= deadline {
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
        let folder = (361, 93, 125, 88);
        assert!(rect_contains_point(folder, 400, 130, 4));
        assert!(rect_contains_point(folder, 361, 93, 4));
        assert!(rect_contains_point(folder, 486, 181, 4));
        assert!(!rect_contains_point(folder, 351, 130, 4));
        assert!(!rect_contains_point(folder, 900, 500, 4));
        assert!(!rect_contains_point((0, 0, 0, 0), 0, 0, 4));
    }

    #[test]
    fn pid_cache_enforces_capacity_and_ttl_eviction() {
        let mut cache = PidCache::new(3, Duration::from_secs(10));
        let t0 = Instant::now();

        cache.insert(101, ":1.10".to_string(), t0);
        cache.insert(102, ":1.11".to_string(), t0 + Duration::from_secs(1));
        cache.insert(103, ":1.12".to_string(), t0 + Duration::from_secs(2));
        assert_eq!(cache.len(), 3);
        assert_eq!(cache.get(101, t0 + Duration::from_secs(2)).as_deref(), Some(":1.10"));

        // Inserting a 4th entry evicts the oldest (PID 101)
        cache.insert(104, ":1.13".to_string(), t0 + Duration::from_secs(3));
        assert_eq!(cache.len(), 3);
        assert!(cache.get(101, t0 + Duration::from_secs(3)).is_none());
        assert_eq!(cache.get(104, t0 + Duration::from_secs(3)).as_deref(), Some(":1.13"));

        // Entries expire once TTL has elapsed
        assert!(cache.get(102, t0 + Duration::from_secs(12)).is_none());
        cache.purge_expired(t0 + Duration::from_secs(14));
        assert_eq!(cache.len(), 0);
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

        let cfg = ConfigManager::try_new(None).expect("config");
        let inspector = AtSpiInspector::new();
        inspector.start(cfg);
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
