//! Running a testbench somebody else wrote.
//!
//! Everything else in this crate *writes* a testbench: the design goes in, a
//! harness comes out, and the waveform is of a stimulus this program invented.
//! That answers "what does this design do", and it is the wrong tool for the
//! question a verification engineer actually has, which is "what does it do
//! **under my testbench**" — the one with the real bus transactions in it, the
//! one whose checks are the specification.
//!
//! So this is the other direction. The files are read for two facts and
//! otherwise left alone: **which module to start at**, and **whether it writes
//! a waveform of its own**. Nothing is generated over the top of them, nothing
//! is rewritten, and a testbench that already dumps is not given a second
//! dumper to race with.
//!
//! The design is still read separately, as a design. A testbench instantiates
//! the thing under test and is therefore a second module nothing instantiates —
//! hand both to elaboration at once and the answer to "which of these is the
//! design" becomes a coin toss. Keeping them apart is what lets the diagram go
//! on being of the design while the waveform is of the run.

use std::path::{Path, PathBuf};

use rtlscope_ir::{FileTable, Span};

use crate::run::{Engine, SimError, SimOutcome, Tools};

/// A testbench the reader wrote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bench {
    /// Its files, in the order they were given. Order matters to a compiler:
    /// a declaration has to arrive before its use.
    pub files: Vec<PathBuf>,
    /// The module the simulation starts at.
    pub top: String,
    /// Whether it writes a waveform itself.
    ///
    /// When it does, nothing is added. Two `$dumpfile` calls in one simulation
    /// is not twice the recording — it is two writers with one file between
    /// them, and which one wins is not something the reader can be told in
    /// advance.
    pub dumps: bool,
    /// What was noticed on the way, for a reader to check when the run is not
    /// what they expected.
    pub notes: Vec<String>,
    /// Where the top module is declared, against [`Bench::table`].
    ///
    /// A testbench is not part of the design, so its files are not in the
    /// design's table and a span into them cannot be resolved there. The
    /// bench carries its own table for the one thing a window wants to do
    /// with the span: show the file.
    pub span: Span,
    /// The bench's files, as the table [`Bench::span`] is a span into.
    pub table: FileTable,
    /// What the top instantiates, as `(instance, module)` pairs in the order
    /// written. The pair naming the design's top is where the two hierarchies
    /// join, and it is the one thing about a testbench a tree can draw.
    pub instances: Vec<(String, String)>,
}

/// Why a set of files could not be read as a testbench.
#[derive(Debug, thiserror::Error)]
pub enum BenchError {
    #[error("no file was given as a testbench")]
    Empty,
    #[error(
        "no module here could be a testbench: every one takes ports, and a testbench takes none.\n\
         Modules seen: {seen}"
    )]
    NoTop { seen: String },
    #[error(
        "more than one module here could be the testbench ({seen}); name the one to run.\n\
         A testbench is a module with no ports, and these files hold {count} of them."
    )]
    ManyTops { seen: String, count: usize },
    #[error("`{wanted}` is not a module in these files. They hold: {seen}")]
    NoSuchTop { wanted: String, seen: String },
    #[error("could not read {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
}

