//! Properties a block diagram has to hold, whatever the layout engine does.
//!
//! Checked against the geometry rather than the SVG: the SVG is one renderer of
//! two, and a text comparison would fail on formatting while missing a box
//! drawn on top of another.

use std::collections::HashSet;

use rtlscope_graph::block::{BlockNode, WireKind};
use rtlscope_graph::geom::{BoxKind, DiagramGeom, Point};
use rtlscope_ir::Design;
use rtlscope_sv::ParseOptions;

fn elaborate(fixture: &str, top: Option<&str>) -> Design {
    let path = rtlscope_fixtures::path(fixture);
    let (uir, _) = rtlscope_sv::lower_files(&[path], &ParseOptions::default());
    let (design, _) = rtlscope_elab::elaborate(&uir, top);
    design.expect("elaboration produced a design")
}

fn diagram(fixture: &str, top: Option<&str>) -> (DiagramGeom, Design) {
    let design = elaborate(fixture, top);
    let geom = rtlscope_graph::diagram(&design, design.top);
    (geom, design)
}

/// Every fixture worth drawing, so a layout regression shows up on all of them.
fn all_diagrams() -> Vec<(&'static str, DiagramGeom)> {
    [
        ("hier.sv", Some("hier_top")),
        ("params.sv", Some("params_top")),
        ("fifo.sv", None),
        ("counter.sv", None),
        ("fsm.sv", None),
        ("pipeline3.sv", None),
        ("genblk.sv", None),
        ("adder.sv", None),
    ]
    .into_iter()
    .map(|(fixture, top)| (fixture, diagram(fixture, top).0))
    .collect()
}

#[test]
fn no_two_boxes_overlap() {
    for (fixture, geom) in all_diagrams() {
        for (i, a) in geom.boxes.iter().enumerate() {
            for b in &geom.boxes[i + 1..] {
                assert!(
                    !a.rect.overlaps(&b.rect),
                    "{fixture}: `{}` and `{}` overlap at {:?} / {:?}",
                    a.label,
                    b.label,
                    a.rect,
                    b.rect
                );
            }
        }
    }
}

#[test]
fn every_wire_segment_is_horizontal_or_vertical() {
    // The whole point of routing by hand rather than taking the layout
    // engine's straight lines.
    for (fixture, geom) in all_diagrams() {
        for wire in &geom.wires {
            assert!(wire.points.len() >= 2, "{fixture}: `{}` has no run", wire.label);
            for pair in wire.points.windows(2) {
                let horizontal = (pair[0].y - pair[1].y).abs() < 1e-6;
                let vertical = (pair[0].x - pair[1].x).abs() < 1e-6;
                assert!(
                    horizontal || vertical,
                    "{fixture}: `{}` runs diagonally from {:?} to {:?}",
                    wire.label,
                    pair[0],
                    pair[1]
                );
            }
        }
    }
}

#[test]
fn every_wire_starts_and_ends_on_a_pin() {
    // A wire whose end floats near a box rather than on it looks like a
    // disconnection that is not there.
    for (fixture, geom) in all_diagrams() {
        for wire in &geom.wires {
            let start = *wire.points.first().unwrap();
            let end = *wire.points.last().unwrap();
            assert!(
                lands_on_a_pin(&geom, start),
                "{fixture}: `{}` starts at {start:?}, which is not a pin",
                wire.label
            );
            assert!(
                lands_on_a_pin(&geom, end),
                "{fixture}: `{}` ends at {end:?}, which is not a pin",
                wire.label
            );
        }
    }
}

fn lands_on_a_pin(geom: &DiagramGeom, point: Point) -> bool {
    geom.boxes
        .iter()
        .flat_map(|node| &node.pins)
        .any(|pin| (pin.at.x - point.x).abs() < 1e-6 && (pin.at.y - point.y).abs() < 1e-6)
}

