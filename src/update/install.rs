use super::{failure, io_failure};
use crate::domain::Result;
use std::{
    fs, io,
    path::{Path, PathBuf},
};

pub(super) struct Stage(pub(super) PathBuf);

impl Stage {
    pub(super) fn create(parent: &Path) -> Result<Self> {
        let path = parent.join(format!(
            ".anticrumb-update-{}",
            uuid::Uuid::new_v4().simple()
        ));
        fs::create_dir(&path).map_err(|error| {
            io_failure(
                "update_install",
                format!(
                    "cannot stage update beside the executable: {error}; install into a directory you can write"
                ),
            )
        })?;
        let stage = Self(path);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&stage.0, fs::Permissions::from_mode(0o700))
                .map_err(|error| io_failure("update_install", error))?;
        }
        Ok(stage)
    }
}

impl Drop for Stage {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn backup_file(destination: &Path) -> Result<PathBuf> {
    let metadata =
        fs::symlink_metadata(destination).map_err(|error| io_failure("update_install", error))?;
    if !metadata.is_file() {
        return Err(failure(
            "update_install",
            "update destinations must be regular files, not symbolic links",
        ));
    }
    let backup = destination.with_file_name(format!(
        ".anticrumb-backup-{}",
        uuid::Uuid::new_v4().simple()
    ));
    let result = (|| {
        let mut input = fs::File::open(destination)?;
        let mut output = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&backup)?;
        io::copy(&mut input, &mut output)?;
        output.set_permissions(metadata.permissions())?;
        output.sync_all()
    })();
    if let Err(error) = result {
        let _ = fs::remove_file(&backup);
        return Err(io_failure("update_install", error));
    }
    Ok(backup)
}

pub(super) fn replace_file(staged: &Path, destination: &Path) -> io::Result<Option<PathBuf>> {
    #[cfg(not(windows))]
    {
        fs::rename(staged, destination)?;
        Ok(None)
    }
    #[cfg(windows)]
    {
        // A running Windows image cannot be overwritten. Move it aside without
        // deleting it; if the new image cannot be installed, restore that path.
        let old = destination.with_file_name(format!(
            ".anticrumb-running-{}.old",
            uuid::Uuid::new_v4().simple()
        ));
        fs::rename(destination, &old)?;
        if let Err(error) = fs::rename(staged, destination) {
            fs::rename(&old, destination).map_err(|restore| {
                io::Error::other(format!(
                    "install failed ({error}) and restore failed ({restore}); original retained at {}",
                    old.display()
                ))
            })?;
            return Err(error);
        }
        Ok(Some(old))
    }
}

pub(super) fn install_with(
    staged: &Path,
    destination: &Path,
    mut replace: impl FnMut(&Path, &Path) -> io::Result<Option<PathBuf>>,
) -> Result<Vec<PathBuf>> {
    if !staged.is_file() {
        return Err(failure("update_install", "staged executable is missing"));
    }
    let permissions = fs::metadata(destination)
        .map_err(|error| io_failure("update_install", error))?
        .permissions();
    fs::set_permissions(staged, permissions)
        .map_err(|error| io_failure("update_install", error))?;
    let backup = backup_file(destination)?;
    let mut retained = Vec::new();
    match replace(staged, destination) {
        Ok(Some(old)) => retained.push(old),
        Ok(None) => (),
        Err(error) => {
            let restored = if destination.exists() {
                replace_file(&backup, destination)
            } else {
                fs::rename(&backup, destination).map(|_| None)
            };
            let detail = match restored {
                Ok(Some(old)) => {
                    retained.push(old);
                    "previous executable restored".to_owned()
                }
                Ok(None) => "previous executable restored".to_owned(),
                Err(restore) => format!(
                    "restore failed ({restore}); original backup retained at {}",
                    backup.display()
                ),
            };
            let recovery = if retained.is_empty() {
                String::new()
            } else {
                format!(
                    "; recovery files retained at {}",
                    retained
                        .iter()
                        .map(|path| path.display().to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            };
            return Err(io_failure(
                "update_install",
                format!("cannot install release: {error}; {detail}{recovery}"),
            ));
        }
    }
    if fs::remove_file(&backup).is_err() {
        retained.push(backup);
    }
    Ok(retained)
}

pub(super) fn cleanup_old_executables_in(directory: &Path) {
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if name.starts_with(".anticrumb-running-") && name.ends_with(".old") {
            let path = entry.path();
            if fs::symlink_metadata(&path).is_ok_and(|metadata| metadata.is_file()) {
                let _ = fs::remove_file(path);
            }
        }
    }
}

pub(crate) fn cleanup_old_executables() {
    if let Ok(executable) = std::env::current_exe() {
        if let Some(parent) = executable.parent() {
            cleanup_old_executables_in(parent);
        }
    }
}
