//! Configuration management for Breeze Autoscroll.
//!
//! Preserves full JSON compatibility with the legacy Python `config.json`
//! while adding 3D/CAD/Slicer protection, drag-cancellation settings,
//! atomic secure persistence (`0600` / `O_NOFOLLOW`), and lock-free snapshot sharing (`Arc`).

use std::env;
use std::ffi::{OsStr, OsString};
use std::fs::{self, DirBuilder, OpenOptions, Permissions};
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use serde::{Deserialize, Serialize};

/// Systemd user service name used as the primary single-autostart mechanism.
pub const SYSTEMD_USER_UNIT_NAME: &str = "breeze-autoscroll.service";

/// Current configuration schema version.
/// Version 2 switches default interaction to authentic Windows autoscroll
/// (`protect_3d_drag = false`, keeping `auto_exclude_3d_apps = true`).
pub const CURRENT_CONFIG_VERSION: u32 = 2;

/// Maximum number of entries allowed in `excluded_apps_and_titles`.
pub const MAX_EXCLUDED_ENTRIES: usize = 128;

/// Maximum UTF-8 byte length allowed for each entry in `excluded_apps_and_titles`.
pub const MAX_EXCLUDED_ENTRY_BYTES: usize = 128;

static TEMP_FILE_SEQ: AtomicU64 = AtomicU64::new(0);

/// Default list of 3D CAD programs, slicers, engines, and browser 3D websites
/// where middle-click drag is used for 3D orbit/pan navigation.
pub fn default_excluded_apps_and_titles() -> Vec<String> {
    [
        "onshape",
        "bambu-studio",
        "bambustudio",
        "bambu studio",
        "orca-slicer",
        "orcaslicer",
        "prusa-slicer",
        "prusaslicer",
        "superslicer",
        "cura",
        "freecad",
        "blender",
        "openscad",
        "solvespace",
        "kicad",
        "meshlab",
        "cloudcompare",
        "tinkercad",
        "sketchup",
        "sketchfab",
        "fusion 360",
        "three.js",
        "babylon.js",
        "spline",
        "3d viewer",
        "godot",
        "unity",
        "unreal",
    ]
    .iter()
    .map(|s| (*s).to_string())
    .collect()
}

fn default_zero_u32() -> u32 {
    0
}
fn default_true() -> bool {
    true
}
fn default_false() -> bool {
    false
}
fn default_mode() -> String {
    "smart".to_string()
}
fn default_deadzone() -> u32 {
    12
}
fn default_scroll_speed() -> f32 {
    1.0
}
fn default_scroll_curve_exponent() -> f32 {
    1.3
}
fn default_theme() -> String {
    "dark".to_string()
}
fn default_indicator_size() -> u32 {
    44
}
fn default_indicator_opacity() -> f32 {
    0.92
}
fn default_tick_interval_ms() -> u64 {
    16
}
fn default_drag_cancel_threshold_px() -> f32 {
    5.0
}
fn default_hold_delay_ms() -> u64 {
    180
}
fn default_browser_top_chrome_px() -> u32 {
    95
}

fn sanitize_f32(val: f32, default: f32, min: f32, max: f32) -> f32 {
    if val.is_finite() {
        val.clamp(min, max)
    } else {
        default
    }
}