#[test]
fn inputs_are_on_the_left_and_outputs_on_the_right() {
    for (fixture, geom) in all_diagrams() {
        // Inputs share the edge their pins sit on, and so do outputs, so the
        // wires leaving and entering the diagram all start at one x.
        let input_edges: Vec<i64> = geom
            .boxes
            .iter()
            .filter(|b| b.kind == BoxKind::InputPort)
            .map(|b| b.rect.right().round() as i64)
            .collect();
        let output_edges: Vec<i64> = geom
            .boxes
            .iter()
            .filter(|b| b.kind == BoxKind::OutputPort)
            .map(|b| b.rect.x.round() as i64)
            .collect();

        assert!(
            input_edges.windows(2).all(|pair| pair[0] == pair[1]),
            "{fixture}: input pins are not aligned: {input_edges:?}"
        );
        assert!(
            output_edges.windows(2).all(|pair| pair[0] == pair[1]),
            "{fixture}: output pins are not aligned: {output_edges:?}"
        );

        // And they really are the outermost columns: nothing else is further
        // left than an input, or further right than an output.
        let inner: Vec<&_> = geom
            .boxes
            .iter()
            .filter(|b| !matches!(b.kind, BoxKind::InputPort | BoxKind::OutputPort))
            .collect();

        for node in &geom.boxes {
            match node.kind {
                BoxKind::InputPort => assert!(
                    inner.iter().all(|other| node.rect.right() <= other.rect.x + 1.0),
                    "{fixture}: input `{}` is not left of everything else",
                    node.label
                ),
                BoxKind::OutputPort => assert!(
                    inner.iter().all(|other| node.rect.x + 1.0 >= other.rect.right()),
                    "{fixture}: output `{}` is not right of everything else",
                    node.label
                ),
                _ => {}
            }
        }
    }
}

#[test]
fn nothing_in_the_design_is_left_undrawn() {
    // A box missing from the diagram is a silent hole, which is exactly what
    // the diagram exists to rule out.
    for (fixture, top) in [("hier.sv", Some("hier_top")), ("fifo.sv", None), ("fsm.sv", None)] {
        let design = elaborate(fixture, top);
        let module = design.top_module();
        let geom = rtlscope_graph::diagram(&design, design.top);

        // By what a box came from rather than by count: a process cut into
        // stages is several boxes for one thing, and that is not a hole.
        let drawn: HashSet<String> = geom
            .boxes
            .iter()
            .map(|node| match node.node {
                BlockNode::InPort(port) | BlockNode::OutPort(port) => format!("port {}", port.0),
                BlockNode::Inst(inst) => format!("inst {}", inst.0),
                BlockNode::Proc(proc) | BlockNode::Stage { proc, .. } => format!("proc {}", proc.0),
            })
            .collect();
        let expected = module.ports.len() + module.insts.len() + module.procs.len();
        assert_eq!(
            drawn.len(),
            expected,
            "{fixture}: {} things drawn for {} ports + {} instances + {} processes",
            drawn.len(),
            module.ports.len(),
            module.insts.len(),
            module.procs.len()
        );
    }
}

#[test]
fn clock_and_reset_wires_are_marked_and_hidden_by_default() {
    // counter.sv has one flop, so `clk` and `rst_n` both reach it.
    let (geom, _) = diagram("counter.sv", None);

    let clocks = geom.wires.iter().filter(|w| w.kind == WireKind::Clock).count();
    let resets = geom.wires.iter().filter(|w| w.kind == WireKind::Reset).count();
    assert!(clocks > 0, "the clock wire should be recognised");
    assert!(resets > 0, "the reset wire should be recognised");

    let visible = geom.without_clocks();
    assert!(
        visible.wires.iter().all(|w| w.kind == WireKind::Signal),
        "the default view carries signals only"
    );
    assert_eq!(visible.wires.len(), geom.wires.len() - clocks - resets);
}

#[test]
fn a_clock_is_recognised_through_the_hierarchy() {
    // `clk` in hier_top never drives a flop there — it goes into u_rf and
    // u_ctrl. Recognising it needs a look inside the children, and getting it
    // wrong puts a clock line across the whole diagram.
    let (geom, design) = diagram("hier.sv", Some("hier_top"));
    let module = design.top_module();
    let (clk, _) = module.net_by_name("clk").expect("clk");

    let kinds: Vec<WireKind> = geom.wires_of(clk).map(|w| w.kind).collect();
    assert!(!kinds.is_empty(), "clk should be drawn as some wire");
    assert!(
        kinds.iter().all(|kind| *kind == WireKind::Clock),
        "every clk wire is a clock, got {kinds:?}"
    );
}

#[test]
fn a_box_can_be_found_by_the_point_the_user_clicked() {
    let (geom, _) = diagram("hier.sv", Some("hier_top"));
    let target = geom.boxes.iter().find(|b| b.label == "u_alu").expect("u_alu");
    let centre = Point {
        x: target.rect.x + target.rect.width / 2.0,
        y: target.rect.y + target.rect.height / 2.0,
    };

    let hit = geom.hit(centre).expect("the centre of a box hits it");
    assert_eq!(hit.label, "u_alu");
    assert!(!hit.span.is_unknown(), "and knows where it came from");
}

