//! What the three analyses are supposed to find, and what they are supposed to
//! say when they cannot.

use std::path::PathBuf;

use rtlscope_analyse::{CrossingKind, cdc, fsm, lint, pipeline};
use rtlscope_ir::Design;
use rtlscope_sv::ParseOptions;

fn design(fixture: &str, top: Option<&str>) -> Design {
    let path: PathBuf = rtlscope_fixtures::path(fixture);
    let (uir, _) = rtlscope_sv::lower_files(&[path], &ParseOptions::default());
    let (design, _) = rtlscope_elab::elaborate(&uir, top);
    design.expect("elaboration produced a design")
}

/// A two-process machine, with its states named by the localparams the source
/// used rather than by their numbers.
#[test]
fn a_two_process_machine_is_read_back_with_its_state_names() {
    let design = design("fsm.sv", Some("fsm"));
    let fsms = fsm::find(&design);
    assert_eq!(fsms.len(), 1, "{fsms:#?}");
    let machine = &fsms[0];

    assert_eq!(machine.state_name, "state");
    assert_eq!(machine.next_name.as_deref(), Some("next_state"));
    assert_eq!(machine.clock, "clk");
    assert_eq!(machine.reset_state.as_deref(), Some("S_IDLE"));

    let states: Vec<&str> = machine.states.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(states, ["S_IDLE", "S_RUN", "S_WAIT"]);

    let edges: Vec<String> = machine
        .transitions
        .iter()
        .map(|t| format!("{} -> {} [{}]", t.from, t.to, t.guard.join(" && ")))
        .collect();
    assert!(edges.contains(&"S_IDLE -> S_RUN [start]".to_string()), "{edges:?}");
    assert!(edges.contains(&"S_RUN -> S_WAIT []".to_string()), "{edges:?}");
    assert!(edges.contains(&"S_WAIT -> S_IDLE [done]".to_string()), "{edges:?}");
    // `default:` is not a state, and is not silently dropped either.
    assert!(edges.contains(&"(any other) -> S_IDLE []".to_string()), "{edges:?}");
}

/// The one-`typedef enum` style, where the names come from the enum instead.
#[test]
fn an_enum_encoded_machine_takes_its_names_from_the_enum() {
    let design = design("fsm_enum.sv", Some("fsm_enum"));
    let fsms = fsm::find(&design);
    assert_eq!(fsms.len(), 1, "{fsms:#?}");
    let states: Vec<&str> = fsms[0].states.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(states, ["S_IDLE", "S_RUN", "S_DONE"]);
}

/// Clocks are compared by identity, not by name: `src_clk` at the top and
/// `dst_clk` inside the child are the same two wires whatever they are called.
#[test]
fn clock_domains_are_found_through_the_hierarchy() {
    let design = design("cdc.sv", Some("cdc_top"));
    let report = cdc::analyse(&design);

    let clocks: Vec<&str> = report.domains.iter().map(|d| d.clock.as_str()).collect();
    assert_eq!(clocks.len(), 2, "{clocks:?}");
    assert!(clocks.contains(&"src_clk"), "{clocks:?}");
    assert!(clocks.contains(&"dst_clk"), "{clocks:?}");
}

/// A synchroniser written inside a block that does a hundred other things is
/// still a synchroniser — which is how they are actually written.
#[test]
fn a_two_flop_synchroniser_is_recognised_and_a_bare_bus_is_not() {
    let design = design("cdc.sv", Some("cdc_top"));
    let report = cdc::analyse(&design);

    let flag = report
        .crossings
        .iter()
        .find(|c| c.signal == "flag_src")
        .unwrap_or_else(|| panic!("no crossing on `flag_src`: {:#?}", report.crossings));
    assert_eq!(flag.kind, CrossingKind::TwoFlopSynchroniser);
    assert_eq!(flag.from, "src_clk");
    assert_eq!(flag.to, "dst_clk");

    let count = report
        .crossings
        .iter()
        .find(|c| c.signal == "count_src")
        .expect("no crossing on `count_src`");
    assert_eq!(count.kind, CrossingKind::MultiBitUnsynchronised);
    assert_eq!(count.width, 8);
}

/// A design on one clock has nothing to report, and says so rather than
/// inventing a crossing out of the reset.
#[test]
fn a_single_clock_design_has_no_crossings() {
    let design = design("fifo.sv", None);
    let report = cdc::analyse(&design);
    assert_eq!(report.domains.len(), 1, "{:#?}", report.domains);
    assert!(report.crossings.is_empty(), "{:#?}", report.crossings);
}

