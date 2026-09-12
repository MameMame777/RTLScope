//! Puts the mark on the executable file itself.
//!
//! The window and the taskbar get their icon at runtime from
//! `assets/rtlscope-64.rgba`, but Explorer, the Start menu and the installer read
//! it out of the binary's resources, which have to be there before the linker
//! runs.
//!
//! Done with `windres` and a two-line script rather than with a crate. The
//! crates that do this (`winres`, `winresource`) exist to *find* a resource
//! compiler, and on this machine there is no Windows SDK to find one in —
//! `rc.exe` is absent and `windres` is present, which is the case they handle
//! by giving up. So the two lines are written here, and a build that cannot
//! find `windres` says so once and carries on: the icon is on the shortcut the
//! installer makes either way, and a missing picture is not a reason to refuse
//! to compile.

use std::path::PathBuf;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=../../assets/rtlscope.ico");

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    // Absolute, because the script is written into `OUT_DIR` and a relative
    // path in it would be resolved from there. Forward slashes: a backslash
    // starts an escape in a resource script.
    let icon = match PathBuf::from("../../assets/rtlscope.ico").canonicalize() {
        Ok(path) => path.display().to_string().replace('\\', "/").replace("//?/", ""),
        Err(error) => {
            println!("cargo:warning=no icon to put on the executable: {error}");
            return;
        }
    };

    let out = PathBuf::from(std::env::var("OUT_DIR").expect("cargo sets OUT_DIR"));
    let script = out.join("rtlscope.rc");
    let object = out.join("rtlscope-icon.o");
    // `1` is the lowest id, and Explorer shows the lowest-numbered icon.
    if let Err(error) = std::fs::write(&script, format!("1 ICON \"{icon}\"\n")) {
        println!("cargo:warning=could not write the resource script: {error}");
        return;
    }

    // `--preprocessor=cat` because windres otherwise runs the script through
    // `gcc -E`, which fails here — measured — and a script naming one icon has
    // nothing in it to preprocess anyway.
    let ran = Command::new("windres")
        .args(["--preprocessor=cat", "-J", "rc", "-O", "coff", "-i"])
        .arg(&script)
        .arg("-o")
        .arg(&object)
        .status();

    match ran {
        Ok(status) if status.success() => {
            println!("cargo:rustc-link-arg-bins={}", object.display());
        }
        Ok(status) => {
            println!("cargo:warning=windres refused the icon ({status}); the binary has none");
        }
        Err(error) => {
            println!("cargo:warning=no windres, so the binary carries no icon: {error}");
        }
    }
}