/// Reads a testbench: which module to start at, and whether it dumps.
///
/// `wanted` names the top when the reader knows it. Left out, the module with
/// **no ports** is taken, because that is what a testbench is — everything it
/// drives, it drives from inside itself. That rule is the one the file itself
/// carries, so it needs no naming convention and does not care whether
/// somebody calls their testbench `tb`, `testbench`, `top_tb` or `main`.
///
/// The parse is handed in rather than done here. This crate reads a `Design`
/// and writes text; it has never depended on the front end, and starting now
/// so that one function can count ports would put the SystemVerilog parser
/// underneath everything that generates a harness.
pub fn read(
    files: &[PathBuf],
    uir: &rtlscope_ir::UDesign,
    wanted: Option<&str>,
) -> Result<Bench, BenchError> {
    if files.is_empty() {
        return Err(BenchError::Empty);
    }
    // Read for the one thing the parse does not keep: whether the source calls
    // `$dumpvars`. A system task is not a module, a port or a statement this
    // subset lowers, so the text is where that fact still is.
    let mut text = String::new();
    for path in files {
        let read = std::fs::read_to_string(path)
            .map_err(|source| BenchError::Io { path: path.display().to_string(), source })?;
        text.push_str(&read);
        text.push('\n');
    }

    let named: Vec<&str> = uir.modules.iter().map(|module| module.name.as_str()).collect();
    let seen = || match named.is_empty() {
        true => "nothing".to_string(),
        false => named.join(", "),
    };

    let top = match wanted {
        Some(wanted) => match named.contains(&wanted) {
            true => wanted.to_string(),
            false => {
                return Err(BenchError::NoSuchTop { wanted: wanted.to_string(), seen: seen() });
            }
        },
        None => {
            let portless: Vec<&str> = uir
                .modules
                .iter()
                .filter(|module| module.ports.is_empty())
                .map(|module| module.name.as_str())
                .collect();
            match portless.as_slice() {
                [only] => (*only).to_string(),
                [] => return Err(BenchError::NoTop { seen: seen() }),
                many => {
                    return Err(BenchError::ManyTops { seen: many.join(", "), count: many.len() });
                }
            }
        }
    };

    // Said rather than assumed. A testbench that dumps and one that does not
    // are run differently, and a reader whose waveform came from somewhere
    // other than where they think has no way to notice.
    let dumps = live_code(&text).contains("$dumpvars");
    let mut notes = Vec::new();
    notes.push(match dumps {
        true => format!("`{top}` writes its own waveform; nothing was added to it"),
        false => format!(
            "`{top}` writes no waveform, so it is run inside `{WRAPPER}`, which records it — \
             every name in the recording is one component longer for it"
        ),
    });
    if named.len() > 1 {
        notes.push(format!("{} module(s) read as testbench: {}", named.len(), seen()));
    }

    // Where it is and what it holds, for a tree to draw. The module was
    // found by name above, so it is there.
    let module = uir.modules.iter().find(|module| module.name == top).expect("the top was named");
    let instances = module
        .items
        .iter()
        .filter_map(|item| match item {
            rtlscope_ir::UItem::Inst { inst } => {
                Some((inst.name.clone(), inst.module_name.clone()))
            }
            _ => None,
        })
        .collect();

    Ok(Bench {
        files: files.to_vec(),
        top,
        dumps,
        notes,
        span: module.span,
        table: uir.files.clone(),
        instances,
    })
}

/// The source with its comments taken out.
///
/// Commenting the dumping out is the ordinary way somebody makes a long run
/// quick, and a scan of the raw text reads that as "this one dumps" — so
/// nothing is added, nothing is written, and the reader is handed a run with no
/// waveform and no reason given. Measured on the first test written for this.
///
/// String literals are stepped over as well, since a `$dumpfile("a//b.vcd")`
/// would otherwise take the rest of its line with it.
fn live_code(text: &str) -> String {
    #[derive(PartialEq)]
    enum In {
        Code,
        Line,
        Block,
        Text,
    }
    let mut out = String::with_capacity(text.len());
    let mut state = In::Code;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match state {
            In::Code => match (c, chars.peek()) {
                ('/', Some('/')) => {
                    chars.next();
                    state = In::Line;
                }
                ('/', Some('*')) => {
                    chars.next();
                    state = In::Block;
                }
                _ => {
                    if c == '"' {
                        state = In::Text;
                    }
                    out.push(c);
                }
            },
            // Kept, so line numbers and the shape of the file survive for
            // anything that reads this after.
            In::Line => {
                if c == '\n' {
                    state = In::Code;
                    out.push(c);
                }
            }
            In::Block => {
                if c == '*' && chars.peek() == Some(&'/') {
                    chars.next();
                    state = In::Code;
                } else if c == '\n' {
                    out.push(c);
                }
            }
            In::Text => {
                // A backslash escape cannot end the string, and takes whatever
                // follows it with it.
                if c == '\\' {
                    chars.next();
                } else if c == '"' {
                    state = In::Code;
                }
                out.push(c);
            }
        }
    }
    out
}

/// What is written beside a testbench that does not dump.
const DUMPER: &str = "rtlscope_dump.sv";

/// The module a testbench that does not dump is run through.
pub const WRAPPER: &str = "rtlscope_top";

/// A module that instantiates the testbench and records it.
///
/// **A wrapper rather than a second root**, which is what this was first and
/// what does not work: a simulator runs the roots it is told to run, and both
/// of them are told exactly one — `-s` for Icarus, `--top-module` for
/// Verilator. A dumper sitting beside the testbench is simply never
/// elaborated, and the run comes back with the testbench's own output, a
/// passing result, and no waveform at all. Measured on both engines.
///
/// The reader's file is not touched either way. Their testbench is theirs, and
/// a tool that edited it to make its own job easier would have changed the
/// thing under test.
///
/// The cost is one level of hierarchy: what was `counter_tb.dut.count` is
/// `rtlscope_top.tb.dut.count` while the recorder is in. That is said out
/// loud rather than left to be discovered by somebody searching the dump for
/// a name that is now one component longer.
fn wrapper(top: &str, file: &str) -> String {
    format!(
        "// Written by RTLScope, around a testbench that records nothing.\n\
         // Your testbench is instantiated untouched; this only records it.\n\
         module {WRAPPER};\n    \
         {top} tb ();\n    \
         initial begin\n        \
         $dumpfile(\"{file}\");\n        \
         $dumpvars(0, {WRAPPER});\n    \
         end\n\
         endmodule\n"
    )
}

