//! Development aid: where does drawing a big module spend its time?

use std::time::Instant;

use rtlscope_graph::layout::{LayeredLayout, Sugiyama};
use rust_sugiyama::configure::Config;

fn main() {
    let mut args = std::env::args().skip(1);
    let top = args.next().expect("usage: probe_layout_cost <top> <file...>");
    let paths: Vec<std::path::PathBuf> = args.map(Into::into).collect();

    let started = Instant::now();
    let (uir, _) = rtlscope_sv::lower_files(&paths, &rtlscope_sv::ParseOptions::default());
    println!("parse+lower  {:>8.1?}", started.elapsed());

    let started = Instant::now();
    let (design, _) = rtlscope_elab::elaborate(&uir, Some(&top));
    let design = design.expect("elaborates");
    println!("elaborate    {:>8.1?}", started.elapsed());

    // Every module, smallest first, so the point where it stops finishing is
    // visible rather than guessed at.
    let mut all: Vec<_> =
        design.modules.iter_enumerated().map(|(id, m)| (id, m.name.clone())).collect();
    all.sort_by_key(|(id, _)| {
        let m = design.module(*id);
        m.ports.len() + m.insts.len() + m.procs.len()
    });
    for (id, name) in &all {
        let block = rtlscope_graph::block::build(&design, *id);
        let n = block.graph.node_count();
        let e = block.graph.edge_count();
        if n == 0 {
            continue;
        }
        let started = Instant::now();
        let ranking = Sugiyama::default().rank(&block.graph);
        println!(
            "{:>4} nodes {:>5} edges  {:>9.1?}  {:>3} layers  {name}",
            n,
            e,
            started.elapsed(),
            ranking.layers.len()
        );
        if started.elapsed().as_secs() > 20 {
            println!("  (stopping: too slow)");
            break;
        }
    }
    return;

    #[allow(unreachable_code)]
    let started = Instant::now();
    let block = rtlscope_graph::block::build(&design, design.top);
    println!(
        "block graph  {:>8.1?}   {} nodes, {} edges",
        started.elapsed(),
        block.graph.node_count(),
        block.graph.edge_count()
    );

    for (label, transpose) in [("transpose=off", false), ("transpose=on", true)] {
        let engine =
            Sugiyama { config: Config { vertex_spacing: 40.0, transpose, ..Config::default() } };
        let started = Instant::now();
        let ranking = engine.rank(&block.graph);
        println!("rank {label:<14} {:>8.1?}   {} layers", started.elapsed(), ranking.layers.len());
    }
}
