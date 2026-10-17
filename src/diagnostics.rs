//! Bounded local logs and an explicit, user-selected diagnostic export.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

static LOG_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
static WAKE: AtomicU64 = AtomicU64::new(0);
const MAX_LOG_SIZE: u64 = 128 * 1024;

pub fn begin_wake() {
    WAKE.fetch_add(1, Ordering::Relaxed);
}

fn log_folder() -> Option<std::path::PathBuf> {
    dirs::home_dir().map(|home| home.join("Library/Logs"))
}

pub fn record(message: &str) {
    let Ok(_guard) = LOG_LOCK.get_or_init(|| Mutex::new(())).lock() else {
        return;
    };
    let Some(folder) = log_folder() else {
        return;
    };
    let _ = append(&folder, message);
}

fn append(folder: &Path, message: &str) -> io::Result<()> {
    fs::create_dir_all(folder)?;
    let path = folder.join("lidup.log");
    if fs::metadata(&path).is_ok_and(|meta| meta.len() >= MAX_LOG_SIZE) {
        fs::rename(&path, folder.join("lidup.previous.log"))?;
    }
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    writeln!(
        file,
        "{}.{:03} v={} pid={} wake={} {message}",
        now.as_secs(),
        now.subsec_millis(),
        env!("CARGO_PKG_VERSION"),
        std::process::id(),
        WAKE.load(Ordering::Relaxed)
    )
}

/// Uses cached display state. Export must not enumerate modes or read HDR/HID.
pub fn export(destination: &Path, context: &str) -> Result<(), String> {
    let folder = log_folder().ok_or_else(|| "找不到日志目录".to_string())?;
    let report = {
        let _guard = LOG_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .map_err(|error| error.to_string())?;
        collect_report(&folder, destination, context).map_err(|error| error.to_string())?
    };
    // A chosen destination may be on a slow disk. Never hold up worker logging
    // while writing or syncing the exported copy.
    crate::config::write_atomic(destination, report.as_bytes()).map_err(|error| error.to_string())
}

fn collect_report(folder: &Path, destination: &Path, context: &str) -> io::Result<String> {
    // Never replace a live log with its own exported report.
    if let (Some(parent), Some(name)) = (destination.parent(), destination.file_name()) {
        if ["lidup.log", "lidup.previous.log"]
            .iter()
            .any(|reserved| name == *reserved)
            && parent
                .canonicalize()
                .ok()
                .zip(folder.canonicalize().ok())
                .is_some_and(|(a, b)| a == b)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "请选择其他导出文件名",
            ));
        }
    }
    if let Ok(target) = destination.canonicalize() {
        for name in ["lidup.log", "lidup.previous.log"] {
            if folder
                .join(name)
                .canonicalize()
                .is_ok_and(|path| path == target)
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "请选择日志目录以外的导出文件",
                ));
            }
        }
    }
    let mut report = format!(
        "lidup diagnostic report\nversion={}\narchitecture={}\n{}\n",
        env!("CARGO_PKG_VERSION"),
        std::env::consts::ARCH,
        context
    );
    for name in ["lidup.previous.log", "lidup.log"] {
        report.push_str(&format!("\n--- {name} ---\n"));
        match fs::read_to_string(folder.join(name)) {
            Ok(text) => report.push_str(&text),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                report.push_str("(no log yet)\n")
            }
            Err(error) => return Err(error),
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rotation_and_export_preserve_previous_cycle_and_cached_context() {
        let root =
            std::env::temp_dir().join(format!("lidup-diagnostics-test-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let old = "x".repeat(MAX_LOG_SIZE as usize);
        fs::write(root.join("lidup.log"), &old).unwrap();
        append(&root, "power wake test").unwrap();
        assert_eq!(
            fs::read_to_string(root.join("lidup.previous.log")).unwrap(),
            old
        );
        let current = fs::read_to_string(root.join("lidup.log")).unwrap();
        assert!(current.contains("wake="));
        assert!(current.contains("pid="));
        let report_path = root.join("report.txt");
        let report = collect_report(&root, &report_path, "bound=external angle=90").unwrap();
        assert!(report.contains("bound=external angle=90"));
        assert!(report.contains(&old));
        assert!(report.contains("power wake test"));
        assert!(collect_report(&root, &root.join("lidup.log"), "").is_err());
        assert_eq!(fs::read_to_string(root.join("lidup.log")).unwrap(), current);
        fs::remove_dir_all(root).unwrap();
    }
}
