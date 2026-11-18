//! Configuration management for Breeze Autoscroll.
//!
//! Preserves full JSON compatibility with the legacy Python `config.json`
//! while adding 3D/CAD/Slicer protection and drag-cancellation settings.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use serde::{Deserialize, Serialize};

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

/// Persistent user configuration stored in `~/.config/breeze-autoscroll/config.json`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct ConfigData {
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

    /// Abort autoscroll activation if the mouse moves immediately while BTN_MIDDLE is held
    /// (prevents interrupting 3D orbit/pan drags in CAD/slicers/browsers).
    #[serde(default = "default_true")]
    pub protect_3d_drag: bool,

    /// Movement threshold in pixels during the pending hold phase that cancels autoscroll.
    #[serde(default = "default_drag_cancel_threshold_px")]
    pub drag_cancel_threshold_px: f32,

    /// Stationary hold duration in milliseconds before autoscroll activates.
    #[serde(default = "default_hold_delay_ms")]
    pub hold_delay_ms: u64,

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
            protect_3d_drag: default_true(),
            drag_cancel_threshold_px: default_drag_cancel_threshold_px(),
            hold_delay_ms: default_hold_delay_ms(),
            auto_exclude_3d_apps: default_true(),
            excluded_apps_and_titles: default_excluded_apps_and_titles(),
        }
    }
}

impl ConfigData {
    /// Normalizes out-of-range fields after loading or editing.
    pub fn normalize(&mut self) {
        self.mode = match self.mode.trim().to_ascii_lowercase().as_str() {
            "toggle" => "toggle".to_string(),
            "drag" => "drag".to_string(),
            _ => "smart".to_string(),
        };
        self.deadzone = self.deadzone.clamp(4, 30);
        self.scroll_speed = self.scroll_speed.clamp(0.2, 3.0);
        self.scroll_curve_exponent = self.scroll_curve_exponent.clamp(1.0, 2.5);
        self.theme = if self.theme.trim().eq_ignore_ascii_case("light") {
            "light".to_string()
        } else {
            "dark".to_string()
        };
        self.tick_interval_ms = self.tick_interval_ms.clamp(8, 50);
        self.drag_cancel_threshold_px = self.drag_cancel_threshold_px.clamp(2.0, 25.0);
        self.hold_delay_ms = self.hold_delay_ms.clamp(80, 600);
        self.excluded_apps_and_titles = self
            .excluded_apps_and_titles
            .iter()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
    }

    /// Formats `excluded_apps_and_titles` as a comma-separated string for the settings UI.
    pub fn excluded_as_csv(&self) -> String {
        self.excluded_apps_and_titles.join(", ")
    }

    /// Parses a comma-separated string into `excluded_apps_and_titles`.
    pub fn set_excluded_from_csv(&mut self, csv: &str) {
        self.excluded_apps_and_titles = csv
            .split([',', '\n', ';'])
            .map(|item| item.trim().to_string())
            .filter(|item| !item.is_empty())
            .collect();
    }
}

/// Thread-safe manager for loading, modifying, and persisting `ConfigData`.
#[derive(Clone)]
pub struct ConfigManager {
    config_dir: PathBuf,
    config_file: PathBuf,
    data: Arc<RwLock<ConfigData>>,
}

impl ConfigManager {
    /// Creates a `ConfigManager` pointing to `~/.config/breeze-autoscroll` (or custom directory).
    pub fn new(config_dir: Option<PathBuf>) -> Self {
        let dir = config_dir.unwrap_or_else(default_config_dir);
        let file = dir.join("config.json");
        let manager = Self {
            config_dir: dir,
            config_file: file,
            data: Arc::new(RwLock::new(ConfigData::default())),
        };
        manager.load();
        manager
    }

    /// Returns a snapshot of the current configuration.
    pub fn snapshot(&self) -> ConfigData {
        self.data
            .read()
            .map(|guard| guard.clone())
            .unwrap_or_default()
    }