/// Truncates `s` to at most `max_bytes` UTF-8 bytes without splitting a multi-byte character.
pub(crate) fn truncate_utf8_bytes(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Persistent user configuration stored in `~/.config/breeze-autoscroll/config.json`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct ConfigData {
    #[serde(default = "default_zero_u32")]
    pub config_version: u32,

    #[serde(default = "default_true")]
    pub enabled: bool,

    /// Interaction mode: `"smart"`, `"toggle"`, or `"drag"`.
    #[serde(default = "default_mode")]
    pub mode: String,

    /// Neutral deadzone radius in pixels around the anchor point.
    #[serde(default = "default_deadzone")]
    pub deadzone: u32,

    /// Scroll speed multiplier (`0.2` to `3.0`).
    #[serde(default = "default_scroll_speed")]
    pub scroll_speed: f32,

    /// Speed curve exponent (`1.3` default).
    #[serde(default = "default_scroll_curve_exponent")]
    pub scroll_curve_exponent: f32,

    /// Whether horizontal 2D scrolling (`REL_HWHEEL`) is enabled.
    #[serde(default = "default_true")]
    pub allow_horizontal: bool,

    #[serde(default = "default_false")]
    pub invert_vertical: bool,

    #[serde(default = "default_false")]
    pub invert_horizontal: bool,

    /// Theme name: `"dark"` or `"light"`.
    #[serde(default = "default_theme")]
    pub theme: String,

    #[serde(default = "default_indicator_size")]
    pub indicator_size: u32,

    #[serde(default = "default_indicator_opacity")]
    pub indicator_opacity: f32,

    /// Start automatically on KDE login (`~/.config/autostart/breeze-autoscroll.desktop`).
    #[serde(default = "default_true")]
    pub autostart: bool,

    /// Physics & hi-res scroll update loop interval in milliseconds (~60Hz = 16ms).
    #[serde(default = "default_tick_interval_ms")]
    pub tick_interval_ms: u64,

    /// Optional legacy guard: abort autoscroll activation if the mouse moves immediately while
    /// BTN_MIDDLE is held (disables Windows Halte-Modus drag-out activation).
    #[serde(default = "default_false")]
    pub protect_3d_drag: bool,

    /// Movement threshold in pixels during the pending hold phase that cancels autoscroll.
    #[serde(default = "default_drag_cancel_threshold_px")]
    pub drag_cancel_threshold_px: f32,

    /// System-wide short-click protection: quick stationary middle-clicks (< `hold_delay_ms`
    /// without dragging out of the deadzone) pass through untouched to every application
    /// (closing tabs in Firefox/Obsidian/VS Code/Nautilus/Dolphin, opening links & folders,
    /// primary-selection paste). Pulling outside the deadzone still starts Halte-Modus immediately;
    /// holding stationary >= `hold_delay_ms` activates Einrast-Modus.
    #[serde(default = "default_true")]
    pub short_click_passthrough: bool,

    /// Stationary hold duration in milliseconds before Einrast-Modus activates when
    /// `short_click_passthrough` (or `protect_3d_drag`) is enabled.
    #[serde(default = "default_hold_delay_ms")]
    pub hold_delay_ms: u64,

    /// Do not activate autoscroll when middle-clicking interactive elements
    /// (browser tabs, address/bookmarks bar, window titlebars, or hyperlinks).
    #[serde(default = "default_true")]
    pub ignore_hyperlinks_and_tabs: bool,

    /// Opt-in: automatically enable `org.a11y.Status.IsEnabled` for the desktop session so
    /// AT-SPI2 hit-testing works even when KDE accessibility is off.
    /// Default `false`: only use AT-SPI2 when already enabled in the session.
    #[serde(default = "default_false")]
    pub auto_enable_atspi: bool,

    /// Height in pixels of the top browser chrome (tab bar + navigation/address bar)
    /// where middle-click closes tabs or interacts with toolbar controls.
    #[serde(default = "default_browser_top_chrome_px")]
    pub browser_top_chrome_px: u32,

    /// Automatically disable autoscroll when a recognized 3D/CAD app or browser tab is active.
    #[serde(default = "default_true")]
    pub auto_exclude_3d_apps: bool,

    /// Case-insensitive keywords matched against active window class, desktop file, or title.
    #[serde(default = "default_excluded_apps_and_titles")]
    pub excluded_apps_and_titles: Vec<String>,
}

impl Default for ConfigData {
    fn default() -> Self {
        Self {
            config_version: CURRENT_CONFIG_VERSION,
            enabled: default_true(),
            mode: default_mode(),
            deadzone: default_deadzone(),
            scroll_speed: default_scroll_speed(),
            scroll_curve_exponent: default_scroll_curve_exponent(),
            allow_horizontal: default_true(),
            invert_vertical: default_false(),
            invert_horizontal: default_false(),
            theme: default_theme(),
            indicator_size: default_indicator_size(),
            indicator_opacity: default_indicator_opacity(),
            autostart: default_true(),
            tick_interval_ms: default_tick_interval_ms(),
            protect_3d_drag: default_false(),
            drag_cancel_threshold_px: default_drag_cancel_threshold_px(),
            short_click_passthrough: default_true(),
            hold_delay_ms: default_hold_delay_ms(),
            ignore_hyperlinks_and_tabs: default_true(),
            auto_enable_atspi: default_false(),
            browser_top_chrome_px: default_browser_top_chrome_px(),
            auto_exclude_3d_apps: default_true(),
            excluded_apps_and_titles: default_excluded_apps_and_titles(),
        }
    }
}

impl ConfigData {
    /// Normalizes out-of-range, non-finite, or oversized fields after loading or editing.
    pub fn normalize(&mut self) {
        self.mode = match self.mode.trim().to_ascii_lowercase().as_str() {
            "toggle" => "toggle".to_string(),
            "drag" => "drag".to_string(),
            _ => "smart".to_string(),
        };
        self.deadzone = self.deadzone.clamp(4, 30);
        self.scroll_speed = sanitize_f32(self.scroll_speed, default_scroll_speed(), 0.2, 3.0);
        self.scroll_curve_exponent = sanitize_f32(
            self.scroll_curve_exponent,
            default_scroll_curve_exponent(),
            1.0,
            2.5,
        );
        self.theme = if self.theme.trim().eq_ignore_ascii_case("light") {
            "light".to_string()
        } else {
            "dark".to_string()
        };
        self.indicator_size = self.indicator_size.clamp(24, 96);
        self.indicator_opacity = sanitize_f32(
            self.indicator_opacity,
            default_indicator_opacity(),
            0.2,
            1.0,
        );
        self.tick_interval_ms = self.tick_interval_ms.clamp(8, 50);
        self.drag_cancel_threshold_px = sanitize_f32(
            self.drag_cancel_threshold_px,
            default_drag_cancel_threshold_px(),
            2.0,
            25.0,
        );
        self.hold_delay_ms = self.hold_delay_ms.clamp(80, 600);
        self.browser_top_chrome_px = self.browser_top_chrome_px.clamp(40, 180);
        self.excluded_apps_and_titles = self
            .excluded_apps_and_titles
            .iter()
            .map(|s| {
                truncate_utf8_bytes(s.trim(), MAX_EXCLUDED_ENTRY_BYTES)
                    .trim()
                    .to_string()
            })
            .filter(|s| !s.is_empty())
            .take(MAX_EXCLUDED_ENTRIES)
            .collect();
    }

    /// Formats `excluded_apps_and_titles` as a comma-separated string for the settings UI.
    pub fn excluded_as_csv(&self) -> String {
        self.excluded_apps_and_titles.join(", ")
    }

    /// Parses a comma-separated string into `excluded_apps_and_titles` with size/length caps.
    pub fn set_excluded_from_csv(&mut self, csv: &str) {
        self.excluded_apps_and_titles = csv
            .split([',', '\n', ';'])
            .map(|item| {
                truncate_utf8_bytes(item.trim(), MAX_EXCLUDED_ENTRY_BYTES)
                    .trim()
                    .to_string()
            })
            .filter(|item| !item.is_empty())
            .take(MAX_EXCLUDED_ENTRIES)
            .collect();
    }
}

/// Thread-safe manager for loading, modifying, and persisting `ConfigData` via immutable `Arc` snapshots.
#[derive(Clone)]
pub struct ConfigManager {
    config_file: PathBuf,
    data: Arc<RwLock<Arc<ConfigData>>>,
    sync_autostart_on_save: bool,
}

impl ConfigManager {
    /// Creates a `ConfigManager` pointing to `$XDG_CONFIG_HOME/breeze-autoscroll` or
    /// `$HOME/.config/breeze-autoscroll` (or a custom directory). Fails closed if neither
    /// `XDG_CONFIG_HOME` nor `HOME` is set and `config_dir` is `None`.
    pub fn try_new(config_dir: Option<PathBuf>) -> Result<Self, String> {
        let sync_autostart_on_save = config_dir.is_none();
        let dir = match config_dir {
            Some(d) => d,
            None => default_config_dir()?,
        };
        let file = dir.join("config.json");
        let manager = Self {
            config_file: file,
            data: Arc::new(RwLock::new(Arc::new(ConfigData::default()))),
            sync_autostart_on_save,
        };
        manager.load();
        Ok(manager)
    }

    /// Returns a cheap `Arc` handle to the current configuration without deep-cloning.
    pub fn current(&self) -> Arc<ConfigData> {
        self.data
            .read()
            .map(|guard| Arc::clone(&*guard))
            .unwrap_or_else(|_| Arc::new(ConfigData::default()))
    }

    /// Returns an owned copy of the current configuration (e.g. for UI editing or tests).
    pub fn snapshot(&self) -> ConfigData {
        (*self.current()).clone()
    }

    /// Reloads configuration from disk (ignoring missing/corrupt files safely).
    pub fn load(&self) {
        if self.config_file.exists() {
            match fs::read_to_string(&self.config_file) {
                Ok(content) => match serde_json::from_str::<ConfigData>(&content) {
                    Ok(mut parsed) => {
                        let needs_migration = parsed.config_version < CURRENT_CONFIG_VERSION;
                        if needs_migration {
                            parsed.protect_3d_drag = false;
                            parsed.config_version = CURRENT_CONFIG_VERSION;
                        }
                        parsed.normalize();
                        if let Ok(mut guard) = self.data.write() {
                            *guard = Arc::new(parsed);
                        }
                        if needs_migration {
                            self.save();
                        }
                    }
                    Err(err) => {
                        eprintln!("[Config] Warnung beim Parsen der Konfiguration: {err}");
                    }
                },
                Err(err) => {
                    eprintln!("[Config] Warnung beim Lesen der Konfiguration: {err}");
                }
            }
        } else {
            self.save();
        }
    }

    /// Persists the current in-memory configuration atomically to disk (`0700` dir, `0600` file).
    pub fn save(&self) {
        let current = self.current();
        match serde_json::to_string_pretty(&*current) {
            Ok(json) => {
                if let Err(err) =
                    write_file_atomic_secure(&self.config_file, json.as_bytes(), true)
                {
                    eprintln!("[Config] Fehler beim Speichern der Konfiguration: {err}");
                }
            }
            Err(err) => {
                eprintln!("[Config] Fehler beim Serialisieren der Konfiguration: {err}");
            }
        }
    }

    /// Updates configuration via closure, normalizes, optionally saves, and syncs autostart
    /// only when the `autostart` setting changed.
    pub fn update<F>(&self, save_to_disk: bool, f: F)
    where
        F: FnOnce(&mut ConfigData),
    {
        self.update_with_autostart_sync(save_to_disk, false, f);
    }

    /// Updates configuration via closure, normalizes, optionally saves, and syncs autostart when
    /// `autostart` changed or `force_sync_autostart` is set.
    pub fn update_with_autostart_sync<F>(
        &self,
        save_to_disk: bool,
        force_sync_autostart: bool,
        f: F,
    ) where
        F: FnOnce(&mut ConfigData),
    {
        let (prev_autostart, autostart) = if let Ok(mut guard) = self.data.write() {
            let prev = guard.autostart;
            let mut next = (**guard).clone();
            f(&mut next);
            next.normalize();
            let auto = next.autostart;
            *guard = Arc::new(next);
            (prev, auto)
        } else {
            return;
        };
        if save_to_disk {
            self.save();
            if self.sync_autostart_on_save && (force_sync_autostart || prev_autostart != autostart)
            {
                sync_kde_autostart(autostart);
            }
        }
    }

    /// Resets configuration to defaults and persists it.
    pub fn reset_defaults(&self) {
        if let Ok(mut guard) = self.data.write() {
            *guard = Arc::new(ConfigData::default());
        }
        self.save();
        if self.sync_autostart_on_save {
            sync_kde_autostart(true);
        }
    }

    /// Returns the path to `config.json`.
    #[cfg(test)]
    pub fn config_path(&self) -> &Path {
        &self.config_file
    }
}

/// Resolves `~/.config/breeze-autoscroll` (or `$XDG_CONFIG_HOME/breeze-autoscroll`).
/// Fails closed if neither `XDG_CONFIG_HOME` nor `HOME` is set (never falls back to `/tmp`).
pub fn default_config_dir() -> Result<PathBuf, String> {
    default_config_dir_from(env::var_os("XDG_CONFIG_HOME"), env::var_os("HOME"))
}

fn default_config_dir_from(
    xdg_config_home: Option<OsString>,
    home: Option<OsString>,
) -> Result<PathBuf, String> {
    if let Some(xdg) = xdg_config_home
        && !xdg.is_empty()
    {
        let p = PathBuf::from(xdg);
        if p.is_absolute() {
            return Ok(p.join("breeze-autoscroll"));
        }
    }
    if let Some(h) = home
        && !h.is_empty()
    {
        let p = PathBuf::from(h);
        if p.is_absolute() {
            return Ok(p.join(".config/breeze-autoscroll"));
        }
    }
    Err(
        "Weder XDG_CONFIG_HOME noch HOME ist auf einen absoluten Pfad gesetzt; Konfigurationsverzeichnis kann nicht sicher bestimmt werden."
            .to_string(),
    )
}

/// Ensures `dir` exists as a real user-owned directory (never a symlink) and optionally enforces
/// `0700` permissions (only for application-owned directories like `.../breeze-autoscroll`,
/// never for shared XDG directories like `~/.config/autostart`).
pub(crate) fn ensure_secure_dir(dir: &Path, enforce_dir_mode_0700: bool) -> io::Result<()> {
    match fs::symlink_metadata(dir) {
        Ok(meta) => {
            if meta.file_type().is_symlink() {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!("Zielverzeichnis darf kein Symlink sein: {}", dir.display()),
                ));
            }
            if !meta.is_dir() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("Zielpfad ist kein Verzeichnis: {}", dir.display()),
                ));
            }
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            if let Some(parent) = dir.parent()
                && !parent.as_os_str().is_empty()
                && !parent.exists()
            {
                fs::create_dir_all(parent)?;
            }
            let mut builder = DirBuilder::new();
            if enforce_dir_mode_0700 {
                builder.mode(0o700);
            }
            match builder.create(dir) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(e),
            }
        }
        Err(err) => return Err(err),
    }

    let meta = fs::symlink_metadata(dir)?;
    if meta.file_type().is_symlink() || !meta.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("Unsicheres Zielverzeichnis abgelehnt: {}", dir.display()),
        ));
    }

    let my_uid = unsafe { libc::geteuid() };
    if meta.uid() != my_uid {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "Zielverzeichnis gehört nicht dem aktuellen Benutzer (uid {} != {}): {}",
                meta.uid(),
                my_uid,
                dir.display()
            ),
        ));
    }

    if enforce_dir_mode_0700 {
        fs::set_permissions(dir, Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// Atomically writes `content` to `path` via a sibling temporary file created with
/// `O_CREAT | O_EXCL | O_NOFOLLOW | O_CLOEXEC` and mode `0600`, calls `sync_all()`,
/// rejects symlinks at both directory and target paths, and renames atomically into place.
///
/// Set `enforce_dir_mode_0700 = true` ONLY for application-owned directories
/// (`.../breeze-autoscroll`), and `false` for shared XDG directories (`~/.config/autostart`).
pub(crate) fn write_file_atomic_secure(
    path: &Path,
    content: &[u8],
    enforce_dir_mode_0700: bool,
) -> io::Result<()> {
    let parent = path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "Dateipfad besitzt kein Elternverzeichnis",
        )
    })?;
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "Ungültiger Dateiname"))?;

    ensure_secure_dir(parent, enforce_dir_mode_0700)?;

    if let Ok(meta) = fs::symlink_metadata(path) {
        if meta.file_type().is_symlink() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("Zieldatei darf kein Symlink sein: {}", path.display()),
            ));
        }
        if !meta.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("Zielpfad ist keine reguläre Datei: {}", path.display()),
            ));
        }
    }

    let seq = TEMP_FILE_SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp_name = format!(".{file_name}.tmp.{}.{seq}", std::process::id());
    let tmp_path = parent.join(tmp_name);

    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&tmp_path)?;

    let write_res = (|| -> io::Result<()> {
        file.write_all(content)?;
        file.sync_all()?;
        Ok(())
    })();

    drop(file);

    if let Err(err) = write_res {
        let _ = fs::remove_file(&tmp_path);
        return Err(err);
    }

    if let Ok(meta) = fs::symlink_metadata(path)
        && meta.file_type().is_symlink()
    {
        let _ = fs::remove_file(&tmp_path);
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("Zieldatei darf kein Symlink sein: {}", path.display()),
        ));
    }

    if let Err(err) = fs::rename(&tmp_path, path) {
        let _ = fs::remove_file(&tmp_path);
        return Err(err);
    }

    Ok(())
}

