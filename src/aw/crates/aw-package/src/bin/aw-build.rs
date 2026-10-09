//! Maintainer-only Rust build entrypoint; this binary is not distributed to users.

use aw_package::{Error, Result};
use std::{
    path::PathBuf,
    sync::atomic::{AtomicBool, Ordering},
};
static CANCEL: AtomicBool = AtomicBool::new(false);
extern "C" fn interrupt(_: libc::c_int) {
    CANCEL.store(true, Ordering::Relaxed);
}

fn main() {
    if let Err(error) = run() {
        eprintln!("aw-build: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let mut version = None;
    let mut output = None;
    while let Some(flag) = args.next() {
        if flag == "--help" {
            println!("aw-build --version X.Y.Z-preview.N --output ABS_DIR");
            return Ok(());
        }
        let value = args
            .next()
            .ok_or_else(|| Error::Invalid(format!("missing value for {flag}")))?;
        match flag.as_str() {
            "--version" if version.is_none() => version = Some(value),
            "--output" if output.is_none() => output = Some(PathBuf::from(value)),
            _ => return Err(Error::Invalid(format!("unknown or duplicate flag: {flag}"))),
        }
    }
    let version = version.ok_or_else(|| Error::Invalid("missing --version".into()))?;
    let output = output.ok_or_else(|| Error::Invalid("missing --output".into()))?;
    // SAFETY: handlers only update the lock-free cancellation flag; aw-exec owns reaping.
    unsafe {
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = interrupt as *const () as usize;
        libc::sigemptyset(&mut action.sa_mask);
        for signal in [libc::SIGINT, libc::SIGTERM] {
            if libc::sigaction(signal, &action, std::ptr::null_mut()) != 0 {
                return Err(std::io::Error::last_os_error().into());
            }
        }
    }
    let repo = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../../..")
        .canonicalize()?;
    for archive in aw_package::package::build(&repo, &version, &output, &CANCEL)? {
        println!("{}", archive.display());
    }
    Ok(())
}
