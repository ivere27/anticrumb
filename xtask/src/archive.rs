use flate2::read::GzDecoder;
use sha2::{Digest, Sha256};
use std::fs::File;
use std::io::{self, BufReader, Cursor, Read};
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchiveMember {
    pub name: String,
    pub directory: bool,
    pub size: u64,
    pub mode: u32,
}

pub fn sha256_bytes(data: &[u8]) -> String {
    Sha256::digest(data)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

pub fn sha256_file(path: &Path) -> Result<String, String> {
    let mut file = File::open(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file
            .read(&mut buf)
            .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

pub fn is_valid_sha256_digest(value: &str) -> bool {
    let Some(hex) = value.strip_prefix("sha256:") else {
        return false;
    };
    hex.len() == 64 && hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

pub fn archive_members(path: &Path) -> Result<Vec<ArchiveMember>, String> {
    let fname = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or_default();
    if fname.ends_with(".zip") {
        let file = File::open(path).map_err(|e| format!("cannot read package {fname}: {e}"))?;
        let mut archive = zip::ZipArchive::new(BufReader::new(file))
            .map_err(|e| format!("cannot read package {fname}: {e}"))?;
        let mut members = Vec::with_capacity(archive.len());
        for i in 0..archive.len() {
            let mut entry = archive
                .by_index(i)
                .map_err(|e| format!("cannot read package {fname}: {e}"))?;
            if entry.encrypted() {
                return Err(format!("{fname}: encrypted or special ZIP member"));
            }
            let mode = entry.unix_mode().unwrap_or(0);
            let kind = mode & 0o170000;
            if !matches!(kind, 0 | 0o100000 | 0o040000) || entry.is_symlink() {
                return Err(format!("{fname}: encrypted or special ZIP member"));
            }
            let directory = entry.is_dir();
            if kind == 0o040000 && !directory {
                return Err(format!("{fname}: invalid ZIP directory entry"));
            }
            let name = entry.name().to_string();
            let size = entry.size();
            io::copy(&mut entry, &mut io::sink()).map_err(|e| {
                format!("cannot read package {fname}: ZIP member checksum/CRC error: {e}")
            })?;
            members.push(ArchiveMember {
                name,
                directory,
                size,
                mode,
            });
        }
        Ok(members)
    } else {
        let file = File::open(path).map_err(|e| format!("cannot read package {fname}: {e}"))?;
        let gz = GzDecoder::new(BufReader::new(file));
        let mut archive = tar::Archive::new(gz);
        let mut members = Vec::new();
        {
            let entries = archive
                .entries()
                .map_err(|e| format!("cannot read package {fname}: {e}"))?;
            for entry_res in entries {
                let mut entry =
                    entry_res.map_err(|e| format!("cannot read package {fname}: {e}"))?;
                let etype = entry.header().entry_type();
                if !(etype.is_file() || etype.is_dir()) {
                    return Err(format!(
                        "{fname}: links and special TAR members are not allowed"
                    ));
                }
                let raw_name = entry.path_bytes();
                let name = std::str::from_utf8(&raw_name)
                    .map_err(|_| format!("{fname}: unsafe or unexpected archive path"))?;
                let directory = etype.is_dir();
                let size = entry.size();
                let mode = entry
                    .header()
                    .mode()
                    .map_err(|e| format!("cannot read package {fname}: {e}"))?;
                let name_owned = name.to_string();
                io::copy(&mut entry, &mut io::sink())
                    .map_err(|e| format!("cannot read package {fname}: {e}"))?;
                members.push(ArchiveMember {
                    name: name_owned,
                    directory,
                    size,
                    mode,
                });
            }
        }
        // Consume the gzip footer too; TAR's end marker can precede it.
        let mut gz = archive.into_inner();
        io::copy(&mut gz, &mut io::sink())
            .map_err(|e| format!("cannot read package {fname}: {e}"))?;
        Ok(members)
    }
}

pub fn extract_single_file(data: &[u8], suffix: &str, name: &str) -> Result<Vec<u8>, String> {
    let mut matches: Vec<Vec<u8>> = Vec::new();
    if suffix == "zip" {
        let mut archive = zip::ZipArchive::new(Cursor::new(data))
            .map_err(|e| format!("invalid release zip: {e}"))?;
        for i in 0..archive.len() {
            let mut file = archive
                .by_index(i)
                .map_err(|e| format!("read zip entry: {e}"))?;
            if file.is_file() && file.name().rsplit('/').next() == Some(name) {
                let mut buf = Vec::new();
                file.read_to_end(&mut buf)
                    .map_err(|e| format!("extract {name}: {e}"))?;
                matches.push(buf);
            }
        }
    } else {
        let gz = GzDecoder::new(Cursor::new(data));
        let mut archive = tar::Archive::new(gz);
        let entries = archive
            .entries()
            .map_err(|e| format!("invalid release tar.gz: {e}"))?;
        for entry_res in entries {
            let mut entry = entry_res.map_err(|e| format!("read tar entry: {e}"))?;
            if entry.header().entry_type().is_file() {
                let path = entry.path().map_err(|e| format!("tar path: {e}"))?;
                if path.file_name().and_then(|s| s.to_str()) == Some(name) {
                    let mut buf = Vec::new();
                    entry
                        .read_to_end(&mut buf)
                        .map_err(|e| format!("extract {name}: {e}"))?;
                    matches.push(buf);
                }
            }
        }
    }
    if matches.len() != 1 {
        return Err("Unexpected release archive".to_string());
    }
    Ok(matches.into_iter().next().unwrap())
}
