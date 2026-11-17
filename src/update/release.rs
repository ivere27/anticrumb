use super::{failure, MAX_ARCHIVE, REPOSITORY};
use crate::domain::Result;
use semver::Version;
use serde::Deserialize;

// GitHub adds API fields over time; only the fields needed for validation are read.
#[derive(Deserialize)]
pub(super) struct Release {
    pub(super) tag_name: String,
    draft: bool,
    prerelease: bool,
    assets: Vec<Asset>,
}

#[derive(Deserialize)]
pub(super) struct Asset {
    name: String,
    pub(super) browser_download_url: String,
    pub(super) size: u64,
    digest: Option<String>,
    state: String,
}

pub(super) fn parse_release(bytes: &[u8]) -> Result<Release> {
    serde_json::from_slice(bytes)
        .map_err(|_| failure("update_release", "invalid GitHub release metadata"))
}

impl Release {
    pub(super) fn validate(&self, target: &str) -> Result<(Version, &Asset)> {
        let version = self
            .tag_name
            .strip_prefix('v')
            .and_then(|tag| Version::parse(tag).ok())
            .ok_or_else(|| {
                failure(
                    "update_release",
                    "latest release tag must use vMAJOR.MINOR.PATCH",
                )
            })?;
        if self.draft
            || self.prerelease
            || !version.pre.is_empty()
            || !version.build.is_empty()
            || self.tag_name != format!("v{version}")
        {
            return Err(failure(
                "update_release",
                "latest release must be published and stable without prerelease or build metadata",
            ));
        }
        let suffix = if target.contains("windows") {
            "zip"
        } else {
            "tar.gz"
        };
        let name = format!("anticrumb-{target}.{suffix}");
        let mut matching = self.assets.iter().filter(|asset| asset.name == name);
        let asset = matching.next().ok_or_else(|| {
            failure(
                "update_release",
                format!("no published release asset for build target {target}"),
            )
        })?;
        if matching.next().is_some() {
            return Err(failure(
                "update_release",
                "GitHub release contains duplicate assets for this build target",
            ));
        }
        let url = format!(
            "https://github.com/{REPOSITORY}/releases/download/{}/{name}",
            self.tag_name
        );
        if asset.browser_download_url != url {
            return Err(failure(
                "update_release",
                "release asset URL does not match the trusted repository, release tag, and build target",
            ));
        }
        if asset.state != "uploaded" || asset.size == 0 || asset.size > MAX_ARCHIVE {
            return Err(failure(
                "update_release",
                "release asset is not fully uploaded or has an invalid size",
            ));
        }
        asset.sha256()?;
        Ok((version, asset))
    }
}

impl Asset {
    pub(super) fn sha256(&self) -> Result<&str> {
        self.digest
            .as_deref()
            .and_then(|digest| digest.strip_prefix("sha256:"))
            .filter(|hash| hash.len() == 64 && hash.bytes().all(|byte| byte.is_ascii_hexdigit()))
            .ok_or_else(|| {
                failure(
                    "update_release",
                    "GitHub release asset is missing a valid SHA-256 digest; refusing to install an unverified download",
                )
            })
    }
}
