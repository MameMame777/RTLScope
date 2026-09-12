//! Reading a Veryl project: by having Veryl say what it means in SystemVerilog.
//!
//! Veryl is a language that compiles to SystemVerilog, and its compiler writes
//! two things RTLScope can use as they are. The `.sv` it emits is ordinary
//! synthesisable SystemVerilog, which the front end in `rtlscope-sv` reads
//! without knowing where it came from. And beside every `.sv` it leaves a
//! `.sv.map` — a Source Map, the same format browsers use for minified
//! JavaScript — saying which line of which `.veryl` each token came from;
//! `rtlscope-sv` reads those too, so every span in the design lands in the file
//! the author wrote. What is left for this crate is small: find the project,
//! ask Veryl to build it, and say which files it wrote.
//!
//! Veryl is a program on the machine rather than a library in this one, for
//! the same reason Verilator and Yosys are. A Veryl project is more than its
//! files — a `Veryl.toml` with clock and reset conventions, a standard library,
//! dependencies fetched from git — and the compiler is the only thing that
//! knows all of that. Linking its crates would mean re-implementing the parts
//! around them and pulling a dozen crates and a git client into every build of
//! this tool, to save a reader who writes Veryl an install they have already
//! done.
//!
//! Which files it wrote is read from the filelist Veryl writes for exactly
//! this purpose, `<name>.f` in the project root: the same one-path-per-line
//! format `rtlscope -f` takes. Scanning the output directory instead would pick
//! up whatever an earlier build left behind and — with the standard library
//! emitted beside the project by default — some sixty modules the design never
//! asked for.
//!
//! Two things about `veryl build` shape the code here, both measured on
//! 0.21.0. It must be run from inside the project, or it says only that
//! `Veryl.toml` was not found from wherever it was run. And **it exits 0
//! whether or not the build succeeded**: a syntax error is printed, nothing is
//! written, and the exit code says nothing about it. So failure is read from
//! the report it prints and from what it left on disk, never from the code.

use std::path::{Path, PathBuf};
use std::process::Command;

use rtlscope_ir::{Diag, DiagCode, Diagnostics, FileTable, Severity, Span};

/// The file that marks a directory as a Veryl project.
pub const MANIFEST: &str = "Veryl.toml";

/// A Veryl project: where it is, and what it is called.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Project {
    /// The directory holding `Veryl.toml`, canonical.
    pub root: PathBuf,
    /// The `Veryl.toml` itself.
    pub manifest: PathBuf,
    /// `[project] name`, which is what Veryl names its filelist after and
    /// prefixes every generated module with.
    pub name: String,
}

/// Whether a path is something Veryl, rather than something SystemVerilog.
///
/// A `.veryl` file, a `Veryl.toml`, or a directory with one in it. Not a
/// directory somewhere *under* a project — see [`project_of`] for that, which
/// is a question rather than a test.
pub fn is_veryl(path: &Path) -> bool {
    if path.is_dir() {
        return path.join(MANIFEST).is_file();
    }
    let manifest = path
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.eq_ignore_ascii_case(MANIFEST));
    manifest || path.extension().is_some_and(|ext| ext.eq_ignore_ascii_case("veryl"))
}

/// The project a path belongs to: the nearest `Veryl.toml` at or above it.
///
/// A `.veryl` file cannot be built on its own — its clock convention, its
/// project prefix and the modules it instantiates all live elsewhere in the
/// project — so a file named is the project named.
pub fn project_of(path: &Path) -> Option<Project> {
    let start = if path.is_dir() { path } else { path.parent()? };
    let mut here = Some(start);
    while let Some(dir) = here {
        let manifest = dir.join(MANIFEST);
        if manifest.is_file() {
            let root = dunce::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
            let manifest = root.join(MANIFEST);
            // The manifest's own name for the project, or the directory's when
            // it has none: Veryl would refuse the latter, and the reader will
            // hear that from Veryl rather than from a guess made here.
            let name = project_name(&manifest)
                .or_else(|| root.file_name().map(|name| name.to_string_lossy().into_owned()))?;
            return Some(Project { root, manifest, name });
        }
        here = dir.parent();
    }
    None
}

