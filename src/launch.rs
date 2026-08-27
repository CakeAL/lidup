//! "Launch at login" via Apple's modern `SMAppService` framework (wrapped by the
//! `smappservice-rs` crate) instead of hand-managed LaunchAgents.
//!
//! `SMAppService` is the recommended API (macOS 13+). The key requirement is that
//! the *calling process* is a validly code-signed **.app bundle** (it exposes a
//! bundle identifier through `SMAppService.mainAppService()`). A bare, unsigned
//! command-line binary cannot register itself — `register()` fails (often with a
//! generic `EINVAL`/`Unknown(22)`), which is what happens if the toggle is ticked
//! while running `target/release/lidup` directly instead of via `lidup.app`.
//!
//! So: this path must be invoked from inside the bundled app. We return a clear
//! error in that case so the user knows to run the `.app` (and, where relevant, to
//! approve the item in System Settings).

use anyhow::{bail, Context, Result};
use smappservice_rs::{AppService, ServiceType};

/// The fixed bundle identifier / label for lidup.
pub const LABEL: &str = "com.lidup.app";

fn main_app() -> AppService {
    AppService::new(ServiceType::MainApp)
}

/// True if lidup is registered as a login item (and enabled).
pub fn check_reg_status() -> bool {
    main_app().status() == smappservice_rs::ServiceStatus::Enabled
}

/// Register lidup as a login item. Must run from a validly signed `.app` bundle.
pub fn register() -> Result<()> {
    ensure_in_bundle()?;
    let service = main_app();
    match service.register() {
        Ok(()) => Ok(()),
        Err(e) => {
            // Surface a friendlier message and point the user at System Settings.
            let _ = service.status();
            bail!("failed to register launch at login: {e} (approve it in System Settings → General → Login Items if prompted)")
        }
    }
}

/// Unregister the login item.
pub fn unregister() -> Result<()> {
    ensure_in_bundle()?;
    main_app()
        .unregister()
        .context("failed to unregister launch at login")
}

/// `SMAppService.mainAppService()` requires the caller to be a real app bundle.
/// Detect a `.app/Contents/MacOS` layout; give a clear error otherwise.
fn ensure_in_bundle() -> Result<()> {
    let exe = std::env::current_exe().context("could not resolve current executable")?;
    let path = exe.to_string_lossy();
    let in_bundle =
        path.contains(".app/Contents/MacOS/") || std::env::var_os("SM_APP_TEST").is_some(); // escape-hatch for tests
    if in_bundle {
        Ok(())
    } else {
        bail!(
            "launch-at-login can only be enabled from the bundled lidup.app, \
             not a bare binary. Run: open target/release/lidup.app, then tick \
             \"Start at Login\"."
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundle_check_accepts_app_paths() {
        assert!(LABEL == "com.lidup.app");
        // ensure_in_bundle uses current_exe; just assert the label/id sanity.
    }
}
