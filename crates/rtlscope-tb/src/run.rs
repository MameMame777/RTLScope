//! Running a generated harness, so a dump can be had from sources alone.
//!
//! Everything else in this crate writes text and stops there, deliberately: the
//! caller decides where files land and whether to overwrite. This is the one
//! part that runs something, and it exists because "generate a testbench, go
//! and simulate it yourself, then come back" is three steps too many for
//! someone who only wants to look at a waveform of their own design.
//!
//! Two simulators, because they fail in different places. Verilator compiles
//! the design to C++ and needs a working C++ toolchain; Icarus interprets and
//! needs nothing but itself. When one is missing or broken the other usually is
//! not, and neither is asked to be the only way in.
//!
//! Nothing here goes through a shell. `Command` is given a program and its
//! arguments, which is why the trap that has cost this project the most —
//! Icarus started from a POSIX shell exiting 127 with empty stderr, because it
//! builds its own pipeline with Windows paths — cannot be sprung.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::cocotb::TbError;

/// How long any one tool may run before it is stopped.
///
/// A simulation is bounded in the design's time — the harness counts cycles and
/// finishes — but nothing bounded it in ours. A clock that never toggles never
/// reaches the last cycle, a combinational loop never settles, and a C++ build
/// of a large design can simply take longer than anybody is prepared to wait
/// staring at a spinner with no way out. Then the window turns forever and the
/// only remedy is killing it, losing whatever else was open.
///
/// Five minutes is chosen to be far longer than a build that is going to
/// finish, and far shorter than forever. `RTLSCOPE_SIM_SECONDS` moves it, for the
/// design where it turns out to be wrong.
pub fn budget() -> Duration {
    let said = std::env::var("RTLSCOPE_SIM_SECONDS").ok().and_then(|it| it.parse().ok());
    Duration::from_secs(said.unwrap_or(300))
}

/// How often the wait wakes up to see whether the tool has finished.
///
/// Short enough that a quick build is not padded by it, long enough that
/// waiting costs nothing.
const POLL: Duration = Duration::from_millis(25);

/// Which simulator to run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Engine {
    /// Compiles the design to C++ and runs that. Fast, and needs a C++
    /// compiler that works.
    #[default]
    Verilator,
    /// Interprets the design. Slower, and needs nothing but itself.
    Icarus,
}

impl Engine {
    pub fn name(self) -> &'static str {
        match self {
            Engine::Verilator => "verilator",
            Engine::Icarus => "icarus",
        }
    }

    /// How to install it, for an error that would otherwise be a dead end.
    pub fn how_to_get_it(self) -> &'static str {
        match self {
            Engine::Verilator => "pacman -S mingw-w64-ucrt-x86_64-verilator",
            Engine::Icarus => "pacman -S mingw-w64-ucrt-x86_64-iverilog",
        }
    }
}

/// Where the outside tools are, for a window nobody started from a shell.
///
/// The installer registers `rtlscope-gui.exe` as what opens a `.sv` file, so the
/// ordinary way in is a double-click — and a process started that way inherits
/// no terminal's `PATH` and stands in whatever directory Explorer chose. Both
/// of the assumptions this module used to make, "the simulator is on the PATH"
/// and "the venv is beside the working directory", are true only for a
/// developer running from a checkout. Everybody else needs somewhere to say it,
/// and this is what a settings file fills in.
///
/// Empty is the old behaviour exactly: the `PATH`, and a `.venv-cocotb` found
/// by looking upwards. Nothing here replaces a search, it only goes first.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Tools {
    /// The interpreter to play a pattern with, named outright.
    pub python: Option<PathBuf>,
    /// Where the simulator lives, looked in ahead of the `PATH`.
    pub sim_dir: Option<PathBuf>,
    /// Directories to look for a `.venv-cocotb` in and above. The design's own,
    /// normally: a checkout that has a venv keeps it at its root, which is
    /// somewhere above the RTL.
    pub near: Vec<PathBuf>,
}

impl Tools {
    /// Anchored at the design's own files, which is the one location a window
    /// always knows — it was given them to open.
    ///
    /// Files rather than directories, because that is what the caller holds;
    /// the directory each one is in is what gets looked in.
    pub fn near_sources(mut self, sources: &[PathBuf]) -> Tools {
        for source in sources {
            if let Some(dir) = source.parent()
                && !self.near.iter().any(|had| had == dir)
            {
                self.near.push(dir.to_path_buf());
            }
        }
        self
    }

    /// Where to look for a program: what was chosen, then the `PATH`.
    fn search_path(&self) -> Vec<PathBuf> {
        let mut dirs = Vec::new();
        if let Some(chosen) = &self.sim_dir {
            // Forgiving about being handed `verilator.exe` rather than the
            // directory holding it: a file picker gives back the file, and both
            // name the same place. Refusing one of them would be pedantry
            // wearing the face of a missing simulator.
            let dir = match chosen.is_file() {
                true => chosen.parent().map(Path::to_path_buf),
                false => Some(chosen.clone()),
            };
            dirs.extend(dir);
        }
        dirs.extend(search_path());
        dirs
    }