/// `name = "…"` under `[project]`, read by eye rather than by a TOML parser:
/// it is one line, and the manifest is Veryl's to interpret.
fn project_name(manifest: &Path) -> Option<String> {
    let text = std::fs::read_to_string(manifest).ok()?;
    let mut in_project = false;
    for line in text.lines() {
        let line = line.trim();
        if let Some(section) = line.strip_prefix('[') {
            in_project = section.trim_end_matches(']').trim() == "project";
            continue;
        }
        if !in_project {
            continue;
        }
        if let Some(rest) = line.strip_prefix("name")
            && let Some(value) = rest.trim_start().strip_prefix('=')
        {
            let value = value.split('#').next().unwrap_or("").trim();
            let name = value.trim_matches(|c| c == '"' || c == '\'');
            if !name.is_empty() {
                return Some(name.to_string());
            }
        }
    }
    None
}

impl Project {
    /// Where Veryl writes the list of files it generated.
    pub fn filelist_path(&self) -> PathBuf {
        self.root.join(format!("{}.f", self.name))
    }

    /// The SystemVerilog files the last build wrote, absolute, in the order
    /// Veryl listed them. `None` when there has never been a build.
    ///
    /// Relative entries are relative to the project root, which is where
    /// Veryl writes the list and what `filelist_type = "relative"` means.
    /// Separators are taken either way round: a list written on Windows reads
    /// on Linux, which matters for one checked into a repository.
    pub fn filelist(&self) -> Option<Vec<PathBuf>> {
        let text = std::fs::read_to_string(self.filelist_path()).ok()?;
        let paths = text
            .lines()
            .map(|line| line.split("//").next().unwrap_or("").trim())
            .filter(|line| !line.is_empty())
            .map(|line| {
                let line = line.strip_prefix(r"\\?\").unwrap_or(line).replace('\\', "/");
                let path = Path::new(&line);
                if path.is_absolute() { path.to_path_buf() } else { self.root.join(path) }
            })
            .collect();
        Some(paths)
    }

    /// Every file whose change should have the project built again: the
    /// sources, and the manifest that says how to build them.
    ///
    /// Generated files are left out. They change *because* of a build, and
    /// watching them would read every build as an edit.
    pub fn watch(&self) -> Vec<PathBuf> {
        let mut found = vec![self.manifest.clone()];
        let mut stack = vec![self.root.clone()];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else { continue };
            let mut here: Vec<PathBuf> =
                entries.filter_map(|entry| Some(entry.ok()?.path())).collect();
            here.sort();
            for path in here {
                if path.is_dir() {
                    let name = path.file_name().map(|n| n.to_string_lossy().into_owned());
                    let skip = name.as_deref().is_none_or(|name| {
                        name.starts_with('.')
                            || matches!(name, "target" | "dependencies" | "node_modules")
                    });
                    if !skip {
                        stack.push(path);
                    }
                } else if path.extension().is_some_and(|ext| ext.eq_ignore_ascii_case("veryl")) {
                    found.push(path);
                }
            }
        }
        found
    }
}

/// Something `veryl build` said, kept until there is a file table to place it
/// in — the table belongs to the design, which does not exist yet when Veryl
/// runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    pub severity: Severity,
    pub message: String,
    /// The `.veryl` file and the 1-based line and column, when Veryl said.
    pub location: Option<(PathBuf, u32, u32)>,
}

impl Report {
    fn error(message: impl Into<String>, location: Option<(PathBuf, u32, u32)>) -> Self {
        Report { severity: Severity::Error, message: message.into(), location }
    }

    fn warning(message: impl Into<String>) -> Self {
        Report { severity: Severity::Warning, message: message.into(), location: None }
    }
}

