//! Persistent settings for lidup.

use serde::{Deserialize, Serialize};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

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
    /// When auto mode is active, light the built-in above this hinge angle.
    /// `None` keeps the original external-monitor-only auto rule.
    #[serde(default)]
    pub open_above_angle: Option<u16>,
    /// True if lidup is registered to launch at login through SMAppService.
    pub launch_at_login: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            bound_key: None,
            restore_builtin: true,
            builtin_id: None,
            builtin_key: None,
            open_above_angle: None,
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

    /// UI snapshots can use defaults; the worker surfaces load errors explicitly.
    pub fn load() -> Self {
        Self::load_checked().unwrap_or_default()
    }

    pub fn load_checked() -> Result<Self, String> {
        Self::load_from(&Self::path())
    }

    fn load_from(path: &Path) -> Result<Self, String> {
        match std::fs::read_to_string(path) {
            Ok(text) => {
                serde_json::from_str(&text).map_err(|error| format!("配置格式错误：{error}"))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(format!("无法读取配置：{error}")),
        }
    }

    pub fn save(&self) -> Result<(), String> {
        let text = serde_json::to_string_pretty(self).map_err(|error| error.to_string())?;
        write_atomic(&Self::path(), text.as_bytes()).map_err(|error| error.to_string())
    }
}

/// Write beside the destination and rename only after the full file is synced.
/// A failed write leaves the previous config or exported report intact.
pub(crate) fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)?;
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let temp = parent.join(format!(
        ".lidup-{}-{nonce}-{}.tmp",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temp)?;
    let result = (|| {
        file.write_all(bytes)?;
        file.sync_all()?;
        std::fs::rename(&temp, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::{write_atomic, Settings};

    #[test]
    fn corrupt_config_is_reported_and_preserved() {
        let root = std::env::temp_dir().join(format!("lidup-config-test-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("config.json");
        write_atomic(&path, b"broken-json").unwrap();
        assert!(Settings::load_from(&path).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"broken-json");
        write_atomic(
            &path,
            serde_json::to_string(&Settings::default())
                .unwrap()
                .as_bytes(),
        )
        .unwrap();
        assert!(Settings::load_from(&path).is_ok());
        // A failed replacement must not leave temporary files behind.
        assert!(write_atomic(&root, b"cannot replace a directory").is_err());
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 1);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn existing_settings_keep_angle_control_disabled() {
        let old = r#"{"bound_key":"external","restore_builtin":true,"builtin_id":1,"builtin_key":"built-in","launch_at_login":true}"#;
        let settings: Settings = serde_json::from_str(old).unwrap();
        assert_eq!(settings.open_above_angle, None);
    }
}