    /// The `PATH` a child that looks tools up by name should be given: what
    /// was chosen, then this process's own. `None` when nothing was chosen,
    /// so the child inherits its environment untouched.
    ///
    /// This is how the settings box reaches the generated harness. It
    /// resolves `iverilog` and `verilator_bin.exe` by name, and a window
    /// started from a shortcut has no other way to say where they are.
    pub fn child_path(&self) -> Option<std::ffi::OsString> {
        self.sim_dir.as_ref()?;
        std::env::join_paths(self.search_path()).ok()
    }

    /// Where a tool actually is, looking the way Windows would if it bothered.
    ///
    /// Not `Command::new(name)`. Spawning a process on Windows does not consult
    /// `PATHEXT`, and a `.bat` is not an executable image in any case — it needs
    /// a command interpreter to read it. MSYS2 ships `verilator` as exactly
    /// that, so the obvious call finds nothing and the tool looks uninstalled on
    /// a machine where it plainly is.
    pub fn locate(&self, name: &str) -> Option<PathBuf> {
        for dir in self.search_path() {
            for extension in ["exe", "bat", "cmd", ""] {
                let candidate = match extension {
                    "" => dir.join(name),
                    _ => dir.join(format!("{name}.{extension}")),
                };
                if candidate.is_file() {
                    return Some(candidate);
                }
            }
        }
        None
    }

    /// A command that will start a program named the way a shell would name it,
    /// or nothing if there is no such program.
    ///
    /// Not `Command::new(name)`, for two reasons Windows gives and never
    /// explains. `CreateProcess` does not consult `PATHEXT`, so a bare name only
    /// ever finds an `.exe` — measured with VS Code, whose `code` on the `PATH`
    /// is `code.cmd`, and whose absence comes back as "The system cannot find
    /// the file specified". And a `.cmd` is not an executable image at all: it
    /// needs a command interpreter to read it.
    pub fn program(&self, name: &str) -> Option<Command> {
        let tool = self.locate(name)?;
        let batch = tool
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("bat") || ext.eq_ignore_ascii_case("cmd"));
        let mut command = if batch {
            let mut command = Command::new("cmd");
            command.arg("/C").arg(&tool);
            command
        } else {
            Command::new(&tool)
        };
        alongside(&mut command, &tool);
        Some(command)
    }

    /// A command that will start this tool, with its own libraries in front.
    pub fn start(&self, name: &str, engine: Engine) -> Result<Command, SimError> {
        self.program(name).ok_or_else(|| SimError::ToolMissing {
            tool: name.to_string(),
            how: engine.how_to_get_it(),
        })
    }

    /// Whether a simulator's tools can be found at all.
    pub fn available(&self, engine: Engine) -> bool {
        let tools: &[&str] = match engine {
            Engine::Verilator => &["verilator"],
            Engine::Icarus => &["iverilog", "vvp"],
        };
        tools.iter().all(|tool| self.start(tool, engine).is_ok())
    }

    /// The places a `.venv-cocotb` is looked for, each with what it is, in the
    /// order they are tried.
    ///
    /// Named in the error when there is none. "Make one beside this directory"
    /// without saying which directory is how somebody ends up with a second
    /// venv in a place nothing will look — measured, on this machine, with a
    /// working venv already sitting at the root of the checkout.
    fn venv_anchors(&self) -> Vec<(PathBuf, &'static str)> {
        let mut anchors: Vec<(PathBuf, &'static str)> =
            self.near.iter().map(|dir| (dir.clone(), "the design")).collect();
        if let Ok(cwd) = std::env::current_dir() {
            anchors.push((cwd, "where this was started"));
        }
        if let Some(dir) =
            std::env::current_exe().ok().and_then(|exe| exe.parent().map(Path::to_path_buf))
        {
            anchors.push((dir, "where the program is"));
        }
        anchors
    }

    /// The Python to play a pattern with.
    ///
    /// In order: the one named outright, then `RTLSCOPE_PYTHON`, then a
    /// `.venv-cocotb` in or above each anchor. The walk upwards is what makes
    /// this work from a window: the venv lives at the root of a checkout and
    /// the RTL somewhere under it, so a design file is enough to find one.
    pub fn python(&self) -> Result<PathBuf, SimError> {
        if let Some(named) = &self.python {
            // Only when it is really there. A stale path in a settings file
            // should say so by name, not turn into "no Python" and send its
            // reader off to build a venv they already have.
            if named.is_file() {
                return Ok(named.clone());
            }
            return Err(SimError::NoPython {
                looked: format!("    {} (named in settings, and not there)", named.display()),
            });
        }
        if let Some(set) = std::env::var_os("RTLSCOPE_PYTHON") {
            let named = PathBuf::from(set);
            if named.is_file() {
                return Ok(named);
            }
        }

        venv_from(&self.venv_anchors())
    }
}

