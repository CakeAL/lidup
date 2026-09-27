//! Check and install the published macOS app bundle from GitHub Releases.

use self_update::backends::github;
use semver::Version;
use std::process::Command;
use std::time::Duration;

const ASSET_NAME: &str = "lidup.zip";
const BUNDLE_NAME: &str = "lidup.app";

fn updater() -> github::UpdateBuilder {
    let mut builder = github::Update::configure();
    builder
        .repo_owner("CakeAL")
        .repo_name("lidup")
        .bin_name("lidup")
        .current_version(env!("CARGO_PKG_VERSION"))
        .timeout(Duration::from_secs(15))
        .unattended();
    builder
}

/// Return the tag of a newer published release that contains the expected app archive.
pub fn check_latest() -> Result<Option<String>, String> {
    let releases = updater()
        .build()
        .and_then(|updater| updater.get_latest_release())
        .map_err(|error| error.to_string())?;
    let release = releases
        .latest()
        .ok_or_else(|| "GitHub has no published release".to_string())?;
    available_release(release, env!("CARGO_PKG_VERSION"))
}

fn available_release(
    release: &self_update::Release,
    current: &str,
) -> Result<Option<String>, String> {
    let latest = Version::parse(release.version()).map_err(|error| error.to_string())?;
    let current = Version::parse(current).map_err(|error| error.to_string())?;
    if latest <= current {
        return Ok(None);
    }
    if !release
        .assets()
        .iter()
        .any(|asset| asset.name() == ASSET_NAME)
    {
        return Err(format!("release v{latest} has no {ASSET_NAME} asset"));
    }
    Ok(Some(format!("v{latest}")))
}

/// Download, verify and atomically replace the running `.app` bundle.
pub fn install(tag: &str) -> Result<String, String> {
    let version = tag
        .strip_prefix('v')
        .and_then(|version| Version::parse(version).ok())
        .ok_or_else(|| format!("invalid release tag: {tag}"))?;
    if version <= Version::parse(env!("CARGO_PKG_VERSION")).map_err(|error| error.to_string())? {
        return Err(format!("release {tag} is not newer than this app"));
    }

    let mut builder = updater();
    let expected_version = version.to_string();
    builder
        .release_tag(tag)
        .bundle_path_in_archive(BUNDLE_NAME)
        .asset_matcher(|assets| {
            assets
                .iter()
                .find(|asset| asset.name() == ASSET_NAME)
                .cloned()
        })
        .verify_release_digest(true)
        .check_install_path_writable(true)
        .verify_binary(move |bundle| {
            let plist = bundle.join("Contents/Info.plist");
            let output = Command::new("/usr/libexec/PlistBuddy")
                .args(["-c", "Print CFBundleShortVersionString"])
                .arg(&plist)
                .output()
                .map_err(|error| self_update::Error::verification_rejected(error.to_string()))?;
            if !output.status.success()
                || String::from_utf8_lossy(&output.stdout).trim() != expected_version
            {
                return Err(self_update::Error::verification_rejected(format!(
                    "downloaded app version does not match {expected_version}"
                )));
            }
            let status = Command::new("/usr/bin/codesign")
                .args(["--verify", "--deep", "--strict"])
                .arg(bundle)
                .status()
                .map_err(|error| self_update::Error::verification_rejected(error.to_string()))?;
            if status.success() {
                Ok(())
            } else {
                Err(self_update::Error::verification_rejected(
                    "downloaded app bundle failed code-signature verification",
                ))
            }
        });

    let status = builder
        .build()
        .and_then(|updater| updater.update())
        .map_err(|error| error.to_string())?;
    if status.is_updated() {
        Ok(tag.to_owned())
    } else {
        Err(format!("release {tag} was not installed"))
    }
}

#[cfg(test)]
mod tests {
    use super::{available_release, updater};
    use self_update::{Release, ReleaseAsset};

    fn release(version: &str, asset: &str) -> Release {
        Release::builder()
            .version(version)
            .asset(ReleaseAsset::new(asset, "https://example.test/download"))
            .build()
            .unwrap()
    }

    #[test]
    fn only_offers_newer_release_with_app_archive() {
        assert_eq!(
            available_release(&release("0.1.10", "lidup.zip"), "0.1.9").unwrap(),
            Some("v0.1.10".into())
        );
        assert_eq!(
            available_release(&release("0.1.9", "lidup.zip"), "0.1.9").unwrap(),
            None
        );
        assert!(available_release(&release("0.2.0", "source.zip"), "0.1.9").is_err());
    }

    #[test]
    fn update_check_builder_works_outside_app_bundle() {
        updater().build().unwrap();
    }
}
