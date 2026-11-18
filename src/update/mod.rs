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
const REPOSITORY: &str = "ivere27/anticrumb";
const MAX_RELEASE_METADATA: u64 = 1024 * 1024;
const MAX_ARCHIVE: u64 = 256 * 1024 * 1024;
const MAX_UNPACKED: u64 = 512 * 1024 * 1024;
const MAX_BINARY: u64 = 192 * 1024 * 1024;

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
    if matches!(
        args.first().map(String::as_str),
        Some("--help" | "-h" | "help")
    ) {
        println!("anticrumb update [--check]\nInstall the latest verified release for this executable's build target.\n--check checks the version without installing. Existing services keep running; restart them when their work is finished.");
        return Ok(());
    }
    let check = match args {
        [] => false,
        [flag] if flag == "--check" => true,
        _ => return Err(failure("update", "usage: anticrumb update [--check]")),
    };
    let client = client::network_client()?;
    let release = release::parse_release(&client::fetch(
        &client,
        LATEST_RELEASE_URL,
        MAX_RELEASE_METADATA,
    )?)?;
    let (version, asset) = release.validate(TARGET)?;
    let current = Version::parse(env!("CARGO_PKG_VERSION"))
        .map_err(|_| failure("update", "current build has an invalid version"))?;
    if !version.cmp_precedence(&current).is_gt() {
        println!("anticrumb {current} is up to date (published: {version}).");
        return Ok(());
    }
    if check {
        println!("Update available: anticrumb {current} → {version} ({TARGET}). Run anticrumb update to install.");
        return Ok(());
    }
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
    eprintln!("Downloading anticrumb {version} for {TARGET}…");
    let bytes = client::fetch(&client, &asset.browser_download_url, asset.size)?;
    archive::verify_archive(&bytes, asset)?;
    archive::extract_archive(&bytes, TARGET, &stage.0)?;
    let retained = install::install_with(
        &stage.0.join(executable_name(TARGET)),
        &executable,
        install::replace_file,
    )?;
    println!("Updated anticrumb {current} → {version}.");
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
        let missing = http_failure(http::StatusCode::NOT_FOUND, &headers, true);
        assert!(missing
            .message
            .contains("No public stable anticrumb release"));
        assert!(missing.message.contains("HTTP 404"));
        headers.insert("x-ratelimit-remaining", "0".parse().unwrap());
        headers.insert("retry-after", "60".parse().unwrap());
        let limited = http_failure(http::StatusCode::FORBIDDEN, &headers, true);
        assert_eq!(limited.code, ErrorCode::RateLimited);
        assert!(limited.message.contains("60 seconds"));
        headers.insert("retry-after", "fixture-secret".parse().unwrap());
        let limited = http_failure(http::StatusCode::TOO_MANY_REQUESTS, &headers, true);
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
}