/// What a set of paths turned out to mean, with every Veryl project among them
/// built and replaced by the SystemVerilog it produced.
#[derive(Debug, Default)]
pub struct Expanded {
    /// What to hand to the SystemVerilog front end, in the order given: a
    /// plain `.sv` where one was named, and a project's generated files where
    /// the project was.
    pub sources: Vec<PathBuf>,
    /// What to watch for changes: the plain sources as they are, and for a
    /// project its `.veryl` files and manifest rather than its output.
    pub watch: Vec<PathBuf>,
    /// The projects met, each once.
    pub projects: Vec<Project>,
    /// What Veryl had to say, to become diagnostics.
    pub reports: Vec<Report>,
}

impl Expanded {
    /// Whether any of the paths was Veryl.
    pub fn any_veryl(&self) -> bool {
        !self.projects.is_empty()
    }

    /// Veryl's reports as diagnostics, placed in the files they name.
    ///
    /// The table is the design's own, so a location Veryl gave — a line of a
    /// `.veryl` — renders and links like every other span in the design.
    pub fn diagnostics(&self, files: &mut FileTable) -> Diagnostics {
        let mut diags = Diagnostics::default();
        for report in &self.reports {
            let mut diag =
                Diag::new(report.severity, DiagCode::TranspileFailed, report.message.clone());
            if let Some((path, line, col)) = &report.location {
                let canonical = dunce::canonicalize(path).unwrap_or_else(|_| path.clone());
                diag = diag.at(Span::new(files.intern(canonical), *line, *col, 1));
            }
            diags.push(diag);
        }
        diags
    }
}

/// What to watch for changes among the paths named, without building anything.
///
/// A plain source is watched as itself. A Veryl path stands for its project,
/// whose `.veryl` files and manifest are what change by hand — the same list
/// [`Expanded::watch`] gives, for a caller that only wants to know whether to
/// read again and not yet to read.
pub fn watched(paths: &[PathBuf]) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut projects: Vec<PathBuf> = Vec::new();
    for path in paths {
        if !is_veryl(path) {
            out.push(path.clone());
            continue;
        }
        if let Some(project) = project_of(path)
            && !projects.contains(&project.root)
        {
            out.extend(project.watch());
            projects.push(project.root);
        }
    }
    out
}

/// Turns the paths a reader named into the ones the front end reads.
///
/// Anything that is not Veryl passes through untouched. Each Veryl project is
/// built once, however many of its files were named, and stands in for them.
pub fn expand(paths: &[PathBuf]) -> Expanded {
    expand_with(paths, locate().as_deref())
}

/// The same, with the `veryl` to use said explicitly — or `None` for a
/// machine without one, which a test can then be about.
pub fn expand_with(paths: &[PathBuf], tool: Option<&Path>) -> Expanded {
    let mut out = Expanded::default();
    for path in paths {
        if !is_veryl(path) {
            out.sources.push(path.clone());
            out.watch.push(path.clone());
            continue;
        }
        let Some(project) = project_of(path) else {
            out.reports.push(Report::error(
                format!(
                    "{}: no {MANIFEST} in any directory above it, so Veryl cannot build it",
                    path.display()
                ),
                None,
            ));
            continue;
        };
        if out.projects.iter().any(|known| known.root == project.root) {
            continue;
        }
        let built = build_with(&project, tool);
        out.sources.extend(built.sources);
        out.reports.extend(built.reports);
        out.watch.extend(project.watch());
        out.projects.push(project);
    }
    out
}

/// What one build produced.
#[derive(Debug, Default)]
pub struct Built {
    /// The generated SystemVerilog, from the filelist.
    pub sources: Vec<PathBuf>,
    pub reports: Vec<Report>,
    /// Everything Veryl printed, for a reader who wants the whole story.
    pub log: String,
}

/// Builds a project, or reads what the last build left when it cannot.
pub fn build(project: &Project) -> Built {
    build_with(project, locate().as_deref())
}

