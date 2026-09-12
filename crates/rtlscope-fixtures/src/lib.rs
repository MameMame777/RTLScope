//! Locates the shared SystemVerilog fixtures in `tests/fixtures/`.
//!
//! Every crate that tests against real SystemVerilog needs these paths, and
//! resolving them from `CARGO_MANIFEST_DIR` in each one is three chances to get
//! it subtly wrong. Paths are built relative to this crate's manifest, so tests
//! do not depend on the working directory.

use std::path::{Path, PathBuf};

/// The fixtures, listed rather than globbed.
///
/// An explicit list means deleting a fixture breaks the build instead of
/// quietly shrinking coverage.
pub const NAMES: &[&str] = &[
    "adder.sv",
    "axi4_demo.sv",
    "cdc.sv",
    "comb_loop.sv",
    "connect_expr.sv",
    "counter.sv",
    "counter_tb.sv",
    "depth_demo.sv",
    "fifo.sv",
    "fsm.sv",
    "fsm_enum.sv",
    "function.sv",
    "genblk.sv",
    "hier.sv",
    "interfaces.sv",
    "latch.sv",
    "nonansi.sv",
    "packages.sv",
    "params.sv",
    "pipeline3.sv",
    "precedence.sv",
    "reconverge.sv",
    "port_group.sv",
    "task_loop.sv",
    "trace_demo.sv",
    "unsupported.sv",
];

/// Fixtures that are legal SystemVerilog Icarus cannot read.
///
/// Icarus 12 rejects a port typed by an interface and a modport — `bus_if.master
/// m` — with "Errors in port declarations", while Verilator 5.048 lints the
/// same file clean (`verilator_bin --lint-only -Wall`, with `VERILATOR_ROOT`
/// set). The legality check skips these rather than the whole file set losing
/// its referee; everything else about them is tested like any other fixture.
pub const BEYOND_ICARUS: &[&str] = &["interfaces.sv"];

/// Fixtures that only compile with another fixture beside them.
///
/// A testbench instantiates the thing it is testing, so on its own it is a
/// reference to a module that is not there. That is not a fault in it — it is
/// what a testbench *is* — but the legality check compiles one file at a time,
/// and without this it would read a correct testbench as a broken fixture.
pub const NEEDS: &[(&str, &[&str])] = &[("counter_tb.sv", &["counter.sv"])];

/// What has to be compiled with a fixture, itself included, in order.
///
/// The companions come first: a compiler reads a file list in order, and the
/// module has to be declared before the testbench instantiates it.
pub fn with_companions(name: &str) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = NEEDS
        .iter()
        .find(|(who, _)| *who == name)
        .map(|(_, needs)| needs.iter().map(|it| path(it)).collect())
        .unwrap_or_default();
    files.push(path(name));
    files
}

/// Absolute path to the fixture directory.
pub fn dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures")
}

/// Absolute path to one fixture. Panics if it is missing, because a test that
/// silently skips its input is worse than one that fails.
pub fn path(name: &str) -> PathBuf {
    let p = dir().join(name);
    assert!(p.is_file(), "fixture not found: {}", p.display());
    p
}

/// Absolute path to a Yosys netlist, in `tests/fixtures/netlists/`.
///
/// Written by Yosys rather than by hand, which is the only way a cross-check
/// against it means anything: a netlist written to match RTLScope's idea of the
/// answer would agree with it by construction.
pub fn netlist(name: &str) -> PathBuf {
    let p = dir().join("netlists").join(name);
    assert!(p.is_file(), "netlist fixture not found: {}", p.display());
    p
}

/// Absolute path to a recorded waveform or run, in `tests/fixtures/waves/`.
///
/// Dumps and results files are shared for the same reason the sources are: the
/// CLI renders them, the server serves them, and both should be looking at the
/// same bytes.
pub fn wave(name: &str) -> PathBuf {
    let p = dir().join("waves").join(name);
    assert!(p.is_file(), "wave fixture not found: {}", p.display());
    p
}

/// The Veryl project in `tests/fixtures/lights/`: its sources, and the
/// SystemVerilog and source maps Veryl wrote from them.
///
/// Checked in built, so that reading it needs no Veryl on the machine — the
/// same reason the waveforms are checked in recorded. Rebuild with `veryl
/// build` in that directory after editing a `.veryl`, and commit what it wrote.
pub fn veryl_project() -> PathBuf {
    let p = dir().join("lights");
    assert!(p.join("Veryl.toml").is_file(), "Veryl fixture not found: {}", p.display());
    p
}

/// Every fixture, in [`NAMES`] order.
pub fn all() -> Vec<PathBuf> {
    NAMES.iter().map(|n| path(n)).collect()
}

/// The Zybo file list. Empty until the real project path is pinned down
/// (Step 7), so callers should treat "no entries" as "skip".
pub fn zybo_file_list() -> PathBuf {
    dir().join("zybo.f")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_listed_fixture_exists() {
        for path in all() {
            assert!(path.is_file(), "{}", path.display());
        }
    }

    #[test]
    fn no_fixture_on_disk_is_missing_from_the_list() {
        let mut found: Vec<String> = std::fs::read_dir(dir())
            .expect("fixture directory")
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".sv"))
            .collect();
        found.sort();
        let mut listed: Vec<String> = NAMES.iter().map(|s| (*s).to_owned()).collect();
        listed.sort();
        assert_eq!(found, listed, "tests/fixtures/ and NAMES have drifted apart");
    }
}