/// The first `.venv-cocotb` in or above any of these, or all of them by name.
///
/// Apart from [`Tools::python`] so it can be tried against directories chosen
/// by a test: two of the three anchors are the working directory and the
/// program's own, and a checkout that happens to have a venv at its root — the
/// normal state of this one — would answer for them and hide whatever was
/// being tested.
fn venv_from(anchors: &[(PathBuf, &'static str)]) -> Result<PathBuf, SimError> {
    // Two anchors are the same directory more often than not — a command line
    // run from where the program sits, a window started beside its own exe —
    // and a list that says one place twice reads as two places tried, which is
    // the opposite of what this message is for.
    let mut looked: Vec<String> = Vec::new();
    let mut seen: Vec<&Path> = Vec::new();
    for (anchor, what) in anchors {
        if seen.contains(&anchor.as_path()) {
            continue;
        }
        seen.push(anchor);
        looked.push(format!("    {} ({what})", anchor.display()));
        for dir in anchor.ancestors() {
            if let Some(python) = venv_in(dir) {
                return Ok(python);
            }
        }
    }
    Err(SimError::NoPython { looked: looked.join("\n") })
}

/// The interpreter of a `.venv-cocotb` directly under this directory.
///
/// Both layouts, because both happen: an MSYS2 venv puts its programs in `bin`
/// and a native Windows one in `Scripts`, and the ucrt64 Python this needs
/// makes the first kind.
fn venv_in(dir: &Path) -> Option<PathBuf> {
    for candidate in ["bin/python.exe", "Scripts/python.exe", "bin/python3", "bin/python"] {
        let python = dir.join(".venv-cocotb").join(candidate);
        if python.is_file() {
            return Some(python);
        }
    }
    None
}

/// What a run produced.
#[derive(Debug, Clone)]
pub struct SimOutcome {
    pub dump: PathBuf,
    /// Everything both tools said, kept whole. A simulation that ran but
    /// warned is the interesting case, and summarising it away is how the
    /// warning stops being seen.
    pub log: String,
    pub engine: Engine,
}

#[derive(Debug, thiserror::Error)]
pub enum SimError {
    #[error(
        "`{tool}` is not on the PATH.\n  Install it with `{how}`, and make sure its directory \
         is on the PATH — a window started from Explorer often does not inherit a shell's."
    )]
    ToolMissing { tool: String, how: &'static str },
    #[error("`{tool}` failed ({code}):\n{output}")]
    Failed { tool: String, code: String, output: String },
    #[error(
        "`{tool}` was still running after {seconds}s and was stopped.\n  A clock that never \
         toggles never reaches the last cycle, and a large design can take longer to build than \
         this allows. Set RTLSCOPE_SIM_SECONDS to wait longer.\n{output}"
    )]
    TookTooLong { tool: String, seconds: u64, output: String },
    #[error("`{tool}` said it succeeded but wrote no {expected}:\n{output}")]
    NoDump {
        tool: String,
        expected: String,
        /// Everything the tool said. Kept because this error is reached exactly
        /// when the obvious signals — the exit code, the file — disagree with
        /// each other, and then the log is the only thing left that knows why.
        output: String,
    },
    #[error("could not write the harness into `{path}`: {source}")]
    Io { path: String, source: std::io::Error },
    #[error(
        "no Python to play the pattern with.\n  Looked for a `.venv-cocotb` in and above:\n\
         {looked}\n  Name one under `settings`, or make one where it is looked for:\n    \
         <msys2>/ucrt64/bin/python.exe -m venv .venv-cocotb\n    \
         .venv-cocotb/bin/python.exe -m pip install cocotb\n  On Windows it has to be the \
         ucrt64 Python: cocotb's VPI is linked into a binary that toolchain built."
    )]
    NoPython {
        /// The directories, already laid out one to a line and said with what
        /// each one is. A message that names no directory is what made this
        /// error cost an afternoon.
        looked: String,
    },
    #[error("cocotb could not play the pattern:\n{output}")]
    Cocotb { output: String },
    #[error(transparent)]
    Tb(#[from] TbError),
}

/// Puts the generated files where they go, making the directories they name.
///
/// A name may hold a directory of its own — the Perl shim lives one level down,
/// away from the working directory, so `shutil.which` cannot answer with a path
/// relative to it. `write` alone fails on a parent that is not there yet.
pub fn write_files(work: &Path, files: &[(String, String)]) -> Result<(), SimError> {
    for (name, contents) in files {
        let path = work.join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|source| SimError::Io { path: parent.display().to_string(), source })?;
        }
        std::fs::write(&path, contents)
            .map_err(|source| SimError::Io { path: path.display().to_string(), source })?;
    }
    Ok(())
}