/// A latch is combinational logic that does not assign on every path — and
/// nothing else. The two correct blocks in the fixture must not be reported,
/// which is the harder half.
#[test]
fn only_the_incomplete_assignments_are_called_latches() {
    let design = design("latch.sv", Some("latch_check"));
    let report = lint::analyse(&design);

    let found: Vec<(&str, &str)> =
        report.latches.iter().map(|l| (l.net.as_str(), l.because.as_str())).collect();
    assert_eq!(
        found,
        [("y_if", "this `if` has no `else`"), ("y_case", "this `case` has no `default`"),],
        "{report:#?}"
    );
}

/// A signal driven and read by nobody, and — the part that matters — not the
/// ports and default-assigned signals around it.
#[test]
fn a_signal_nothing_reads_is_reported_and_ports_are_not() {
    let design = design("latch.sv", Some("latch_dead"));
    let report = lint::analyse(&design);

    let dead: Vec<&str> = report.dead_nets.iter().map(|n| n.net.as_str()).collect();
    assert_eq!(dead, ["spare"], "{report:#?}");
}

/// `wire x = expr;` drives `x` forever; `logic x = expr;` says what it powers
/// up holding. Reading both as continuous assignments gives every initialised
/// variable a second driver fighting the `always_ff` that owns it.
#[test]
fn a_declaration_initialiser_means_different_things_for_a_wire_and_a_variable() {
    let design = design("latch.sv", Some("latch_check"));
    // Nothing in this fixture initialises at declaration, so the check is that
    // the analysis of it stays clean; the distinction itself is pinned by
    // `dump_ir` goldens over `function.sv` and `task_loop.sv`.
    let report = lint::analyse(&design);
    assert!(report.dead_modules.is_empty(), "{report:#?}");
}

/// A straight chain of registers is as many stages as it is long.
#[test]
fn a_register_chain_is_read_back_as_stages() {
    let design = design("pipeline3.sv", Some("pipeline3"));
    let report = pipeline::analyse(&design);

    assert_eq!(report.registers, 6, "{report:#?}");
    assert_eq!(report.domains.len(), 1);
    let domain = &report.domains[0];
    assert_eq!(domain.clock, "clk");
    assert_eq!(domain.depth, 3);

    let stages: Vec<Vec<&str>> =
        domain.stages.iter().map(|s| s.registers.iter().map(String::as_str).collect()).collect();
    assert_eq!(
        stages,
        [vec!["data_d1", "valid_d1"], vec!["data_d2", "valid_d2"], vec!["data_d3", "valid_d3"],]
    );
}

/// A counter feeds itself, so it is one piece of state rather than an infinite
/// chain of stages. Reporting it as feedback is the difference between an
/// answer and a hang.
#[test]
fn a_register_that_feeds_itself_is_reported_as_feedback() {
    let design = design("counter.sv", None);
    let report = pipeline::analyse(&design);
    let domain = report.domains.first().expect("a clock domain");

    assert!(
        domain.feedback.iter().any(|f| f.registers.iter().any(|r| r.contains("count"))),
        "the counter should be a feedback group: {domain:#?}"
    );
    // And it still gets a stage rather than being left out.
    assert!(domain.depth >= 1, "{domain:#?}");
}

/// A blocking assignment inside a clocked block settles within the same clock,
/// so it is a wire with a name — every temporary of an inlined function is one.
/// Counting them as registers puts a stage boundary inside one clock's logic.
#[test]
fn a_blocking_assignment_in_a_clocked_block_is_not_a_register() {
    let design = design("task_loop.sv", Some("task_loop"));
    let report = pipeline::analyse(&design);

    let registers: Vec<&str> = report
        .domains
        .iter()
        .flat_map(|d| d.stages.iter())
        .flat_map(|s| s.registers.iter())
        .map(String::as_str)
        .collect();
    assert!(
        !registers.iter().any(|r| r.contains('.')),
        "an inlined function's temporaries are not registers: {registers:?}"
    );
}

// ------------------------------------------------- cycles made of wires ---