#[test]
fn an_instance_shows_every_port_of_its_child_even_unconnected_ones() {
    let (geom, _) = diagram("hier.sv", Some("hier_top"));
    let alu = geom.boxes.iter().find(|b| b.label == "u_alu").unwrap();

    let pins: Vec<&str> = alu.pins.iter().map(|p| p.name.as_str()).collect();
    assert_eq!(pins, ["a", "b", "op", "y"]);
    assert_eq!(alu.sublabel.as_deref(), Some("hier_alu"));
}

#[test]
fn a_black_box_is_marked_so_the_reader_knows_it_is_opaque() {
    let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../rtlscope-elab/tests/data/missing_child.sv");
    let (uir, _) = rtlscope_sv::lower_files(&[path], &ParseOptions::default());
    let (design, _) = rtlscope_elab::elaborate(&uir, None);
    let design = design.unwrap();
    let geom = rtlscope_graph::diagram(&design, design.top);

    let bb = geom.boxes.iter().find(|b| b.label == "u_bb").expect("u_bb");
    assert!(bb.blackbox, "a module with no source is drawn as opaque");
}

#[test]
fn the_layout_is_deterministic() {
    // Two runs must place every box identically, or a golden test of the SVG
    // would flap and a GUI would jump on every redraw.
    for _ in 0..3 {
        let (first, _) = diagram("hier.sv", Some("hier_top"));
        let (second, _) = diagram("hier.sv", Some("hier_top"));
        let places = |geom: &DiagramGeom| -> Vec<(String, i64, i64)> {
            geom.boxes.iter().map(|b| (b.label.clone(), b.rect.x as i64, b.rect.y as i64)).collect()
        };
        assert_eq!(places(&first), places(&second), "boxes moved between runs");

        // Wires too. Which routing track a wire gets depends on the order its
        // edge was added, and building the graph by walking a `HashMap` made
        // that order random — the same design drew differently every run, and
        // comparing only boxes did not notice.
        let routes = |geom: &DiagramGeom| -> Vec<String> {
            let mut out: Vec<String> = geom
                .wires
                .iter()
                .map(|wire| {
                    let points: Vec<String> = wire
                        .points
                        .iter()
                        .map(|p| format!("{},{}", p.x as i64, p.y as i64))
                        .collect();
                    format!("{} {}", wire.label, points.join(" "))
                })
                .collect();
            out.sort();
            out
        };
        assert_eq!(routes(&first), routes(&second), "wires moved between runs");
    }
}

#[test]
fn no_wire_is_drawn_through_a_box() {
    // A wire that crosses two columns cannot run straight at the target's
    // height: the column between them has boxes there. Drawn that way the wire
    // passes through them and the diagram reads as a short.
    for (fixture, geom) in all_diagrams() {
        for wire in &geom.wires {
            for pair in wire.points.windows(2) {
                for node in &geom.boxes {
                    // A wire is allowed to touch the box it starts or ends on.
                    if touches_endpoint(wire, node) {
                        continue;
                    }
                    assert!(
                        !segment_enters(pair[0], pair[1], &node.rect),
                        "{fixture}: `{}` runs through `{}` from {:?} to {:?} ({:?})",
                        wire.label,
                        node.label,
                        pair[0],
                        pair[1],
                        node.rect
                    );
                }
            }
        }
    }
}

fn touches_endpoint(wire: &rtlscope_graph::Wire, node: &rtlscope_graph::NodeBox) -> bool {
    let ends = [*wire.points.first().unwrap(), *wire.points.last().unwrap()];
    ends.iter().any(|end| {
        node.pins
            .iter()
            .any(|pin| (pin.at.x - end.x).abs() < 1e-6 && (pin.at.y - end.y).abs() < 1e-6)
    })
}

/// True when an axis-aligned segment passes through a rectangle's interior.
fn segment_enters(a: Point, b: Point, rect: &rtlscope_graph::Rect) -> bool {
    const EPS: f64 = 0.5;
    let (x0, x1) = (a.x.min(b.x), a.x.max(b.x));
    let (y0, y1) = (a.y.min(b.y), a.y.max(b.y));
    x0 < rect.right() - EPS && rect.x + EPS < x1 && y0 < rect.bottom() - EPS && rect.y + EPS < y1
}

