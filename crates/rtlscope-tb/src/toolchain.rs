//! The environment a cocotb + Verilator run needs, written beside the harness.
//!
//! On Linux and macOS none of this is needed: cocotb's wheel ships a Verilator
//! VPI library and its runner finds its own way. On Windows it ships none, and
//! eight separate things stand between a generated testbench and a running one.
//! Each was measured rather than guessed — a sibling project on the machine
//! this was written for had already paid for every one of them, and its
//! runbook is where these came from.
//!
//! | # | What is wrong | What is done about it |
//! |---|---|---|
//! | 1 | The tools are not on `PATH` | prepend ucrt64's `bin`, and MSYS's `usr/bin` for `perl` |
//! | 2 | `shutil.which("verilator")` finds `verilator.bat`, and cocotb then runs it through `perl`, which cannot parse a batch file | a file named `verilator.cmd` whose *contents are Perl*, placed first on `PATH` |
//! | 3 | cocotb runs `make`; ucrt64 ships `mingw32-make.exe` | copy it to a `make.exe` shim |
//! | 4 | Backslashes in `VERILATOR_ROOT` are eaten by the shell under `make` | forward slashes |
//! | 5 | No `libcocotbvpi_verilator` on Windows — Verilator links the VPI *into* the executable, and a DLL cannot export VPI symbols to a host exe | compile cocotb's own VPI sources into a static archive, into the directory the runner already puts on the link line |
//! | 6 | ucrt64's libstdc++ does not export the out-of-line `std::string` move constructor Verilator's `-Os` references | force `-O2` |
//! | 7 | The Verilated exe's embedded Python cannot load stdlib `.pyd` files: Windows resolves their dependent DLLs from the *executable's* directory, never from `PATH` | `os.add_dll_directory` at the simulation interpreter's startup, via `sitecustomize` |
//! | 8 | Windows searches the *current* directory before `PATH`, so `shutil.which` can answer with a relative path that cocotb then runs from another directory | set `NoDefaultCurrentDirectoryInExePath`, and keep the shim out of the current directory |
//!
//! Behind all of them is one that produces no error of its own until the link
//! fails: **the interpreter must be the ucrt64 one**. cocotb's VPI is linked
//! into a binary built by ucrt64's gcc, so a Python built by MSVC cannot host
//! it. The generated `run.py` checks this first and says so, because the
//! symptom otherwise arrives three steps later wearing a linker's clothes.
//!
//! All of it is Verilator's. The Icarus engine needs none of it — cocotb ships
//! its Icarus VPI on Windows, and its runner calls `iverilog` and `vvp` by
//! name — so `prepend_path` takes the engine, and for Icarus does nothing
//! beyond making sure those two resolve and saying where. Making Icarus pass
//! Verilator's checks would ask a machine with no C++ compiler for one.

/// The support files a cocotb harness needs beside it.
///
/// Written as text like everything else this crate produces, so the caller
/// decides where they land and whether to overwrite anything.
pub fn files() -> Vec<(String, String)> {
    vec![
        ("rtlscope_site.py".to_string(), SITE.to_string()),
        ("rtlscope_vpi.py".to_string(), VPI.to_string()),
        ("sitecustomize.py".to_string(), SITECUSTOMIZE.to_string()),
        (SHIM.to_string(), VERILATOR_CMD.to_string()),
    ]
}

/// Where the Perl shim goes: beside `make.exe`, and **not** in the work
/// directory.
///
/// The work directory is the process's own current directory, which is the one
/// place a relative answer from `shutil.which` can resolve. cocotb then runs
/// what it found from the *build* directory one level down, so a relative
/// answer becomes perl's "Can't open perl script", naming a path relative to a
/// directory it was never resolved against and saying nothing about why.
/// Keeping the shim somewhere that is never the current directory means the
/// answer can only ever be absolute.
pub const SHIM: &str = ".rtlscope-toolchain/verilator.cmd";

