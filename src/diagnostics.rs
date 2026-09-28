//! Small local trace for correlating lidup actions with WindowServer wake logs.

use std::fs::OpenOptions;
use std::io::Write;
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

static LOG_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

pub fn record(message: &str) {
    let Ok(_guard) = LOG_LOCK.get_or_init(|| Mutex::new(())).lock() else {
        return;
    };
    let Some(home) = dirs::home_dir() else {
        return;
    };
    let folder = home.join("Library/Logs");
    if std::fs::create_dir_all(&folder).is_err() {
        return;
    }
    let Ok(mut file) = OpenOptions::new()
        .create(true)
        .append(true)
        .open(folder.join("lidup.log"))
    else {
        return;
    };
    if file.metadata().is_ok_and(|meta| meta.len() > 128 * 1024) {
        let _ = file.set_len(0);
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let _ = writeln!(
        file,
        "{}.{:03} {message}",
        now.as_secs(),
        now.subsec_millis()
    );
}
