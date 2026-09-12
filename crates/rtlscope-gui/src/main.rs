// Windows opens a console beside every process built for the console
// subsystem, and a window with a black rectangle behind it is not a program
// anybody installed on purpose. Only in release: a debug build is what gets
// driven from a script, and the output is worth having then.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! `rtlscope-gui` — the block diagram, in a window.
//!
//! A separate binary from `rtlscope`, and a thin one: it parses, elaborates and
//! lays out through the same crates the CLI uses, then draws the result. D4 of
//! the design spec puts the analysis in libraries precisely so the GUI cannot
//! grow its own opinion about what a design is.

mod app;
mod canvas;
mod cone;
mod dock;
mod fonts;
mod layout;
mod palette;
mod samples;
mod session;
mod source;
mod states;
mod stim;
mod theme;
mod tree;
mod views;
mod wave;

use std::path::PathBuf;

use anyhow::Context as _;
use clap::Parser;
use rtlscope_sv::ParseOptions;

#[derive(Parser)]
#[command(name = "rtlscope-gui", version, about = "Browse a SystemVerilog design")]
struct Cli {
    /// What to open: sources, a folder of them, a waveform, or a run's results.
    #[arg(value_name = "FILE")]
    files: Vec<PathBuf>,

    /// Read source paths from a file list, one per line (`//` comments allowed).
    #[arg(short = 'f', long = "file-list", value_name = "LIST")]
    file_lists: Vec<PathBuf>,

    /// Define a macro, as `-D NAME` or `-D NAME=VALUE`.
    #[arg(short = 'D', long = "define", value_name = "NAME[=VALUE]")]
    defines: Vec<String>,

    /// Add a directory to the `include` search path.
    #[arg(short = 'I', long = "include", value_name = "DIR")]
    include_paths: Vec<PathBuf>,

    /// Top module. Inferred when exactly one module is instantiated by nothing.
    #[arg(long)]
    top: Option<String>,

    /// How to open a source location. `{file}`, `{line}` and `{col}` are
    /// substituted.
    ///
    /// Wins over what is under `settings` for this run, and is not written
    /// there — the box is the reader's choice and a flag is one occasion.
    /// Left out and never set, `code -g {file}:{line}:{col}`.
    #[arg(long)]
    editor: Option<String>,

    /// A waveform to open alongside the diagram. One can also be dragged in.
    #[arg(long, value_name = "FILE")]
    dump: Option<PathBuf>,

    /// A second waveform to hold `--dump` against: the known-good recording.
    ///
    /// Signals both have are drawn with this one faint behind, rows that differ
    /// are marked, and `first difference` goes to the earliest of them.
    #[arg(long, value_name = "FILE", requires = "dump")]
    reference: Option<PathBuf>,

    /// Open one of the designs that ship inside the window, by name — what
    /// `open ▸ a sample` does. `--sample list` names them.
    #[arg(long, value_name = "NAME")]
    sample: Option<String>,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    // Sorted by what each argument is, the same way a dropped file is. The
    // window has read waveforms and results by drag for a long time; the
    // command line took only sources, which meant `rtlscope-gui run.vcd` handed a
    // dump to the SystemVerilog parser and reported a page of syntax errors
    // about a file it should simply have opened. It also meant the file
    // associations an installer makes could not work.
    let mut paths = Vec::new();
    let mut gathered_includes = Vec::new();
    let mut dumps = Vec::new();
    let mut results = Vec::new();
    for path in &cli.files {
        match app::Dropped::of(path) {
            app::Dropped::Folder => {
                let found = app::gather_sources(path);
                eprintln!("{}", app::describe(path, &found));
                paths.extend(found.sources);
                gathered_includes.extend(found.includes);
            }
            app::Dropped::FileList => paths.extend(read_file_list(path)?),
            app::Dropped::Dump => dumps.push(path.clone()),
            app::Dropped::Results => results.push(path.clone()),
            // Sources, and anything unrecognised: handed to the parser, which
            // says what is wrong with it better than a guess here could.
            app::Dropped::Source | app::Dropped::Unknown => paths.push(path.clone()),
        }
    }
    for list in &cli.file_lists {
        paths.extend(read_file_list(list)?);
    }

    let options = ParseOptions {
        defines: rtlscope_read::parse_defines(&cli.defines),
        include_paths: cli
            .include_paths
            .iter()
            .cloned()
            .chain(gathered_includes)
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect(),
    };

    // No sources is not an error: the window opens on its welcome screen and
    // takes them by drag and drop. Exiting instead is what makes a
    // double-clicked binary look broken.
    if cli.sample.as_deref() == Some("list") {
        for sample in samples::ALL {
            println!("{:<12} {}", sample.id, sample.what);
        }
        return Ok(());
    }

