//! Public CLI contracts, independent of local Agent or backend installations.
use std::process::Command;
#[test]
fn help_exposes_native_installation_and_dual_agent_configuration() {
    let output = Command::new(env!("CARGO_BIN_EXE_aw-package"))
        .arg("--help")
        .output()
        .unwrap();
    assert!(output.status.success());
    let text = String::from_utf8(output.stdout).unwrap();
    for expected in [
        "install --prefix",
        "configure --prefix",
        "--qoder",
        "--openclaw",
        "uninstall --prefix",
    ] {
        assert!(text.contains(expected), "missing {expected}");
    }
    assert!(!text.contains("python"));
}
#[test]
fn invalid_commands_and_duplicate_flags_fail_before_touching_disk() {
    for args in [
        vec!["unknown"],
        vec!["install", "--prefix", "/unused", "--prefix", "/other"],
        vec!["configure", "--prefix"],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_aw-package"))
            .args(args)
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(String::from_utf8(output.stderr)
            .unwrap()
            .contains("aw-package:"));
    }
}