/// Writes a cocotb harness into a directory and plays it.
///
/// Everything platform-specific lives in the generated `run.py` and the files
/// [`crate::toolchain`] writes beside it, so all this has to do is find a
/// Python and start one. The harness is the authority on whether that Python
/// can work, and when it cannot, what it says is better than anything added
/// here.
///
/// A drawn expectation that did not hold is an *answer*, not a breakdown, so a
/// non-zero exit is only an error when the run left no verdict behind — then
/// something really did go wrong.
pub fn play(
    work: &Path,
    base: &str,
    files: &[(String, String)],
    tools: &Tools,
    engine: Engine,
) -> Result<SimOutcome, SimError> {
    std::fs::create_dir_all(work)
        .map_err(|source| SimError::Io { path: work.display().to_string(), source })?;
    write_files(work, files)?;

    // Nothing an earlier run in this directory left behind may survive into
    // this one. The verdict is load-bearing: it is what decides whether a
    // non-zero exit is an *answer* or a breakdown, so a stale one turned a real
    // failure into "cocotb said it succeeded but wrote no waveform it could
    // name" — with the log that said what actually went wrong thrown away.
    // Measured on the second run in one work directory, which for a window is
    // every run after the first.
    let verdict_at = work.join(format!("{base}_verdict.json"));
    let _ = std::fs::remove_file(&verdict_at);
    for name in crate::cocotb::dump_names(base) {
        let _ = std::fs::remove_file(work.join(name));
    }
    // And where the Perl shim used to live, which is this directory. A work
    // directory made by an earlier version still holds one, and Windows looks
    // in the current directory before `PATH` — so the old copy would be found
    // ahead of the one that moved, and answered relative. See
    // [`crate::toolchain::SHIM`].
    let _ = std::fs::remove_file(work.join("verilator.cmd"));

    let python = tools.python()?;

    let mut command = Command::new(&python);
    command
        .current_dir(work)
        .arg("run.py")
        // A POSIX shell reaching the simulator eats the backslashes out of the
        // Windows paths `make` builds. See this module's own doc comment.
        .env_remove("SHELL")
        .env_remove("MAKESHELL")
        .env("RTLSCOPE_ENGINE", engine.name());
    // The directory the reader named for the simulator goes first on the
    // child's PATH, exactly as it goes first in this process's own search.
    if let Some(path) = tools.child_path() {
        command.env("PATH", path);
    }
    let done = command
        .output()
        .map_err(|source| SimError::Io { path: python.display().to_string(), source })?;
    let log = format!(
        "{}{}",
        String::from_utf8_lossy(&done.stdout),
        String::from_utf8_lossy(&done.stderr)
    );

    if !done.status.success() && !verdict_at.is_file() {
        return Err(SimError::Cocotb { output: log });
    }

    // Where the waveform went is the simulator's decision — cocotb's Verilator
    // runner writes `dump.vcd` beside the test, Icarus an FST under the build
    // directory — so the harness reports it rather than this guessing.
    let dump = log
        .lines()
        .rev()
        .find_map(|line| line.strip_prefix("RTLSCOPE_DUMP="))
        .map(|path| PathBuf::from(path.trim()))
        .ok_or_else(|| SimError::NoDump {
            tool: "cocotb".to_string(),
            expected: "waveform it could name".to_string(),
            output: log.clone(),
        })?;
    if !dump.is_file() {
        return Err(SimError::NoDump {
            tool: "cocotb".to_string(),
            expected: format!("waveform at {}", dump.display()),
            output: log,
        });
    }
    Ok(SimOutcome { dump, log, engine })
}

/// Writes a generated harness into a directory and simulates it.
///
/// `files` is what [`crate::generate_with`] produced, `sources` the design's
/// own files. What comes back is the dump both of them made between them.
pub fn simulate(
    engine: Engine,
    work_dir: &Path,
    top: &str,
    files: &[(String, String)],
    sources: &[PathBuf],
    tools: &Tools,
) -> Result<SimOutcome, SimError> {
    std::fs::create_dir_all(work_dir)
        .map_err(|source| SimError::Io { path: work_dir.display().to_string(), source })?;
    write_files(work_dir, files)?;

    // The harness is the only file that has to be named; the design's own
    // sources come from the caller as absolute paths.
    let harness = format!("tb_{top}.sv");
    let dump = work_dir.join(format!("{top}.fst"));
    // A stale dump would otherwise be reported as this run's.
    let _ = std::fs::remove_file(&dump);

    let log = match engine {
        Engine::Verilator => verilator(work_dir, &harness, sources, tools)?,
        Engine::Icarus => icarus(work_dir, top, &harness, sources, tools)?,
    };

    if !dump.is_file() {
        return Err(SimError::NoDump {
            tool: engine.name().to_string(),
            expected: dump.display().to_string(),
            output: log,
        });
    }
    Ok(SimOutcome { dump, log, engine })
}