/// Workaround 2: found by `shutil.which` for its extension, run by `perl`
/// because that is what cocotb does with whatever it found.
const VERILATOR_CMD: &str = r#"#!perl
# Not a batch file. cocotb resolves the simulator with shutil.which("verilator")
# and then runs `perl <that path>`. On Windows shutil.which only considers names
# carrying a PATHEXT extension, so it never sees Verilator's real Perl driver,
# which sits beside it with no extension at all — it finds verilator.bat, which
# perl cannot parse.
#
# rtlscope_site puts this file's directory first on PATH, so shutil.which returns
# this instead. The `.cmd` exists only to be matched; the body is Perl. Run
# directly by cmd.exe it would do nothing useful, and it is never meant to be.
exec("verilator_bin.exe", @ARGV);
die "rtlscope: could not start verilator_bin.exe (is ucrt64/bin on PATH?): $!\n";
"#;

/// Workaround 7.
const SITECUSTOMIZE: &str = r#""""Runs at startup inside the *simulation* interpreter.

The Verilated executable embeds Python, and Windows resolves an extension
module's dependent DLLs from the executable's own directory, from directories
registered with AddDllDirectory, and from System32 — never from PATH. That
executable is built in a scratch directory, so ucrt64's runtime DLLs are
invisible to it and even `import binascii` fails with "DLL load failed".

The directories arrive in COCOTB_DLL_DIRS, set by run.py before the simulation
starts, rather than being worked out here: this file is imported before almost
anything else exists, and importing a project module this early is a good way
to make a confusing failure.
"""
import os

for _dir in os.environ.get("COCOTB_DLL_DIRS", "").split(os.pathsep):
    if _dir and os.path.isdir(_dir):
        try:
            os.add_dll_directory(_dir)
        except OSError:
            pass
"#;

/// Workarounds 1, 2, 3, 4 and 6, and the interpreter check behind them.
const SITE: &str = r#""""Where the tools are, and what Verilator needs on this platform.

Generated by RTLScope. On anything but Windows every function here is a no-op:
cocotb ships a Verilator VPI library on those platforms and its runner finds
its own way. On Windows it does not, and this module owns the way round.

Nothing absolute is written down. The MSYS2 root is resolved from $MSYS2_ROOT,
then from a verilator already on PATH, then from the usual places — and if none
of those work it says what to set rather than failing somewhere further on.
"""
from __future__ import annotations

import os
import shutil
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
SHIM_DIR = HERE / ".rtlscope-toolchain"

WINDOWS = os.name == "nt"

_WELL_KNOWN = (r"C:\msys64", r"C:\msys2", r"C:\tools\msys64")


class ToolchainError(RuntimeError):
    """The toolchain is missing or is not the one that can work."""


# What marks an MSYS2 as having each engine, by the file Windows can run.
# Verilator is looked for by its real binary rather than the `verilator.bat`
# beside it: the batch file is what `shutil.which` finds, and what perl then
# cannot read.
_MARK = {"verilator": "verilator_bin.exe", "icarus": "iverilog.exe"}


def _valid(root, tool) -> bool:
    return root is not None and (Path(root) / "ucrt64" / "bin" / tool).is_file()


def _from_path(tool):
    # The tool lives in <root>/ucrt64/bin, so the root is two directories up.
    hit = shutil.which(tool)
    if hit:
        candidate = Path(hit).resolve().parents[2]
        if _valid(candidate, tool):
            return candidate
    return None


def msys2_root(engine="verilator") -> Path:
    """The directory holding `ucrt64/`, with the engine's own tool under it."""
    tool = _MARK[engine]
    named = os.environ.get("MSYS2_ROOT")
    if named:
        if _valid(named, tool):
            return Path(named)
        raise ToolchainError(
            f"MSYS2_ROOT={named!r} is set, but {Path(named) / 'ucrt64' / 'bin' / tool} "
            "is not there."
        )
    found = _from_path(tool)
    if found:
        return found
    for candidate in _WELL_KNOWN:
        if _valid(candidate, tool):
            return Path(candidate)
    raise ToolchainError(
        f"No MSYS2 ucrt64 toolchain with {tool} found. Put its ucrt64\\bin on PATH, or set\n"
        "    $env:MSYS2_ROOT = '<the directory holding ucrt64>'"
    )


def ucrt64_bin(root=None) -> Path:
    return Path(root or msys2_root()) / "ucrt64" / "bin"


def usr_bin(root=None) -> Path:
    return Path(root or msys2_root()) / "usr" / "bin"