/// The same, with the `veryl` to use said explicitly.
pub fn build_with(project: &Project, tool: Option<&Path>) -> Built {
    let Some(tool) = tool else {
        // No Veryl on this machine. What it wrote last time is still a faithful
        // reading of the sources as they were then, and says so.
        return match project.filelist() {
            Some(sources) if !sources.is_empty() => Built {
                sources,
                reports: vec![Report::warning(format!(
                    "veryl is not installed, so `{}` is read as the SystemVerilog Veryl last \
                     wrote for it; edits to the .veryl files will not show until it is. To \
                     install it: cargo install verylup && verylup setup",
                    project.name
                ))],
                log: String::new(),
            },
            _ => Built {
                sources: Vec::new(),
                reports: vec![Report::error(
                    format!(
                        "veryl is not installed and `{}` has never been built, so there is no \
                         SystemVerilog to read. To install it: cargo install verylup && verylup \
                         setup",
                        project.name
                    ),
                    None,
                )],
                log: String::new(),
            },
        };
    };

    // From inside the project: that is the only place `veryl build` looks for
    // the manifest. Quiet, so the log is what went wrong and not a line for
    // every file it processed.
    let output = Command::new(tool).arg("build").arg("--quiet").current_dir(&project.root).output();
    let output = match output {
        Ok(output) => output,
        Err(error) => {
            return Built {
                sources: project.filelist().unwrap_or_default(),
                reports: vec![Report::error(
                    format!(
                        "could not run {} to build `{}`: {error}",
                        tool.display(),
                        project.name
                    ),
                    None,
                )],
                log: String::new(),
            };
        }
    };
    let mut log = String::from_utf8_lossy(&output.stdout).into_owned();
    log.push_str(&String::from_utf8_lossy(&output.stderr));

    let mut reports = parse_report(&log);
    // The filelist is from this build when it succeeded, and from the last
    // one when it did not — Veryl writes nothing on a failed build and removes
    // nothing either. Either way it is the design as it last compiled, which
    // with the error beside it is the most useful thing to show.
    let sources = project.filelist().unwrap_or_default();
    if sources.is_empty() && reports.is_empty() {
        reports.push(Report::error(
            format!(
                "`veryl build` wrote no filelist for `{}` and reported nothing: {}",
                project.name,
                log.trim()
            ),
            None,
        ));
    }
    Built { sources, reports, log }
}

/// Reads Veryl's report back as locations.
///
/// Veryl prints its diagnostics the way `miette` draws them: a line with `×`
/// and the message, then a frame whose top edge names the file, line and
/// column as `╭─[path:line:col]`. That is a display format, not a contract,
/// so this reads the two things it needs and keeps the message for anything
/// it did not understand.
pub fn parse_report(log: &str) -> Vec<Report> {
    let mut reports = Vec::new();
    let mut severity = Severity::Error;
    let mut lines = log.lines().peekable();
    while let Some(line) = lines.next() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("Warning") {
            severity = Severity::Warning;
        } else if trimmed.starts_with("Error") {
            severity = Severity::Error;
        }
        let Some((_, message)) = line.split_once('×') else { continue };
        let message = message.trim();
        // The summary Veryl prints ahead of the real reports, which say more.
        if message.is_empty() || message == "veryl check failed" {
            continue;
        }
        // The frame's top edge, on the next line or the one after.
        let mut location = None;
        for _ in 0..2 {
            let Some(next) = lines.peek() else { break };
            if let Some(found) = location_in(next) {
                location = Some(found);
                lines.next();
                break;
            }
            if next.contains('×') {
                break;
            }
            lines.next();
        }
        reports.push(Report { severity, message: message.to_string(), location });
    }
    reports
}

