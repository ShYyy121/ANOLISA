//! Installer regression contracts, including source-open and rollback failures.

use super::*;
use crate::{digest, Component, Manifest, Payload};
use std::os::unix::fs::{symlink, MetadataExt};

struct Fixture {
    _directory: tempfile::TempDir,
    root: PathBuf,
    prefix: PathBuf,
    core: PathBuf,
    provider: PathBuf,
    combined: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let target = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/preview");
        fs::create_dir_all(&target).unwrap();
        let directory = tempfile::Builder::new()
            .prefix("install-test-")
            .tempdir_in(target)
            .unwrap();
        let root = directory.path().canonicalize().unwrap();
        fs::set_permissions(&root, Permissions::from_mode(0o700)).unwrap();
        let core = bundle(&root.join("core"), &["aw-core"]);
        let provider = bundle(&root.join("provider"), &["aw-provider-sec-core"]);
        let combined = bundle(&root.join("combined"), &["aw-core", "aw-provider-sec-core"]);
        Self {
            _directory: directory,
            prefix: root.join("installed"),
            root,
            core,
            provider,
            combined,
        }
    }
}
fn bundle(root: &Path, names: &[&str]) -> PathBuf {
    let mut components = Vec::new();
    for name in names {
        let entries = if *name == "aw-core" {
            vec!["bin/aw".into(), "bin/aw-package".into()]
        } else {
            ["aw-provider-sec-core", "agent-sec-cli", "agent-sec-daemon"]
                .iter()
                .map(|name| format!("libexec/aw/providers/sec-core/{name}"))
                .collect()
        };
        let mut files = BTreeMap::new();
        for entry in entries {
            let path = root.join("payload").join(&entry);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, format!("fixture {entry}")).unwrap();
            files.insert(
                entry,
                Payload {
                    sha256: digest(&path).unwrap(),
                    mode: 0o755,
                },
            );
        }
        components.push(Component {
            format: 1,
            component: (*name).into(),
            version: "0.1.0-preview.1".into(),
            source_commit: "a".repeat(40),
            os: "linux".into(),
            arch: std::env::consts::ARCH.into(),
            provider_protocol: "aw-provider/v1alpha1".into(),
            requires: if *name == "aw-core" {
                BTreeMap::new()
            } else {
                BTreeMap::from([("aw-core".into(), "0.1.0-preview.1".into())])
            },
            files,
        });
    }
    fs::write(
        root.join("manifest.json"),
        serde_json::to_vec(&Manifest {
            format: 1,
            components,
        })
        .unwrap(),
    )
    .unwrap();
    root.into()
}
fn modify_manifest(bundle: &Path, update: impl FnOnce(&mut Manifest)) {
    let path = bundle.join("manifest.json");
    let mut manifest: Manifest = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    update(&mut manifest);
    fs::write(path, serde_json::to_vec(&manifest).unwrap()).unwrap();
}
#[test]
fn split_matches_combined_and_reinstall_is_refused() {
    let f = Fixture::new();
    install(&f.core, &f.prefix).unwrap();
    install(&f.provider, &f.prefix).unwrap();
    let other = f.root.join("other");
    install(&f.combined, &other).unwrap();
    assert_eq!(receipts(&f.prefix).unwrap(), receipts(&other).unwrap());
    assert!(install(&f.combined, &other)
        .unwrap_err()
        .to_string()
        .contains("already installed"));
}
#[test]
fn missing_core_rolls_back_new_prefix() {
    let f = Fixture::new();
    assert!(install(&f.provider, &f.prefix).is_err());
    assert!(!f.prefix.exists());
}
#[test]
fn provider_requires_matching_core_declaration() {
    for existing_core in [false, true] {
        for dependency in [None, Some("other")] {
            let f = Fixture::new();
            if existing_core {
                install(&f.core, &f.prefix).unwrap();
            }
            let before = receipts(&f.prefix).unwrap();
            modify_manifest(&f.provider, |m| {
                m.components[0].requires.clear();
                if let Some(version) = dependency {
                    m.components[0]
                        .requires
                        .insert("aw-core".into(), version.into());
                }
            });
            assert!(inspect(&f.provider).is_err());
            assert!(install(&f.provider, &f.prefix).is_err());
            assert_eq!(receipts(&f.prefix).unwrap(), before);
            assert!(!f.prefix.join("libexec").exists());
            assert_eq!(f.prefix.exists(), existing_core);
        }
    }
}
#[test]
fn version_and_source_mismatch_preserve_existing_core() {
    for field in ["version", "source"] {
        let f = Fixture::new();
        install(&f.core, &f.prefix).unwrap();
        modify_manifest(&f.provider, |m| {
            if field == "source" {
                m.components[0].source_commit = "b".repeat(40);
            } else {
                m.components[0]
                    .requires
                    .insert("aw-core".into(), "other".into());
            }
        });
        assert!(install(&f.provider, &f.prefix)
            .unwrap_err()
            .to_string()
            .contains("requires matching"));
        assert_eq!(receipts(&f.prefix).unwrap().len(), 1);
        assert!(!f.prefix.join("libexec").exists());
    }
}
#[test]
fn tampered_payload_is_rejected_before_creating_prefix() {
    let f = Fixture::new();
    fs::write(f.core.join("payload/bin/aw"), "tampered").unwrap();
    assert!(install(&f.core, &f.prefix).is_err());
    assert!(!f.prefix.exists());
}
#[test]
fn source_change_after_inspection_rolls_back_and_allows_retry() {
    for existing_core in [false, true] {
        let f = Fixture::new();
        let bundle = if existing_core {
            install(&f.core, &f.prefix).unwrap();
            &f.provider
        } else {
            &f.combined
        };
        let before = receipts(&f.prefix).unwrap();
        let source = bundle.join("payload/libexec/aw/providers/sec-core/aw-provider-sec-core");
        let original = fs::read(&source).unwrap();
        let result = install_using(bundle, &f.prefix, |path, output| {
            // The final Provider payload changes only after all preflight hashes pass.
            if path == source {
                fs::write(path, "changed after inspection")?;
            }
            std::io::copy(&mut File::open(path)?, output)?;
            Ok(())
        });
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("installed payload checksum mismatch"));
        assert_eq!(receipts(&f.prefix).unwrap(), before);
        assert!(!f.prefix.join("libexec").exists());
        assert_eq!(f.prefix.exists(), existing_core);
        for component in before.values() {
            for (name, metadata) in &component.files {
                assert_eq!(digest(&f.prefix.join(name)).unwrap(), metadata.sha256);
            }
        }
        fs::write(source, original).unwrap();
        install(bundle, &f.prefix).unwrap();
        assert_eq!(receipts(&f.prefix).unwrap().len(), 2);
    }
}
#[test]
fn symlink_payload_and_prefix_are_rejected() {
    let f = Fixture::new();
    symlink(&f.core, &f.prefix).unwrap();
    assert!(install(&f.core, &f.prefix).is_err());
    fs::remove_file(&f.prefix).unwrap();
    let path = f.core.join("payload/bin/aw");
    let outside = f.root.join("outside");
    fs::rename(&path, &outside).unwrap();
    symlink(outside, path).unwrap();
    assert!(install(&f.core, &f.prefix).is_err());
    assert!(!f.prefix.exists());
}
#[test]
fn writable_parent_is_rejected() {
    for mode in [0o720, 0o702] {
        let f = Fixture::new();
        fs::set_permissions(&f.root, Permissions::from_mode(mode)).unwrap();
        assert!(install(&f.core, &f.prefix).is_err());
        fs::set_permissions(&f.root, Permissions::from_mode(0o700)).unwrap();
        assert!(!f.prefix.exists());
    }
}
#[test]
fn copy_failure_rolls_back_only_new_files() {
    let f = Fixture::new();
    let result = install_using(&f.combined, &f.prefix, |_, _| {
        Err(std::io::Error::from_raw_os_error(libc::ENOSPC).into())
    });
    assert!(
        matches!(result,Err(crate::Error::Io(error)) if error.raw_os_error()==Some(libc::ENOSPC))
    );
    assert!(!f.prefix.exists());
}
#[test]
fn source_open_failure_preserves_core_and_allows_retry() {
    let f = Fixture::new();
    install(&f.core, &f.prefix).unwrap();
    let before = receipts(&f.prefix).unwrap();
    let result = install_using(&f.provider, &f.prefix, |_, _| {
        Err(std::io::Error::from_raw_os_error(libc::EIO).into())
    });
    assert!(matches!(result,Err(crate::Error::Io(error)) if error.raw_os_error()==Some(libc::EIO)));
    assert_eq!(receipts(&f.prefix).unwrap(), before);
    assert!(!f.prefix.join("libexec").exists());
    install(&f.provider, &f.prefix).unwrap();
    assert_eq!(receipts(&f.prefix).unwrap().len(), 2);
}
#[test]
fn rollback_failure_preserves_error_and_other_cleanup() {
    let f = Fixture::new();
    let prefix = f.prefix.clone();
    let result = install_using(&f.core, &f.prefix, |_, _| {
        fs::write(prefix.join("bin/unknown"), "keep")?;
        Err(std::io::Error::from_raw_os_error(libc::ENOSPC).into())
    });
    assert!(
        matches!(result,Err(crate::Error::Io(error)) if error.raw_os_error()==Some(libc::ENOSPC))
    );
    assert!(!f.prefix.join("bin/aw").exists());
    assert!(f.prefix.join("bin/unknown").exists());
    assert!(f.prefix.join(".aw-install.lock").exists());
}
#[test]
fn competing_lock_is_never_removed() {
    let f = Fixture::new();
    install(&f.core, &f.prefix).unwrap();
    let held = paths::lock(&f.prefix).unwrap();
    let inode = held.metadata().unwrap().ino();
    assert!(
        matches!(install(&f.provider,&f.prefix),Err(crate::Error::Io(error)) if error.kind()==std::io::ErrorKind::WouldBlock)
    );
    assert_eq!(
        f.prefix.join(".aw-install.lock").metadata().unwrap().ino(),
        inode
    );
    assert!(paths::lock(&f.prefix).is_err());
}
#[test]
fn uninstall_preflights_missing_and_modified_files() {
    for missing in [false, true] {
        let f = Fixture::new();
        install(&f.combined, &f.prefix).unwrap();
        let path = f
            .prefix
            .join("libexec/aw/providers/sec-core/agent-sec-daemon");
        if missing {
            fs::remove_file(path).unwrap();
        } else {
            fs::write(path, "changed").unwrap();
        }
        assert!(uninstall(&f.prefix)
            .unwrap_err()
            .to_string()
            .contains("modified or missing"));
        assert!(f.prefix.join("bin/aw").exists());
        assert_eq!(receipts(&f.prefix).unwrap().len(), 2);
    }
}
#[test]
fn uninstall_preserves_unknown_files_and_external_data() {
    let f = Fixture::new();
    install(&f.combined, &f.prefix).unwrap();
    fs::write(f.prefix.join("note"), "keep").unwrap();
    fs::write(f.root.join("audit"), "keep").unwrap();
    uninstall(&f.prefix).unwrap();
    assert!(receipts(&f.prefix).unwrap().is_empty());
    assert!(f.prefix.join("note").exists());
    assert!(f.root.join("audit").exists());
}
#[test]
fn exclusive_private_write_rejects_existing_files_and_symlinks() {
    let f = Fixture::new();
    let path = f.root.join("config");
    paths::write_private(&path, b"{}").unwrap();
    assert_eq!(path.metadata().unwrap().permissions().mode() & 0o777, 0o600);
    assert!(paths::write_private(&path, b"overwrite").is_err());
    let link = f.root.join("link");
    symlink(&path, &link).unwrap();
    assert!(paths::write_private(&link, b"overwrite").is_err());
    assert_eq!(fs::read(path).unwrap(), b"{}");
}
#[test]
fn one_configuration_maps_both_agents_and_rejects_long_socket() {
    let f = Fixture::new();
    install(&f.combined, &f.prefix).unwrap();
    let config = f.root.join("aw.yaml");
    let state = f.root.join("state");
    let socket = f.root.join("sec.sock");
    let mut settings = crate::Settings {
        prefix: &f.prefix,
        config: &config,
        state: &state,
        socket: &socket,
        qoder: Path::new("/bin/true"),
        node: Path::new("/bin/true"),
        openclaw: Path::new("/bin/true"),
    };
    // Keep Unix sockets short even when the test checkout lives in a long directory.
    let runtime = tempfile::tempdir().unwrap();
    fs::set_permissions(runtime.path(), Permissions::from_mode(0o700)).unwrap();
    let state = runtime.path().join("state");
    settings.state = &state;
    let socket = runtime.path().join("sec.sock");
    settings.socket = &socket;
    crate::configure(&settings).unwrap();
    let value: serde_json::Value = serde_yaml_ng::from_slice(&fs::read(&config).unwrap()).unwrap();
    assert_eq!(value["spec"]["agents"].as_object().unwrap().len(), 2);
    assert_eq!(
        value["spec"]["providers"]["security"]["config"]["tools"]
            .as_object()
            .unwrap()
            .len(),
        2
    );
    assert!(crate::configure(&settings).is_err());
    fs::remove_file(&config).unwrap();
    let long = runtime.path().join("s".repeat(108));
    settings.socket = &long;
    assert!(crate::configure(&settings)
        .unwrap_err()
        .to_string()
        .contains("sec-core"));
    assert!(!config.exists());
}
#[test]
fn concurrent_first_install_keeps_one_complete_receipt_set() {
    let f = Fixture::new();
    let barrier = std::sync::Barrier::new(2);
    let outcomes = std::thread::scope(|scope| {
        let install_once = || {
            barrier.wait();
            install(&f.combined, &f.prefix)
        };
        let other = scope.spawn(install_once);
        let first = install_once();
        (first, other.join().unwrap())
    });
    assert!(outcomes.0.is_ok() || outcomes.1.is_ok(), "{outcomes:?}");
    assert!(!(outcomes.0.is_ok() && outcomes.1.is_ok()));
    assert_eq!(receipts(&f.prefix).unwrap().len(), 2);
    for component in receipts(&f.prefix).unwrap().values() {
        for (name, metadata) in &component.files {
            assert_eq!(digest(&f.prefix.join(name)).unwrap(), metadata.sha256);
        }
    }
}

#[test]
fn concurrent_observer_refuses_uninitialized_prefix_before_locking() {
    let f = Fixture::new();
    fs::create_dir(&f.prefix).unwrap();
    fs::set_permissions(&f.prefix, Permissions::from_mode(0o755)).unwrap();
    let held = paths::lock(&f.prefix).unwrap();
    let inode = held.metadata().unwrap().ino();
    // Reproduce a creator paused between mkdir and the first receipt. An observer
    // must reject the uninitialized prefix without entering lock contention.
    let error = install(&f.combined, &f.prefix).unwrap_err();
    assert!(matches!(error, crate::Error::Invalid(_)), "{error}");
    assert_eq!(
        f.prefix.join(".aw-install.lock").metadata().unwrap().ino(),
        inode
    );
    drop(held);
    assert!(receipts(&f.prefix).unwrap().is_empty());
}
