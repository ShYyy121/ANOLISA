//! Owned-path validation and nonblocking installation locks.

use crate::{require, Result};
use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    os::{
        fd::AsRawFd,
        unix::fs::{MetadataExt, OpenOptionsExt},
    },
    path::{Component, Path},
};

pub(crate) fn absolute(path: &Path) -> Result<()> {
    require(
        path.is_absolute() && !path.components().any(|p| matches!(p, Component::ParentDir)),
        format!("expected absolute path without '..': {}", path.display()),
    )?;
    for parent in path.ancestors() {
        require(
            !parent.is_symlink(),
            format!("symlink in installation path: {}", parent.display()),
        )?;
    }
    Ok(())
}

pub(crate) fn relative(path: &str) -> Result<()> {
    let parts: Vec<_> = path.split('/').collect();
    require(
        matches!(parts.first(), Some(&"bin" | &"libexec" | &"share"))
            && parts
                .iter()
                .all(|p| !p.is_empty() && *p != "." && *p != ".."),
        format!("invalid payload path: {path}"),
    )
}

pub(crate) fn owned_directory(path: &Path) -> Result<()> {
    absolute(path)?;
    let metadata = path.metadata()?;
    // SAFETY: geteuid reads the current effective identity without side effects.
    require(
        metadata.is_dir()
            && metadata.uid() == unsafe { libc::geteuid() }
            && metadata.mode() & 0o022 == 0,
        format!(
            "directory must be owned and not group/world writable: {}",
            path.display()
        ),
    )
}

pub(crate) fn parent(path: &Path) -> Result<()> {
    absolute(path)?;
    owned_directory(
        path.parent()
            .ok_or_else(|| crate::Error::Invalid("path has no parent".into()))?,
    )
}

pub(crate) fn create(path: &Path, mode: u32) -> Result<File> {
    Ok(OpenOptions::new()
        .write(true)
        .create_new(true)
        .custom_flags(libc::O_NOFOLLOW)
        .mode(mode)
        .open(path)?)
}

pub(crate) fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = create(path, 0o600)?;
    file.write_all(bytes)?;
    Ok(())
}

pub(crate) fn lock(prefix: &Path) -> Result<File> {
    let path = prefix.join(".aw-install.lock");
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .custom_flags(libc::O_NOFOLLOW)
        .mode(0o600)
        .open(&path)?;
    // SAFETY: the file descriptor is owned and remains open throughout flock.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let held = file.metadata()?;
    let current = fs::symlink_metadata(path)?;
    require(
        current.is_file() && held.ino() == current.ino() && held.dev() == current.dev(),
        "installation lock changed; retry with the current prefix",
    )?;
    Ok(file)
}