/// Every waveform under a directory, with when it was written.
fn dumps_under(dir: &Path) -> Vec<(PathBuf, std::time::SystemTime)> {
    let mut found = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else { return found };
    for entry in entries.flatten() {
        let path = entry.path();
        let is_dump = path
            .extension()
            .and_then(|it| it.to_str())
            .is_some_and(|it| it.eq_ignore_ascii_case("fst") || it.eq_ignore_ascii_case("vcd"));
        if !is_dump {
            continue;
        }
        let when = entry.metadata().and_then(|it| it.modified()).unwrap_or(std::time::UNIX_EPOCH);
        found.push((path, when));
    }
    found
}

/// Builds and runs a testbench, and says where the waveform went.
///
/// The design's own files come second on the command line, after the
/// testbench's: a compiler reads a file list in order, and the testbench is
/// what names the module it instantiates.
pub fn simulate(
    engine: Engine,
    work_dir: &Path,
    bench: &Bench,
    sources: &[PathBuf],
    tools: &Tools,
) -> Result<SimOutcome, SimError> {
    std::fs::create_dir_all(work_dir)
        .map_err(|source| SimError::Io { path: work_dir.display().to_string(), source })?;

    // Where a waveform would be if this wrote one, and what was there before
    // the run — so a dump left by an earlier run cannot be handed back as this
    // one's. Measured on the generated harness, where exactly that happened.
    let ours = work_dir.join(format!("{}.fst", bench.top));
    let _ = std::fs::remove_file(&ours);
    let before = dumps_under(work_dir);

    let mut files: Vec<PathBuf> = bench.files.clone();
    // The module the simulation starts at: theirs when it records itself, and
    // the wrapper around theirs when it does not.
    let mut top = bench.top.clone();
    if !bench.dumps {
        let at = work_dir.join(DUMPER);
        let name = ours.file_name().and_then(|it| it.to_str()).unwrap_or("dump.fst");
        std::fs::write(&at, wrapper(&bench.top, name))
            .map_err(|source| SimError::Io { path: at.display().to_string(), source })?;
        files.push(at);
        top = WRAPPER.to_string();
    }
    files.extend(sources.iter().cloned());

    let log = match engine {
        Engine::Verilator => verilator(work_dir, &top, &files, tools)?,
        Engine::Icarus => icarus(work_dir, &top, &files, tools)?,
    };

    // Ours if we asked for it; otherwise whatever the testbench wrote, which
    // it named itself and did not tell us about. Newest wins, and only if it
    // was not already there: a testbench that failed before its `$dumpfile`
    // leaves the last run's file sitting in the directory looking current.
    let dump = match ours.is_file() {
        true => Some(ours),
        false => dumps_under(work_dir)
            .into_iter()
            .filter(|(path, when)| !before.iter().any(|(had, then)| had == path && then >= when))
            .max_by_key(|(_, when)| *when)
            .map(|(path, _)| path),
    };
    let Some(dump) = dump else {
        return Err(SimError::NoDump {
            tool: engine.name().to_string(),
            expected: match bench.dumps {
                true => "a waveform from the testbench's own `$dumpfile`".to_string(),
                false => ours_expected(work_dir, &bench.top),
            },
            output: log,
        });
    };
    Ok(SimOutcome { dump, log, engine })
}

fn ours_expected(work_dir: &Path, top: &str) -> String {
    work_dir.join(format!("{top}.fst")).display().to_string()
}