def verilator_root(root=None) -> str:
    # Forward slashes: the shell under `make` eats backslashes, and the build
    # then hunts for a path with the separators gone.
    return str(Path(root or msys2_root()) / "ucrt64" / "share" / "verilator").replace("\\", "/")


def check_interpreter(root=None) -> None:
    """Refuses a Python that cannot host the VPI, before anything else fails.

    cocotb's Verilator VPI is linked into a binary that ucrt64's gcc built, so
    the interpreter embedded in it has to be ucrt64's too. A Python built by
    MSVC gets all the way to the link before saying anything, and what it says
    then is about a missing library.
    """
    if not WINDOWS:
        return
    root = Path(root or msys2_root())
    base = Path(sys.base_prefix).resolve()
    if root.resolve() in base.parents or base == (root / "ucrt64").resolve():
        return
    raise ToolchainError(
        "This Python cannot run cocotb on Verilator here.\n"
        f"  running : {sys.executable}\n"
        f"  based on: {sys.base_prefix}\n"
        f"  wanted  : a venv made from {ucrt64_bin(root) / 'python.exe'}\n\n"
        "cocotb's VPI is linked into a binary built by ucrt64's gcc, so the interpreter\n"
        "inside it must come from the same toolchain. Make one:\n\n"
        f"    {ucrt64_bin(root) / 'python.exe'} -m venv .venv\n"
        "    .venv\\bin\\python.exe -m pip install cocotb\n"
    )


def _make_shim(root) -> None:
    # cocotb runs `make`; ucrt64 ships `mingw32-make.exe` and no `make.exe`.
    SHIM_DIR.mkdir(parents=True, exist_ok=True)
    made = SHIM_DIR / "make.exe"
    if not made.is_file():
        shutil.copy2(ucrt64_bin(root) / "mingw32-make.exe", made)


def prepend_path(engine="verilator"):
    """Makes the engine's tools discoverable to cocotb's runner.

    Returns the MSYS2 root Verilator was found under, and None for Icarus,
    which has no use for one: cocotb ships its Icarus VPI on Windows, and its
    runner calls `iverilog` and `vvp` by name. All Icarus asks is that those
    two resolve -- from the directory the caller put first on PATH, or failing
    that from an MSYS2 that has them.
    """
    if not WINDOWS:
        return None
    # Windows looks in the current directory for an executable before it looks
    # along PATH, and `shutil.which` follows that rule -- it asks
    # NeedCurrentDirectoryForExePath, which answers "no" only when this variable
    # is set. cocotb runs whatever `which` hands it from the *build* directory
    # one level down, so a hit found in the current directory comes back
    # relative and does not resolve there.
    #
    # Measured, and the reason this was invisible for so long: a shell sets this
    # variable -- both PowerShell and MSYS bash do -- so from a terminal the
    # answer was always absolute. A window started from its Start-menu shortcut
    # inherits no such variable, and answered `.\verilator.CMD`. Set before
    # anything is looked up, so every answer below is absolute.
    os.environ["NoDefaultCurrentDirectoryInExePath"] = "1"

    if engine == "icarus":
        if shutil.which("iverilog") is None or shutil.which("vvp") is None:
            _prepend(str(ucrt64_bin(msys2_root(engine))))
        # Windows resolves a DLL along PATH, and Icarus's own helpers -- `ivl`
        # and `ivlpp`, under lib/ivl -- have no runtime beside them, so the
        # first libstdc++ on PATH is the one they get. Measured: Git for
        # Windows' mingw64/bin ahead on PATH ended in STATUS_ENTRYPOINT_NOT_FOUND
        # from iverilog, which prints nothing; a second Icarus or MSYS2 does the
        # same. The directory of the iverilog that will run goes first, the
        # rule the recording side already applies.
        found = shutil.which("iverilog")
        if found:
            _prepend(str(Path(found).resolve().parent))
        for tool in ("iverilog", "vvp"):
            print(f"[rtlscope] {tool}: {shutil.which(tool)}", flush=True)
        return None

    root = msys2_root(engine)
    _make_shim(root)
    _prepend(
        str(SHIM_DIR),        # the perl `verilator.cmd`, and make.exe
        str(ucrt64_bin(root)),  # gcc, verilator_bin.exe
        str(usr_bin(root)),   # perl
    )
    os.environ["VERILATOR_ROOT"] = verilator_root(root)
    # Said out loud: cocotb runs whatever `shutil.which` hands it, from another
    # directory, so an answer that will not resolve there fails three steps
    # later with a message that names perl and not the path it was given.
    print(f"[rtlscope] verilator: {shutil.which('verilator')}", flush=True)
    return root


