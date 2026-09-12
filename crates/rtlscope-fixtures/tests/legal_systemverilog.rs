//! Checks the fixtures are legal SystemVerilog before RTLScope is blamed for
//! failing on them.
//!
//! A typo in a fixture otherwise shows up as a mysterious RTLScope bug. Icarus is
//! only a referee here — nothing in RTLScope depends on it at runtime — so the
//! test skips itself rather than failing when Icarus cannot run.
//!
//! One environment trap used to make this skip itself whenever `cargo test`
//! was run from anything but PowerShell: `iverilog -V` succeeds even when
//! compiling cannot work, and a native Icarus launched from MSYS or Git Bash
//! exits 127 with an empty stderr — a signature indistinguishable from success
//! if the status is not checked.
//!
//! Two causes, both now handled here. Icarus spawns its own `ivlpp | ivl`
//! pipeline through `$SHELL` when one is set, and a POSIX shell eats the
//! backslashes out of the Windows paths it builds; and Windows resolves DLLs
//! by searching `PATH`, so a second Icarus installation wins for the
//! *libraries* even when the first was chosen for the *program*. Dropping
//! `SHELL` and putting Icarus's own directory in front of its `PATH` fixes
//! both, and this check now runs from any shell. The skip remains for a
//! machine that genuinely has no Icarus.

use std::path::PathBuf;
use std::process::Command;

/// Where Icarus is, looked for the way Windows would if it bothered.
///
/// Process spawning does not consult `PATHEXT`, so a bare name finds only an
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

/// Returns `None` if iverilog could not be launched at all, otherwise the exit
/// status and whatever it wrote to stderr.
fn run_iverilog(files: &[PathBuf]) -> Option<(bool, String)> {
    let tool = find("iverilog")?;
    let mut command = Command::new(&tool);
    // The two things that made this skip from Git Bash. See the module docs.
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
    Some((output.status.success(), complaints(&stderr)))
}

/// What Icarus said about the *source*, dropping what it said about itself.
///
/// `sorry:` is its prefix for a construct it does not fully implement — a
/// statement about the referee, not about the fixture, and treating it as one
/// would make a legal file look illegal. Everything else it prints is kept and
/// still fails the check.
fn complaints(stderr: &str) -> String {
    let kept: Vec<&str> =
        stderr.lines().filter(|line| !line.contains(": sorry:")).map(str::trim).collect();
    kept.join(
        "
",
    )
    .trim()
    .to_owned()
}

#[test]
fn fixtures_parse_as_ieee_1800_2017() {
    let fixtures = rtlscope_fixtures::all();
    let probe = fixtures.first().expect("at least one fixture").clone();

    let Some((ok, stderr)) = run_iverilog(&[probe]) else {
        eprintln!("skipping: iverilog is not on PATH");
        return;
    };
    if !ok && stderr.is_empty() {
        eprintln!(
            "skipping: iverilog is installed but cannot spawn its ivlpp|ivl \
             pipeline in this shell; run `cargo test` from PowerShell to check \
             the fixtures"
        );
        return;
    }

    let mut failures = Vec::new();
    for path in &fixtures {
        // Legal, but past what Icarus implements; see the list's own note.
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        if rtlscope_fixtures::BEYOND_ICARUS.contains(&name.as_str()) {
            eprintln!("skipping {name}: Icarus cannot read it (checked with Verilator instead)");
            continue;
        }
        // A testbench is compiled with the design it instantiates. On its
        // own it names a module that is not there, which is what a testbench
        // is rather than a fault in it.
        match run_iverilog(&rtlscope_fixtures::with_companions(&name)) {
            Some((true, warnings)) if warnings.is_empty() => {}
            Some((true, warnings)) => {
                failures.push(format!("{}: warnings\n{warnings}", path.display()))
            }
            Some((false, stderr)) => failures.push(format!("{}:\n{stderr}", path.display())),
            None => failures.push(format!("{}: could not launch iverilog", path.display())),
        }
    }

    assert!(failures.is_empty(), "fixtures rejected by iverilog:\n\n{}", failures.join("\n\n"));
}