/// Verilator: compile the testbench into a program, then run it.
fn verilator(
    work_dir: &Path,
    top: &str,
    files: &[PathBuf],
    tools: &Tools,
) -> Result<String, SimError> {
    let mut build = tools.start("verilator", Engine::Verilator)?;
    build
        .current_dir(work_dir)
        // `--binary` builds a whole testbench, which is what this is;
        // `--timing` is what makes `always #5` and `repeat (n) @(posedge clk)`
        // run rather than be rejected. `--trace-fst` is needed even when the
        // testbench calls `$dumpfile` itself: without it those calls compile
        // to nothing at all.
        .args(["--binary", "--timing", "--trace-fst", "-Wno-fatal", "-j", "0"])
        .args(["--Mdir", "obj", "-o", "rtlscope_sim"])
        .args(["--top-module", top])
        // Both explained under `run::quirks`.
        .args(["-CFLAGS", "-O2", "-CFLAGS", "-Wno-attributes"]);
    for file in files {
        build.arg(file);
    }
    if let Some(root) = crate::run::verilator_root(tools) {
        build.env("VERILATOR_ROOT", root);
    }
    let mut log = crate::run::run(&mut build, "verilator", Engine::Verilator)?;

    let program = ["rtlscope_sim.exe", "rtlscope_sim"]
        .iter()
        .map(|name| work_dir.join("obj").join(name))
        .find(|path| path.is_file())
        .ok_or_else(|| SimError::NoDump {
            tool: "verilator".to_string(),
            expected: "program to run".to_string(),
            output: log.clone(),
        })?;

    let mut sim = std::process::Command::new(&program);
    sim.current_dir(work_dir);
    // Verilated code is linked against the compiler's runtime and has to find
    // that one, not whichever copy the launching shell saw first.
    if let Some(tool) = tools.locate("verilator") {
        crate::run::alongside(&mut sim, &tool);
    }
    log.push_str(&crate::run::run(&mut sim, &program.display().to_string(), Engine::Verilator)?);
    Ok(log)
}

