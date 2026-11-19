//! Verified updates using GitHub's release tag and uploaded asset digest.
mod archive;
mod client;
mod install;
mod release;

use crate::domain::{ErrorCode, Result, ServiceError};
use fs2::FileExt;
use semver::Version;
use std::fs;

pub(crate) use install::cleanup_old_executables;
pub const TARGET: &str = env!("ANTICRUMB_BUILD_TARGET");
const LATEST_RELEASE_URL: &str = "https://api.github.com/repos/ivere27/anticrumb/releases/latest";
const NIGHTLY_RELEASE_URL: &str =
    "https://api.github.com/repos/ivere27/anticrumb/releases/tags/nightly";
const REPOSITORY: &str = "ivere27/anticrumb";
const MAX_RELEASE_METADATA: u64 = 1024 * 1024;
const MAX_ARCHIVE: u64 = 256 * 1024 * 1024;
const MAX_UNPACKED: u64 = 512 * 1024 * 1024;
const MAX_BINARY: u64 = 192 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UpdateChannel {
    Stable,
    Nightly,
}

impl UpdateChannel {
    fn metadata_url(self) -> &'static str {
        match self {
            Self::Stable => LATEST_RELEASE_URL,
            Self::Nightly => NIGHTLY_RELEASE_URL,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct UpdateOptions {
    check: bool,
    channel: UpdateChannel,
    force: bool,
}

enum ParsedArgs {
    Help,
    Run(UpdateOptions),
}

fn parse_args(args: &[String]) -> Result<ParsedArgs> {
    let mut options = UpdateOptions {
        check: false,
        channel: UpdateChannel::Stable,
        force: false,
    };
    for arg in args {
        match arg.as_str() {
            "--help" | "-h" | "help" => return Ok(ParsedArgs::Help),
            "--check" => options.check = true,
            "--nightly" => options.channel = UpdateChannel::Nightly,
            "--force" => options.force = true,
            _ => {
                return Err(failure(
                    "update",
                    "usage: anticrumb update [--check] [--nightly] [--force]",
                ));
            }
        }
    }
    Ok(ParsedArgs::Run(options))
}

#[derive(Debug)]
enum UpdatePlan<'a> {
    UpToDate(String),
    CheckAvailable(String),
    Install {
        asset: &'a release::Asset,
        downloading_label: String,
        updated_from: String,
        updated_to: String,
    },
}

fn plan_update<'a>(
    options: UpdateOptions,
    current: &Version,
    current_commit: Option<&str>,
    release: &'a release::Release,
    target: &str,
) -> Result<UpdatePlan<'a>> {
    match options.channel {
        UpdateChannel::Stable => {
            let (version, asset) = release.validate(target)?;
            let newer = version.cmp_precedence(current).is_gt();
            if !newer && !options.force {
                return Ok(UpdatePlan::UpToDate(format!(
                    "anticrumb {current} is up to date (published: {version})."
                )));
            }
            if options.check {
                let message = if newer {
                    format!(
                        "Update available: anticrumb {current} → {version} ({target}). Run anticrumb update to install."
                    )
                } else {
                    format!(
                        "Reinstall available (--force): anticrumb {current} → {version} ({target}). Run anticrumb update --force to install."
                    )
                };
                return Ok(UpdatePlan::CheckAvailable(message));
            }
            Ok(UpdatePlan::Install {
                asset,
                downloading_label: version.to_string(),
                updated_from: current.to_string(),
                updated_to: version.to_string(),
            })
        }
        UpdateChannel::Nightly => {
            let (nightly_commit, asset) = release.validate_nightly(target)?;
            let normalized_local = current_commit.and_then(release::normalize_hex_sha);
            let current_label = match current_commit {
                Some(commit) => format!(
                    "{current} ({})",
                    normalized_local.as_deref().unwrap_or(commit)
                ),
                None => current.to_string(),
            };
            let target_label = match &nightly_commit {
                Some(commit) => format!("nightly ({commit})"),
                None => "nightly".to_string(),
            };
            let same_commit = matches!(
                (normalized_local.as_deref(), nightly_commit.as_deref()),
                (Some(local), Some(remote)) if local.eq_ignore_ascii_case(remote)
            );
            if same_commit && !options.force {
                return Ok(UpdatePlan::UpToDate(format!(
                    "anticrumb {current_label} is up to date with {target_label}."
                )));
            }
            if options.check {
                let message = if !same_commit {
                    format!(
                        "Update available: anticrumb {current_label} → {target_label} ({target}). Run anticrumb update --nightly to install."
                    )
                } else {
                    format!(
                        "Reinstall available (--force): anticrumb {current_label} → {target_label} ({target}). Run anticrumb update --nightly --force to install."
                    )
                };
                return Ok(UpdatePlan::CheckAvailable(message));
            }
            Ok(UpdatePlan::Install {
                asset,
                downloading_label: target_label.clone(),
                updated_from: current_label,
                updated_to: target_label,
            })
        }
    }
}