/// Active single-autostart backend: `systemd --user` when the unit is installed, otherwise
/// XDG autostart `.desktop` fallback.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutostartBackend {
    SystemdUser,
    XdgAutostartDesktop,
}

fn systemd_user_unit_candidates_from(
    xdg_config_home: Option<&OsStr>,
    home: Option<&OsStr>,
) -> Vec<PathBuf> {
    let mut candidates = Vec::with_capacity(4);
    if let Some(xdg) = xdg_config_home
        && !xdg.is_empty()
    {
        let p = PathBuf::from(xdg);
        if p.is_absolute() {
            candidates.push(p.join("systemd/user").join(SYSTEMD_USER_UNIT_NAME));
        }
    }
    if let Some(h) = home
        && !h.is_empty()
    {
        let p = PathBuf::from(h);
        if p.is_absolute() {
            let home_unit = p.join(".config/systemd/user").join(SYSTEMD_USER_UNIT_NAME);
            if !candidates.contains(&home_unit) {
                candidates.push(home_unit);
            }
        }
    }
    candidates.push(PathBuf::from("/etc/systemd/user").join(SYSTEMD_USER_UNIT_NAME));
    candidates.push(PathBuf::from("/usr/lib/systemd/user").join(SYSTEMD_USER_UNIT_NAME));
    candidates
}

