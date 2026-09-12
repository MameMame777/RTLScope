//! Paths in, an elaborated design out.
//!
//! Four steps that always go together and always go in this order: build any
//! Veryl among the paths, lower the SystemVerilog, gather what each step said
//! into one list, elaborate. Every entrance did this for itself — the command
//! line, the MCP server, the window's open, and the window's reread — and the
//! four copies had already drifted: only the window recovered when a pinned
//! `--top` was no longer in the sources. A reader who asked the same question
//! of the terminal and of the window got two different answers.
//!
//! This crate sits above both `rtlscope-sv` and `rtlscope-elab` because it needs
//! both, and elaboration is not allowed to know the SystemVerilog front end —
//! that is what lets Veryl, or any later language, reach the same elaborator.
//! So the step that joins them cannot live in either, and lives here.
//!
//! Nothing here decides what to *do* about a failure. The terminal exits, the
//! server returns an error, the window keeps the diagnostics and offers the
//! tops it found. Those are three different answers to the same reading, so
//! what comes back is the reading itself.

use std::path::PathBuf;

use rtlscope_ir::{Design, Diagnostics, UDesign};
use rtlscope_sv::ParseOptions;
use rtlscope_veryl::{Expanded, Project};

/// What to do when the top module asked for is not in the sources.
///
/// The difference is not cosmetic, which is why it is named rather than
/// assumed. A `--top` on a command line was typed for *this* run and being
/// told it is not there is the answer. A top pinned in a window was chosen
/// once and outlives every later read, so holding a reader to a name they
/// picked before the module was renamed turns every subsequent drop into the
/// same complaint until they restart.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StaleTop {
    /// Report it and stop. The reader asked for that module by name.
    #[default]
    Keep,
    /// Read again without it, and say whose name was dropped.
    Infer,
}

/// The sources read, before anything was resolved.
///
/// Its own stop on the way rather than a detail of [`Read`], because one
/// question is asked here and nowhere else: `rtlscope dump-ports` reports what
/// the front end found *before* elaboration, so that widths and parameters are
/// printed as the expressions they still are. Making it elaborate to reach the
/// same file table would add elaboration's diagnostics to a report that is not
/// about elaboration.
#[derive(Debug)]
pub struct Lowered {
    pub uir: UDesign,
    /// Veryl's reports, then the lowering's: the order the steps happened in.
    pub diags: Diagnostics,
    /// What the SystemVerilog front end was given, which is also what a
    /// simulator must be given: a Veryl project stands in for itself as the
    /// files Veryl wrote.
    pub sources: Vec<PathBuf>,
    /// What to watch for changes — for a project its `.veryl` files and
    /// manifest, not its output, since the output is not what anybody edits.
    pub watch: Vec<PathBuf>,
    /// The Veryl projects met, each once. What a caller names the reading after.
    pub projects: Vec<Project>,
}

/// A reading of some paths: what came out, and everything said on the way.
#[derive(Debug)]
pub struct Read {
    /// The unresolved design, kept whole rather than reduced to its file table.
    ///
    /// Two callers need more than the table: rendering a diagnostic needs
    /// `uir.files`, but offering a reader the tops to choose from when there
    /// was no single one needs [`rtlscope_elab::candidate_tops`], which reads the
    /// modules. Handing back only the table would send that caller off to parse
    /// the sources a second time to answer a question this read already knew.
    pub uir: UDesign,
    /// The elaborated design, or nothing — with the reason in `diags`.
    pub design: Option<Design>,
    /// Veryl's reports, then the lowering's, then elaboration's: the order the
    /// steps happened in, which is the order a reader can follow.
    pub diags: Diagnostics,
    /// What the SystemVerilog front end was given, which is also what a
    /// simulator must be given: a Veryl project stands in for itself as the
    /// files Veryl wrote.
    pub sources: Vec<PathBuf>,
    /// What to watch for changes — for a project its `.veryl` files and
    /// manifest, not its output, since the output is not what anybody edits.
    pub watch: Vec<PathBuf>,
    /// The Veryl projects met, each once. What a caller names the reading after.
    pub projects: Vec<Project>,
    /// The top that was asked for and dropped, under [`StaleTop::Infer`].
    ///
    /// Said rather than silently forgotten: the design that comes back is not
    /// the one that was asked for, and a window that swapped it without a word
    /// would be answering a different question than the one on screen.
    pub unpinned: Option<String>,
}