#[test]
fn the_hierarchy_diagram_has_a_stable_shape() {
    // A compact description rather than the SVG: the SVG would flap on
    // formatting, and this is what both renderers actually draw from.
    let (geom, _) = diagram("hier.sv", Some("hier_top"));

    let mut lines = vec![format!("module {}", geom.module)];
    for node in &geom.boxes {
        lines.push(format!(
            "box  {:>5},{:<5} {:>4}x{:<4} {:?} {}{}",
            geom_round(node.rect.x),
            geom_round(node.rect.y),
            geom_round(node.rect.width),
            geom_round(node.rect.height),
            node.kind,
            node.label,
            node.sublabel.as_ref().map_or(String::new(), |s| format!(" : {s}"))
        ));
    }
    let mut wires: Vec<String> = geom
        .wires
        .iter()
        .map(|wire| {
            let points: Vec<String> = wire
                .points
                .iter()
                .map(|p| format!("{},{}", geom_round(p.x), geom_round(p.y)))
                .collect();
            format!("wire {:?} {:<12} {}", wire.kind, wire.label, points.join(" -> "))
        })
        .collect();
    wires.sort();
    lines.extend(wires);

    insta::assert_snapshot!(lines.join("\n"));
}

/// Logic with no clock is a cloud and a register is a box, and the geometry
/// says which before either renderer draws it.
#[test]
fn a_process_without_a_clock_is_a_cloud_and_a_register_is_not() {
    let (geom, _) = diagram("fsm.sv", None);
    let processes: Vec<(&str, bool)> = geom
        .boxes
        .iter()
        .filter(|node| node.kind == rtlscope_graph::BoxKind::Process)
        .map(|node| (node.label.as_str(), node.comb))
        .collect();
    assert!(processes.contains(&("comb", true)), "{processes:?}");
    assert!(processes.contains(&("flop + reset", false)), "{processes:?}");
    assert!(
        geom.boxes
            .iter()
            .filter(|node| node.kind != rtlscope_graph::BoxKind::Process)
            .all(|n| !n.comb),
        "only a process can be one"
    );
}

/// A cloud's bumps clear the box by a quarter radius and no more, so the pins
/// on the box's edge sit on the outline; and the ring is unbroken.
#[test]
fn a_cloud_hugs_its_box() {
    let rect = rtlscope_graph::Rect { x: 100.0, y: 50.0, width: 120.0, height: 60.0 };
    let cloud = rtlscope_graph::geom::cloud(&rect);
    assert!(cloud.radius > 0.0 && cloud.radius <= rtlscope_graph::geom::CLOUD_BUMP);
    let crown = cloud.radius * 0.25;
    for bump in &cloud.bumps {
        let left = rect.x - (bump.x - cloud.radius);
        let right = (bump.x + cloud.radius) - rect.right();
        let top = rect.y - (bump.y - cloud.radius);
        let bottom = (bump.y + cloud.radius) - rect.bottom();
        let reach = left.max(right).max(top).max(bottom);
        assert!((reach - crown).abs() < 1e-6, "a bump reaches {reach}, wanted {crown}: {bump:?}");
    }
    // Along the top edge, no two neighbours are further apart than a diameter.
    let mut top: Vec<f64> =
        cloud.bumps.iter().filter(|b| (b.y - cloud.body.y).abs() < 1e-9).map(|b| b.x).collect();
    top.sort_by(f64::total_cmp);
    assert!(top.windows(2).all(|pair| pair[1] - pair[0] <= 2.0 * cloud.radius), "{top:?}");

    // A small box gets small bumps rather than being all bump.
    let tiny = rtlscope_graph::geom::cloud(&rtlscope_graph::Rect {
        x: 0.0,
        y: 0.0,
        width: 20.0,
        height: 12.0,
    });
    assert!(tiny.body.width > 0.0 && tiny.body.height > 0.0, "{tiny:?}");
}

fn geom_round(value: f64) -> i64 {
    value.round() as i64
}

#[test]
fn the_canvas_contains_everything_it_draws() {
    // Feedback wires run below the boxes and long wires above them, so a canvas
    // measured from the boxes alone clips exactly the wires that were hardest
    // to route.
    for (fixture, geom) in all_diagrams() {
        let mut points: Vec<Point> = geom.wires.iter().flat_map(|w| w.points.clone()).collect();
        for node in &geom.boxes {
            points.push(Point { x: node.rect.x, y: node.rect.y });
            points.push(Point { x: node.rect.right(), y: node.rect.bottom() });
        }
        for point in points {
            assert!(
                point.x >= 0.0 && point.y >= 0.0,
                "{fixture}: {point:?} is off the top or left of the canvas"
            );
            assert!(
                point.x <= geom.width && point.y <= geom.height,
                "{fixture}: {point:?} is outside the {}x{} canvas",
                geom.width,
                geom.height
            );
        }
    }
}