/// `path:line:col` out of a `╭─[…]` frame edge.
fn location_in(line: &str) -> Option<(PathBuf, u32, u32)> {
    let start = line.find("╭─[")? + "╭─[".len();
    let end = line[start..].rfind(']')? + start;
    let inside = &line[start..end];
    let mut parts = inside.rsplitn(3, ':');
    let col: u32 = parts.next()?.trim().parse().ok()?;
    let row: u32 = parts.next()?.trim().parse().ok()?;
    let path = parts.next()?.trim();
    let path = path.strip_prefix(r"\\?\").unwrap_or(path);
    Some((PathBuf::from(path), row, col))
}

/// Where `veryl` is on this machine, if anywhere.
///
/// `RTLSCOPE_VERYL` names one outright; otherwise `PATH`, looked at the way
/// Windows would if it bothered: a spawn does not consult `PATHEXT`, so the
/// extensions are tried by hand. `verylup` puts a `veryl.exe` in `~/.cargo/bin`,
/// which is on `PATH` on any machine with Rust.
pub fn locate() -> Option<PathBuf> {
    if let Some(named) = std::env::var_os("RTLSCOPE_VERYL") {
        let named = PathBuf::from(named);
        return named.is_file().then_some(named);
    }
    let dirs =
        std::env::var_os("PATH").map(|paths| std::env::split_paths(&paths).collect::<Vec<_>>());
    for dir in dirs.unwrap_or_default() {
        for extension in ["exe", "cmd", "bat", ""] {
            let candidate = match extension {
                "" => dir.join("veryl"),
                _ => dir.join(format!("veryl.{extension}")),
            };
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> PathBuf {
        rtlscope_fixtures::veryl_project()
    }

    /// Somewhere disposable, holding a copy of the fixture project.
    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rtlscope-veryl-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        copy_tree(&fixture(), &dir);
        dir
    }

    fn copy_tree(from: &Path, to: &Path) {
        std::fs::create_dir_all(to).unwrap();
        for entry in std::fs::read_dir(from).unwrap() {
            let entry = entry.unwrap();
            let target = to.join(entry.file_name());
            if entry.path().is_dir() {
                copy_tree(&entry.path(), &target);
            } else {
                std::fs::copy(entry.path(), target).unwrap();
            }
        }
    }

    #[test]
    fn a_file_names_its_project_and_the_project_names_itself() {
        let project = project_of(&fixture().join("src").join("top.veryl")).expect("a project");
        assert_eq!(project.root, dunce::canonicalize(fixture()).unwrap());
        assert_eq!(project.name, "lights", "from [project] name, not the directory");
        assert!(project.manifest.ends_with(MANIFEST));

        // A directory anywhere under the project is the project too.
        assert_eq!(
            project_of(&fixture().join("target")).map(|p| p.name).as_deref(),
            Some("lights")
        );
        // And a directory with no manifest above it is nobody's.
        assert_eq!(project_of(&std::env::temp_dir()), None);
    }

    #[test]
    fn what_counts_as_veryl() {
        assert!(is_veryl(Path::new("src/top.veryl")));
        assert!(is_veryl(Path::new("SRC/TOP.VERYL")), "case is not the point");
        assert!(is_veryl(Path::new("some/where/Veryl.toml")));
        assert!(is_veryl(&fixture()), "a directory holding a manifest");
        assert!(!is_veryl(&fixture().join("src")), "but not one under it — that is a question");
        assert!(!is_veryl(Path::new("top.sv")));
        assert!(!is_veryl(Path::new("Cargo.toml")));
    }

    /// The list Veryl writes is read relative to the project, whichever way its
    /// separators lean.
    #[test]
    fn the_filelist_is_read_relative_to_the_project() {
        let project = project_of(&fixture()).unwrap();
        let files = project.filelist().expect("the fixture is built");
        assert_eq!(files.len(), 6);
        assert!(files.iter().all(|path| path.is_absolute() && path.is_file()), "{files:?}");
        // In the order Veryl wrote them, which is dependency order: the
        // package and the interface before the modules that use them.
        assert!(
            files[0].ends_with("target/defs.sv") || files[0].ends_with("target\\defs.sv"),
            "{files:?}"
        );

        // Backslashes and the verbatim prefix, as Veryl writes them on Windows.
        let dir = std::env::temp_dir().join("rtlscope-veryl-filelist");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("target")).unwrap();
        std::fs::write(dir.join(MANIFEST), "[project]\nname = \"p\"\n").unwrap();
        std::fs::write(dir.join("target").join("a.sv"), "module a; endmodule\n").unwrap();
        let listed = format!("target{}a.sv // the generated one\n\n", '\\');
        std::fs::write(dir.join("p.f"), listed).unwrap();
        let project = project_of(&dir).unwrap();
        let files = project.filelist().unwrap();
        assert_eq!(files.len(), 1);
        assert!(files[0].is_file(), "{}", files[0].display());
    }

    #[test]
    fn what_is_watched_is_what_is_written_by_hand() {
        let project = project_of(&fixture()).unwrap();
        let watched = project.watch();
        assert!(watched.iter().any(|p| p.ends_with(MANIFEST)));
        assert_eq!(
            watched.iter().filter(|p| p.extension().is_some_and(|e| e == "veryl")).count(),
            6
        );
        assert!(
            !watched.iter().any(|p| p.extension().is_some_and(|e| e == "sv")),
            "not the output"
        );
    }

    /// The report Veryl prints, read back to the line it points at. Both
    /// texts are what 0.21.0 printed, verbatim.
    #[test]
    fn a_report_is_read_back_to_its_line() {
        let syntax = concat!(
            "Error: ParserError::SyntaxError\n",
            "\n",
            "  × Unexpected token: ';'\n",
            "   ╭─[\\\\?\\C:\\work\\broken\\src\\bad.veryl:5:20]\n",
            " 4 │     var a: logic<8>;\n",
            " 5 │     assign a = b + ;\n",
            "   ·                    ┬\n",
            "   ·                    ╰── Error location\n",
            " 6 │ }\n",
            "   ╰────\n",
            "  help: \n",
        );
        let reports = parse_report(syntax);
        assert_eq!(reports.len(), 1, "{reports:?}");
        assert_eq!(reports[0].severity, Severity::Error);
        assert_eq!(reports[0].message, "Unexpected token: ';'");
        let (path, line, col) = reports[0].location.clone().expect("a location");
        assert_eq!(path, PathBuf::from(r"C:\work\broken\src\bad.veryl"), "prefix stripped");
        assert_eq!((line, col), (5, 20));

        let analysis = concat!(
            "Error:   × veryl check failed\n",
            "\n",
            "Error: undefined_identifier (https://doc.veryl-lang.org/book/07_appendix/02_semantic_error.html#undefined_identifier)\n",
            "\n",
            "  × \"b\" is undefined\n",
            "   ╭─[/home/me/broken/src/bad.veryl:5:16]\n",
            " 4 │     var a: logic<8>;\n",
            " 5 │     assign a = b;\n",
            "   ·                ┬\n",
            "   ·                ╰── Error location\n",
            " 6 │ }\n",
            "   ╰────\n",
        );
        let reports = parse_report(analysis);
        assert_eq!(reports.len(), 1, "the summary line is not a report of its own: {reports:?}");
        assert_eq!(reports[0].message, "\"b\" is undefined");
        assert_eq!(
            reports[0].location,
            Some((PathBuf::from("/home/me/broken/src/bad.veryl"), 5, 16))
        );

        assert!(parse_report("").is_empty());
        assert!(parse_report("[INFO ]       Output filelist (x.f)\n").is_empty());
    }

    /// A machine without Veryl still reads a project that was built once.
    #[test]
    fn without_veryl_the_last_build_is_read_and_said_so() {
        let expanded = expand_with(&[fixture().join(MANIFEST)], None);
        assert_eq!(expanded.sources.len(), 6, "{expanded:?}");
        assert_eq!(expanded.projects.len(), 1);
        assert_eq!(expanded.reports.len(), 1);
        assert_eq!(expanded.reports[0].severity, Severity::Warning);
        assert!(
            expanded.reports[0].message.contains("not installed"),
            "{}",
            expanded.reports[0].message
        );

        let mut files = FileTable::new();
        let diags = expanded.diagnostics(&mut files);
        assert_eq!(diags.count(Severity::Warning), 1);
        assert_eq!(diags.count(Severity::Error), 0);
    }

    #[test]
    fn without_veryl_and_without_a_build_there_is_nothing_to_read() {
        let dir = std::env::temp_dir().join("rtlscope-veryl-unbuilt");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join(MANIFEST), "[project]\nname = \"unbuilt\"\n").unwrap();
        std::fs::write(dir.join("src").join("a.veryl"), "module A {}\n").unwrap();

        let expanded = expand_with(&[dir.join("src").join("a.veryl")], None);
        assert!(expanded.sources.is_empty());
        assert_eq!(expanded.reports.len(), 1);
        assert_eq!(expanded.reports[0].severity, Severity::Error);
        assert!(expanded.reports[0].message.contains("never been built"));
    }

    /// Plain SystemVerilog goes through untouched, and one project named three
    /// ways is built once.
    #[test]
    fn plain_sources_pass_and_a_project_is_counted_once() {
        let sv = rtlscope_fixtures::path("hier.sv");
        let paths = vec![
            sv.clone(),
            fixture().join("src").join("top.veryl"),
            fixture().join("src").join("timer.veryl"),
            fixture().join(MANIFEST),
        ];
        let expanded = expand_with(&paths, None);
        assert_eq!(expanded.projects.len(), 1, "one project, however many ways it was named");
        assert_eq!(expanded.sources.len(), 7, "hier.sv and the six generated files");
        assert_eq!(expanded.sources[0], sv, "in the order given");
        assert!(expanded.watch.contains(&sv));
    }

    #[test]
    fn what_is_watched_can_be_asked_without_a_build() {
        let sv = rtlscope_fixtures::path("hier.sv");
        let watched = watched(&[sv.clone(), fixture().join("src").join("top.veryl"), fixture()]);
        assert_eq!(watched[0], sv);
        assert_eq!(
            watched.iter().filter(|p| p.ends_with(MANIFEST)).count(),
            1,
            "one project, once"
        );
        assert_eq!(
            watched.iter().filter(|p| p.extension().is_some_and(|e| e == "veryl")).count(),
            6
        );
    }

    #[test]
    fn a_veryl_file_with_no_project_above_it_is_reported() {
        let dir = std::env::temp_dir().join("rtlscope-veryl-orphan");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("alone.veryl"), "module A {}\n").unwrap();
        let expanded = expand_with(&[dir.join("alone.veryl")], None);
        assert!(expanded.sources.is_empty());
        assert!(expanded.reports[0].message.contains(MANIFEST), "{}", expanded.reports[0].message);
    }

    /// With Veryl on the machine the project is built afresh. Skipped, and
    /// said so, where it is not: the fixture already covers reading a build.
    #[test]
    fn with_veryl_the_project_is_built_afresh() {
        let Some(tool) = locate() else {
            eprintln!("veryl is not installed here; not building");
            return;
        };
        let dir = scratch("rebuild");
        let _ = std::fs::remove_dir_all(dir.join("target"));
        let _ = std::fs::remove_file(dir.join("lights.f"));
        let project = project_of(&dir).unwrap();
        assert_eq!(project.filelist(), None, "nothing built yet");

        let built = build_with(&project, Some(&tool));
        assert!(built.reports.is_empty(), "{:?}\n{}", built.reports, built.log);
        assert_eq!(built.sources.len(), 6);
        assert!(built.sources.iter().all(|p| p.is_file()), "{:?}", built.sources);
        assert!(dir.join("target").join("control.sv.map").is_file(), "with its source map");

        // A source that no longer reads: the report names the line, and the
        // last good build is still what is listed.
        std::fs::write(
            dir.join("src").join("show.veryl"),
            "module Show {\n    var a: logic = ;\n}\n",
        )
        .unwrap();
        let built = build_with(&project, Some(&tool));
        assert!(!built.reports.is_empty(), "{}", built.log);
        assert_eq!(built.reports[0].severity, Severity::Error);
        let (path, line, _) = built.reports[0].location.clone().expect("a location");
        assert!(path.ends_with("show.veryl"), "{}", path.display());
        assert_eq!(line, 2);
        assert_eq!(built.sources.len(), 6, "the last good build is what there is to read");
    }
}