impl Lowered {
    /// Resolves it: parameters to values, widths to numbers, generate unrolled.
    pub fn elaborate(self, top: Option<&str>, stale: StaleTop) -> Read {
        let Lowered { uir, mut diags, sources, watch, projects } = self;

        let (mut design, mut elaborated) = rtlscope_elab::elaborate(&uir, top);
        let mut unpinned = None;
        if stale == StaleTop::Infer
            && design.is_none()
            && let Some(asked) = top
            && elaborated.iter().any(|diag| diag.code == rtlscope_ir::DiagCode::TopNotFound)
        {
            unpinned = Some(asked.to_string());
            (design, elaborated) = rtlscope_elab::elaborate(&uir, None);
        }
        diags.extend(elaborated);

        Read { uir, design, diags, sources, watch, projects, unpinned }
    }
}

/// Reads the paths named, and resolves what they mean.
pub fn read(paths: &[PathBuf], parse: &ParseOptions, top: Option<&str>, stale: StaleTop) -> Read {
    lower(paths, parse).elaborate(top, stale)
}

/// The same, from an expansion already in hand.
pub fn read_expanded(
    expanded: Expanded,
    parse: &ParseOptions,
    top: Option<&str>,
    stale: StaleTop,
) -> Read {
    lower_expanded(expanded, parse).elaborate(top, stale)
}

/// Builds any Veryl among the paths and reads the SystemVerilog, stopping
/// short of resolving it.
pub fn lower(paths: &[PathBuf], parse: &ParseOptions) -> Lowered {
    lower_expanded(rtlscope_veryl::expand(paths), parse)
}

/// The same, from an expansion already in hand.
///
/// Expanding is not free — it runs `veryl build` — so a caller that already
/// needed the expansion for something else hands it over rather than paying
/// twice. `rtlscope tb-init` and `rtlscope sim` did pay twice: each asked for the
/// source list to give the simulator, then read the sources, and each ask built
/// the project again.
///
/// It is also how a test says "a machine with no Veryl on it", by way of
/// [`rtlscope_veryl::expand_with`] — the same door that crate opened for its own
/// tests, for the same reason.
pub fn lower_expanded(expanded: Expanded, parse: &ParseOptions) -> Lowered {
    let (mut uir, lowered) = rtlscope_sv::lower_files(&expanded.sources, parse);

    // Veryl's own report first, having happened first. It is placed into the
    // design's file table, so a line of a `.veryl` renders and links like any
    // other span — which is why this cannot be done before there is a table.
    let mut diags = expanded.diagnostics(&mut uir.files);
    diags.extend(lowered);

    Lowered {
        uir,
        diags,
        sources: expanded.sources,
        watch: expanded.watch,
        projects: expanded.projects,
    }
}