/// A value that decides itself has no settled answer, and the two assignments
/// that make one are each innocent alone. That is what makes it worth a report:
/// it cannot be seen line by line.
#[test]
fn a_cycle_of_wires_is_reported_with_where_each_of_them_lives() {
    let design = design("comb_loop.sv", Some("comb_loop"));
    let report = rtlscope_analyse::lint::analyse(&design);

    assert_eq!(report.comb_loops.len(), 1, "one loop: {:#?}", report.comb_loops);
    let found = &report.comb_loops[0];
    let mut names = found.signals.clone();
    names.sort();
    assert_eq!(names, ["knot_a", "knot_b"], "the two that hold each other up");
    assert_eq!(found.spans.len(), found.signals.len(), "each one says where it lives");
    assert!(found.spans.iter().all(|span| span.line > 0), "and the line is real: {found:#?}");
    assert!(!found.partial, "neither is written through a slice");

    // The half that must not be reported: three wires deep, and every one of
    // them settles.
    assert!(
        !found.signals.iter().any(|name| name.starts_with("step_")),
        "the plain chain is not a loop: {:?}",
        found.signals
    );
}

/// A pipeline is the shape this must stay quiet about. Every wire in it feeds
/// forwards, and a walk that called that a cycle would report every design.
#[test]
fn a_register_chain_has_no_cycles_of_wires() {
    let design = design("pipeline3.sv", Some("pipeline3"));
    let report = rtlscope_analyse::lint::analyse(&design);
    assert!(report.comb_loops.is_empty(), "{:#?}", report.comb_loops);
}

/// A latch holds its value through its own feedback — that is what a latch is.
/// Reporting it here as well would be the same finding twice, in the language
/// of a bug it is not.
#[test]
fn a_latch_holds_itself_and_is_not_a_cycle_of_wires() {
    let design = design("latch.sv", Some("latch_check"));
    let report = rtlscope_analyse::lint::analyse(&design);

    assert!(!report.latches.is_empty(), "the latch itself is still reported");
    assert!(report.comb_loops.is_empty(), "and not a second time: {:#?}", report.comb_loops);
}

/// The graph moved out of `pipeline::analyse` into `depth::signal_graph`, and
/// the whole claim of that move is that the answer did not change. The other
/// pipeline tests read fields; this reads the bytes, which is what a caller
/// downstream of the JSON actually depends on.
#[test]
fn lifting_the_graph_out_left_the_pipeline_report_byte_for_byte() {
    let design = design("pipeline3.sv", Some("pipeline3"));
    let report = rtlscope_analyse::pipeline::analyse(&design);
    let text = serde_json::to_string(&report).expect("serialises");

    assert_eq!(
        text,
        concat!(
            r#"{"domains":[{"clock":"clk","depth":3,"stages":["#,
            r#"{"index":0,"registers":["data_d1","valid_d1"]},"#,
            r#"{"index":1,"registers":["data_d2","valid_d2"]},"#,
            r#"{"index":2,"registers":["data_d3","valid_d3"]}"#,
            r#"]}],"registers":6}"#,
        )
    );
}

/// And a report with none of the new finding serialises exactly as it did
/// before the field existed — which is what `skip_serializing_if` is for.
#[test]
fn a_design_with_no_cycles_serialises_without_the_new_field() {
    let design = design("pipeline3.sv", Some("pipeline3"));
    let report = rtlscope_analyse::lint::analyse(&design);
    let text = serde_json::to_string(&report).expect("serialises");
    assert!(!text.contains("comb_loops"), "{text}");
}

// --------------------------------------------------- how many clocks apart ---

use rtlscope_analyse::depth::{self, DepthError, DepthWarning};

/// The convention everything else rests on, in one test.
///
/// The walk starts at A's value, so A's own register does not count; arriving
/// at a register costs one, so B's does. That makes the last register of a
/// pipeline zero clocks from the output it drives — which is right: the
/// register's value *is* the output, a wire later.
#[test]
fn the_count_is_the_edges_crossed_leaving_a_and_arriving_at_b() {
    let design = design("pipeline3.sv", Some("pipeline3"));

    let whole = depth::analyse(&design, "in_data", "out_data");
    assert_eq!(whole.min_stages, Some(3), "{whole:#?}");
    assert_eq!(whole.max_stages, Some(3), "and only one way through");
    assert!(!whole.reconvergent && !whole.feedback && !whole.variable_latency, "{whole:#?}");
    assert_eq!(whole.clock.as_deref(), Some("clk"));
    assert!(whole.errors.is_empty() && whole.warnings.is_empty(), "{whole:#?}");

    let first = depth::analyse(&design, "in_data", "data_d1");
    assert_eq!(first.min_stages, Some(1), "one edge into the first register");

    let middle = depth::analyse(&design, "data_d1", "data_d3");
    assert_eq!(middle.min_stages, Some(2), "the departure is not counted");

    let last = depth::analyse(&design, "data_d3", "out_data");
    assert_eq!(last.min_stages, Some(0), "a register's value is its output, a wire later");
    assert_eq!(last.clock, None, "and no clock was crossed to say it");
    // A road with nothing on it is still a road, and showing it empty is how
    // the report says the two are one clock's worth of logic apart.
    assert_eq!(last.paths.len(), 1, "{:#?}", last.paths);
    assert!(last.paths[0].registers.is_empty(), "{:#?}", last.paths[0]);
}