fn failure(stage: &str, message: impl Into<String>) -> ServiceError {
    ServiceError::new(ErrorCode::InvalidRequest, stage, message)
}

fn io_failure(stage: &str, error: impl std::fmt::Display) -> ServiceError {
    ServiceError::new(ErrorCode::StorageFailure, stage, error.to_string())
}

fn executable_name(target: &str) -> &'static str {
    if target.contains("windows") {
        "anticrumb.exe"
    } else {
        "anticrumb"
    }
}

pub fn run(args: &[String]) -> Result<()> {
    let options = match parse_args(args)? {
        ParsedArgs::Help => {
            println!(
                "anticrumb update [--check] [--nightly] [--force]\n\
                 Install the latest verified release for this executable's build target.\n\
                 --check    check for updates without installing\n\
                 --nightly  use the rolling nightly pre-release channel\n\
                 --force    install even if the published build matches the current version or commit\n\
                 Existing services keep running; restart them when their work is finished."
            );
            return Ok(());
        }
        ParsedArgs::Run(options) => options,
    };
    let client = client::network_client()?;
    let release = release::parse_release(&client::fetch(
        &client,
        options.channel.metadata_url(),
        MAX_RELEASE_METADATA,
    )?)?;
    let current = Version::parse(env!("CARGO_PKG_VERSION"))
        .map_err(|_| failure("update", "current build has an invalid version"))?;
    let current_commit = crate::cli::commit_head();
    let (asset, downloading_label, updated_from, updated_to) = match plan_update(
        options,
        &current,
        current_commit.as_deref(),
        &release,
        TARGET,
    )? {
        UpdatePlan::UpToDate(message) | UpdatePlan::CheckAvailable(message) => {
            println!("{message}");
            return Ok(());
        }
        UpdatePlan::Install {
            asset,
            downloading_label,
            updated_from,
            updated_to,
        } => (asset, downloading_label, updated_from, updated_to),
    };
    let executable =
        std::env::current_exe().map_err(|error| io_failure("update_install", error))?;
    let parent = executable
        .parent()
        .ok_or_else(|| failure("update_install", "cannot locate executable directory"))?;
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(parent.join(".anticrumb-update.lock"))
        .map_err(|error| {
            io_failure(
                "update_install",
                format!("cannot write executable directory: {error}"),
            )
        })?;
    lock.try_lock_exclusive().map_err(|_| {
        failure(
            "update_install",
            "another update is already using this installation",
        )
    })?;
    let stage = install::Stage::create(parent)?;
    eprintln!("Downloading anticrumb {downloading_label} for {TARGET}…");
    let bytes = client::fetch(&client, &asset.browser_download_url, asset.size)?;
    archive::verify_archive(&bytes, asset)?;
    archive::extract_archive(&bytes, TARGET, &stage.0)?;
    let retained = install::install_with(
        &stage.0.join(executable_name(TARGET)),
        &executable,
        install::replace_file,
    )?;
    println!("Updated anticrumb {updated_from} → {updated_to}.");
    println!("Existing workspace and daemon services keep their current version. Finish active work, then restart them to use the update.");
    for backup in retained {
        println!(
            "Previous running file retained at {}; remove it after the old processes exit.",
            backup.display()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::archive::*;
    use super::client::*;
    use super::install::*;
    use super::release::*;
    use super::*;
    use crate::domain::digest;
    use std::io::{self, Cursor, Write};

    const LINUX: &str = "x86_64-unknown-linux-gnu";
    const WINDOWS: &str = "x86_64-pc-windows-msvc";

    fn release_value(version: &str, target: &str) -> serde_json::Value {
        let suffix = if target.contains("windows") {
            "zip"
        } else {
            "tar.gz"
        };
        let name = format!("anticrumb-{target}.{suffix}");
        serde_json::json!({
            "tag_name":format!("v{version}"),"draft":false,"prerelease":false,
            "id":123,"body":"release notes","html_url":format!("https://github.com/{REPOSITORY}/releases/tag/v{version}"),
            "author":{"login":"fixture-publisher"},
            "assets":[{
                "name":name,"browser_download_url":format!("https://github.com/{REPOSITORY}/releases/download/v{version}/{name}"),
                "size":3,"digest":format!("sha256:{}",digest(b"abc")),"state":"uploaded",
                "id":456,"content_type":"application/octet-stream","download_count":7
            }]
        })
    }
    fn release(version: &str, target: &str) -> Release {
        parse_release(&serde_json::to_vec(&release_value(version, target)).unwrap()).unwrap()
    }

    fn nightly_release_value(commit: &str, target: &str) -> serde_json::Value {
        let suffix = if target.contains("windows") {
            "zip"
        } else {
            "tar.gz"
        };
        let name = format!("anticrumb-{target}.{suffix}");
        serde_json::json!({
            "tag_name": "nightly",
            "target_commitish": commit,
            "name": format!("anticrumb nightly ({})", &commit[..7.min(commit.len())]),
            "draft": false,
            "prerelease": true,
            "assets": [{
                "name": name,
                "browser_download_url": format!("https://github.com/{REPOSITORY}/releases/download/nightly/{name}"),
                "size": 3,
                "digest": format!("sha256:{}", digest(b"abc")),
                "state": "uploaded"
            }]
        })
    }

    fn gzip(bytes: &[u8]) -> Vec<u8> {
        let mut compressed = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        compressed.write_all(bytes).unwrap();
        compressed.finish().unwrap()
    }

    fn archive(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut builder = tar::Builder::new(Vec::new());
        for (name, bytes) in files {
            let mut header = tar::Header::new_gnu();
            header.set_mode(0o755);
            header.set_size(bytes.len() as u64);
            header.set_cksum();
            builder.append_data(&mut header, name, *bytes).unwrap();
        }
        gzip(&builder.into_inner().unwrap())
    }

    #[test]
    fn github_release_accepts_typical_extra_api_fields_and_exact_target_assets() {
        let release = release("1.2.3", LINUX);
        let (version, asset) = release.validate(LINUX).unwrap();
        assert_eq!(version.to_string(), "1.2.3");
        assert_eq!(asset.sha256().unwrap(), digest(b"abc"));
        assert!(release.validate(WINDOWS).is_err());
    }

    #[test]
    fn github_release_rejects_missing_duplicate_or_unuploaded_assets() {
        for value in [
            {
                let mut value = release_value("1.2.3", LINUX);
                value["assets"] = serde_json::json!([]);
                value
            },
            {
                let mut value = release_value("1.2.3", LINUX);
                let asset = value["assets"][0].clone();
                value["assets"].as_array_mut().unwrap().push(asset);
                value
            },
            {
                let mut value = release_value("1.2.3", LINUX);
                value["assets"][0]["state"] = serde_json::json!("starter");
                value
            },
            {
                let mut value = release_value("1.2.3", LINUX);
                value["assets"][0]["size"] = serde_json::json!(0);
                value
            },
            {
                let mut value = release_value("1.2.3", LINUX);
                value["assets"][0]["size"] = serde_json::json!(MAX_ARCHIVE + 1);
                value
            },
        ] {
            assert!(parse_release(&serde_json::to_vec(&value).unwrap())
                .unwrap()
                .validate(LINUX)
                .is_err());
        }
        for field in ["tag_name", "draft", "prerelease", "assets"] {
            let mut value = release_value("1.2.3", LINUX);
            value.as_object_mut().unwrap().remove(field);
            assert!(parse_release(&serde_json::to_vec(&value).unwrap()).is_err());
        }
    }

    #[test]
    fn github_digest_must_be_present_and_exact_sha256() {
        let mut missing = release_value("1.2.3", LINUX);
        missing["assets"][0]
            .as_object_mut()
            .unwrap()
            .remove("digest");
        assert!(parse_release(&serde_json::to_vec(&missing).unwrap())
            .unwrap()
            .validate(LINUX)
            .is_err());
        for digest in [
            serde_json::Value::Null,
            serde_json::json!(""),
            serde_json::json!("sha256:abc"),
            serde_json::json!(format!("sha512:{}", "a".repeat(64))),
            serde_json::json!(format!("sha256:{}", "g".repeat(64))),
            serde_json::json!("https://example.invalid/checksum"),
        ] {
            let mut value = release_value("1.2.3", LINUX);
            value["assets"][0]["digest"] = digest;
            assert!(parse_release(&serde_json::to_vec(&value).unwrap())
                .unwrap()
                .validate(LINUX)
                .is_err());
        }
    }

    #[test]
    fn github_asset_url_must_match_the_repository_tag_and_exact_target() {
        for url in [
            "http://github.com/ivere27/anticrumb/releases/download/v1.2.3/a.tar.gz",
            "https://github.com/another/repository/releases/download/v1.2.3/a.tar.gz",
            "https://example.invalid/release.tar.gz",
            "https://github.com/ivere27/anticrumb/releases/download/v9.9.9/anticrumb-x86_64-unknown-linux-gnu.tar.gz",
            "https://github.com/ivere27/anticrumb/releases/download/v1.2.3/anticrumb-x86_64-unknown-linux-gnu.tar.gz?token=fixture-secret",
        ] {
            let mut value = release_value("1.2.3", LINUX);
            value["assets"][0]["browser_download_url"] = serde_json::json!(url);
            let release = parse_release(&serde_json::to_vec(&value).unwrap()).unwrap();
            let error = release.validate(LINUX).err().unwrap();
            assert!(!error.to_string().contains("fixture-secret"));
        }
    }

    #[test]
    fn github_http_errors_explain_missing_release_and_rate_limits_without_echoing_headers() {
        let mut headers = http::HeaderMap::new();
        let missing = http_failure_for_kind(
            http::StatusCode::NOT_FOUND,
            &headers,
            Some(ReleaseMetadataKind::Stable),
        );
        assert!(missing
            .message
            .contains("No public stable anticrumb release"));
        assert!(missing.message.contains("HTTP 404"));
        headers.insert("x-ratelimit-remaining", "0".parse().unwrap());
        headers.insert("retry-after", "60".parse().unwrap());
        let limited = http_failure_for_kind(
            http::StatusCode::FORBIDDEN,
            &headers,
            Some(ReleaseMetadataKind::Stable),
        );
        assert_eq!(limited.code, ErrorCode::RateLimited);
        assert!(limited.message.contains("60 seconds"));
        headers.insert("retry-after", "fixture-secret".parse().unwrap());
        let limited = http_failure_for_kind(
            http::StatusCode::TOO_MANY_REQUESTS,
            &headers,
            Some(ReleaseMetadataKind::Stable),
        );
        assert_eq!(limited.code, ErrorCode::RateLimited);
        assert!(!limited.message.contains("fixture-secret"));
    }

    #[test]
    fn update_redirects_enforce_https_and_trusted_github_hosts() {
        let valid = crate::url_lite::Url::parse(
            "https://objects.githubusercontent.com/github-production-release-asset/123",
        )
        .unwrap();
        assert!(validate_https_url(&valid, 1).is_ok());
        assert!(validate_https_url(&valid, 5).is_ok());
        assert!(validate_https_url(&valid, 6).is_err());

        for bad in [
            "http://github.com/ivere27/anticrumb",
            "https://evil.invalid/release.tar.gz",
            "https://user:pass@github.com/ivere27/anticrumb",
        ] {
            let parsed = crate::url_lite::Url::parse(bad).unwrap();
            assert!(validate_https_url(&parsed, 1).is_err(), "{bad}");
        }
    }

    #[test]
    fn release_selection_uses_stable_semver_precedence_without_downgrades() {
        let current = Version::parse("1.2.3").unwrap();
        for (candidate, newer) in [
            ("1.2.4", true),
            ("2.0.0", true),
            ("1.2.3", false),
            ("1.2.2", false),
        ] {
            let candidate = release(candidate, LINUX);
            let (version, _) = candidate.validate(LINUX).unwrap();
            assert_eq!(version.cmp_precedence(&current).is_gt(), newer);
        }
        for tag in [
            "1.2.3",
            "v1.2",
            "v01.2.3",
            "not-a-version",
            "v1.2.3-rc.1",
            "v1.2.3+rebuilt",
        ] {
            let mut candidate = release("1.2.3", LINUX);
            candidate.tag_name = tag.into();
            assert!(candidate.validate(LINUX).is_err());
        }
        for field in ["draft", "prerelease"] {
            let mut value = release_value("1.2.3", LINUX);
            value[field] = serde_json::json!(true);
            assert!(parse_release(&serde_json::to_vec(&value).unwrap())
                .unwrap()
                .validate(LINUX)
                .is_err());
        }
    }

    #[test]
    fn archive_checksum_and_size_must_match_before_extraction() {
        let release = release("1.2.3", LINUX);
        let (_, asset) = release.validate(LINUX).unwrap();
        assert!(verify_archive(b"abc", asset).is_ok());
        assert!(verify_archive(b"abd", asset).is_err());
        assert!(verify_archive(b"ab", asset).is_err());
    }

    #[test]
    fn tar_extraction_selects_only_the_executable_and_rejects_links_duplicates_and_size_bombs() {
        let root = format!("anticrumb-{LINUX}");
        let executable = format!("{root}/anticrumb");
        let library = format!("{root}/libanticrumb.so");
        let bytes = archive(&[
            (&executable, b"new binary"),
            (&library, b"new library"),
            (&format!("{root}/docs/guide.md"), b"guide"),
        ]);
        let directory = tempfile::tempdir().unwrap();
        extract_archive(&bytes, LINUX, directory.path()).unwrap();
        assert_eq!(
            fs::read(directory.path().join("anticrumb")).unwrap(),
            b"new binary"
        );
        assert!(!directory.path().join("libanticrumb.so").exists());
        assert!(!directory.path().join("docs").exists());

        let directory = tempfile::tempdir().unwrap();
        let duplicate = archive(&[(&executable, b"one"), (&executable, b"two")]);
        assert!(extract_archive(&duplicate, LINUX, directory.path()).is_err());

        let mut builder = tar::Builder::new(Vec::new());
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Symlink);
        header.set_size(0);
        header.set_link_name("../../outside").unwrap();
        header.set_cksum();
        builder
            .append_data(&mut header, &executable, io::empty())
            .unwrap();
        let directory = tempfile::tempdir().unwrap();
        assert!(extract_archive(
            &gzip(&builder.into_inner().unwrap()),
            LINUX,
            directory.path()
        )
        .is_err());

        let mut header = tar::Header::new_gnu();
        header.set_path(&executable).unwrap();
        header.set_size(MAX_UNPACKED + 1);
        header.set_cksum();
        let mut bomb = header.as_bytes().to_vec();
        bomb.extend_from_slice(&[0; 1024]);
        assert!(extract_archive(&gzip(&bomb), LINUX, directory.path()).is_err());
    }

    #[test]
    fn gzip_trailer_corruption_or_truncation_is_rejected_after_tar_end_marker() {
        let valid = archive(&[(&format!("anticrumb-{LINUX}/anticrumb"), b"new executable")]);
        let mut corrupt = valid.clone();
        let footer = corrupt.len() - 8;
        corrupt[footer] ^= 0xff;
        for invalid in [corrupt, valid[..valid.len() - 4].to_vec()] {
            let directory = tempfile::tempdir().unwrap();
            assert!(extract_archive(&invalid, LINUX, directory.path()).is_err());
        }
    }

    #[test]
    fn zip_crc_is_checked_for_documentation_that_is_not_installed() {
        let mut builder = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        builder
            .start_file(format!("anticrumb-{WINDOWS}/anticrumb.exe"), options)
            .unwrap();
        builder.write_all(b"new executable").unwrap();
        builder
            .start_file(format!("anticrumb-{WINDOWS}/docs/guide.md"), options)
            .unwrap();
        let documentation = b"unique documentation CRC payload";
        builder.write_all(documentation).unwrap();
        let mut bytes = builder.finish().unwrap().into_inner();
        let directory = tempfile::tempdir().unwrap();
        extract_archive(&bytes, WINDOWS, directory.path()).unwrap();
        let offset = bytes
            .windows(documentation.len())
            .position(|window| window == documentation)
            .unwrap();
        bytes[offset] ^= 0xff;
        let directory = tempfile::tempdir().unwrap();
        assert!(extract_archive(&bytes, WINDOWS, directory.path()).is_err());
    }

    #[test]
    fn zip_extraction_rejects_traversal_and_wrong_archive_roots() {
        let root = format!("anticrumb-{WINDOWS}");
        for path in [
            "../outside.exe".to_owned(),
            format!("{root}/../outside.exe"),
            "another-target/anticrumb.exe".to_owned(),
            format!("{root}/C:\\outside.exe"),
        ] {
            let mut builder = zip::ZipWriter::new(Cursor::new(Vec::new()));
            builder
                .start_file(&path, zip::write::SimpleFileOptions::default())
                .unwrap();
            builder.write_all(b"not an executable").unwrap();
            let bytes = builder.finish().unwrap().into_inner();
            let directory = tempfile::tempdir().unwrap();
            assert!(
                extract_archive(&bytes, WINDOWS, directory.path()).is_err(),
                "{path}"
            );
            assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 0);
        }
        for path in ["/absolute", "root/./file", "root/../file", "root\\file"] {
            assert!(archive_path(path, "root").is_err());
        }
    }

    #[test]
    fn successful_install_replaces_only_the_executable_and_preserves_leftover_libraries() {
        let directory = tempfile::tempdir().unwrap();
        let executable = directory.path().join("anticrumb");
        let library = directory.path().join("libanticrumb.so");
        fs::write(&executable, "old executable").unwrap();
        fs::write(&library, "leftover library").unwrap();
        let stage = Stage::create(directory.path()).unwrap();
        let package = archive(&[(&format!("anticrumb-{LINUX}/anticrumb"), b"new executable")]);
        extract_archive(&package, LINUX, &stage.0).unwrap();
        let retained = install_with(&stage.0.join("anticrumb"), &executable, replace_file).unwrap();
        assert_eq!(fs::read_to_string(executable).unwrap(), "new executable");
        assert_eq!(fs::read_to_string(library).unwrap(), "leftover library");
        #[cfg(not(windows))]
        assert!(retained.is_empty());
        #[cfg(windows)]
        assert!(retained.iter().all(|path| path.is_file()));
    }

    #[test]
    fn installation_failure_restores_the_executable_after_partial_replacement() {
        let directory = tempfile::tempdir().unwrap();
        let library = directory.path().join("libanticrumb.so");
        let executable = directory.path().join("executable");
        let staged = directory.path().join("new-executable");
        fs::write(&library, "leftover library").unwrap();
        fs::write(&executable, "old executable").unwrap();
        fs::write(&staged, "new executable").unwrap();
        let result = install_with(&staged, &executable, |_, destination| {
            fs::write(destination, "partial replacement")?;
            Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "simulated install failure",
            ))
        });
        assert!(result.is_err());
        assert_eq!(fs::read_to_string(executable).unwrap(), "old executable");
        assert_eq!(fs::read_to_string(library).unwrap(), "leftover library");
    }

    #[test]
    fn missing_staged_executable_never_modifies_the_existing_installation() {
        let directory = tempfile::tempdir().unwrap();
        let executable = directory.path().join("executable");
        fs::write(&executable, "old").unwrap();
        assert!(
            install_with(&directory.path().join("missing"), &executable, replace_file).is_err()
        );
        assert_eq!(fs::read_to_string(executable).unwrap(), "old");
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 1);
    }

    #[test]
    fn cleanup_old_executables_removes_only_stale_running_old_files() {
        let directory = tempfile::tempdir().unwrap();
        let old = directory.path().join(".anticrumb-running-1234.old");
        let exe = directory.path().join("anticrumb.exe");
        let other = directory.path().join("other.old");
        let lock = directory.path().join(".anticrumb-update.lock");
        fs::write(&old, "old running binary").unwrap();
        fs::write(&exe, "current binary").unwrap();
        fs::write(&other, "unrelated file").unwrap();
        fs::write(&lock, "").unwrap();
        cleanup_old_executables_in(directory.path());
        assert!(!old.exists());
        assert!(exe.exists());
        assert!(other.exists());
        assert!(lock.exists());
    }

    #[test]
    fn update_cli_flags_parse_check_nightly_force_and_help() {
        let to_args = |slice: &[&str]| slice.iter().map(|s| (*s).to_string()).collect::<Vec<_>>();
        match parse_args(&to_args(&[])).unwrap() {
            ParsedArgs::Run(o) => {
                assert_eq!(
                    o,
                    UpdateOptions {
                        check: false,
                        channel: UpdateChannel::Stable,
                        force: false
                    }
                );
            }
            ParsedArgs::Help => panic!("expected Run"),
        }
        match parse_args(&to_args(&["--force", "--nightly", "--check"])).unwrap() {
            ParsedArgs::Run(o) => {
                assert_eq!(
                    o,
                    UpdateOptions {
                        check: true,
                        channel: UpdateChannel::Nightly,
                        force: true
                    }
                );
            }
            ParsedArgs::Help => panic!("expected Run"),
        }
        assert!(matches!(
            parse_args(&to_args(&["--help"])).unwrap(),
            ParsedArgs::Help
        ));
        assert!(parse_args(&to_args(&["--unknown"])).is_err());
    }

    #[test]
    fn nightly_release_validation_requires_prerelease_nightly_tag_and_trusted_asset() {
        let full_sha = "1234567890abcdef1234567890abcdef12345678";
        let valid =
            parse_release(&serde_json::to_vec(&nightly_release_value(full_sha, LINUX)).unwrap())
                .unwrap();
        let (commit, asset) = valid.validate_nightly(LINUX).unwrap();
        assert_eq!(commit.as_deref(), Some("1234567"));
        assert_eq!(asset.sha256().unwrap(), digest(b"abc"));

        let mut from_title = nightly_release_value("master", LINUX);
        from_title["name"] = serde_json::json!("anticrumb nightly (AbCdEf1)");
        let parsed = parse_release(&serde_json::to_vec(&from_title).unwrap()).unwrap();
        assert_eq!(
            parsed.validate_nightly(LINUX).unwrap().0.as_deref(),
            Some("abcdef1")
        );

        let mut bad_tag = nightly_release_value(full_sha, LINUX);
        bad_tag["tag_name"] = serde_json::json!("v1.2.3");
        assert!(parse_release(&serde_json::to_vec(&bad_tag).unwrap())
            .unwrap()
            .validate_nightly(LINUX)
            .is_err());

        let mut stable_flag = nightly_release_value(full_sha, LINUX);
        stable_flag["prerelease"] = serde_json::json!(false);
        assert!(parse_release(&serde_json::to_vec(&stable_flag).unwrap())
            .unwrap()
            .validate_nightly(LINUX)
            .is_err());

        let mut draft_flag = nightly_release_value(full_sha, LINUX);
        draft_flag["draft"] = serde_json::json!(true);
        assert!(parse_release(&serde_json::to_vec(&draft_flag).unwrap())
            .unwrap()
            .validate_nightly(LINUX)
            .is_err());

        let mut bad_url = nightly_release_value(full_sha, LINUX);
        bad_url["assets"][0]["browser_download_url"] =
            serde_json::json!("https://github.com/ivere27/anticrumb/releases/download/v1.0.0/anticrumb-x86_64-unknown-linux-gnu.tar.gz");
        assert!(parse_release(&serde_json::to_vec(&bad_url).unwrap())
            .unwrap()
            .validate_nightly(LINUX)
            .is_err());
    }

    #[test]
    fn nightly_and_force_update_planning_handles_commits_dirty_builds_and_checks() {
        let current = Version::parse("0.8.3").unwrap();
        let nightly = parse_release(
            &serde_json::to_vec(&nightly_release_value(
                "abcdef1234567890abcdef1234567890abcdef12",
                LINUX,
            ))
            .unwrap(),
        )
        .unwrap();

        let nightly_opts = UpdateOptions {
            check: false,
            channel: UpdateChannel::Nightly,
            force: false,
        };
        let nightly_check = UpdateOptions {
            check: true,
            ..nightly_opts
        };

        // Matching commit (7-char, 40-char, or uppercase hex) -> up to date
        for matching in [
            "abcdef1",
            "ABCDEF1",
            "ABCDEF1234567890ABCDEF1234567890ABCDEF12",
            "abcdef1234567890abcdef1234567890abcdef12",
        ] {
            match plan_update(nightly_opts, &current, Some(matching), &nightly, LINUX).unwrap() {
                UpdatePlan::UpToDate(msg) => {
                    assert!(msg.contains("0.8.3 (abcdef1)"), "{msg}");
                    assert!(msg.contains("up to date with nightly (abcdef1)"), "{msg}");
                }
                other => panic!("expected UpToDate for {matching}, got {other:?}"),
            }
        }

        // Different commit -> update available / install
        match plan_update(nightly_check, &current, Some("3d9d844"), &nightly, LINUX).unwrap() {
            UpdatePlan::CheckAvailable(msg) => {
                assert!(msg.contains("0.8.3 (3d9d844) → nightly (abcdef1)"));
                assert!(msg.contains("anticrumb update --nightly"));
            }
            other => panic!("expected CheckAvailable, got {other:?}"),
        }

        // Dirty local build or missing commit -> updates to clean nightly
        for commit in [Some("abcdef1-dirty"), None] {
            match plan_update(nightly_opts, &current, commit, &nightly, LINUX).unwrap() {
                UpdatePlan::Install { updated_to, .. } => {
                    assert_eq!(updated_to, "nightly (abcdef1)")
                }
                other => panic!("expected Install, got {other:?}"),
            }
        }

        // Forced nightly on same commit -> reinstalls
        let forced_nightly = UpdateOptions {
            force: true,
            ..nightly_opts
        };
        assert!(matches!(
            plan_update(forced_nightly, &current, Some("abcdef1"), &nightly, LINUX).unwrap(),
            UpdatePlan::Install { .. }
        ));

        // Forced stable on same version -> reinstalls (e.g. reverting from nightly to stable)
        let stable = release("0.8.3", LINUX);
        let stable_opts = UpdateOptions {
            check: false,
            channel: UpdateChannel::Stable,
            force: false,
        };
        assert!(matches!(
            plan_update(stable_opts, &current, Some("9999999"), &stable, LINUX).unwrap(),
            UpdatePlan::UpToDate(_)
        ));
        let forced_stable = UpdateOptions {
            force: true,
            ..stable_opts
        };
        assert!(matches!(
            plan_update(forced_stable, &current, Some("9999999"), &stable, LINUX).unwrap(),
            UpdatePlan::Install { .. }
        ));
    }

    #[test]
    fn nightly_404_error_explains_missing_nightly_release() {
        let headers = http::HeaderMap::new();
        let err = http_failure_for_kind(
            http::StatusCode::NOT_FOUND,
            &headers,
            Some(ReleaseMetadataKind::Nightly),
        );
        assert!(err.message.contains("No public nightly anticrumb release"));
        assert!(err.message.contains("HTTP 404"));
    }
}