/// `-D NAME` and `-D NAME=VALUE`, as the parser wants them.
///
/// Split on the *first* `=`, so `-D WIDTH=A=B` defines `WIDTH` as `A=B` rather
/// than being refused: the value is whatever text follows, and a macro body is
/// allowed to contain an equals sign.
pub fn parse_defines(entries: &[String]) -> Vec<(String, Option<String>)> {
    entries
        .iter()
        .map(|entry| match entry.split_once('=') {
            Some((name, value)) => (name.to_string(), Some(value.to_string())),
            None => (entry.clone(), None),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read_fixture(name: &str, top: Option<&str>, stale: StaleTop) -> Read {
        read(&[rtlscope_fixtures::path(name)], &ParseOptions::default(), top, stale)
    }

    /// A `--top` that is not there is the answer to what was asked, for a
    /// caller that asked once.
    #[test]
    fn a_top_that_is_not_there_is_kept_as_the_answer() {
        let read = read_fixture("pipeline3.sv", Some("nope"), StaleTop::Keep);

        assert!(read.design.is_none(), "there is no module called `nope`");
        assert_eq!(read.unpinned, None, "nothing was dropped, because nothing was retried");
        assert!(
            read.diags.iter().any(|diag| diag.code == rtlscope_ir::DiagCode::TopNotFound),
            "{}",
            read.diags.render(&read.uir.files)
        );
    }

    /// A top pinned in a window outlives the read it was chosen in. When the
    /// module it names has gone, holding the reader to it would answer every
    /// later drop with the same complaint until they restart.
    #[test]
    fn a_top_that_is_not_there_is_inferred_again_and_the_name_is_said() {
        let read = read_fixture("pipeline3.sv", Some("nope"), StaleTop::Infer);

        let design = read.design.expect("the top is inferred once the pin is let go");
        assert_eq!(design.modules[design.top].base_name, "pipeline3");
        assert_eq!(read.unpinned.as_deref(), Some("nope"), "and whose name was dropped");
    }

    /// Inferring is only for a top that is *missing*. Sources that do not
    /// elaborate for any other reason must not be quietly read a second way.
    #[test]
    fn a_top_that_is_there_is_used_under_either_answer() {
        for stale in [StaleTop::Keep, StaleTop::Infer] {
            let read = read_fixture("pipeline3.sv", Some("pipeline3"), stale);
            let design = read.design.expect("the module is right there");
            assert_eq!(design.modules[design.top].base_name, "pipeline3");
            assert_eq!(read.unpinned, None, "nothing was dropped under {stale:?}");
        }
    }

    /// Veryl runs before the SystemVerilog is read, so what it said comes
    /// first — a reader following the list is following the order of events.
    ///
    /// Told there is no Veryl on the machine, which is a state this has to
    /// report rather than crash on, and the shortest way to a Veryl diagnostic
    /// without building anything.
    #[test]
    fn what_veryl_said_comes_before_what_the_front_end_said() {
        let project = rtlscope_fixtures::veryl_project();
        let expanded = rtlscope_veryl::expand_with(&[project], None);
        let read = read_expanded(expanded, &ParseOptions::default(), None, StaleTop::Keep);

        let first = read.diags.iter().next().expect("no veryl is something to say");
        assert_eq!(
            first.code,
            rtlscope_ir::DiagCode::TranspileFailed,
            "{}",
            read.diags.render(&read.uir.files)
        );
    }

    /// The sources a simulator is handed are the ones the front end read, so
    /// a Veryl project is carried as the SystemVerilog it wrote.
    #[test]
    fn a_veryl_project_is_read_as_what_it_wrote_and_watched_as_what_was_typed() {
        let project = rtlscope_fixtures::veryl_project();
        let read =
            read(std::slice::from_ref(&project), &ParseOptions::default(), None, StaleTop::Keep);

        assert!(
            read.sources.iter().all(|path| path.extension().is_some_and(|it| it == "sv")),
            "the front end reads SystemVerilog: {:?}",
            read.sources
        );
        assert!(
            read.watch.iter().any(|path| path.extension().is_some_and(|it| it == "veryl")),
            "what changes by hand is the Veryl: {:?}",
            read.watch
        );
        assert_eq!(read.projects.len(), 1, "one project, met once");
    }

    /// A macro body may hold an equals sign, so only the first one separates.
    #[test]
    fn a_define_splits_on_its_first_equals_and_no_further() {
        let given = ["PLAIN".to_string(), "W=8".to_string(), "EXPR=A=B".to_string()];
        assert_eq!(
            parse_defines(&given),
            vec![
                ("PLAIN".to_string(), None),
                ("W".to_string(), Some("8".to_string())),
                ("EXPR".to_string(), Some("A=B".to_string())),
            ]
        );
    }
}