fn detect_autostart_backend_in(unit_candidates: &[PathBuf]) -> AutostartBackend {
    if unit_candidates
        .iter()
        .any(|p| fs::metadata(p).is_ok_and(|m| m.is_file()))
    {
        AutostartBackend::SystemdUser
    } else {
        AutostartBackend::XdgAutostartDesktop
    }
}

/// Detects whether `breeze-autoscroll.service` exists as a systemd user unit.
pub fn detect_autostart_backend() -> AutostartBackend {
    let xdg = env::var_os("XDG_CONFIG_HOME");
    let home = env::var_os("HOME");
    let candidates = systemd_user_unit_candidates_from(xdg.as_deref(), home.as_deref());
    detect_autostart_backend_in(&candidates)
}

fn autostart_desktop_paths_from(
    xdg_config_home: Option<&OsStr>,
    home: Option<&OsStr>,
) -> Vec<PathBuf> {
    let mut paths = Vec::with_capacity(2);
    if let Some(xdg) = xdg_config_home
        && !xdg.is_empty()
    {
        let p = PathBuf::from(xdg);
        if p.is_absolute() {
            paths.push(p.join("autostart/breeze-autoscroll.desktop"));
        }
    }
    if let Some(h) = home
        && !h.is_empty()
    {
        let p = PathBuf::from(h);
        if p.is_absolute() {
            let legacy = p.join(".config/autostart/breeze-autoscroll.desktop");
            if !paths.contains(&legacy) {
                paths.push(legacy);
            }
        }
    }
    paths
}

