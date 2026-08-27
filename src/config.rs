//! Persistent settings for lidup.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Settings {
    /// Stable `vendor:model:serial` key of the external display that, when
    /// connected, auto-turns-off the built-in display. `None` disables auto-off.
    pub bound_key: Option<String>,
    /// Start the built-in display automatically whenever the bound display is
    /// absent (i.e. undo an auto-off). Defaults to true.
    pub restore_builtin: bool,
    /// Cached id of the built-in display. A powered-off built-in leaves the online
    /// list, so we keep its id around to bring it back.
    pub builtin_id: Option<u32>,
    /// Stable identity key of the built-in display, used to keep its menu entry
    /// stable (same key whether it is on or off) so the tray menu isn't rebuilt.
    pub builtin_key: Option<String>,
    /// True if lidup is set to launch at login (a LaunchAgent is installed).
    pub launch_at_login: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            bound_key: None,
            restore_builtin: true,
            builtin_id: None,
            builtin_key: None,
            launch_at_login: false,
        }
    }
}

impl Settings {
    /// The config file path. `LIDUP_CONFIG` overrides the default location, which
    /// is useful for relocating the file or for testing in restricted sandboxes.
    pub fn path() -> PathBuf {
        if let Some(p) = std::env::var_os("LIDUP_CONFIG") {
            return PathBuf::from(p);
        }
        dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("lidup")
            .join("config.json")
    }

    /// Load settings from disk; returns defaults (and does not error) if missing.
    pub fn load() -> Self {
        let path = Self::path();
        match std::fs::read_to_string(&path) {
            Ok(s) => serde_json::from_str(&s).unwrap_or_default(),
            Err(_) => Settings::default(),
        }
    }

    pub fn save(&self) -> Result<(), String> {
        let path = Self::path();
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        let s = serde_json::to_string_pretty(self).map_err(|e| e.to_string())?;
        std::fs::write(&path, s).map_err(|e| e.to_string())?;
        Ok(())
    }
}