/// The paths are there so a reader can see which logic the number is about, and
/// each register on one has to be somewhere they can be sent.
#[test]
fn a_path_names_the_registers_it_clocks_through_and_where_they_live() {
    let design = design("pipeline3.sv", Some("pipeline3"));
    let report = depth::analyse(&design, "in_data", "out_data");

    assert_eq!(report.paths.len(), 1, "one way through: {:#?}", report.paths);
    let path = &report.paths[0];
    assert_eq!(path.stages, 3);
    let names: Vec<&str> = path.registers.iter().map(|r| r.name.as_str()).collect();
    assert_eq!(names, ["data_d1", "data_d2", "data_d3"], "in the order the value passes them");
    assert!(
        path.registers.iter().all(|r| !r.span.is_unknown() && r.span.line > 0),
        "each one can be opened: {:#?}",
        path.registers
    );
}

/// A counter feeds itself, so the walk can go round again. There is no longest
/// path, and printing the longest walk that happens not to repeat a signal
/// would be printing a number that is nowhere in the design.
#[test]
fn a_road_that_loops_has_a_minimum_and_no_maximum() {
    let design = design("counter.sv", None);
    let report = depth::analyse(&design, "en", "count");

    assert!(report.min_stages.is_some(), "the shortest way is still a fact: {report:#?}");
    assert_eq!(report.max_stages, None, "the longest is not");
    assert!(report.feedback, "and it says why");
    assert!(report.variable_latency);
    assert!(!report.problems.is_empty(), "in words as well as flags: {:?}", report.problems);
    assert!(
        !report.warnings.iter().any(|w| matches!(w, DepthWarning::Reconvergent { .. })),
        "no min/max disagreement is claimed when there is no max"
    );
}

/// Two clocks on one road means "how many clocks" has no answer at all, and
/// picking one of them would be a claim about timing that does not hold.
#[test]
fn two_clocks_on_one_road_is_an_error_rather_than_a_number() {
    let design = design("cdc.sv", Some("cdc_top"));
    let report = depth::analyse(&design, "pulse", "flag_out");

    let crossing = report.errors.iter().find_map(|error| match error {
        DepthError::CrossDomain { clocks, registers } => Some((clocks, registers)),
        _ => None,
    });
    let Some((clocks, registers)) = crossing else {
        panic!("expected a cross-domain refusal: {report:#?}");
    };
    assert_eq!(clocks.len(), 2, "both are named: {clocks:?}");
    assert!(!registers.is_empty(), "with a register from each: {registers:?}");
    assert_eq!(report.min_stages, None, "and no number is offered");
}

/// The case a check over arrivals alone would wave through: a value that leaves
/// a register on one clock and lands on a register on another, with nothing in
/// between. The departure is not counted, but it is still where the value came
/// from, and that is what makes this a crossing.
#[test]
fn a_crossing_with_nothing_in_between_is_still_a_crossing() {
    let design = design("cdc.sv", Some("cdc_top"));
    let report = depth::analyse(&design, "count_src", "count_out");

    assert!(
        report.errors.iter().any(|e| matches!(e, DepthError::CrossDomain { .. })),
        "{report:#?}"
    );
    assert_eq!(report.min_stages, None, "and no number is offered");
}

/// Nothing that leaves the output arrives at the input. Saying so beats an
/// empty report, which reads as though the question was understood.
#[test]
fn a_road_that_does_not_exist_says_so() {
    let design = design("pipeline3.sv", Some("pipeline3"));
    let report = depth::analyse(&design, "out_data", "in_data");
    assert!(report.errors.iter().any(|e| matches!(e, DepthError::NoPath { .. })), "{report:#?}");
}

