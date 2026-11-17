use super::{executable_name, failure, io_failure, release::Asset, MAX_BINARY, MAX_UNPACKED};
use crate::domain::{digest, Result};
use std::{
    collections::HashSet,
    fs,
    io::{self, Cursor, Read},
    path::Path,
};

pub(super) fn verify_archive(bytes: &[u8], asset: &Asset) -> Result<()> {
    if bytes.len() as u64 != asset.size {
        return Err(failure(
            "update_verify",
            "release archive size does not match GitHub release metadata",
        ));
    }
    if digest(bytes) != asset.sha256()?.to_ascii_lowercase() {
        return Err(failure(
            "update_verify",
            "release archive SHA256 does not match the GitHub asset digest; nothing was installed",
        ));
    }
    Ok(())
}

pub(super) fn archive_path(raw: &str, root: &str) -> Result<String> {
    let name = raw.trim_end_matches('/');
    if name.len() > 4096
        || name.contains(['\\', ':', '\0'])
        || name
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
        || !(name == root || name.starts_with(&format!("{root}/")))
    {
        return Err(failure(
            "update_archive",
            "release archive contains an unsafe path",
        ));
    }
    Ok(name.to_owned())
}

pub(super) fn extract_archive(bytes: &[u8], target: &str, stage: &Path) -> Result<()> {
    let root = format!("anticrumb-{target}");
    let executable = format!("{root}/{}", executable_name(target));
    let mut seen = HashSet::new();
    let mut total = 0u64;
    let mut consume =
        |name: &str, size: u64, directory: bool, reader: &mut dyn Read| -> Result<()> {
            let name = archive_path(name, &root)?;
            if !seen.insert(name.clone()) || seen.len() > 4096 {
                return Err(failure(
                    "update_archive",
                    "release archive contains duplicate or excessive entries",
                ));
            }
            total = total
                .checked_add(size)
                .ok_or_else(|| failure("update_archive", "archive size overflow"))?;
            if total > MAX_UNPACKED {
                return Err(failure(
                    "update_archive",
                    "release archive expands beyond its size limit",
                ));
            }
            if name != executable {
                // ZIP validates a member's CRC only when its payload is read.
                // Even documentation that is not installed must be validated.
                let copied = io::copy(&mut reader.take(size + 1), &mut io::sink())
                    .map_err(|error| io_failure("update_archive", error))?;
                if copied != size {
                    return Err(failure(
                        "update_archive",
                        "archive member size is inconsistent",
                    ));
                }
                return Ok(());
            }
            if directory || size == 0 || size > MAX_BINARY {
                return Err(failure(
                    "update_archive",
                    "release executable has an invalid type or size",
                ));
            }
            let path = stage.join(executable_name(target));
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(path)
                .map_err(|error| io_failure("update_archive", error))?;
            let copied = io::copy(&mut reader.take(size + 1), &mut file)
                .map_err(|error| io_failure("update_archive", error))?;
            if copied != size {
                return Err(failure(
                    "update_archive",
                    "archive member size is inconsistent",
                ));
            }
            file.sync_all()
                .map_err(|error| io_failure("update_archive", error))
        };
    if target.contains("windows") {
        let mut archive = zip::ZipArchive::new(Cursor::new(bytes))
            .map_err(|_| failure("update_archive", "invalid ZIP release archive"))?;
        for index in 0..archive.len() {
            let mut entry = archive
                .by_index(index)
                .map_err(|_| failure("update_archive", "invalid ZIP archive entry"))?;
            let kind = entry.unix_mode().unwrap_or(0) & 0o170000;
            if !matches!(kind, 0 | 0o100000 | 0o040000) {
                return Err(failure(
                    "update_archive",
                    "release archive must not contain links or special files",
                ));
            }
            let name = entry.name().to_owned();
            let size = entry.size();
            let directory = entry.is_dir() || kind == 0o040000;
            consume(&name, size, directory, &mut entry)?;
        }
    } else {
        // Bound the entire decoded stream, including TAR metadata and padding.
        let decoded = flate2::read::MultiGzDecoder::new(bytes).take(MAX_UNPACKED + 1);
        let mut archive = tar::Archive::new(decoded);
        for entry in archive
            .entries()
            .map_err(|_| failure("update_archive", "invalid TAR release archive"))?
        {
            let mut entry =
                entry.map_err(|_| failure("update_archive", "invalid TAR archive entry"))?;
            let kind = entry.header().entry_type();
            if !kind.is_file() && !kind.is_dir() {
                return Err(failure(
                    "update_archive",
                    "release archive must not contain links or special files",
                ));
            }
            let name = String::from_utf8(entry.path_bytes().into_owned())
                .map_err(|_| failure("update_archive", "archive paths must be UTF-8"))?;
            let size = entry.size();
            consume(&name, size, kind.is_dir(), &mut entry)?;
        }
        // TAR's end marker precedes the gzip checksum/footer. Drain to actual
        // EOF so a truncated stream or corrupt trailer cannot be installed.
        let mut decoded = archive.into_inner();
        io::copy(&mut decoded, &mut io::sink())
            .map_err(|error| io_failure("update_archive", error))?;
        if decoded.limit() == 0 {
            return Err(failure(
                "update_archive",
                "release archive expands beyond its size limit",
            ));
        }
    }
    if !seen.contains(&executable) {
        return Err(failure(
            "update_archive",
            "release archive is missing the executable for this build target",
        ));
    }
    Ok(())
}