/// Verilator: build the design into a program, then run it.
fn verilator(
    work_dir: &Path,
    harness: &str,
    sources: &[PathBuf],
    tools: &Tools,
) -> Result<String, SimError> {
    let mut build = tools.start("verilator", Engine::Verilator)?;
    build
        .current_dir(work_dir)
        // `--binary` wants a whole testbench, which is exactly what the SV
        // flavour writes; `--timing` is what makes `always #5` and
        // `repeat (n) @(posedge clk)` run rather than be rejected.
        .args(["--binary", "--timing", "--trace-fst", "-Wno-fatal", "-j", "0"])
        .args(["--Mdir", "obj", "-o", "rtlscope_sim"])
        // Two workarounds, both explained under `quirks` below.
        .args(["-CFLAGS", "-O2", "-CFLAGS", "-Wno-attributes"])
        .arg(harness);
    for source in sources {
        build.arg(source);
    }
    if let Some(root) = verilator_root(tools) {
        build.env("VERILATOR_ROOT", root);
    }
    let mut log = run(&mut build, "verilator", Engine::Verilator)?;

    let program = ["rtlscope_sim.exe", "rtlscope_sim"]
        .iter()
        .map(|name| work_dir.join("obj").join(name))
        .find(|path| path.is_file())
        .ok_or_else(|| SimError::NoDump {
            tool: "verilator".to_string(),
            expected: "program to run".to_string(),
            output: log.clone(),
        })?;

    let mut sim = Command::new(&program);
    sim.current_dir(work_dir);
    // Verilated code is linked against the compiler's runtime, so it needs the
    // same libraries the build used — not whichever copy of them the launching
    // shell happens to find first.
    if let Some(tool) = tools.locate("verilator") {
        alongside(&mut sim, &tool);
    }
    log.push_str(&run(&mut sim, &program.display().to_string(), Engine::Verilator)?);
    Ok(log)
}

/// Icarus: compile to bytecode, then interpret it.
fn icarus(
    work_dir: &Path,
    top: &str,
    harness: &str,
    sources: &[PathBuf],
    tools: &Tools,
) -> Result<String, SimError> {
    let vvp = format!("tb_{top}.vvp");
    let mut build = tools.start("iverilog", Engine::Icarus)?;
    build.current_dir(work_dir).args(["-g2012", "-o", &vvp]).arg(harness);
    for source in sources {
        build.arg(source);
    }
    let mut log = run(&mut build, "iverilog", Engine::Icarus)?;

    let mut sim = tools.start("vvp", Engine::Icarus)?;
    sim.current_dir(work_dir).arg(&vvp).arg("-fst");
    log.push_str(&run(&mut sim, "vvp", Engine::Icarus)?);
    Ok(log)
}

fn search_path() -> Vec<PathBuf> {
    std::env::var_os("PATH")
        .map(|paths| std::env::split_paths(&paths).collect())
        .unwrap_or_default()
}

/// Puts one directory at the front of a command's `PATH`.
///
/// Windows resolves a DLL by searching `PATH`, and two installations of the
/// same simulator on one machine — the normal state of an FPGA workstation —
/// means the first one found wins for the *libraries* even when the second was
/// chosen for the *program*. Started that way, Icarus dies with
/// `STATUS_ENTRYPOINT_NOT_FOUND` before printing anything, which reads exactly
/// like nothing happening; a Verilated binary does the same, because it is
/// linked against the compiler's runtime and has to find that one.
pub(crate) fn alongside(command: &mut Command, tool: &Path) {
    let Some(dir) = tool.parent() else { return };
    let ahead = std::iter::once(dir.to_path_buf()).chain(search_path());
    if let Ok(joined) = std::env::join_paths(ahead) {
        command.env("PATH", joined);
    }
}

/// Whether a simulator's tools can be found on the `PATH` alone.
///
/// [`Tools::available`] is the same question asked of a reader who has said
/// where their simulator is; this is it asked from a shell, where the `PATH` is
/// the answer.
pub fn available(engine: Engine) -> bool {
    Tools::default().available(engine)
}

/// Runs one command and keeps everything it said.
pub(crate) fn run(command: &mut Command, tool: &str, engine: Engine) -> Result<String, SimError> {
    // Measured, and the reason this used to work from PowerShell and fail from
    // Git Bash: `mingw32-make` runs its recipes through `$SHELL` when one is
    // set, and a POSIX shell eats the backslashes out of the Windows paths in
    // the compile line — the include directory arrives as `E:Nautilus...` and
    // nothing is found. With it unset, make uses the command interpreter, which
    // leaves them alone. Removing it here is what makes a simulation behave the
    // same however the tool was started, including from Explorer.
    command.env_remove("SHELL").env_remove("MAKESHELL");

    // Spawned rather than run to completion, so there is somewhere to stand
    // while it works and a moment to decide it has had long enough.
    let mut child = match command.stdout(Stdio::piped()).stderr(Stdio::piped()).spawn() {
        Ok(child) => child,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(SimError::ToolMissing {
                tool: tool.to_string(),
                how: engine.how_to_get_it(),
            });
        }
        Err(error) => {
            return Err(SimError::Failed {
                tool: tool.to_string(),
                code: error.kind().to_string(),
                output: error.to_string(),
            });
        }
    };

    // Drained on threads of their own. A pipe holds a few kilobytes and then
    // blocks the writer: a tool chatty enough to fill one would hang waiting
    // for a reader that is itself waiting for the tool, and the timeout below
    // would be reporting a deadlock this function caused.
    let drain = |from: Option<Box<dyn std::io::Read + Send>>| {
        std::thread::spawn(move || {
            let mut buffer = Vec::new();
            if let Some(mut from) = from {
                let _ = std::io::Read::read_to_end(&mut from, &mut buffer);
            }
            buffer
        })
    };
    let out = drain(child.stdout.take().map(|it| Box::new(it) as Box<dyn std::io::Read + Send>));
    let err = drain(child.stderr.take().map(|it| Box::new(it) as Box<dyn std::io::Read + Send>));

    let allowed = budget();
    let began = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if began.elapsed() < allowed => std::thread::sleep(POLL),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                let mut said =
                    String::from_utf8_lossy(&out.join().unwrap_or_default()).into_owned();
                said.push_str(&String::from_utf8_lossy(&err.join().unwrap_or_default()));
                return Err(SimError::TookTooLong {
                    tool: tool.to_string(),
                    seconds: allowed.as_secs(),
                    output: said,
                });
            }
            Err(error) => {
                return Err(SimError::Failed {
                    tool: tool.to_string(),
                    code: error.kind().to_string(),
                    output: error.to_string(),
                });
            }
        }
    };

    let mut said = String::from_utf8_lossy(&out.join().unwrap_or_default()).into_owned();
    said.push_str(&String::from_utf8_lossy(&err.join().unwrap_or_default()));
    if !status.success() {
        return Err(SimError::Failed {
            tool: tool.to_string(),
            code: match status.code() {
                Some(code) => code.to_string(),
                None => "killed".to_string(),
            },
            output: said,
        });
    }
    Ok(said)
}