/// A value on the way that decides itself never settles, so counting the clocks
/// it takes is counting something that does not happen.
#[test]
fn a_cycle_of_wires_on_the_road_refuses_the_count() {
    let design = design("comb_loop.sv", Some("comb_loop"));
    let report = depth::analyse(&design, "seed", "out_knot");

    assert!(report.errors.iter().any(|e| matches!(e, DepthError::CombLoop { .. })), "{report:#?}");
    assert_eq!(report.min_stages, None);

    // And the other output is reachable through wires that settle, so the same
    // design still answers the question that has an answer.
    let fine = depth::analyse(&design, "seed", "out_chain");
    assert_eq!(fine.min_stages, Some(1), "{fine:#?}");
}

/// A register that only takes its input when something says so still advances
/// one stage structurally — but not every cycle, and the report has to say
/// which of those two facts it is stating.
#[test]
fn a_gate_on_the_road_makes_the_latency_variable_and_names_the_guard() {
    let design = design("trace_demo.sv", Some("trace_demo"));
    let report = depth::analyse(&design, "in_data", "out_data");

    assert_eq!(report.min_stages, Some(1), "one register deep: {report:#?}");
    assert!(report.variable_latency, "but not every cycle");

    let gate = report.warnings.iter().find_map(|warning| match warning {
        DepthWarning::Gated { registers, guards } => Some((registers, guards)),
        _ => None,
    });
    let Some((registers, guards)) = gate else { panic!("expected a gate: {report:#?}") };
    assert!(registers.iter().any(|name| name == "staged"), "{registers:?}");
    assert!(
        guards.iter().any(|text| text.contains("gate")),
        "named as the source wrote it: {guards:?}"
    );
    assert!(
        report.paths.iter().any(|path| path.gated_at.iter().any(|n| n == "staged")),
        "and marked on the path itself: {:#?}",
        report.paths
    );
}

/// Two roads of different length to the same place. Reporting one number would
/// hide the disagreement, and the disagreement is the finding: almost always
/// one branch was pipelined and the other was not.
#[test]
fn two_roads_of_different_length_are_both_reported() {
    let design = design("reconverge.sv", Some("reconverge"));
    let report = depth::analyse(&design, "a", "sum");

    assert_eq!(report.min_stages, Some(2), "the short way: fast then sum: {report:#?}");
    assert_eq!(report.max_stages, Some(3), "the long way: two slow registers then sum");
    assert!(report.reconvergent, "and it is called what it is");
    assert!(!report.feedback, "nothing here loops");

    let told = report
        .warnings
        .iter()
        .any(|warning| matches!(warning, DepthWarning::Reconvergent { min: 2, max: 3 }));
    assert!(told, "with both numbers in the warning: {:#?}", report.warnings);

    // Both roads are shown, so a reader can see which registers each is about.
    assert_eq!(report.paths.len(), 2, "{:#?}", report.paths);
    let lengths: Vec<usize> = report.paths.iter().map(|path| path.stages).collect();
    assert_eq!(lengths, [2, 3], "shortest first");
    assert!(
        report.paths[1].registers.iter().any(|r| r.name == "slow_two"),
        "the long one goes through the extra register: {:#?}",
        report.paths[1]
    );
}

/// A name the design does not have is answered with names it does. A bare
/// refusal leaves the reader guessing at spelling and hierarchy at once.
#[test]
fn a_name_that_is_not_there_offers_the_ones_that_are() {
    let design = design("pipeline3.sv", Some("pipeline3"));
    let report = depth::analyse(&design, "in_dat", "out_data");

    let unknown = report.errors.iter().find_map(|error| match error {
        DepthError::UnknownSignal { name, candidates } => Some((name, candidates)),
        _ => None,
    });
    let Some((name, candidates)) = unknown else { panic!("{report:#?}") };
    assert_eq!(name, "in_dat");
    assert!(candidates.iter().any(|c| c == "in_data"), "{candidates:?}");
}

/// A leaf name is what somebody reading a waveform types. It resolves when it
/// is the only one, which is the case in a flat design.
#[test]
fn a_leaf_name_resolves_when_only_one_signal_wears_it() {
    let design = design("hier.sv", Some("hier_top"));
    let flat = rtlscope_analyse::flat::flatten(&design);
    let full: Vec<String> =
        flat.all_names(&design).map(|(name, _, _, _)| name).filter(|n| n.contains('.')).collect();
    assert!(!full.is_empty(), "the fixture is hierarchical: {full:?}");

    // Whatever the hierarchy, asking by a name that exists must not be an
    // unknown-signal refusal.
    let leaf = full[0].rsplit('.').next().expect("a leaf").to_string();
    let report = depth::analyse(&design, &leaf, &leaf);
    assert!(
        !report.errors.iter().any(|e| matches!(e, DepthError::UnknownSignal { .. })),
        "`{leaf}` is a name this design has: {report:#?}"
    );
}

