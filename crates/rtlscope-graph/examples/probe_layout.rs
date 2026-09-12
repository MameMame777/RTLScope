//! Development aid: which axis does rust-sugiyama use for layers?
//!
//! A straight chain a -> b -> c -> d must come out with four distinct values on
//! the layering axis and one shared value on the other.

use petgraph::stable_graph::StableDiGraph;
use rust_sugiyama::configure::Config;

fn main() {
    let mut graph: StableDiGraph<&str, ()> = StableDiGraph::new();
    let a = graph.add_node("a");
    let b = graph.add_node("b");
    let c = graph.add_node("c");
    let d = graph.add_node("d");
    let e = graph.add_node("e");
    graph.add_edge(a, b, ());
    graph.add_edge(b, c, ());
    graph.add_edge(c, d, ());
    // A second node in the same rank as b, to see how siblings are spread.
    graph.add_edge(a, e, ());
    graph.add_edge(e, c, ());

    let config = Config { vertex_spacing: 50.0, ..Default::default() };
    let layouts = rust_sugiyama::from_graph(&graph, &|_, _| (40.0, 20.0), &config);

    println!("subgraphs: {}", layouts.len());
    for (positions, width, height) in &layouts {
        println!("  size {width} x {height}");
        for (index, (x, y)) in positions {
            println!("    {:<3} x={x:>8.1}  y={y:>8.1}", graph[*index]);
        }
    }
}
