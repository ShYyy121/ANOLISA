//! Transactional owned-file installation; existing components and user data are preserved.

use crate::{filesystem as paths, inspect, require, Component, Result};
use std::{
    collections::BTreeMap,
    fs::{self, File, Permissions},
    io::Write,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};

pub(crate) fn receipts(prefix: &Path) -> Result<BTreeMap<String, Component>> {
    let directory = prefix.join(".aw-packages");
    paths::absolute(&directory)?;
    let mut components = BTreeMap::new();
    if !directory.exists() {
        return Ok(components);
    }
    for entry in fs::read_dir(directory)? {
        let path = entry?.path();
        if path
            .extension()
            .is_some_and(|extension| extension == "json")
        {
            paths::absolute(&path)?;
            let value: Component = serde_json::from_reader(File::open(&path)?)?;
            require(
                matches!(value.component.as_str(), "aw-core" | "aw-provider-sec-core")
                    && path
                        .file_stem()
                        .is_some_and(|stem| stem == value.component.as_str())
                    && !components.contains_key(&value.component),
                "invalid package receipt",
            )?;
            components.insert(value.component.clone(), value);
        }
    }
    Ok(components)
}

struct Transaction {
    files: Vec<PathBuf>,
    directories: Vec<PathBuf>,
    prefix: Option<PathBuf>,
    committed: bool,
}

impl Transaction {
    fn directory(&mut self, path: &Path) -> Result<()> {
        if path.exists() {
            return Ok(());
        }
        let parent = path
            .parent()
            .ok_or_else(|| crate::Error::Invalid("missing parent".into()))?;
        self.directory(parent)?;
        fs::create_dir(path)?;
        self.directories.push(path.into());
        fs::set_permissions(path, Permissions::from_mode(0o755))?;
        Ok(())
    }

    fn create(&mut self, path: &Path) -> Result<File> {
        self.directory(
            path.parent()
                .ok_or_else(|| crate::Error::Invalid("missing parent".into()))?,
        )?;
        let file = paths::create(path, 0o600)?;
        // Register before opening a source or writing a receipt can fail.
        self.files.push(path.into());
        Ok(file)
    }
}

impl Drop for Transaction {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        let mut complete = true;
        for path in self.files.iter().rev() {
            if let Err(error) = fs::remove_file(path) {
                eprintln!(
                    "aw-package: rollback could not remove {}: {error}",
                    path.display()
                );
                complete = false;
            }
        }
        for path in self.directories.iter().rev() {
            if let Err(error) = fs::remove_dir(path) {
                eprintln!(
                    "aw-package: rollback could not remove {}: {error}",
                    path.display()
                );
                complete = false;
            }
        }
        if let Some(prefix) = self.prefix.as_ref().filter(|_| complete) {
            if let Err(error) = fs::remove_file(prefix.join(".aw-install.lock"))
                .and_then(|()| fs::remove_dir(prefix))
            {
                eprintln!(
                    "aw-package: rollback could not retire {}: {error}",
                    prefix.display()
                );
            }
        }
    }
}

/// Install a verified bundle into a new prefix or add its matching Provider.
/// Never overwrites existing files; failures roll back only this invocation's writes.
pub fn install(bundle: &Path, prefix: &Path) -> Result<()> {
    install_using(bundle, prefix, |source, output| {
        std::io::copy(&mut File::open(source)?, output)?;
        Ok(())
    })
}

fn install_using(
    bundle: &Path,
    prefix: &Path,
    mut copy: impl FnMut(&Path, &mut File) -> Result<()>,
) -> Result<()> {
    let manifest = inspect(bundle)?;
    paths::parent(prefix)?;
    let created_prefix = !prefix.exists();
    if created_prefix {
        fs::create_dir(prefix)?;
        fs::set_permissions(prefix, Permissions::from_mode(0o755))?;
    }
    paths::owned_directory(prefix)?;
    // A concurrent first installer must not take the creator's lock before its
    // initial receipt directory exists. Otherwise both processes can fail.
    require(
        created_prefix || prefix.join(".aw-packages").exists(),
        "refusing a prefix without AW package receipts",
    )?;
    // Declared before the transaction so rollback finishes before unlocking.
    let _lock = paths::lock(prefix)?;
    let mut transaction = Transaction {
        files: vec![],
        directories: vec![],
        prefix: created_prefix.then(|| prefix.into()),
        committed: false,
    };
    let installed = receipts(prefix)?;
    require(
        created_prefix || !installed.is_empty(),
        "refusing a prefix without AW package receipts",
    )?;
    let mut available = installed.clone();
    for component in &manifest.components {
        require(
            !available.contains_key(&component.component),
            "component already installed; use a new prefix",
        )?;
        available.insert(component.component.clone(), component.clone());
    }
    for component in &manifest.components {
        for (name, version) in &component.requires {
            require(
                available.get(name).is_some_and(|required| {
                    required.version == *version
                        && required.source_commit == component.source_commit
                        && required.arch == component.arch
                }),
                format!("requires matching {name} from the same Preview build"),
            )?;
        }
        for name in component.files.keys() {
            let target = prefix.join(name);
            paths::absolute(&target)?;
            require(
                !target.exists(),
                format!("refusing to overwrite: {}", target.display()),
            )?;
        }
    }
    for component in &manifest.components {
        for (name, metadata) in &component.files {
            let target = prefix.join(name);
            let mut output = transaction.create(&target)?;
            copy(&bundle.join("payload").join(name), &mut output)?;
            require(
                crate::digest(&target)? == metadata.sha256,
                format!("installed payload checksum mismatch: {name}"),
            )?;
            output.set_permissions(Permissions::from_mode(metadata.mode))?;
        }
        let target = prefix
            .join(".aw-packages")
            .join(format!("{}.json", component.component));
        let mut output = transaction.create(&target)?;
        output.write_all(&serde_json::to_vec_pretty(component)?)?;
        output.set_permissions(Permissions::from_mode(0o644))?;
    }
    transaction.committed = true;
    Ok(())
}

/// Remove unchanged package-owned files after preflighting the entire installation.
/// Preserves the prefix, lock, unknown files and all external configuration/state.
pub fn uninstall(prefix: &Path) -> Result<()> {
    paths::owned_directory(prefix)?;
    let _lock = paths::lock(prefix)?;
    let installed = receipts(prefix)?;
    require(!installed.is_empty(), "no AW package receipts")?;
    let mut owned = Vec::new();
    for component in installed.values() {
        for (name, metadata) in &component.files {
            paths::relative(name)?;
            let path = prefix.join(name);
            paths::absolute(&path)?;
            require(
                path.is_file() && crate::digest(&path)? == metadata.sha256,
                format!("installed file modified or missing: {}", path.display()),
            )?;
            owned.push(path);
        }
    }
    for path in owned {
        fs::remove_file(path)?;
    }
    for name in installed.keys() {
        fs::remove_file(prefix.join(".aw-packages").join(format!("{name}.json")))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests;
