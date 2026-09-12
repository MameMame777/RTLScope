//! The CPU project in `tests/fixtures/cpu/` is legal SystemVerilog, as a whole.
//!
//! `legal_systemverilog.rs` compiles the flat fixtures one file at a time. The
//! CPU cannot be checked that way: six of its files import a package and three
//! instantiate a module from another, so on their own they name things that
//! are not there — which is what a project *is*, not a fault in it. So this
//! hands Icarus the folder in one go.
//!
//! The launcher below is the same as the one in `legal_systemverilog.rs`, and
//! carries the same two fixes (the `SHELL` variables, and the tool's own
//! directory ahead of `PATH`). It is repeated here rather than shared because
//! that file's launcher takes one path, and widening it belongs to the change
//! that needs it there. Once it takes a list, this should call it instead.

use std::path::PathBuf;
use std::process::Command;

/// Every file of the project, in the order a compiler needs them: the package
/// first, then the leaves, then what instantiates them.
const FILES: &[&str] = &[
    "cpu_pkg.sv",
    "imem.sv",
    "alu.sv",
    "regfile.sv",
    "control.sv",
    "datapath.sv",
    "cpu.sv",
    "blink.sv",
    // The testbench, last: it instantiates `cpu`, and on its own it would name
    // a module that is not there.
    "cpu_tb.sv",
];

/// Where Icarus is, looked for the way Windows would if it bothered:
/// process spawning does not consult `PATHEXT`, so a bare name finds only an
/// extensionless file or an `.exe`.
fn find(name: &str) -> Option<PathBuf> {
    let paths: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|paths| std::env::split_paths(&paths).collect())
        .unwrap_or_default();
    paths.iter().find_map(|dir| {
        ["exe", ""].iter().find_map(|extension| {
            let candidate = match *extension {
                "" => dir.join(name),
                _ => dir.join(format!("{name}.{extension}")),
            };
            candidate.is_file().then_some(candidate)
        })
    })
}

/// `None` if iverilog could not be launched at all; otherwise whether it
/// accepted the files, and what it said about them (its `sorry:` lines, which
/// are about itself, left out).
fn run_iverilog(files: &[PathBuf]) -> Option<(bool, String)> {
    let tool = find("iverilog")?;
    let mut command = Command::new(&tool);
    command.env_remove("SHELL").env_remove("MAKESHELL");
    if let Some(dir) = tool.parent() {
        let existing = std::env::var_os("PATH").unwrap_or_default();
        let ahead = std::iter::once(dir.to_path_buf()).chain(std::env::split_paths(&existing));
        if let Ok(joined) = std::env::join_paths(ahead) {
            command.env("PATH", joined);
        }
    }
    command.args(["-g2012", "-tnull"]);
    for file in files {
        command.arg(file);
    }
    let output = command.output().ok()?;
    let stderr = String::from_utf8_lossy(&output.stderr);
    let complaints: Vec<&str> = stderr.lines().filter(|line| !line.starts_with("sorry:")).collect();
    Some((output.status.success(), complaints.join("\n")))
}

#[test]
fn the_cpu_project_parses_as_ieee_1800_2017() {
    let dir = rtlscope_fixtures::dir().join("cpu");
    let files: Vec<PathBuf> = FILES.iter().map(|name| dir.join(name)).collect();
    for file in &files {
        assert!(file.is_file(), "the project is missing {}", file.display());
    }

    let Some((ok, stderr)) = run_iverilog(&files) else {
        eprintln!("skipping: iverilog is not on PATH");
        return;
    };
    if !ok && stderr.is_empty() {
        eprintln!(
            "skipping: iverilog is installed but cannot spawn its ivlpp|ivl pipeline in this \
             shell; run `cargo test` from PowerShell to check the project"
        );
        return;
    }
    assert!(ok, "the CPU project was rejected by iverilog:\n\n{stderr}");
    assert!(stderr.is_empty(), "the CPU project drew warnings from iverilog:\n\n{stderr}");
}