/// A pipeline written as one `always_ff` is one process, and drawn as one box
/// it was three registers with `d1`, `d2` and `d3` on both sides and no wire
/// between them — the edge from a box to itself is the one edge the graph
/// drops. Cut by stage it is three boxes and the wires between them, which is
/// what the code says and what a reader came to the diagram to see.
#[test]
fn a_pipeline_process_is_drawn_as_one_box_per_stage() {
    let (geom, _) = diagram("pipeline3.sv", None);
    let stages: Vec<&str> = geom
        .boxes
        .iter()
        .filter(|node| node.label.starts_with("stage "))
        .map(|node| node.label.as_str())
        .collect();
    assert_eq!(stages, ["stage 0", "stage 1", "stage 2"], "{stages:?}");
    assert!(
        !geom.boxes.iter().any(|node| node.label == "flop + reset"),
        "the one box holding every register is gone"
    );

    // The chain itself: each register leaves the stage that writes it and
    // arrives at the one that reads it. This is the wire that did not exist.
    let stage = |label: &str| geom.boxes.iter().find(|node| node.label == label).expect(label);
    for (net, from, to) in [
        ("valid_d1", "stage 0", "stage 1"),
        ("data_d1", "stage 0", "stage 1"),
        ("valid_d2", "stage 1", "stage 2"),
        ("data_d2", "stage 1", "stage 2"),
    ] {
        let wire = geom
            .wires
            .iter()
            .find(|wire| wire.label.split('[').next() == Some(net))
            .unwrap_or_else(|| panic!("no wire carries {net}"));
        assert!(touches_endpoint(wire, stage(from)), "{net} does not leave {from}");
        assert!(touches_endpoint(wire, stage(to)), "{net} does not reach {to}");
    }
}

/// A register that reads itself is drawn reading itself.
///
/// The edge from a box to its own input is the one the graph leaves out — a
/// layered layout cannot rank a cycle of length one. Left out of the picture
/// as well, a counter was a box with `count` on both sides and nothing between
/// them, which cannot be told from a register fed by something that was never
/// drawn. So the wire is drawn after the layout, from the pin it leaves to the
/// pin it re-enters, under the box it belongs to.
#[test]
fn a_register_that_feeds_itself_is_drawn_looping() {
    let (geom, _) = diagram("counter.sv", None);
    let flop = geom.boxes.iter().find(|node| node.label.starts_with("flop")).expect("the flop");
    let on_flop = |point: &Point| {
        flop.pins
            .iter()
            .any(|pin| (pin.at.x - point.x).abs() < 1e-6 && (pin.at.y - point.y).abs() < 1e-6)
    };
    let looping: Vec<&rtlscope_graph::Wire> = geom
        .wires
        .iter()
        .filter(|wire| {
            on_flop(wire.points.first().unwrap()) && on_flop(wire.points.last().unwrap())
        })
        .collect();
    assert_eq!(looping.len(), 1, "one wire leaves the flop and comes back: {looping:?}");
    let count = looping[0];
    assert!(count.label.starts_with("count"), "and it is the counter's own value: {}", count.label);
    assert_eq!(count.kind, WireKind::Signal, "drawn by default, not hidden with the clocks");
    assert!(
        count.points.iter().any(|point| point.y > flop.rect.bottom()),
        "and it runs under the box rather than through it: {:?}",
        count.points
    );
}

/// Every other process is still one box. A cut is a claim that values pass
/// through one clock a box, and it is made only where that is so: not for a
/// register on its own, and not for a FIFO — whose pointers land in two
/// stages by the register-depth count and are state that advances, not a
/// pipeline. The first version of this cut the FIFO's control in three.
#[test]
fn only_a_process_that_passes_values_through_is_cut() {
    for fixture in ["counter.sv", "fifo.sv"] {
        let (geom, _) = diagram(fixture, None);
        assert!(
            geom.boxes.iter().any(|node| node.label.starts_with("flop")),
            "{fixture}: {:?}",
            labels(&geom)
        );
        assert!(
            !geom.boxes.iter().any(|node| node.label.starts_with("stage ")),
            "{fixture} was cut: {:?}",
            labels(&geom)
        );
    }
}

fn labels(geom: &DiagramGeom) -> Vec<&str> {
    geom.boxes.iter().map(|node| node.label.as_str()).collect()
}