fn remove_regular_file_no_symlink(path: &Path) {
    if let Ok(meta) = fs::symlink_metadata(path)
        && !meta.file_type().is_symlink()
        && meta.is_file()
    {
        let _ = fs::remove_file(path);
    }
}

/// Quotes and escapes an executable path argument for a Freedesktop Desktop Entry `Exec` key.
///
/// Follows the Desktop Entry Specification:
/// - Rejects control characters (`\n`, `\r`, `\0`) to prevent key injection.
/// - Escapes `%` as `%%` so literal percent signs are not interpreted as field codes.
/// - Encloses arguments containing reserved characters in `"..."` and applies both Desktop Entry
///   string escaping and `Exec` quoting (`\` -> `\\\\`, `"` -> `\\"`, `$` -> `\\$`, `` ` `` -> `` \\` ``).
pub(crate) fn escape_desktop_exec_arg(arg: &str) -> Result<String, &'static str> {
    if arg.bytes().any(|b| b == b'\n' || b == b'\r' || b == 0) {
        return Err("Exec-Pfad enthält unzulässige Steuerzeichen");
    }

    let needs_quoting = arg.is_empty()
        || arg.chars().any(|c| {
            matches!(
                c,
                ' ' | '\t'
                    | '"'
                    | '\''
                    | '\\'
                    | '>'
                    | '<'
                    | '~'
                    | '|'
                    | '&'
                    | ';'
                    | '$'
                    | '*'
                    | '?'
                    | '#'
                    | '('
                    | ')'
                    | '`'
            )
        });

    if !needs_quoting {
        return Ok(arg.replace('%', "%%"));
    }

    let mut out = String::with_capacity(arg.len() + 8);
    out.push('"');
    for ch in arg.chars() {
        match ch {
            '\\' => out.push_str("\\\\\\\\"),
            '"' => out.push_str("\\\\\""),
            '$' => out.push_str("\\\\$"),
            '`' => out.push_str("\\\\`"),
            '\t' => out.push_str("\\t"),
            '%' => out.push_str("%%"),
            other => out.push(other),
        }
    }
    out.push('"');
    Ok(out)
}

fn format_desktop_exec_path(path: &str) -> String {
    escape_desktop_exec_arg(path).unwrap_or_else(|_| "breeze-autoscroll".to_string())
}

fn render_autostart_desktop_entry(exec_path: &str) -> String {
    let escaped_exec = format_desktop_exec_path(exec_path);
    format!(
        "[Desktop Entry]\n\
         Name=Breeze Autoscroll\n\
         GenericName=Mausrad-Klick Autoscroll\n\
         Comment=Windows-Mittelklick Autoscroll im Kubuntu 26.04 Breeze-Stil\n\
         Exec={escaped_exec}\n\
         Icon=preferences-desktop-mouse\n\
         Terminal=false\n\
         Type=Application\n\
         Categories=Utility;Settings;Qt;KDE;\n\
         StartupNotify=false\n\
         X-KDE-autostart-phase=2\n\
         X-GNOME-Autostart-enabled=true\n"
    )
}

fn parse_systemctl_is_enabled_output(exit_success: bool, stdout: &str) -> Option<bool> {
    match stdout.trim() {
        "enabled" | "enabled-runtime" | "linked" | "linked-runtime" | "alias" | "static" => {
            Some(true)
        }
        "disabled" | "masked" | "masked-runtime" | "indirect" | "not-found" => Some(false),
        _ if exit_success => Some(true),
        _ => None,
    }
}

/// Reads the live system autostart state without mutating disk or systemd:
/// - When a `systemd --user` unit exists: queries `systemctl --user is-enabled breeze-autoscroll.service`.
/// - Otherwise: checks whether the fallback XDG autostart `.desktop` file exists.
pub fn detect_system_autostart_enabled() -> Option<bool> {
    match detect_autostart_backend() {
        AutostartBackend::SystemdUser => {
            let output = Command::new("systemctl")
                .args(["--user", "is-enabled", SYSTEMD_USER_UNIT_NAME])
                .stdin(Stdio::null())
                .output()
                .ok()?;
            let stdout = String::from_utf8_lossy(&output.stdout);
            parse_systemctl_is_enabled_output(output.status.success(), &stdout)
        }
        AutostartBackend::XdgAutostartDesktop => {
            let xdg = env::var_os("XDG_CONFIG_HOME");
            let home = env::var_os("HOME");
            let primary = autostart_desktop_paths_from(xdg.as_deref(), home.as_deref())
                .into_iter()
                .next()?;
            let exists = fs::symlink_metadata(&primary)
                .is_ok_and(|m| !m.file_type().is_symlink() && m.is_file());
            Some(exists)
        }
    }
}

