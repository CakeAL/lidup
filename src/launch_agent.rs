//! Manage "launch at login" via a per-user macOS LaunchAgent.
//!
//! lidup installs a LaunchAgent that runs the current binary at login. The agent
//! label is fixed (`com.lidup.app`) and the program path is the current
//! executable, so it works whether lidup is run standalone or inside an app
//! bundle. Installing/uninstalling is done with `launchctl` so the change takes
//! effect immediately (and persists across logout/login).

use std::path::PathBuf;

/// The fixed label for lidup's LaunchAgent.
pub const LABEL: &str = "com.lidup.app";

fn agent_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("Library")
        .join("LaunchAgents")
}

pub fn agent_plist_path() -> PathBuf {
    agent_dir().join(format!("{LABEL}.plist"))
}

/// The current executable path (resolves symlinks, e.g. inside an .app bundle).
fn current_exe() -> PathBuf {
    std::env::current_exe().unwrap_or_else(|_| PathBuf::from("lidup"))
}

/// True if a LaunchAgent for lbl is currently loaded (installed and enabled).
pub fn is_installed() -> bool {
    let plist = agent_plist_path();
    if !plist.exists() {
        return false;
    }
    // Also confirm it is actually loaded (not just present on disk).
    let output = std::process::Command::new("/bin/launchctl")
        .args(["list", LABEL])
        .output();
    matches!(output, Ok(o) if o.status.success())
}

/// Install (or refresh) the "launch at login" agent so the current binary runs at
/// login. Returns an error message on failure.
pub fn install() -> Result<(), String> {
    let exe = current_exe();
    let pid_plist = agent_plist_path();
    let contents = plist_xml(&exe);
    std::fs::create_dir_all(pid_plist.parent().unwrap()).map_err(|e| e.to_string())?;
    std::fs::write(&pid_plist, contents).map_err(|e| e.to_string())?;

    // Unload any prior version first (ignore failure if not loaded), then load.
    let _ = std::process::Command::new("/bin/launchctl")
        .args(["unload", pid_plist.to_str().unwrap()])
        .output();
    let status = std::process::Command::new("/bin/launchctl")
        .args(["load", pid_plist.to_str().unwrap()])
        .status()
        .map_err(|e| e.to_string())?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("launchctl load failed with {status}"))
    }
}

/// Remove the launch-at-login agent. Returns an error message on failure.
pub fn uninstall() -> Result<(), String> {
    let plist = agent_plist_path();
    if plist.exists() {
        let _ = std::process::Command::new("/bin/launchctl")
            .args(["unload", plist.to_str().unwrap()])
            .output();
        std::fs::remove_file(&plist).map_err(|e| e.to_string())?;
    }
    Ok(())
}

fn plist_xml(exe: &PathBuf) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>{LABEL}</string>
  <key>ProgramArguments</key>
  <array><string>{}</string></array>
  <key>RunAtLoad</key><true/>
</dict>
</plist>
"#,
        exe.display()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plist_points_at_the_given_executable() {
        let p = plist_xml(&PathBuf::from(
            "/Applications/lidup.app/Contents/MacOS/lidup",
        ));
        assert!(p.contains("com.lidup.app"));
        assert!(p.contains("/Applications/lidup.app/Contents/MacOS/lidup"));
        assert!(p.contains("<key>RunAtLoad</key><true/>"));
    }
}