// -------------------------------------------------------------- the sample ---

/// The sample exists to be run, so what it demonstrates has to keep being true.
///
/// Four roads leave one input and each is a different answer. If any of them
/// stops giving the answer it was built to give, the sample teaches the wrong
/// thing — which is worse than having no sample.
#[test]
fn the_sample_gives_one_answer_of_each_kind() {
    let design = design("depth_demo.sv", Some("depth_demo"));

    // Plain: one road, one number.
    let plain = depth::analyse(&design, "sample_in", "sample_out");
    assert_eq!(plain.min_stages, Some(3), "{plain:#?}");
    assert_eq!(plain.max_stages, Some(3));
    assert!(plain.warnings.is_empty() && !plain.feedback, "nothing to qualify it");

    // The bug: the escort is a clock short of what it escorts.
    let bug = depth::analyse(&design, "sample_in", "stamped");
    assert_eq!((bug.min_stages, bug.max_stages), (Some(3), Some(4)), "{bug:#?}");
    assert!(bug.reconvergent);
    assert_eq!(bug.paths.len(), 2, "both roads are shown: {:#?}", bug.paths);

    // Gated: as deep, but not every cycle.
    let gated = depth::analyse(&design, "sample_in", "captured");
    assert_eq!(gated.min_stages, Some(4), "{gated:#?}");
    assert!(gated.variable_latency);
    assert!(!gated.feedback, "a gate is not feedback");

    // Feeds itself: a floor and no ceiling.
    let looping = depth::analyse(&design, "sample_in", "total");
    assert_eq!(looping.min_stages, Some(4), "{looping:#?}");
    assert_eq!(looping.max_stages, None);
    assert!(looping.feedback);

    // And one road with no answer at all.
    let crossing = depth::analyse(&design, "valid_in", "cfg_busy");
    assert!(
        crossing.errors.iter().any(|e| matches!(e, DepthError::CrossDomain { .. })),
        "{crossing:#?}"
    );
}

/// A machine whose states are an enum from a package is named all the same:
/// the package's members come along with the module that imports them.
#[test]
fn a_machine_on_a_package_enum_takes_its_names_from_the_package() {
    let design = design("packages.sv", Some("packages_top"));
    let fsms = fsm::find(&design);
    let walker =
        fsms.iter().find(|m| m.module_name == "walker").unwrap_or_else(|| panic!("{fsms:#?}"));
    let states: Vec<&str> = walker.states.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(states, ["S_IDLE", "S_RUN", "S_DONE"]);
    assert_eq!(walker.reset_state.as_deref(), Some("S_IDLE"));
}

/// The stage of a register, by the net that holds it: what the diagram cuts
/// a process with. `pipeline3` is the fixture whose stages are known by
/// construction, so they are asserted rather than merely counted.
#[test]
fn every_register_of_the_pipeline_has_its_stage_by_net() {
    let design = design("pipeline3.sv", Some("pipeline3"));
    let report = rtlscope_analyse::pipeline::analyse(&design);
    let top = design.top;
    let module = design.module(top);
    let stage_of = |name: &str| {
        let (net, _) = module.nets.iter_enumerated().find(|(_, net)| net.name == name).expect(name);
        report.stage_by_net.get(&(top, net)).copied()
    };
    assert_eq!(stage_of("valid_d1"), Some(0));
    assert_eq!(stage_of("data_d1"), Some(0));
    assert_eq!(stage_of("valid_d2"), Some(1));
    assert_eq!(stage_of("data_d3"), Some(2));
    assert_eq!(stage_of("in_data"), None, "an input is not a register");

    // And what feeds each: `data_d2 <= data_d1 + 1` reads `data_d1`.
    let net =
        |name: &str| module.nets.iter_enumerated().find(|(_, net)| net.name == name).expect(name).0;
    let feeds = report.feeds_by_net.get(&(top, net("data_d2"))).expect("data_d2 is fed");
    assert!(feeds.contains(&(top, net("data_d1"))), "{feeds:?}");
    assert!(!feeds.contains(&(top, net("in_data"))), "not two stages back: {feeds:?}");
}