def _prepend(*dirs) -> None:
    # HERE is deliberately absent. It is this process's current directory, and
    # a hit found there can come back relative -- which cocotb then runs from
    # the build directory one level down, where it does not resolve. Nothing
    # in the work directory is looked up by name, so nothing is lost.
    existing = os.environ.get("PATH", "")
    os.environ["PATH"] = os.pathsep.join(dirs) + (os.pathsep + existing if existing else "")


def sim_dll_dirs(root=None) -> str:
    """The directories the simulation's embedded Python needs registered.

    The interpreter runs inside the simulator's own process -- a Verilated
    executable, or vvp -- which resolves an extension module's DLLs beside
    *itself* and never along PATH. What has to be reachable is the interpreter's
    runtime and cocotb's libraries. With a root, the runtime is ucrt64's `bin`;
    without one it is wherever this interpreter's base keeps it.
    """
    import cocotb_tools.config as config

    base = Path(sys.base_prefix)
    beside = [ucrt64_bin(root)] if root is not None else [base / "bin", base / "DLLs", base]
    return os.pathsep.join(str(d) for d in [*beside, Path(config.libs_dir)])


def common_build_args() -> list:
    """What Verilator needs here, beyond what the runner passes itself.

    `-O2` because ucrt64's shared libstdc++ does not export the out-of-line
    `std::string` move constructor that Verilator's default `-Os` references, so
    every translation unit that moves a string fails to link. `-Wno-attributes`
    because g++ emits a few hundred harmless dllimport redeclaration warnings
    compiling Verilator's VPI against cocotb's headers, and they bury anything
    worth reading. `-lgpi`/`-lgpilog` because the statically linked VPI archive
    has to resolve its own GPI symbols. `-Wno-fatal` keeps Verilator's lint
    visible without stopping a build over a width mismatch in somebody's design.
    """
    if not WINDOWS:
        return ["-Wno-fatal"]
    return [
        "-Wno-fatal",
        "-CFLAGS", "-O2",
        "-CFLAGS", "-Wno-attributes",
        "-LDFLAGS", "-lgpi",
        "-LDFLAGS", "-lgpilog",
    ]


if __name__ == "__main__":
    if not WINDOWS:
        print("not Windows: cocotb ships what it needs here")
    else:
        engine = os.environ.get("RTLSCOPE_ENGINE", "verilator")
        found = msys2_root(engine)
        print(f"engine         : {engine}")
        print(f"MSYS2          : {found}")
        print(f"ucrt64 bin     : {ucrt64_bin(found)}")
        if engine == "verilator":
            print(f"VERILATOR_ROOT : {verilator_root(found)}")
            check_interpreter(found)
            print("interpreter    : ok")
"#;

/// Workaround 5: the library cocotb does not ship on Windows.
const VPI: &str = r#""""Builds the Verilator VPI library cocotb does not ship on Windows.

cocotb's runner links the simulation with `-lcocotbvpi_verilator`, and the
Windows wheel has no such library — only aldec, ghdl, icarus and modelsim. That
is not an oversight. Verilator produces a standalone executable, and a Windows
DLL cannot export VPI symbols into a host executable, so the VPI layer has to be
linked in statically; cocotb builds it that way on POSIX and guards it out here.

So it is built from cocotb's own sources, matching the version installed, and
dropped into cocotb's `libs` directory — the one `-L` the runner puts on the
link line before its `-lcocotbvpi_verilator`.