    let mut app = app::RtlScopeApp::new(options, cli.top, cli.editor);
    app.load(&paths, true);
    for dump in dumps.into_iter().chain(cli.dump) {
        app.open_dump(dump);
    }
    for file in results {
        app.open_results(file);
    }
    if let Some(reference) = cli.reference {
        app.compare_against(reference);
    }
    // What the reader last chose, and where they left their windows.
    app.restore_saved_settings();
    app.restore_saved_layout();
    // After the layout, because a sample picks the view its question is
    // answered in, and a layout that remembered last week's tab must not win
    // over that. After the files too: a reader who typed both gets the sample,
    // which is the one thing they cannot have meant by accident.
    if let Some(name) = &cli.sample {
        app.open_sample(name);
        eprintln!("{}", app.status());
    }
    if let Ok(cycle) = std::env::var("RTLSCOPE_CYCLE")
        && let Ok(cycle) = cycle.parse()
    {
        app.seek_cycle(cycle);
    }
    // Last, so that opening a dump — which moves to the wave tab — does not
    // override what was asked for.
    if let Ok(tab) = std::env::var("RTLSCOPE_TAB") {
        app.show_tab(&tab);
    }
    if let Ok(mode) = std::env::var("RTLSCOPE_PIPE") {
        app.show_pipe_mode(&mode);
    }
    if let Ok(mode) = std::env::var("RTLSCOPE_FSM") {
        app.show_fsm_mode(&mode);
    }
    // Before the tab is chosen, so `RTLSCOPE_TRACE=x RTLSCOPE_TAB=trace` shows the
    // trail rather than the empty state.
    if let Ok(signal) = std::env::var("RTLSCOPE_TRACE") {
        app.trace_signal(&signal);
    }
    if let Ok(what) = std::env::var("RTLSCOPE_STIM")
        && matches!(what.as_str(), "demo" | "run" | "save")
    {
        app.draw_demo();
        match what.as_str() {
            "run" => app.play_drawing(),
            "save" => app.save_drawing(),
            _ => {}
        }
    }
    // Before a simulation is asked for, since it decides what one runs.
    if let Ok(files) = std::env::var("RTLSCOPE_TESTBENCH") {
        let paths: Vec<std::path::PathBuf> =
            files.split(';').filter(|it| !it.is_empty()).map(std::path::PathBuf::from).collect();
        app.open_testbench(paths);
    }

    // After the tab, because a simulation remembers which view asked for it and
    // returns there — the same as pressing the button on that view would.
    if std::env::var("RTLSCOPE_SIM").is_ok_and(|on| on != "0") {
        app.simulate_now();
    }
    if std::env::var("RTLSCOPE_MEASURE").is_ok_and(|on| on != "0") {
        app.measure_demo();
    }
    if let Ok(needle) = std::env::var("RTLSCOPE_PICK") {
        app.pick_signals(&needle);
    }
    // After anything that puts tracks on the panel: how a name is written
    // depends on what else is there.
    if let Ok(how) = std::env::var("RTLSCOPE_NAMES") {
        app.name_style(&how);
    }
    if let Ok(spec) = std::env::var("RTLSCOPE_CONE") {
        app.show_cone(&spec);
    }
    if let Ok(spec) = std::env::var("RTLSCOPE_DEPTH") {
        app.measure_depth(&spec);
    }
    if std::env::var("RTLSCOPE_DIFF").is_ok_and(|on| on != "0") {
        app.seek_first_difference();
    }
    // `RTLSCOPE_POPOUT=Wave,Trace` puts two views in two windows, which is the
    // arrangement worth looking at and the one a click cannot reach in a
    // screenshot.
    if let Ok(what) = std::env::var("RTLSCOPE_POPOUT") {
        app.pop_out_named(&what);
    }
    // The size the reader left it at, when there is one. Everything else about
    // the desk was already remembered — which views had windows and where they
    // were — and the main window was the one thing that came back the same size
    // every launch however often it was made bigger.
    let size = app::saved_main_size().unwrap_or([1400.0, 900.0]);
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size(size)
            .with_min_inner_size([900.0, 600.0])
            .with_title("RTLScope")
            .with_icon(icon()),
        ..Default::default()
    };

    eframe::run_native(
        "rtlscope",
        options,
        Box::new(move |cc| {
            // Before the theme, because the style names sizes for text styles
            // and the faces those sizes apply to should already be the right
            // ones.
            fonts::install(&cc.egui_ctx);
            // Both of egui's themes are styled up front; which one shows is
            // the system's choice until the toolbar pins it.
            theme::install(&cc.egui_ctx);
            Ok(Box::new(app))
        }),
    )
    .map_err(|error| anyhow::anyhow!("{error}"))
}

/// The mark, for the taskbar and the window's corner.
///
/// Raw pixels rather than a PNG: this is one 64×64 image and decoding it would
/// mean a decoder crate in the dependency tree for the length of one startup.
/// `assets/make_icon.py` writes the file, and draws the same square wave
/// `theme::brand_mark` paints inside the window.
fn icon() -> egui::IconData {
    const SIDE: u32 = 64;
    const PIXELS: &[u8] = include_bytes!("../../../assets/rtlscope-64.rgba");
    egui::IconData { rgba: PIXELS.to_vec(), width: SIDE, height: SIDE }
}

/// One path per line, `//` starts a comment, relative to the list's directory.
///
/// Shared with the window, which accepts a `.f` dropped onto it.
pub(crate) fn read_file_list(path: &std::path::Path) -> anyhow::Result<Vec<PathBuf>> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading file list {}", path.display()))?;
    let base = path.parent().unwrap_or_else(|| std::path::Path::new("."));

    Ok(text
        .lines()
        .map(|line| match line.find("//") {
            Some(at) => &line[..at],
            None => line,
        })
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(|line| {
            let entry = std::path::Path::new(line);
            if entry.is_absolute() { entry.to_path_buf() } else { base.join(entry) }
        })
        .collect())
}