/// Synchronizes autostart cleanly using a single mechanism:
/// - If `breeze-autoscroll.service` exists as a systemd user unit: toggles via
///   `systemctl --user enable/disable breeze-autoscroll.service` (without `--now`) and removes
///   any legacy `~/.config/autostart/breeze-autoscroll.desktop` file.
/// - Only if no systemd user unit exists: falls back to XDG autostart `.desktop` written via
///   `write_file_atomic_secure(..., false)` with Freedesktop `Exec` escaping and no shebang.
pub fn sync_kde_autostart(enabled: bool) {
    let xdg = env::var_os("XDG_CONFIG_HOME");
    let home = env::var_os("HOME");
    let desktop_paths = autostart_desktop_paths_from(xdg.as_deref(), home.as_deref());

    match detect_autostart_backend() {
        AutostartBackend::SystemdUser => {
            let action = if enabled { "enable" } else { "disable" };
            let _ = Command::new("systemctl")
                .args(["--user", action, SYSTEMD_USER_UNIT_NAME])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();

            // Prevent double-start at login by cleaning up any legacy XDG autostart entry.
            for legacy_path in &desktop_paths {
                remove_regular_file_no_symlink(legacy_path);
            }
        }
        AutostartBackend::XdgAutostartDesktop => {
            let Some(desktop_file) = desktop_paths.first() else {
                return;
            };
            if enabled {
                let exec_path = env::current_exe()
                    .ok()
                    .and_then(|p| p.to_str().map(str::to_string))
                    .unwrap_or_else(|| "breeze-autoscroll".to_string());
                let content = render_autostart_desktop_entry(&exec_path);
                let _ = write_file_atomic_secure(desktop_file, content.as_bytes(), false);
            } else {
                for path in &desktop_paths {
                    remove_regular_file_no_symlink(path);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_config_dir(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = env::temp_dir().join(format!(
            "breeze_autoscroll_cfg_test_{}_{}_{}",
            tag,
            std::process::id(),
            nanos
        ));
        let _ = fs::create_dir_all(&dir);
        dir
    }

    #[test]
    fn loads_defaults_when_file_missing() {
        let dir = temp_config_dir("defaults");
        let mgr = ConfigManager::try_new(Some(dir.clone())).unwrap();
        let cfg = mgr.current();

        assert!(cfg.enabled);
        assert_eq!(cfg.config_version, CURRENT_CONFIG_VERSION);
        assert_eq!(cfg.mode, "smart");
        assert_eq!(cfg.deadzone, 12);
        assert!((cfg.scroll_speed - 1.0).abs() < 1e-5);
        assert!(cfg.allow_horizontal);
        assert_eq!(cfg.theme, "dark");
        assert!(!cfg.protect_3d_drag);
        assert!(cfg.short_click_passthrough);
        assert!(cfg.ignore_hyperlinks_and_tabs);
        assert!(!cfg.auto_enable_atspi);
        assert_eq!(cfg.browser_top_chrome_px, 95);
        assert!(cfg.auto_exclude_3d_apps);
        assert!(cfg.excluded_apps_and_titles.iter().any(|s| s == "onshape"));
        assert!(
            cfg.excluded_apps_and_titles
                .iter()
                .any(|s| s == "bambu studio")
        );
        assert!(mgr.config_path().exists());

        // Atomic secure write enforces 0700 dir and 0600 file
        let dir_mode = fs::symlink_metadata(&dir).unwrap().permissions().mode() & 0o777;
        let file_mode = fs::symlink_metadata(mgr.config_path())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(dir_mode, 0o700);
        assert_eq!(file_mode, 0o600);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn default_config_dir_fails_closed_without_xdg_or_home() {
        assert!(default_config_dir_from(None, None).is_err());
        assert!(default_config_dir_from(Some(OsString::new()), Some(OsString::new())).is_err());
        assert!(default_config_dir_from(Some("relative/path".into()), None).is_err());
        assert_eq!(
            default_config_dir_from(Some("/home/alice/.xdg".into()), Some("/home/alice".into()))
                .unwrap(),
            PathBuf::from("/home/alice/.xdg/breeze-autoscroll")
        );
        assert_eq!(
            default_config_dir_from(None, Some("/home/alice".into())).unwrap(),
            PathBuf::from("/home/alice/.config/breeze-autoscroll")
        );
    }

    #[test]
    fn normalize_sanitizes_non_finite_floats_clamps_indicator_and_caps_exclusions() {
        let mut cfg = ConfigData {
            scroll_speed: f32::NAN,
            scroll_curve_exponent: f32::INFINITY,
            drag_cancel_threshold_px: f32::NEG_INFINITY,
            indicator_opacity: f32::NAN,
            indicator_size: 9999,
            excluded_apps_and_titles: (0..200)
                .map(|i| format!("app_{i}_{}", "x".repeat(200)))
                .collect(),
            ..Default::default()
        };

        cfg.normalize();

        assert!((cfg.scroll_speed - 1.0).abs() < 1e-5);
        assert!((cfg.scroll_curve_exponent - 1.3).abs() < 1e-5);
        assert!((cfg.drag_cancel_threshold_px - 5.0).abs() < 1e-5);
        assert!((cfg.indicator_opacity - 0.92).abs() < 1e-5);
        assert_eq!(cfg.indicator_size, 96);
        assert_eq!(cfg.excluded_apps_and_titles.len(), MAX_EXCLUDED_ENTRIES);
        assert!(
            cfg.excluded_apps_and_titles
                .iter()
                .all(|s| s.len() <= MAX_EXCLUDED_ENTRY_BYTES)
        );

        cfg.indicator_opacity = 0.01;
        cfg.indicator_size = 5;
        cfg.normalize();
        assert!((cfg.indicator_opacity - 0.2).abs() < 1e-5);
        assert_eq!(cfg.indicator_size, 24);
    }

    #[test]
    fn atomic_secure_write_rejects_symlinks_and_respects_shared_dir_mode() {
        let base = temp_config_dir("atomic_secure");

        // 1. Shared directory (enforce_dir_mode_0700 = false) keeps its existing mode
        let shared_dir = base.join("shared_autostart");
        fs::create_dir(&shared_dir).unwrap();
        fs::set_permissions(&shared_dir, Permissions::from_mode(0o755)).unwrap();
        let desktop = shared_dir.join("test.desktop");
        write_file_atomic_secure(&desktop, b"[Desktop Entry]\n", false).unwrap();
        let shared_mode = fs::symlink_metadata(&shared_dir)
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        let file_mode = fs::symlink_metadata(&desktop).unwrap().permissions().mode() & 0o777;
        assert_eq!(shared_mode, 0o755);
        assert_eq!(file_mode, 0o600);

        // 2. Reject symlink at target file
        let victim = base.join("victim.txt");
        fs::write(&victim, b"original").unwrap();
        let link_file = shared_dir.join("evil.json");
        symlink(&victim, &link_file).unwrap();
        assert!(write_file_atomic_secure(&link_file, b"pwned", false).is_err());
        assert_eq!(fs::read(&victim).unwrap(), b"original");

        // 3. Reject symlink as parent directory
        let real_dir = base.join("real_dir");
        fs::create_dir(&real_dir).unwrap();
        let link_dir = base.join("link_dir");
        symlink(&real_dir, &link_dir).unwrap();
        assert!(write_file_atomic_secure(&link_dir.join("config.json"), b"{}", true).is_err());

        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn current_returns_shared_arc_without_recloning() {
        let dir = temp_config_dir("arc_sharing");
        let mgr = ConfigManager::try_new(Some(dir.clone())).unwrap();
        let a = mgr.current();
        let b = mgr.current();
        assert!(Arc::ptr_eq(&a, &b));

        mgr.update(false, |c| c.deadzone = 18);
        let c = mgr.current();
        assert!(!Arc::ptr_eq(&a, &c));
        assert_eq!(c.deadzone, 18);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn preserves_legacy_python_config_fields_and_adds_new_defaults() {
        let dir = temp_config_dir("legacy");
        let legacy_json = r#"{
            "enabled": true,
            "mode": "smart",
            "deadzone": 14,
            "scroll_speed": 0.72,
            "scroll_curve_exponent": 1.3,
            "allow_horizontal": false,
            "invert_vertical": false,
            "invert_horizontal": false,
            "theme": "dark",
            "indicator_size": 56,
            "indicator_opacity": 0.9,
            "autostart": false,
            "tick_interval_ms": 16
        }"#;
        fs::write(dir.join("config.json"), legacy_json).unwrap();

        let mgr = ConfigManager::try_new(Some(dir.clone())).unwrap();
        let cfg = mgr.snapshot();

        assert_eq!(cfg.config_version, CURRENT_CONFIG_VERSION);
        assert_eq!(cfg.deadzone, 14);
        assert!((cfg.scroll_speed - 0.72).abs() < 1e-5);
        assert!(!cfg.allow_horizontal);
        assert!(!cfg.autostart);
        assert!(!cfg.protect_3d_drag);
        assert!(cfg.short_click_passthrough);
        assert!(cfg.ignore_hyperlinks_and_tabs);
        assert!(!cfg.auto_enable_atspi);
        assert_eq!(cfg.browser_top_chrome_px, 95);
        assert!(cfg.auto_exclude_3d_apps);
        assert!((cfg.drag_cancel_threshold_px - 5.0).abs() < 1e-5);
        assert_eq!(cfg.hold_delay_ms, 180);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn migrates_v1_config_with_protect_3d_drag_true_preserving_user_settings() {
        let dir = temp_config_dir("migrate_v1");
        let v1_json = r#"{
            "enabled": true,
            "mode": "smart",
            "deadzone": 12,
            "scroll_speed": 0.5,
            "scroll_curve_exponent": 1.3,
            "allow_horizontal": false,
            "autostart": false,
            "protect_3d_drag": true,
            "auto_exclude_3d_apps": true
        }"#;
        fs::write(dir.join("config.json"), v1_json).unwrap();

        let mgr = ConfigManager::try_new(Some(dir.clone())).unwrap();
        let cfg = mgr.snapshot();
        assert_eq!(cfg.config_version, CURRENT_CONFIG_VERSION);
        assert!(!cfg.protect_3d_drag);
        assert!(cfg.short_click_passthrough);
        assert!(cfg.ignore_hyperlinks_and_tabs);
        assert_eq!(cfg.browser_top_chrome_px, 95);
        assert!(cfg.auto_exclude_3d_apps);
        assert!((cfg.scroll_speed - 0.5).abs() < 1e-5);
        assert!(!cfg.allow_horizontal);
        assert!(!cfg.autostart);

        // Once migrated to v2, an explicit user opt-in to protect_3d_drag is preserved
        mgr.update(true, |c| c.protect_3d_drag = true);
        let reloaded = ConfigManager::try_new(Some(dir.clone())).unwrap();
        assert!(reloaded.snapshot().protect_3d_drag);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn saves_reloads_and_resets_defaults() {
        let dir = temp_config_dir("save_reload");
        let mgr = ConfigManager::try_new(Some(dir.clone())).unwrap();
        mgr.update(true, |cfg| {
            cfg.scroll_speed = 2.5;
            cfg.deadzone = 20;
            cfg.mode = "drag".to_string();
            cfg.set_excluded_from_csv("onshape, bambu studio, customcad");
        });

        let reloaded = ConfigManager::try_new(Some(dir.clone())).unwrap();
        let cfg = reloaded.snapshot();
        assert!((cfg.scroll_speed - 2.5).abs() < 1e-5);
        assert_eq!(cfg.deadzone, 20);
        assert_eq!(cfg.mode, "drag");
        assert_eq!(
            cfg.excluded_apps_and_titles,
            vec!["onshape", "bambu studio", "customcad"]
        );
        assert_eq!(cfg.excluded_as_csv(), "onshape, bambu studio, customcad");

        reloaded.reset_defaults();
        let reset_cfg = reloaded.snapshot();
        assert!((reset_cfg.scroll_speed - 1.0).abs() < 1e-5);
        assert_eq!(reset_cfg.mode, "smart");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn desktop_exec_escaping_handles_reserved_chars_percents_and_rejects_newlines() {
        // Simple path remains unquoted
        assert_eq!(
            escape_desktop_exec_arg("/usr/bin/breeze-autoscroll").unwrap(),
            "/usr/bin/breeze-autoscroll"
        );
        // Literal percent is doubled even without quoting
        assert_eq!(
            escape_desktop_exec_arg("/opt/100%/breeze-autoscroll").unwrap(),
            "/opt/100%%/breeze-autoscroll"
        );
        // Spaces trigger double-quoting
        assert_eq!(
            escape_desktop_exec_arg("/home/alice/My Apps/breeze-autoscroll").unwrap(),
            "\"/home/alice/My Apps/breeze-autoscroll\""
        );
        // Reserved characters + Desktop Entry string-unescape + Exec-quoting rules
        assert_eq!(
            escape_desktop_exec_arg(r#"/tmp/a "b" $HOME `id` \path %u"#).unwrap(),
            r#""/tmp/a \\"b\\" \\$HOME \\`id\\` \\\\path %%u""#
        );
        // Semicolon / parentheses trigger quoting
        assert_eq!(
            escape_desktop_exec_arg("/opt/app(1);test/bin").unwrap(),
            "\"/opt/app(1);test/bin\""
        );
        // Control characters must be rejected to prevent desktop entry key injection
        assert!(escape_desktop_exec_arg("/usr/bin/ok\nHidden=true").is_err());
        assert!(escape_desktop_exec_arg("/usr/bin/ok\rHidden=true").is_err());
        assert!(escape_desktop_exec_arg("/usr/bin/ok\0bad").is_err());
        assert_eq!(
            format_desktop_exec_path("/usr/bin/ok\nHidden=true"),
            "breeze-autoscroll"
        );

        // Fallback desktop file must not include xdg-open shebang and must quote Exec cleanly
        let desktop = render_autostart_desktop_entry("/home/alice/My Bin/breeze-autoscroll");
        assert!(desktop.starts_with("[Desktop Entry]\n"));
        assert!(!desktop.contains("#!/usr/bin/env xdg-open"));
        assert!(desktop.contains("Exec=\"/home/alice/My Bin/breeze-autoscroll\"\n"));
    }

    #[test]
    fn autostart_backend_detection_checks_systemd_unit_paths_and_systemctl_output() {
        let base = temp_config_dir("autostart_detect");
        let xdg = base.join("xdg");
        let home = base.join("home");

        let candidates = systemd_user_unit_candidates_from(
            Some(xdg.as_os_str()),
            Some(home.as_os_str()),
        );
        assert_eq!(
            candidates,
            vec![
                xdg.join("systemd/user/breeze-autoscroll.service"),
                home.join(".config/systemd/user/breeze-autoscroll.service"),
                PathBuf::from("/etc/systemd/user/breeze-autoscroll.service"),
                PathBuf::from("/usr/lib/systemd/user/breeze-autoscroll.service"),
            ]
        );

        // Isolated candidate list without any unit file -> XdgAutostartDesktop fallback
        let isolated = vec![
            xdg.join("systemd/user/breeze-autoscroll.service"),
            home.join(".config/systemd/user/breeze-autoscroll.service"),
        ];
        assert_eq!(
            detect_autostart_backend_in(&isolated),
            AutostartBackend::XdgAutostartDesktop
        );

        // Once user unit exists -> SystemdUser
        fs::create_dir_all(xdg.join("systemd/user")).unwrap();
        fs::write(&isolated[0], b"[Unit]\nDescription=Test\n").unwrap();
        assert_eq!(
            detect_autostart_backend_in(&isolated),
            AutostartBackend::SystemdUser
        );

        // systemctl is-enabled output parsing
        assert_eq!(parse_systemctl_is_enabled_output(true, "enabled\n"), Some(true));
        assert_eq!(
            parse_systemctl_is_enabled_output(true, "enabled-runtime\n"),
            Some(true)
        );
        assert_eq!(
            parse_systemctl_is_enabled_output(false, "disabled\n"),
            Some(false)
        );
        assert_eq!(
            parse_systemctl_is_enabled_output(false, "masked\n"),
            Some(false)
        );
        assert_eq!(
            parse_systemctl_is_enabled_output(false, "not-found\n"),
            Some(false)
        );
        // Bus/environment error returns None (do not clobber config)
        assert_eq!(parse_systemctl_is_enabled_output(false, ""), None);

        let _ = fs::remove_dir_all(&base);
    }
}