// --------------------------------------------------------------- quirks ---

// Verilator's default `-Os` does not link on the MSYS2 ucrt64 toolchain: gcc
// 16.1.0 emits an out-of-line reference to the `std::string` move constructor
// that the libstdc++ shipped beside it does not export, so every C++ file that
// moves a string fails at the link step — Verilator's own runtime included.
// At `-O2` the constructor is inlined and the reference never appears.
//
// Measured here, and independently arrived at by a neighbouring project,
// whose cocotb harness documents the same thing as its workaround #6.
// Preferred over `-D_GLIBCXX_USE_CXX11_ABI=0`, which also links but changes the
// ABI of everything built — fine for a self-contained binary, a trap the moment
// anything pre-built is linked against.

/// Where Verilator keeps its own sources, with forward slashes.
///
/// `VERILATOR_ROOT` for the build, with forward slashes.
///
/// Also measured: the MSYS2 wrapper hands `VERILATOR_ROOT` to `make` with
/// backslashes, which the shell underneath eats — the build then fails looking
/// for `E:\Nautilusprogramfilesmsys...` with the separators gone. Setting it
/// with forward slashes survives every layer.
///
/// One already set wins. Otherwise it is `share/verilator` beside the
/// `verilator` that was found — which is what the settings box points at, and
/// how an MSYS2 anywhere but the usual places gets it right. Only when that
/// directory is actually there; a Verilator laid out some other way is left to
/// find its own.
pub(crate) fn verilator_root(tools: &Tools) -> Option<String> {
    if let Ok(set) = std::env::var("VERILATOR_ROOT") {
        return Some(set.replace('\\', "/"));
    }
    let tool = tools.locate("verilator")?;
    let share = tool.parent()?.parent()?.join("share").join("verilator");
    share.is_dir().then(|| share.display().to_string().replace('\\', "/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A missing simulator has to say what to install, or the message is a
    /// dead end for exactly the person who most needs it.
    #[test]
    fn a_missing_tool_names_the_way_to_get_it() {
        let error = SimError::ToolMissing {
            tool: "verilator".to_string(),
            how: Engine::Verilator.how_to_get_it(),
        };
        let said = error.to_string();
        assert!(said.contains("pacman -S mingw-w64-ucrt-x86_64-verilator"), "{said}");
        assert!(said.contains("PATH"), "{said}");
    }

    /// The whole output of a failed build is the answer; a summary of it is
    /// not.
    #[test]
    fn a_failure_keeps_everything_the_tool_said() {
        let error = SimError::Failed {
            tool: "iverilog".to_string(),
            code: "1".to_string(),
            output: "tb.sv:12: syntax error\ntb.sv:12: error: malformed statement\n".to_string(),
        };
        let said = error.to_string();
        assert!(said.contains("tb.sv:12: syntax error"), "{said}");
        assert!(said.contains("malformed statement"), "{said}");
    }

    /// This error is reached exactly when the signals disagree — the tool
    /// exited fine, the file is not there — so the log is the only thing left
    /// that knows why. It used to be thrown away, leaving one sentence and
    /// nothing to act on.
    #[test]
    fn a_run_that_left_no_waveform_still_says_what_the_tool_said() {
        let error = SimError::NoDump {
            tool: "cocotb".to_string(),
            expected: "waveform it could name".to_string(),
            output: "ModuleNotFoundError: No module named 'cocotb'\n".to_string(),
        };
        let said = error.to_string();
        assert!(said.contains("wrote no waveform it could name"), "{said}");
        assert!(said.contains("No module named 'cocotb'"), "the evidence:\n{said}");
    }

    #[test]
    fn each_engine_names_itself() {
        assert_eq!(Engine::Verilator.name(), "verilator");
        assert_eq!(Engine::Icarus.name(), "icarus");
        assert_eq!(Engine::default(), Engine::Verilator);
    }

    /// A scratch directory that takes itself away again.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Scratch {
            let path = std::env::temp_dir().join(name);
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).expect("a scratch directory");
            Scratch(path)
        }

        /// An empty file where the path says, directories and all. Only its
        /// being there is under test; nothing runs it.
        fn touch(&self, at: &str) -> PathBuf {
            let path = self.0.join(at);
            std::fs::create_dir_all(path.parent().expect("a parent")).expect("the directories");
            std::fs::write(&path, "").expect("the file");
            path
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// The bug this type was written for.
    ///
    /// A window is handed a design file and nothing else it can trust: its
    /// working directory is whatever Explorer chose, and the directory it was
    /// installed into holds no venv. The venv sits at the root of the checkout
    /// the design is somewhere inside, so the walk upwards is the whole answer.
    #[test]
    fn a_venv_above_the_design_is_found() {
        let scratch = Scratch::new("rtlscope-tools-above");
        let python = scratch.touch(".venv-cocotb/bin/python.exe");
        let source = scratch.touch("rtl/sub/top.sv");

        let tools = Tools::default().near_sources(&[source]);
        assert_eq!(tools.python().expect("the venv two directories up"), python);
    }

    /// A native Windows venv keeps its programs somewhere else, and a reader
    /// who made one of those should not be told there is nothing there.
    #[test]
    fn a_venv_is_found_under_either_of_the_two_layouts() {
        let scratch = Scratch::new("rtlscope-tools-scripts");
        let python = scratch.touch(".venv-cocotb/Scripts/python.exe");
        let source = scratch.touch("top.sv");

        let tools = Tools::default().near_sources(&[source]);
        assert_eq!(tools.python().expect("the venv beside the design"), python);
    }

    /// The message that cost an afternoon: it prescribed a remedy — make a venv
    /// "beside this directory" — without saying which directory, so the remedy
    /// was carried out somewhere nothing would look, on a machine that already
    /// had a working venv.
    #[test]
    fn having_no_python_names_every_place_it_looked() {
        let looked = [
            (PathBuf::from("E:/work/proj/rtl"), "the design"),
            (PathBuf::from("C:/Windows/System32"), "where this was started"),
            (PathBuf::from("C:/Program Files/RTLScope"), "where the program is"),
        ];
        let said = venv_from(&looked).expect_err("nothing is there").to_string();

        for (dir, what) in &looked {
            assert!(said.contains(&dir.display().to_string()), "{dir:?} is not named:\n{said}");
            assert!(said.contains(what), "{what} is not said:\n{said}");
        }
        assert!(said.contains("settings"), "nowhere to put one:\n{said}");
        assert!(said.contains("ucrt64"), "which Python it has to be:\n{said}");
    }

    /// Measured from the command line: run the program from the directory it
    /// sits in, and two of the three anchors are that directory. Saying it
    /// twice reads as two places tried.
    #[test]
    fn one_directory_wearing_two_hats_is_listed_once() {
        let here = PathBuf::from("C:/tools/rtlscope");
        let said = venv_from(&[
            (here.clone(), "where this was started"),
            (here.clone(), "where the program is"),
        ])
        .expect_err("nothing is there")
        .to_string();

        assert_eq!(said.matches("C:/tools/rtlscope").count(), 1, "{said}");
        assert!(said.contains("where this was started"), "the first name for it:\n{said}");
    }

    /// A stale path in a settings file is its own answer. Falling through to
    /// the search would report "no Python" to somebody looking at the box they
    /// filled in, which reads as the setting being ignored.
    #[test]
    fn a_python_named_and_not_there_is_said_by_name() {
        let tools = Tools {
            python: Some(PathBuf::from("E:/gone/.venv-cocotb/bin/python.exe")),
            ..Tools::default()
        };
        let said = tools.python().expect_err("not there").to_string();
        assert!(said.contains("E:/gone"), "{said}");
        assert!(said.contains("named in settings"), "{said}");
    }

    /// What the settings box is for: the tool is found without the `PATH`
    /// having anything to say, which is the state of every window opened by
    /// double-clicking a file.
    #[test]
    fn a_named_directory_is_looked_in_before_the_path() {
        let scratch = Scratch::new("rtlscope-tools-simdir");
        let tool = scratch.touch("verilator.exe");

        let tools = Tools { sim_dir: Some(scratch.0.clone()), ..Tools::default() };
        assert_eq!(tools.locate("verilator"), Some(tool));
        assert!(tools.available(Engine::Verilator), "one tool is all Verilator needs");
    }

    /// A `.cmd` is not an executable image: Windows needs a command interpreter
    /// to read it, and `CreateProcess` will not do that for you. Measured with
    /// VS Code, whose name on the `PATH` is `code.cmd` — spawned directly it
    /// comes back "The system cannot find the file specified".
    #[test]
    fn a_batch_file_is_started_through_the_interpreter_that_can_read_it() {
        let scratch = Scratch::new("rtlscope-tools-batch");
        scratch.touch("editor.cmd");

        let tools = Tools { sim_dir: Some(scratch.0.clone()), ..Tools::default() };
        let command = tools.program("editor").expect("found by its extension");
        assert_eq!(command.get_program(), "cmd", "a batch file needs cmd to read it");

        let args: Vec<_> = command.get_args().collect();
        assert_eq!(args.first().copied(), Some("/C".as_ref()), "{args:?}");
        assert!(
            args.last().is_some_and(|a| a.to_string_lossy().ends_with("editor.cmd")),
            "{args:?}"
        );
    }

    /// Nothing by that name is not an error here — the caller decides what to
    /// say about it, and for an editor that is not "install a simulator".
    #[test]
    fn a_program_that_is_not_there_is_simply_absent() {
        let scratch = Scratch::new("rtlscope-tools-absent");
        let tools = Tools { sim_dir: Some(scratch.0.clone()), ..Tools::default() };
        assert!(tools.program("nothing-by-this-name").is_none());
    }

    /// A file picker gives back the program, not the directory holding it.
    #[test]
    fn the_simulator_box_takes_the_program_as_well_as_its_directory() {
        let scratch = Scratch::new("rtlscope-tools-simfile");
        let tool = scratch.touch("iverilog.exe");
        let _ = scratch.touch("vvp.exe");

        let tools = Tools { sim_dir: Some(tool.clone()), ..Tools::default() };
        assert_eq!(tools.locate("iverilog"), Some(tool));
        assert!(tools.available(Engine::Icarus), "both of Icarus's tools are there");
    }

    /// A work directory made by an earlier version still holds the Perl shim
    /// where it used to live: beside the harness, which is the run's own
    /// current directory. Windows looks there before `PATH`, so the old copy
    /// would be found ahead of the one that moved — and found relative.
    ///
    /// Runs without a simulator or a Python: the clearing happens before
    /// anything is looked for, so a run that cannot start still proves it.
    #[test]
    fn the_shim_an_older_version_left_in_the_way_is_taken_out() {
        let scratch = Scratch::new("rtlscope-old-shim");
        let stale = scratch.touch("verilator.cmd");
        assert!(stale.is_file(), "the landmine is planted");

        let tools =
            Tools { python: Some(PathBuf::from("E:/nowhere/python.exe")), ..Tools::default() };
        let played = play(&scratch.0, "counter", &[], &tools, Engine::Verilator);

        assert!(matches!(played, Err(SimError::NoPython { .. })), "it got as far as the Python");
        assert!(!stale.is_file(), "and cleared the old shim on the way: {}", stale.display());
    }

    /// Sources share a directory far more often than not, and looking in the
    /// same one forty times is forty times the filesystem for one answer.
    #[test]
    fn the_same_directory_is_only_anchored_once() {
        let tools = Tools::default().near_sources(&[
            PathBuf::from("E:/work/rtl/a.sv"),
            PathBuf::from("E:/work/rtl/b.sv"),
            PathBuf::from("E:/work/other/c.sv"),
        ]);
        assert_eq!(tools.near, vec![PathBuf::from("E:/work/rtl"), PathBuf::from("E:/work/other")]);
    }

    /// What the settings box says reaches the generated harness: the chosen
    /// directory leads the `PATH` the child is given, and nothing chosen
    /// leaves that environment alone.
    #[test]
    fn the_chosen_directory_leads_the_path_a_child_is_given() {
        let scratch = Scratch::new("rtlscope-tools-child-path");
        let tools = Tools { sim_dir: Some(scratch.0.clone()), ..Tools::default() };
        let given = tools.child_path().expect("a PATH to hand on");
        let first = std::env::split_paths(&given).next().expect("the chosen directory");
        assert_eq!(first, scratch.0);
        assert!(Tools::default().child_path().is_none(), "nothing chosen, nothing changed");
    }

    /// `VERILATOR_ROOT` is found beside the `verilator` that will run, which is
    /// what makes an MSYS2 anywhere but the usual places build: the wrapper's
    /// own guess arrives at `make` with its backslashes eaten.
    #[test]
    fn verilator_root_is_taken_from_beside_the_tool_that_was_found() {
        if std::env::var_os("VERILATOR_ROOT").is_some() {
            eprintln!("skipping: VERILATOR_ROOT is set in this environment, and wins");
            return;
        }
        let scratch = Scratch::new("rtlscope-tools-verilator-root");
        let tool = scratch.touch("ucrt64/bin/verilator.bat");
        std::fs::create_dir_all(scratch.0.join("ucrt64/share/verilator")).expect("share");
        let tools = Tools { sim_dir: Some(tool), ..Tools::default() };
        let root = verilator_root(&tools).expect("a root beside the tool");
        assert!(!root.contains('\\'), "forward slashes, for the shell under make: {root}");
        assert!(root.ends_with("/ucrt64/share/verilator"), "{root}");

        // Without the directory there is nothing to say, and the wrapper is
        // left to itself.
        let bare = Scratch::new("rtlscope-tools-verilator-bare");
        let lone = bare.touch("bin/verilator.bat");
        let tools = Tools { sim_dir: Some(lone), ..Tools::default() };
        assert_eq!(verilator_root(&tools), None);
    }
}
