//! Build metadata and release-channel display formatting.
//!
//! `package_version()` stays a pure `X.Y.Z` SemVer string from `Cargo.toml` for
//! daemon protocol compatibility, release tag validation, and SemVer comparisons,
//! while `display_version()` appends `-nightly` or `-dev` for non-stable builds.

use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuildChannel {
    Stable,
    Nightly,
    Dev,
}

impl BuildChannel {
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "" | "dev" => Some(Self::Dev),
            "nightly" => Some(Self::Nightly),
            "stable" => Some(Self::Stable),
            _ => None,
        }
    }

    pub fn current() -> Self {
        option_env!("ANTICRUMB_BUILD_CHANNEL")
            .and_then(Self::parse)
            .unwrap_or(Self::Dev)
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Stable => "stable",
            Self::Nightly => "nightly",
            Self::Dev => "dev",
        }
    }

    pub const fn version_suffix(self) -> &'static str {
        match self {
            Self::Stable => "",
            Self::Nightly => "-nightly",
            Self::Dev => "-dev",
        }
    }
}

pub const fn package_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

pub fn format_display_version(base_version: &str, channel: BuildChannel) -> String {
    format!("{base_version}{}", channel.version_suffix())
}

pub fn display_version() -> String {
    format_display_version(package_version(), BuildChannel::current())
}

pub const fn build_target() -> &'static str {
    env!("ANTICRUMB_BUILD_TARGET")
}

pub const fn build_time() -> &'static str {
    match option_env!("ANTICRUMB_BUILD_TIME") {
        Some(time) => time,
        None => "unknown",
    }
}

pub(crate) fn normalize_commit_head(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed == "unknown" {
        return None;
    }
    let (hash, dirty) = match trimmed.strip_suffix("-dirty") {
        Some(prefix) => (prefix, true),
        None => (trimmed, false),
    };
    if hash.len() < 7 || !hash.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let short: String = hash.chars().take(7).collect();
    if dirty {
        Some(format!("{short}-dirty"))
    } else {
        Some(short)
    }
}

pub fn commit_head() -> Option<String> {
    if let Some(normalized) = option_env!("ANTICRUMB_GIT_HEAD").and_then(normalize_commit_head) {
        return Some(normalized);
    }
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let dot_git = manifest_dir.join(".git");
    let git_dir = if dot_git.is_dir() {
        dot_git
    } else if dot_git.is_file() {
        let content = std::fs::read_to_string(&dot_git).ok()?;
        let raw = content.trim().strip_prefix("gitdir:")?.trim();
        let path = Path::new(raw);
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            manifest_dir.join(path)
        }
    } else {
        return None;
    };
    let head = std::fs::read_to_string(git_dir.join("HEAD")).ok()?;
    let trimmed = head.trim();
    let full_sha = if let Some(reference) = trimmed.strip_prefix("ref: ") {
        let reference = reference.trim();
        let ref_path = git_dir.join(reference);
        if let Ok(sha) = std::fs::read_to_string(ref_path) {
            sha.trim().to_string()
        } else {
            let packed = std::fs::read_to_string(git_dir.join("packed-refs")).ok()?;
            packed
                .lines()
                .filter(|line| !line.starts_with('#') && !line.starts_with('^'))
                .find_map(|line| {
                    let (sha, name) = line.split_once(' ')?;
                    (name.trim() == reference).then(|| sha.trim().to_string())
                })?
        }
    } else {
        trimmed.to_string()
    };
    let short: String = full_sha.chars().take(7).collect();
    (short.len() == 7 && short.chars().all(|c| c.is_ascii_hexdigit())).then_some(short)
}

pub fn format_help_banner(version: &str, commit: Option<&str>, built: &str) -> String {
    let commit = commit.unwrap_or("unknown");
    format!("anticrumb {version} (commit: {commit}, built: {built})")
}

pub fn help_banner() -> String {
    format_help_banner(&display_version(), commit_head().as_deref(), build_time())
}

pub fn format_cli_version(version: &str, commit: Option<&str>, target: &str) -> String {
    match commit {
        Some(head) => format!("anticrumb {version} ({head}, {target})"),
        None => format!("anticrumb {version} ({target})"),
    }
}