`ensure()` is idempotent and heals itself: it records what the archive was built
from, and rebuilds when cocotb, gcc, Verilator or the flags change. Fetching the
sources needs the network once; after that they are cached beside this file.
"""
from __future__ import annotations

import contextlib
import hashlib
import json
import os
import subprocess
import sys
import tarfile
import time
import urllib.request
from pathlib import Path

import rtlscope_site as site

LIB = "libcocotbvpi_verilator.a"
CACHE = site.HERE / ".rtlscope-toolchain" / "cocotb-src"

# The symbols are provided here rather than imported, and the vendored
# vpi_user.h decorates them for import unless these are blank.
DEFINES = [
    "-DCOCOTBVPI_EXPORTS=1",
    "-DVERILATOR=1",
    "-D__STDC_FORMAT_MACROS=1",
    "-DWIN32=1",
    "-DPLI_DLLISPEC=",
    "-DPLI_DLLESPEC=",
]
CFLAGS = ["-O2", "-std=c++17", "-fpermissive"]


def _version() -> str:
    import cocotb

    return cocotb.__version__


def _libs_dir() -> Path:
    import cocotb_tools.config as config

    return Path(config.libs_dir)


def _tool_version(exe: str) -> str:
    try:
        done = subprocess.run([exe, "--version"], capture_output=True, text=True, timeout=60)
        return (done.stdout or done.stderr).strip().splitlines()[0]
    except Exception as why:  # noqa: BLE001 — the version is only part of a stamp
        return f"unknown ({why})"


def _sources(version: str) -> Path:
    """cocotb's VPI sources for the installed version, fetched once and kept.

    What counts as "already here" is a file, not the directory holding it. An
    unpacking that was interrupted — the window closed, the run cancelled — is
    left as the whole directory tree with nothing in it, and a cache judged by
    `vpi.is_dir()` calls that done. Every later run then died on `No VPI
    sources`, which nothing would ever repair, because the directory it was
    complaining about went on existing. Measured on this machine: a work
    directory poisoned that way failed the same way for days while the same
    pattern played from a fresh one.
    """
    root = CACHE / f"cocotb-{version}"
    vpi = root / "src" / "cocotb" / "share" / "lib" / "vpi"
    if (vpi / "VpiImpl.cpp").is_file():
        return root / "src" / "cocotb"

    CACHE.mkdir(parents=True, exist_ok=True)
    api = f"https://pypi.org/pypi/cocotb/{version}/json"
    with urllib.request.urlopen(api, timeout=60) as answer:
        release = json.load(answer)
    url = next((u["url"] for u in release["urls"] if u["packagetype"] == "sdist"), None)
    if url is None:
        raise site.ToolchainError(f"cocotb {version} has no source release on PyPI.")

    archive = CACHE / f"cocotb-{version}.tar.gz"
    if not archive.is_file():
        # Downloaded beside the name and moved onto it, so a download that is
        # cut off leaves no half a file for the next run to believe in. Same
        # trap as the directory above, one step earlier.
        part = archive.with_suffix(".part")
        urllib.request.urlretrieve(url, part)
        part.replace(archive)
    with tarfile.open(archive) as tar:
        for member in tar.getmembers():
            if "/share/" in member.name or member.name.endswith(".h"):
                tar.extract(member, CACHE)

    if not (vpi / "VpiImpl.cpp").is_file():
        raise site.ToolchainError(f"cocotb's VPI sources are not under {vpi} after unpacking.")
    return root / "src" / "cocotb"


def _stamp(version: str, sources) -> str:
    return hashlib.sha256(
        json.dumps(
            {
                "cocotb": version,
                "gcc": _tool_version("g++"),
                "verilator": _tool_version("verilator_bin.exe"),
                "sources": sorted(p.name for p in sources),
                "defines": DEFINES,
                "cflags": CFLAGS,
            },
            sort_keys=True,
        ).encode()
    ).hexdigest()


@contextlib.contextmanager
def _one_builder_at_a_time(timeout: float = 600.0):
    """Serialises the build, because the archive is shared.

    It lives in cocotb's own directory, so two runs starting at once — two
    windows, two tests — both decide it is missing and race: one deletes the
    file the other is linking against, and the error names a permission
    problem rather than a race.

    A lock older than the timeout is taken anyway: a crashed builder must not
    stop every later one.
    """
    CACHE.mkdir(parents=True, exist_ok=True)
    lock = CACHE / ".building"
    deadline = time.monotonic() + timeout
    while True:
        try:
            handle = os.open(lock, os.O_CREAT | os.O_EXCL | os.O_WRONLY)
            os.close(handle)
            break
        except FileExistsError:
            stale = False
            try:
                stale = time.time() - lock.stat().st_mtime > timeout
            except OSError:
                stale = True
            if stale:
                lock.unlink(missing_ok=True)
                continue
            if time.monotonic() > deadline:
                raise site.ToolchainError(
                    f"waited {timeout:.0f}s for another build of {LIB} ({lock})."
                )
            time.sleep(0.25)
    try:
        yield
    finally:
        lock.unlink(missing_ok=True)


def _already_built(library: Path, stamp_at: Path, wanted: str) -> bool:
    return (
        library.is_file()
        and stamp_at.is_file()
        and stamp_at.read_text(encoding="utf-8").strip() == wanted
    )


def ensure(force: bool = False) -> Path:
    """Builds the archive if it is missing or out of date, and returns it.

    Call `rtlscope_site.prepend_path()` first: this needs g++ and ar.
    """
    if not site.WINDOWS:
        return _libs_dir() / LIB

    version = _version()
    root = _sources(version)
    vpi = root / "share" / "lib" / "vpi"
    sources = sorted(vpi.glob("*.cpp"))
    if not sources:
        raise site.ToolchainError(f"No VPI sources under {vpi}.")

    library = _libs_dir() / LIB
    stamp_at = CACHE / ".stamp"
    wanted = _stamp(version, sources)
    if not force and _already_built(library, stamp_at, wanted):
        return library

    with _one_builder_at_a_time():
        # Whoever held the lock may have just built exactly this.
        if not force and _already_built(library, stamp_at, wanted):
            return library
        return _build(library, stamp_at, wanted, version, root, vpi, sources)


def _build(library, stamp_at, wanted, version, root, vpi, sources) -> Path:
    print(f"[rtlscope] building {LIB} for cocotb {version} — once, then cached")
    objects = CACHE / "obj"
    objects.mkdir(parents=True, exist_ok=True)
    includes = [f"-I{root / 'share' / 'include'}", f"-I{root}", f"-I{vpi}"]

    built = []
    for source in sources:
        obj = objects / (source.stem + ".o")
        done = subprocess.run(
            ["g++", *CFLAGS, *DEFINES, *includes, "-c", str(source), "-o", str(obj)],
            capture_output=True,
            text=True,
        )
        if done.returncode != 0:
            raise site.ToolchainError(f"compiling {source.name} failed:\n{done.stderr[-4000:]}")
        built.append(str(obj))

    if library.is_file():
        library.unlink()
    done = subprocess.run(["ar", "rcs", str(library), *built], capture_output=True, text=True)
    if done.returncode != 0 or not library.is_file():
        raise site.ToolchainError(f"ar failed:\n{done.stderr}")

    stamp_at.write_text(wanted, encoding="utf-8")
    print(f"[rtlscope] built {library}")
    return library


if __name__ == "__main__":
    site.prepend_path()
    site.check_interpreter()
    ensure(force="--force" in sys.argv)
    print("ok")
"#;

#[cfg(test)]
mod tests {
    use super::*;

    /// Windows searches the current directory for an executable before it
    /// searches `PATH`, and `shutil.which` obeys that rule unless this variable
    /// says not to. cocotb then runs whatever it was handed from the *build*
    /// directory one level down, where a current-directory answer does not
    /// resolve.
    ///
    /// Measured from a window opened by its own Start-menu shortcut: no shell
    /// had set the variable, `which` answered `.\verilator.CMD`, and perl
    /// reported it could not open that script. From a terminal it never
    /// happened, because both PowerShell and MSYS bash set it themselves.
    #[test]
    fn the_current_directory_is_taken_out_of_the_executable_search() {
        assert!(
            SITE.contains(r#"os.environ["NoDefaultCurrentDirectoryInExePath"] = "1""#),
            "the rule has to be turned off before anything looks for a tool"
        );
    }

    /// And nothing that search could match is left in the current directory,
    /// so the two guards do not depend on each other.
    #[test]
    fn no_tool_is_written_beside_the_harness_where_the_search_would_find_it() {
        let names: Vec<String> = files().into_iter().map(|(name, _)| name).collect();

        assert!(names.contains(&SHIM.to_string()), "the shim is written: {names:?}");
        assert!(SHIM.contains('/'), "and into a directory of its own: {SHIM}");
        let beside: Vec<&String> =
            names.iter().filter(|name| !name.contains('/') && !name.ends_with(".py")).collect();
        assert!(
            beside.is_empty(),
            "a name Windows would run, in the working directory: {beside:?}"
        );
    }

    /// Everything this module works around is Verilator's. Icarus is looked
    /// for by its own program and asked for nothing else, so a machine with
    /// Icarus and no C++ compiler can play a pattern.
    #[test]
    fn icarus_is_looked_for_by_its_own_program() {
        assert!(SITE.contains("def prepend_path(engine"), "the engine is what decides");
        assert!(SITE.contains(r#""icarus": "iverilog.exe""#), "and Icarus is found by iverilog");
        assert!(
            SITE.contains(r#"if engine == "icarus":"#),
            "with its own branch, before anything of Verilator's is asked for"
        );
    }
}

/// The paths a simulator on this machine will not be able to open.
///
/// MSYS2's Verilator and Icarus reach the filesystem through Windows' **ANSI**
/// API, so a path holding a character the system code page cannot spell arrives
/// as `?` and the file is simply not found. The error the tool then prints
/// names a path with `??` in it that looks nothing like the one it was given,
/// and blames a missing include directory.
///
/// Measured on a machine whose code page is 1252: a directory called `é` opens,
/// one called `関係` does not, and `-f` file lists do not help — the characters
/// survive the argument and are lost at `fopen`. So this is not something to
/// work around, only something to say before an hour goes into it.
pub fn unspellable(paths: &[String]) -> Vec<String> {
    paths.iter().filter(|path| !spellable(path)).cloned().collect()
}

/// The code page paths are spelled in, for saying which one refused.
pub fn code_page() -> u32 {
    #[cfg(windows)]
    {
        unsafe { GetACP() }
    }
    #[cfg(not(windows))]
    {
        65001
    }
}

#[cfg(windows)]
unsafe extern "system" {
    fn GetACP() -> u32;
    fn WideCharToMultiByte(
        code_page: u32,
        flags: u32,
        wide: *const u16,
        wide_len: i32,
        multi: *mut u8,
        multi_len: i32,
        default_char: *const i8,
        used_default: *mut i32,
    ) -> i32;
}

/// Whether the system code page can spell this, which is what the tools need.
#[cfg(windows)]
fn spellable(text: &str) -> bool {
    // A UTF-8 code page spells everything — and asking about a default
    // character for one is an error rather than an answer.
    if code_page() == 65001 {
        return true;
    }
    let wide: Vec<u16> = text.encode_utf16().collect();
    if wide.is_empty() {
        return true;
    }
    let mut buffer = vec![0u8; wide.len() * 4 + 4];
    let default_char: i8 = b'?' as i8;
    let mut used_default: i32 = 0;
    let written = unsafe {
        WideCharToMultiByte(
            0, // CP_ACP
            0,
            wide.as_ptr(),
            wide.len() as i32,
            buffer.as_mut_ptr(),
            buffer.len() as i32,
            &default_char,
            &mut used_default,
        )
    };
    written > 0 && used_default == 0
}

#[cfg(not(windows))]
fn spellable(_text: &str) -> bool {
    true
}

#[cfg(test)]
mod code_page_tests {
    use super::*;

    /// The boundary is the system code page, not "is it ASCII". Measured on a
    /// machine spelling in 1252: a directory called `é` builds, one called
    /// `関係` does not, and the tool blames a missing include directory.
    #[test]
    fn a_path_the_code_page_cannot_spell_is_named_before_anything_is_written() {
        let ascii = "E:/work/rtl/top.sv".to_string();
        assert!(unspellable(std::slice::from_ref(&ascii)).is_empty(), "nothing to say about ASCII");

        let kanji = "E:/Nautilus/Documents/FPGA関係/top.v".to_string();
        let refused = unspellable(&[ascii, kanji.clone()]);
        match code_page() {
            // A UTF-8 code page spells everything, so there is nothing to
            // refuse — and this is the state the error tells a reader to reach.
            65001 => assert!(refused.is_empty(), "utf-8 spells it: {refused:?}"),
            _ => assert_eq!(refused, vec![kanji], "only the one it cannot spell"),
        }
    }
}