    /// Reloads configuration from disk (ignoring missing/corrupt files safely).
    pub fn load(&self) {
        if self.config_file.exists() {
            match fs::read_to_string(&self.config_file) {
                Ok(content) => match serde_json::from_str::<ConfigData>(&content) {
                    Ok(mut parsed) => {
                        parsed.normalize();
                        if let Ok(mut guard) = self.data.write() {
                            *guard = parsed;
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

    /// Persists the current in-memory configuration to disk.
    pub fn save(&self) {
        let snapshot = self.snapshot();
        if let Err(err) = fs::create_dir_all(&self.config_dir) {
            eprintln!("[Config] Fehler beim Erstellen des Konfigurationsordners: {err}");
            return;
        }
        match serde_json::to_string_pretty(&snapshot) {
            Ok(json) => {
                if let Err(err) = fs::write(&self.config_file, json) {
                    eprintln!("[Config] Fehler beim Speichern der Konfiguration: {err}");
                }
            }
            Err(err) => {
                eprintln!("[Config] Fehler beim Serialisieren der Konfiguration: {err}");
            }
        }
    }

    /// Updates configuration via closure, normalizes, optionally saves, and syncs KDE autostart.
    pub fn update<F>(&self, save_to_disk: bool, f: F)
    where
        F: FnOnce(&mut ConfigData),
    {
        if let Ok(mut guard) = self.data.write() {
            f(&mut guard);
            guard.normalize();
        }
        if save_to_disk {
            self.save();
            sync_kde_autostart(self.snapshot().autostart);
        }
    }

    /// Resets configuration to defaults and persists it.
    pub fn reset_defaults(&self) {
        if let Ok(mut guard) = self.data.write() {
            *guard = ConfigData::default();
        }
        self.save();
        sync_kde_autostart(true);
    }

    /// Returns the path to `config.json`.
    pub fn config_path(&self) -> &Path {
        &self.config_file
    }
}

/// Returns `~/.config/breeze-autoscroll` (or `$XDG_CONFIG_HOME/breeze-autoscroll`).
pub fn default_config_dir() -> PathBuf {
    if let Some(xdg) = env::var_os("XDG_CONFIG_HOME")
        && !xdg.is_empty()
    {
        return PathBuf::from(xdg).join("breeze-autoscroll");
    }
    let home = env::var_os("HOME").unwrap_or_else(|| "/tmp".into());
    PathBuf::from(home).join(".config/breeze-autoscroll")
}

/// Synchronizes `~/.config/autostart/breeze-autoscroll.desktop` with the `autostart` toggle.
pub fn sync_kde_autostart(enabled: bool) {
    let Some(home) = env::var_os("HOME") else {
        return;
    };
    let autostart_dir = PathBuf::from(home).join(".config/autostart");
    let desktop_file = autostart_dir.join("breeze-autoscroll.desktop");

    if enabled {
        let _ = fs::create_dir_all(&autostart_dir);
        let exec_path = env::current_exe()
            .ok()
            .and_then(|p| p.to_str().map(str::to_string))
            .unwrap_or_else(|| "breeze-autoscroll".to_string());
        let content = format!(
            "#!/usr/bin/env xdg-open\n\
             [Desktop Entry]\n\
             Name=Breeze Autoscroll\n\
             GenericName=Mausrad-Klick Autoscroll\n\
             Comment=Windows-Mittelklick Autoscroll im Kubuntu 26.04 Breeze-Stil\n\
             Exec={exec_path}\n\
             Icon=preferences-desktop-mouse\n\
             Terminal=false\n\
             Type=Application\n\
             Categories=Utility;Settings;Qt;KDE;\n\
             StartupNotify=false\n\
             X-KDE-autostart-phase=2\n\
             X-GNOME-Autostart-enabled=true\n"
        );
        let _ = fs::write(&desktop_file, content);
    } else if desktop_file.exists() {
        let _ = fs::remove_file(&desktop_file);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
        let mgr = ConfigManager::new(Some(dir.clone()));
        let cfg = mgr.snapshot();

        assert!(cfg.enabled);
        assert_eq!(cfg.mode, "smart");
        assert_eq!(cfg.deadzone, 12);
        assert!((cfg.scroll_speed - 1.0).abs() < 1e-5);
        assert!(cfg.allow_horizontal);
        assert_eq!(cfg.theme, "dark");
        assert!(cfg.protect_3d_drag);
        assert!(cfg.auto_exclude_3d_apps);
        assert!(cfg.excluded_apps_and_titles.iter().any(|s| s == "onshape"));
        assert!(
            cfg.excluded_apps_and_titles
                .iter()
                .any(|s| s == "bambu studio")
        );
        assert!(mgr.config_path().exists());

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

        let mgr = ConfigManager::new(Some(dir.clone()));
        let cfg = mgr.snapshot();

        assert_eq!(cfg.deadzone, 14);
        assert!((cfg.scroll_speed - 0.72).abs() < 1e-5);
        assert!(!cfg.allow_horizontal);
        assert!(!cfg.autostart);
        assert!(cfg.protect_3d_drag);
        assert!(cfg.auto_exclude_3d_apps);
        assert!((cfg.drag_cancel_threshold_px - 5.0).abs() < 1e-5);
        assert_eq!(cfg.hold_delay_ms, 180);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn saves_reloads_and_resets_defaults() {
        let dir = temp_config_dir("save_reload");
        let mgr = ConfigManager::new(Some(dir.clone()));
        mgr.update(true, |cfg| {
            cfg.scroll_speed = 2.5;
            cfg.deadzone = 20;
            cfg.mode = "drag".to_string();
            cfg.set_excluded_from_csv("onshape, bambu studio, customcad");
        });

        let reloaded = ConfigManager::new(Some(dir.clone()));
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
}