pub fn cli_version() -> String {
    format_cli_version(&display_version(), commit_head().as_deref(), build_target())
}

pub fn format_client_version(version: &str, commit: Option<&str>) -> String {
    let commit = commit.unwrap_or("unknown");
    format!("anticrumb {version} ({commit})")
}

pub fn client_version() -> String {
    format_client_version(&display_version(), commit_head().as_deref())
}

pub fn format_tui_title(version: &str, commit: Option<&str>) -> String {
    match commit {
        Some(head) => format!("anticrumb {version} ({head})"),
        None => format!("anticrumb {version}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_channel_parses_known_values_and_rejects_unknown() {
        assert_eq!(BuildChannel::parse(""), Some(BuildChannel::Dev));
        assert_eq!(BuildChannel::parse("  DEV \n"), Some(BuildChannel::Dev));
        assert_eq!(BuildChannel::parse("Nightly"), Some(BuildChannel::Nightly));
        assert_eq!(BuildChannel::parse("stable"), Some(BuildChannel::Stable));
        assert_eq!(BuildChannel::parse("beta"), None);
        assert_eq!(BuildChannel::parse("release"), None);
    }

    #[test]
    fn display_version_formats_each_channel_consistently() {
        assert_eq!(
            format_display_version("0.8.3", BuildChannel::Stable),
            "0.8.3"
        );
        assert_eq!(
            format_display_version("0.8.3", BuildChannel::Nightly),
            "0.8.3-nightly"
        );
        assert_eq!(
            format_display_version("0.8.3", BuildChannel::Dev),
            "0.8.3-dev"
        );
        assert_eq!(
            display_version(),
            format_display_version(package_version(), BuildChannel::current())
        );
    }

    #[test]
    fn banners_and_version_strings_reflect_channel_version_commit_and_build_time() {
        assert_eq!(
            format_help_banner("0.8.3-nightly", Some("3d9d844"), "2026-11-19 04:54:37 UTC"),
            "anticrumb 0.8.3-nightly (commit: 3d9d844, built: 2026-11-19 04:54:37 UTC)"
        );
        assert_eq!(
            format_cli_version(
                "0.8.3-dev",
                Some("3d9d844-dirty"),
                "x86_64-unknown-linux-gnu"
            ),
            "anticrumb 0.8.3-dev (3d9d844-dirty, x86_64-unknown-linux-gnu)"
        );
        assert_eq!(
            format_cli_version("0.8.3", None, "x86_64-unknown-linux-gnu"),
            "anticrumb 0.8.3 (x86_64-unknown-linux-gnu)"
        );
        assert_eq!(
            format_client_version("0.8.3-nightly", Some("3d9d844")),
            "anticrumb 0.8.3-nightly (3d9d844)"
        );
        assert_eq!(
            format_tui_title("0.8.3-nightly", Some("3d9d844")),
            "anticrumb 0.8.3-nightly (3d9d844)"
        );
        assert_eq!(format_tui_title("0.8.3", None), "anticrumb 0.8.3");

        let banner = help_banner();
        assert!(banner.starts_with(&format!("anticrumb {} (commit: ", display_version())));
        assert!(banner.contains("built: "));
        assert!(cli_version().starts_with(&format!("anticrumb {} ", display_version())));
        assert!(client_version().starts_with(&format!("anticrumb {} ", display_version())));
    }

    #[test]
    fn normalize_commit_head_preserves_dirty_suffix_and_rejects_invalid() {
        assert_eq!(normalize_commit_head("abc1234"), Some("abc1234".into()));
        assert_eq!(
            normalize_commit_head("abc1234def5678"),
            Some("abc1234".into())
        );
        assert_eq!(
            normalize_commit_head(" abc1234-dirty \n"),
            Some("abc1234-dirty".into())
        );
        assert_eq!(
            normalize_commit_head("abc1234def5678-dirty"),
            Some("abc1234-dirty".into())
        );
        assert_eq!(normalize_commit_head("unknown"), None);
        assert_eq!(normalize_commit_head(""), None);
        assert_eq!(normalize_commit_head("-dirty"), None);
        assert_eq!(normalize_commit_head("123456-dirty"), None);
        assert_eq!(normalize_commit_head("not_hex-dirty"), None);
    }
}