/// Icarus: compile to bytecode, then interpret it.
fn icarus(
    work_dir: &Path,
    top: &str,
    files: &[PathBuf],
    tools: &Tools,
) -> Result<String, SimError> {
    let vvp = format!("{top}.vvp");
    let mut build = tools.start("iverilog", Engine::Icarus)?;
    build.current_dir(work_dir).args(["-g2012", "-o", &vvp, "-s", top]);
    for file in files {
        build.arg(file);
    }
    let mut log = crate::run::run(&mut build, "iverilog", Engine::Icarus)?;

    let mut sim = tools.start("vvp", Engine::Icarus)?;
    // `-fst` asks for the FST writer; a testbench whose `$dumpfile` names a
    // `.vcd` gets one anyway, and is found by looking rather than by assuming.
    sim.current_dir(work_dir).arg(&vvp).arg("-fst");
    log.push_str(&crate::run::run(&mut sim, "vvp", Engine::Icarus)?);
    Ok(log)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rtlscope-bench-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("a scratch directory");
        dir
    }

    fn write(dir: &Path, name: &str, text: &str) -> PathBuf {
        let at = dir.join(name);
        std::fs::write(&at, text).expect("the file");
        at
    }

    /// What a caller hands in: the files, parsed.
    fn parsed(files: &[PathBuf]) -> rtlscope_ir::UDesign {
        rtlscope_sv::lower_files(files, &rtlscope_sv::ParseOptions::default()).0
    }

    const TB: &str = "module counter_tb;\n\
         logic clk = 0, rst_n = 0, en = 1;\n\
         logic [7:0] count;\n\
         counter dut (.*);\n\
         always #5 clk = ~clk;\n\
         initial begin\n\
         $dumpfile(\"counter_tb.fst\");\n\
         $dumpvars(0, counter_tb);\n\
         #20 rst_n = 1;\n\
         #200 $finish;\n\
         end\n\
         endmodule\n";

    /// The rule that needs no naming convention: a testbench drives everything
    /// from inside itself, so it is the module with no ports. `tb`, `testbench`,
    /// `top_tb`, `main` — the file says which it is without being told.
    #[test]
    fn the_module_with_no_ports_is_the_testbench() {
        let dir = scratch("top");
        let path = write(&dir, "counter_tb.sv", TB);
        let bench = read(std::slice::from_ref(&path), &parsed(std::slice::from_ref(&path)), None)
            .expect("a testbench");
        assert_eq!(bench.top, "counter_tb");
        assert!(bench.dumps, "it calls $dumpvars, so nothing should be added");
    }

    /// What a tree needs to draw it: where the top is declared, in a table the
    /// span resolves against, and what it instantiates — the pair naming the
    /// design is where a testbench's hierarchy joins the design's.
    #[test]
    fn a_testbench_says_where_it_is_and_what_it_holds() {
        let dir = scratch("holds");
        let path = write(&dir, "counter_tb.sv", TB);
        let bench = read(std::slice::from_ref(&path), &parsed(std::slice::from_ref(&path)), None)
            .expect("a testbench");

        assert_eq!(bench.instances, [("dut".to_string(), "counter".to_string())]);
        assert_eq!(bench.span.line, 1, "declared on the first line of the fixture");
        let file = bench.table.path(bench.span.file).expect("the span has a file");
        assert!(file.ends_with("counter_tb.sv"), "in its own table: {}", file.display());
    }

    /// A testbench that records nothing is given a recorder, and says so. The
    /// reader gets a waveform either way, and is told which of the two
    /// happened — a recording from somewhere other than where they think is
    /// not something they can notice on their own.
    #[test]
    fn a_testbench_that_does_not_dump_is_given_a_dumper_and_told() {
        let dir = scratch("nodump");
        let path = write(
            &dir,
            "quiet_tb.sv",
            &TB.replace("$dumpfile", "// $dumpfile").replace("$dumpvars", "// $dumpvars"),
        );
        let bench = read(std::slice::from_ref(&path), &parsed(std::slice::from_ref(&path)), None)
            .expect("a testbench");
        assert!(!bench.dumps);
        // The note has to carry the consequence, not just the fact: every
        // name in the recording gains a component, and somebody searching the
        // dump for `counter_tb.dut.count` will not find it.
        let note = bench.notes.first().cloned().unwrap_or_default();
        assert!(note.contains("rtlscope_top"), "{note}");
        assert!(note.contains("one component longer"), "{note}");
        // The module's name, not the file's: `quiet_tb.sv` holds `counter_tb`.
        assert_eq!(bench.top, "counter_tb");
        let written = wrapper(&bench.top, "x.fst");
        assert!(written.contains("counter_tb tb ()"), "the testbench is instantiated: {written}");
        assert!(written.contains("$dumpvars(0, rtlscope_top)"), "{written}");
        assert!(written.contains("$dumpfile(\"x.fst\")"), "{written}");
    }

    /// Commenting the dumping out to make a long run quick is ordinary, and a
    /// scan of the raw text reads it as "this one dumps" — leaving the reader
    /// with no waveform and nothing said about why.
    #[test]
    fn a_dump_that_is_commented_out_does_not_count() {
        assert!(!live_code("// $dumpvars(0, tb);\n").contains("$dumpvars"));
        assert!(!live_code("/* $dumpvars(0, tb); */\n").contains("$dumpvars"));
        assert!(live_code("  $dumpvars(0, tb); // on purpose\n").contains("$dumpvars"));
        // A `//` inside a string does not comment out the rest of the line.
        assert!(
            live_code("$dumpfile(\"a//b.vcd\"); $dumpvars(0, tb);\n").contains("$dumpvars"),
            "a slash in a filename swallowed the line"
        );
    }

    /// Two candidates is a question, not a guess. Picking one would put the
    /// reader's checks in a simulation that never ran them.
    #[test]
    fn two_modules_without_ports_are_a_question() {
        let dir = scratch("two");
        let path = write(&dir, "two_tb.sv", &format!("{TB}module other_tb;\nendmodule\n"));
        let said = read(std::slice::from_ref(&path), &parsed(std::slice::from_ref(&path)), None)
            .expect_err("ambiguous")
            .to_string();
        assert!(said.contains("counter_tb") && said.contains("other_tb"), "{said}");
        assert!(said.contains("name the one to run"), "{said}");
    }

    /// And naming one settles it.
    #[test]
    fn naming_the_top_settles_it() {
        let dir = scratch("named");
        let path = write(&dir, "two_tb.sv", &format!("{TB}module other_tb;\nendmodule\n"));
        let uir = parsed(std::slice::from_ref(&path));
        let bench = read(std::slice::from_ref(&path), &uir, Some("other_tb")).expect("named");
        assert_eq!(bench.top, "other_tb");

        let said = read(std::slice::from_ref(&path), &uir, Some("nope"))
            .expect_err("not there")
            .to_string();
        assert!(said.contains("nope") && said.contains("counter_tb"), "{said}");
    }

    /// A design with no portless module is not a testbench, and saying so
    /// beats simulating the wrong thing.
    #[test]
    fn a_design_offered_as_a_testbench_is_refused_by_name() {
        let dir = scratch("design");
        let path = write(
            &dir,
            "counter.sv",
            &std::fs::read_to_string(rtlscope_fixtures::path("counter.sv")).unwrap(),
        );
        let said = read(std::slice::from_ref(&path), &parsed(std::slice::from_ref(&path)), None)
            .expect_err("not a testbench")
            .to_string();
        assert!(said.contains("takes none"), "{said}");
        assert!(said.contains("counter"), "and names what it did see: {said}");
    }
}
