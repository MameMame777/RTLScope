//! What elaboration is supposed to produce.
//!
//! Two of these are regressions for bugs that only showed up when the pass was
//! first run end to end, and both came from the same trap in the `sv-parser`
//! grammar: a container node that is present but empty. See
//! [`parameters_propagate_through_two_levels`] and [`clog2_folds_in_a_localparam`].

use std::path::PathBuf;

use rtlscope_ir::{Design, Diagnostics, NetKind};
use rtlscope_sv::ParseOptions;

fn elaborate(fixture: &str, top: Option<&str>) -> (Design, Diagnostics) {
    let path = rtlscope_fixtures::path(fixture);
    elaborate_paths(&[path], top)
}

fn elaborate_paths(paths: &[PathBuf], top: Option<&str>) -> (Design, Diagnostics) {
    let (uir, mut diags) = rtlscope_sv::lower_files(paths, &ParseOptions::default());
    let (design, elab_diags) = rtlscope_elab::elaborate(&uir, top);
    diags.extend(elab_diags);
    (design.expect("elaboration produced a design"), diags)
}

fn module<'a>(design: &'a Design, name: &str) -> &'a rtlscope_ir::Module {
    design.modules.iter().find(|m| m.name == name).unwrap_or_else(|| {
        let names: Vec<&str> = design.modules.iter().map(|m| m.name.as_str()).collect();
        panic!("no module `{name}`; have {names:?}")
    })
}

/// `.port(net)` pairs for one instance, by name.
fn connections(design: &Design, parent: &rtlscope_ir::Module, inst_name: &str) -> Vec<String> {
    let inst = parent
        .insts
        .iter()
        .find(|i| i.name == inst_name)
        .unwrap_or_else(|| panic!("no instance `{inst_name}`"));
    let child = &design.modules[inst.of];
    inst.conns
        .iter()
        .map(|conn| {
            let port_name = &child.ports[conn.port.0 as usize].name;
            let net = match &conn.net {
                rtlscope_ir::NetRef::Full { net } => parent.net(*net).name.clone(),
                rtlscope_ir::NetRef::Slice { net, msb, lsb } => {
                    format!("{}[{msb}:{lsb}]", parent.net(*net).name)
                }
                rtlscope_ir::NetRef::Const { value } => format!("{:?}", value.to_u64()),
            };
            format!(".{port_name}({net})")
        })
        .collect()
}

#[test]
fn parameters_propagate_through_two_levels() {
    // Regression: `.W(W)` lowered to `Unsupported` because a plain identifier's
    // `Option<ClassQualifierOrPackageScope>` is `Some` but empty. Every
    // parameter override and every connection silently became nothing, and the
    // child kept its default width.
    let (design, _) = elaborate("params.sv", Some("params_top"));

    assert_eq!(module(&design, "params_sub$W=16").ports[0].name, "i");
    let wide = module(&design, "params_subsub$W=16");
    assert_eq!(wide.net(wide.ports[0].net).width, 16, "16 must reach the grandchild");

    let narrow = module(&design, "params_subsub$W=8");
    assert_eq!(narrow.net(narrow.ports[0].net).width, 8);
}

#[test]
fn one_module_with_two_parameter_bindings_becomes_two_modules() {
    let (design, _) = elaborate("params.sv", Some("params_top"));

    let specialisations: Vec<&str> = design
        .modules
        .iter()
        .filter(|m| m.base_name == "params_sub")
        .map(|m| m.name.as_str())
        .collect();
    assert_eq!(specialisations.len(), 2, "got {specialisations:?}");

    let top = module(&design, "params_top");
    let of: Vec<&str> = top.insts.iter().map(|i| design.modules[i.of].name.as_str()).collect();
    assert_eq!(of, ["params_sub$W=16", "params_sub$W=8"]);
}

#[test]
fn clog2_folds_in_a_localparam() {
    // Regression: `$clog2(DEPTH)` parses as `SystemTfCall::ArgExpression`, not
    // the `ArgOptionl` the lowering first handled. It became `Unsupported`, the
    // localparam silently evaluated to 0, and the FIFO pointers came out one bit
    // wide instead of five.
    let (design, _) = elaborate("fifo.sv", None);
    let fifo = module(&design, "fifo");

    let aw = fifo.params.iter().find(|p| p.name == "AW").expect("localparam AW");
    assert_eq!(aw.value, 4, "$clog2(16)");
    assert!(aw.is_local);

    let (_, wr_ptr) = fifo.net_by_name("wr_ptr").expect("wr_ptr");
    assert_eq!(wr_ptr.width, 5, "[AW:0] is AW+1 bits");
}

#[test]
fn an_unpacked_dimension_becomes_a_memory() {
    let (design, _) = elaborate("fifo.sv", None);
    let fifo = module(&design, "fifo");
    let (_, mem) = fifo.net_by_name("mem").expect("mem");

    assert_eq!(mem.width, 8);
    assert_eq!(mem.kind, NetKind::Memory { depth: 16 });
}

#[test]
fn a_computed_array_bound_is_folded() {
    // `regs [0:(1 << AW) - 1]` with AW = 4.
    let (design, _) = elaborate("hier.sv", Some("hier_top"));
    let regfile = module(&design, "hier_regfile");
    let (_, regs) = regfile.net_by_name("regs").expect("regs");

    assert_eq!(regs.kind, NetKind::Memory { depth: 16 });
}

#[test]
fn all_three_connection_styles_resolve_to_the_same_thing() {
    let (design, _) = elaborate("hier.sv", Some("hier_top"));
    let top = module(&design, "hier_top");

    // Named.
    assert_eq!(
        connections(&design, top, "u_alu"),
        [".a(rf_rdata)", ".b(rf_rdata)", ".op(op)", ".y(alu_y)"]
    );
    // Positional — the same shape, bound by order rather than by name.
    assert_eq!(
        connections(&design, top, "u_rf"),
        [
            ".clk(clk)",
            ".we(we)",
            ".waddr(waddr)",
            ".wdata(alu_y)",
            ".raddr(raddr)",
            ".rdata(rf_rdata)"
        ]
    );
    // Wildcard `.*` — every port bound to the same-named net.
    assert_eq!(
        connections(&design, top, "u_ctrl"),
        [".clk(clk)", ".rst_n(rst_n)", ".start(start)", ".busy(busy)"]
    );
}

#[test]
fn generate_unrolls_the_branch_the_condition_selects() {
    // BYPASS = 0, so `g_bypass` is discarded and `g_chain` runs TAPS times.
    // Inside it, i == 0 takes `g_first` and the rest take `g_rest`.
    let (design, _) = elaborate("genblk.sv", None);
    let genblk = module(&design, "genblk");

    // One process per unrolled copy, each keeping the span of the single line
    // the author wrote.
    let lines: Vec<u32> = genblk.procs.iter().map(|p| p.span.line).collect();
    assert_eq!(lines.iter().filter(|l| **l == 19).count(), 0, "g_bypass was not taken");
    assert_eq!(lines.iter().filter(|l| **l == 23).count(), 1, "g_first runs once");
    assert_eq!(lines.iter().filter(|l| **l == 25).count(), 3, "g_rest runs TAPS-1 times");
    assert_eq!(lines.iter().filter(|l| **l == 28).count(), 1, "the output assign");

    let (_, tap) = genblk.net_by_name("tap").expect("tap");
    assert_eq!(tap.kind, NetKind::Memory { depth: 4 });
}

#[test]
fn a_module_with_no_source_becomes_a_labelled_black_box() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/data/missing_child.sv");
    let (design, diags) = elaborate_paths(&[path], None);

    let bb = module(&design, "black_box");
    assert!(bb.is_blackbox, "an unknown module is a box, not a hole");
    // Its pins come from how it was instantiated, since nothing else knows them.
    let pins: Vec<&str> = bb.ports.iter().map(|p| p.name.as_str()).collect();
    assert_eq!(pins, ["clk", "q", "aux"]);

    let messages: Vec<&str> = diags.iter().map(|d| d.message.as_str()).collect();
    assert!(
        messages.iter().any(|m| m.contains("black box")),
        "the substitution must be reported: {messages:?}"
    );
}

#[test]
fn an_undeclared_net_is_inferred_and_reported() {
    // SystemVerilog creates an implicit one-bit net here, and so does RTLScope —
    // but it is far more often a typo than an intention, so it is reported.
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/data/missing_child.sv");
    let (design, diags) = elaborate_paths(&[path], None);

    let top = module(&design, "missing_child");
    let (_, net) = top.net_by_name("undeclared_net").expect("implicit net was created");
    assert_eq!(net.width, 1);

    assert!(
        diags.iter().any(|d| d.code == rtlscope_ir::DiagCode::ImplicitNet),
        "the inference must be reported"
    );
}

#[test]
fn an_ambiguous_top_is_refused_rather_than_guessed() {
    // params.sv has one root, but hier.sv plus params.sv has two, and picking
    // one would silently analyse half the input.
    let paths = [rtlscope_fixtures::path("hier.sv"), rtlscope_fixtures::path("params.sv")];
    let (uir, _) = rtlscope_sv::lower_files(&paths, &ParseOptions::default());
    let (design, diags) = rtlscope_elab::elaborate(&uir, None);

    assert!(design.is_none(), "no design should be produced");
    assert!(diags.iter().any(|d| d.code == rtlscope_ir::DiagCode::TopAmbiguous));
    assert!(diags.iter().any(|d| d.message.contains("--top")), "the fix is suggested");
}

#[test]
fn a_named_top_that_does_not_exist_is_an_error() {
    let (uir, _) =
        rtlscope_sv::lower_files(&[rtlscope_fixtures::path("hier.sv")], &ParseOptions::default());
    let (design, diags) = rtlscope_elab::elaborate(&uir, Some("no_such_module"));

    assert!(design.is_none());
    assert!(diags.iter().any(|d| d.code == rtlscope_ir::DiagCode::TopNotFound));
}

#[test]
fn every_elaborated_node_carries_a_resolvable_span() {
    // The project's core invariant: without this, "jump to source" and the MCP
    // server both have nothing to answer with.
    for (fixture, top) in [
        ("hier.sv", Some("hier_top")),
        ("params.sv", Some("params_top")),
        ("fifo.sv", None),
        ("genblk.sv", None),
        ("counter.sv", None),
        ("pipeline3.sv", None),
    ] {
        let (design, _) = elaborate(fixture, top);
        for module in design.modules.iter() {
            assert!(!module.span.is_unknown(), "{fixture}: module {}", module.name);
            for net in module.nets.iter() {
                assert!(
                    !net.span.is_unknown() && design.files.path(net.span.file).is_some(),
                    "{fixture}: net {} in {}",
                    net.name,
                    module.name
                );
            }
            for inst in &module.insts {
                assert!(!inst.span.is_unknown(), "{fixture}: instance {}", inst.name);
            }
        }
    }
}

#[test]
fn no_parameter_expression_survives_elaboration() {
    // Invariant 3: every width is a number by now. A width of zero would mean
    // an expression quietly failed and left a hole.
    for (fixture, top) in [("hier.sv", Some("hier_top")), ("fifo.sv", None), ("genblk.sv", None)] {
        let (design, _) = elaborate(fixture, top);
        for module in design.modules.iter() {
            for net in module.nets.iter() {
                assert!(net.width > 0, "{fixture}: {} has zero width", net.name);
            }
        }
    }
}

// ---------------------------------------------------------------- processes ---

fn kinds(module: &rtlscope_ir::Module) -> Vec<String> {
    module
        .procs
        .iter()
        .map(|process| match &process.kind {
            rtlscope_ir::ProcKind::Comb => "comb".to_string(),
            rtlscope_ir::ProcKind::Latch => "latch".to_string(),
            rtlscope_ir::ProcKind::Initial => "initial".to_string(),
            rtlscope_ir::ProcKind::Ff { rst, .. } => match rst {
                None => "ff".to_string(),
                Some(reset) => format!("ff+{:?}", reset.kind).to_lowercase(),
            },
        })
        .collect()
}

fn names_of<'a>(module: &'a rtlscope_ir::Module, refs: &[rtlscope_ir::NetRef]) -> Vec<&'a str> {
    refs.iter().filter_map(|r| r.net_id()).map(|net| module.net(net).name.as_str()).collect()
}

#[test]
fn a_body_reset_makes_a_synchronous_reset_and_the_sensitivity_list_an_asynchronous_one() {
    // counter.sv clocks on one edge and tests `!rst_n` in the body: synchronous.
    let (design, _) = elaborate("counter.sv", None);
    let counter = module(&design, "counter");
    assert_eq!(kinds(counter), ["ff+sync"]);

    // fifo.sv lists two edges, so the reset is asynchronous — and which of the
    // two is the clock is decided by which one the body tests first.
    let (design, _) = elaborate("fifo.sv", None);
    let fifo = module(&design, "fifo");
    assert_eq!(kinds(fifo), ["comb", "comb", "ff+async", "ff+async"]);

    let flop = fifo.procs.iter().find(|p| matches!(p.kind, rtlscope_ir::ProcKind::Ff { .. }));
    let rtlscope_ir::ProcKind::Ff { clk, rst, .. } = &flop.unwrap().kind else { unreachable!() };
    assert_eq!(names_of(fifo, std::slice::from_ref(clk)), ["clk"]);
    assert_eq!(rst.as_ref().unwrap().active, rtlscope_ir::Level::Low);
}

#[test]
fn a_write_enable_is_not_mistaken_for_a_reset() {
    // `always_ff @(posedge clk) if (we) regs[waddr] <= wdata;` has a leading
    // `if` like a reset does, but its branch drives a register from another
    // signal rather than to a constant. Calling `we` a reset would put a false
    // edge in the register-adjacency graph.
    let (design, _) = elaborate("hier.sv", Some("hier_top"));
    let regfile = module(&design, "hier_regfile");

    let flop = regfile
        .procs
        .iter()
        .find(|p| matches!(p.kind, rtlscope_ir::ProcKind::Ff { .. }))
        .expect("the register file has a flop");
    let rtlscope_ir::ProcKind::Ff { rst, .. } = &flop.kind else { unreachable!() };
    assert!(rst.is_none(), "`we` is an enable, not a reset");
}

#[test]
fn a_memory_write_reads_its_address() {
    // `regs[waddr] <= wdata` reads `waddr`. A `NetRef` cannot hold the index, so
    // dropping it would leave the dataflow graph without an edge into the write
    // port — the address would look like it drives nothing.
    let (design, _) = elaborate("hier.sv", Some("hier_top"));
    let regfile = module(&design, "hier_regfile");
    let flop =
        regfile.procs.iter().find(|p| matches!(p.kind, rtlscope_ir::ProcKind::Ff { .. })).unwrap();

    let reads = names_of(regfile, &flop.reads);
    assert!(reads.contains(&"waddr"), "the address is a read: {reads:?}");
    assert!(reads.contains(&"wdata"), "{reads:?}");
    assert_eq!(names_of(regfile, &flop.writes), ["regs"]);
}

#[test]
fn a_continuous_assignment_is_combinational_logic() {
    let (design, _) = elaborate("hier.sv", Some("hier_top"));
    let top = module(&design, "hier_top");

    assert_eq!(kinds(top), ["comb"], "the one `assign result = alu_y`");
    assert_eq!(names_of(top, &top.procs[0].reads), ["alu_y"]);
    assert_eq!(names_of(top, &top.procs[0].writes), ["result"]);
}

#[test]
fn a_flop_reads_its_own_clock_and_reset() {
    // The body never names them, but the register-adjacency graph has to see
    // that the process depends on them.
    let (design, _) = elaborate("counter.sv", None);
    let counter = module(&design, "counter");
    let reads = names_of(counter, &counter.procs[0].reads);

    assert!(reads.contains(&"clk"), "{reads:?}");
    assert!(reads.contains(&"rst_n"), "{reads:?}");
}

#[test]
fn a_state_register_shows_up_as_a_self_loop() {
    // What Phase 5 FSM extraction looks for: a flop that writes `state` while a
    // combinational process reads `state` and writes what feeds it back.
    let (design, _) = elaborate("fsm.sv", None);
    let fsm = module(&design, "fsm");

    let flop =
        fsm.procs.iter().find(|p| matches!(p.kind, rtlscope_ir::ProcKind::Ff { .. })).unwrap();
    assert_eq!(names_of(fsm, &flop.writes), ["state"]);
    assert!(names_of(fsm, &flop.reads).contains(&"next_state"));

    let comb = fsm
        .procs
        .iter()
        .find(|p| {
            matches!(p.kind, rtlscope_ir::ProcKind::Comb)
                && names_of(fsm, &p.writes).contains(&"next_state")
        })
        .expect("the next-state logic");
    assert!(names_of(fsm, &comb.reads).contains(&"state"), "the loop closes");
}

#[test]
fn a_statement_outside_the_subset_leaves_a_labelled_hole_not_a_gap() {
    // The `casex` inside unsupported.sv's `always` cannot be modelled, but the
    // process around it still exists and the hole is counted against the module
    // so a GUI badge can show it.
    let (design, _) = elaborate("unsupported.sv", Some("unsupported"));
    let module = module(&design, "unsupported");

    assert!(!module.procs.is_empty(), "the always block is still a process");
    let holes: Vec<&str> = module.skipped.iter().map(|s| s.construct.as_str()).collect();
    assert!(holes.contains(&"casex"), "the hole is recorded on the module: {holes:?}");
}

#[test]
fn the_invariants_hold_for_every_fixture() {
    // `validate` reports RTLScope's own bugs, so anything it finds here is a
    // defect in the elaborator, not in the fixture.
    for (fixture, top) in [
        ("hier.sv", Some("hier_top")),
        ("params.sv", Some("params_top")),
        ("fifo.sv", None),
        ("genblk.sv", None),
        ("counter.sv", None),
        ("fsm.sv", None),
        ("pipeline3.sv", None),
        ("adder.sv", None),
        ("nonansi.sv", None),
    ] {
        let (design, _) = elaborate(fixture, top);
        let report = rtlscope_elab::validate(&design);
        assert!(
            !report.has_errors(),
            "{fixture} violates an invariant:\n{}",
            report.render(&design.files)
        );
    }
}

#[test]
fn a_module_elaborated_once_keeps_its_plain_name() {
    // Naming a module after every parameter it has produced a 408-character
    // label on real RTL, for a module that had no twin to be told apart from.
    // The values are in `params` either way.
    let (design, _) = elaborate("fifo.sv", None);
    let fifo = module(&design, "fifo");

    assert_eq!(fifo.base_name, "fifo");
    assert_eq!(fifo.params.len(), 3, "W, DEPTH and the derived AW are all still here");
}

#[test]
fn two_specialisations_are_named_by_what_differs_between_them() {
    let (design, _) = elaborate("params.sv", Some("params_top"));

    let mut names: Vec<&str> = design
        .modules
        .iter()
        .filter(|m| m.base_name == "params_sub")
        .map(|m| m.name.as_str())
        .collect();
    names.sort();
    assert_eq!(names, ["params_sub$W=16", "params_sub$W=8"]);
}

#[test]
fn a_parameter_that_is_the_same_everywhere_stays_out_of_the_name() {
    // genblk has three parameters and one specialisation, so none of them
    // distinguish anything.
    let (design, _) = elaborate("genblk.sv", None);
    let genblk = module(&design, "genblk");

    assert!(!genblk.name.contains('$'), "got `{}`", genblk.name);
    assert_eq!(genblk.params.len(), 3);
}

// ----------------------------------------------------------------- typedefs ---

#[test]
fn an_enum_gives_its_variables_the_width_of_its_base_type() {
    // `typedef enum logic [1:0] { ... } state_e;` makes `state_e state;` two
    // bits. Before typedefs were read the width came out as one, which is the
    // kind of quiet wrongness that reaches a diagram looking plausible.
    let (design, diags) = elaborate("fsm_enum.sv", None);
    let fsm = module(&design, "fsm_enum");

    for name in ["state", "next_state"] {
        let (_, net) = fsm.net_by_name(name).unwrap_or_else(|| panic!("no net {name}"));
        assert_eq!(net.width, 2, "{name} is two bits wide");
    }
    assert!(diags.is_empty(), "and cleanly: {}", diags.render(&design.files));
}

#[test]
fn enum_members_are_constants_not_phantom_nets() {
    // `IDLE`, `RUN` and `DONE` are values. Read as signals they became
    // one-bit nets that nothing drives — 66 of them across a real design.
    let (design, _) = elaborate("fsm_enum.sv", None);
    let fsm = module(&design, "fsm_enum");

    for member in ["S_IDLE", "S_RUN", "S_DONE"] {
        assert!(fsm.net_by_name(member).is_none(), "`{member}` is an enum value, not a net");
    }
}

#[test]
fn enum_members_number_from_zero_and_carry_on_from_an_explicit_value() {
    // The SystemVerilog rule, and why `{ IDLE, RUN, DONE }` needs no values.
    let dir = std::env::temp_dir().join("rtlscope-enum-numbering");
    std::fs::create_dir_all(&dir).expect("temp dir");
    let path = dir.join("numbering.sv");
    std::fs::write(
        &path,
        "module numbering (output logic [3:0] a, b, c, d);\n\
         \x20   typedef enum logic [3:0] { W, X = 4'd5, Y, Z } e_t;\n\
         \x20   assign a = W;\n\
         \x20   assign b = X;\n\
         \x20   assign c = Y;\n\
         \x20   assign d = Z;\n\
         endmodule\n",
    )
    .expect("writing the generated source");

    let (design, _) = elaborate_paths(&[path], None);
    let module = module(&design, "numbering");

    // Each assign is a process whose body is `<port> = <literal>`.
    let value_of = |port: &str| -> i64 {
        let (net, _) = module.net_by_name(port).expect("the port net");
        let process = module
            .procs
            .iter()
            .find(|p| p.writes.iter().any(|w| w.net_id() == Some(net)))
            .expect("a process driving it");
        match &process.body.kind {
            rtlscope_ir::StmtKind::Assign { rhs, .. } => match &rhs.kind {
                rtlscope_ir::ExprKind::Lit { value } => value.to_u64().unwrap() as i64,
                other => panic!("expected a literal, got {other:?}"),
            },
            other => panic!("expected an assignment, got {other:?}"),
        }
    };

    assert_eq!(value_of("a"), 0, "the first member starts at zero");
    assert_eq!(value_of("b"), 5, "an explicit value is taken as written");
    assert_eq!(value_of("c"), 6, "and the next one carries on from it");
    assert_eq!(value_of("d"), 7);
}

#[test]
fn an_enum_with_no_base_type_is_an_int() {
    let dir = std::env::temp_dir().join("rtlscope-enum-default-width");
    std::fs::create_dir_all(&dir).expect("temp dir");
    let path = dir.join("default_width.sv");
    std::fs::write(
        &path,
        "module default_width (input logic clk);\n\
         \x20   typedef enum { P, Q } plain_t;\n\
         \x20   plain_t state;\n\
         endmodule\n",
    )
    .expect("writing the generated source");

    let (design, _) = elaborate_paths(&[path], None);
    let module = module(&design, "default_width");
    let (_, state) = module.net_by_name("state").expect("state");

    assert_eq!(state.width, 32, "a bare `enum` is an `int`, per IEEE 1800");
}

/// A function is not hardware until it is called, and each call is its own copy.
///
/// The nets are the evidence: `count_ones8.0.value` belongs to call site 0 and
/// to nothing else, which is what lets two calls to the same function be told
/// apart in a diagram — and what makes a loop bounded by an argument unrollable
/// at one site and not at another.
#[test]
fn a_function_becomes_logic_at_each_call_site() {
    let (design, diags) = elaborate("function.sv", Some("function_test"));
    let module = module(&design, "function_test");
    assert!(!diags.has_errors(), "{diags:?}");

    let nets: Vec<&str> = module.nets.iter().map(|n| n.name.as_str()).collect();
    for expected in [
        "count_ones8.0",       // the return value
        "count_ones8.0.value", // its one argument
        "control_code.1.a",
        "control_code.1.b",  // `input logic a, b` — two arguments, one type
        "sum_lanes.2.total", // a variable local to the function
        "scratch",           // a variable local to a `begin ... end`
    ] {
        assert!(nets.contains(&expected), "no net `{expected}`; have {nets:?}");
    }

    let width = |name: &str| module.nets.iter().find(|n| n.name == name).unwrap().width;
    // `int` carries its width in the keyword rather than in a range.
    assert_eq!(width("count_ones8.0"), 32);
    // Inherited from the argument it was declared alongside.
    assert_eq!(width("control_code.1.b"), 1);
    assert_eq!(width("sum_lanes.2.lanes"), 32);
    assert_eq!(width("scratch"), 8);
}

/// `{VC, DT}` is 8 bits only because `VC` is 2 and `DT` is 6.
///
/// Folding it with a guessed 32 bits per part overflowed an `i64` and reported
/// the localparam as unevaluable, so the declared width has to survive from the
/// parameter declaration all the way into the constant scope.
#[test]
fn a_concatenation_of_parameters_folds_by_their_declared_widths() {
    let (design, _) = elaborate("function.sv", Some("function_test"));
    let module = module(&design, "function_test");
    let di = module.params.iter().find(|p| p.name == "DI").expect("localparam DI");
    assert_eq!(di.value, 0b0110_0010, "{{2'b01, 6'h22}}");
}

/// An expression in a connection is logic, and gets the net it deserves.
#[test]
fn an_expression_connection_becomes_a_net_and_a_driver() {
    let (design, diags) = elaborate("connect_expr.sv", Some("connect_expr_top"));
    let module = module(&design, "connect_expr_top");
    assert!(!diags.has_errors(), "{diags:?}");

    let conns = connections(&design, module, "u_child");
    assert!(conns.contains(&".rst_n(u_child.rst_n)".to_string()), "{conns:?}");
    assert!(conns.contains(&".pair(u_child.pair)".to_string()), "{conns:?}");
    assert!(conns.contains(&".sel(u_child.sel)".to_string()), "{conns:?}");
    // The one plain net name is still bound directly, with no net invented.
    assert!(conns.contains(&".q(q)".to_string()), "{conns:?}");

    // Each invented net is driven, so the diagram has an edge to follow back.
    let pair = module.nets.iter().find(|n| n.name == "u_child.pair").expect("net");
    assert_eq!(pair.width, 2, "the width comes from the port it feeds");
    assert!(
        module.procs.iter().any(|p| {
            p.writes
                .iter()
                .any(|w| w.net_id().is_some_and(|n| module.net(n).name == "u_child.pair"))
        }),
        "nothing drives `u_child.pair`"
    );
}

/// `input wire [7:0] a, b` declares two eight-bit ports, not one and a bit.
#[test]
fn a_port_inherits_the_type_of_the_one_it_was_declared_with() {
    let (design, _) = elaborate("port_group.sv", None);
    let module = module(&design, "port_group");
    for name in ["a", "b", "wide", "also_wide"] {
        let port = module.ports.iter().find(|p| p.name == name).expect(name);
        assert_eq!(module.net(port.net).width, 8, "port `{name}`");
    }
}

/// A task is a function called for what it writes back.
///
/// The evidence is in the reads and writes: the caller's `picked` is written
/// even though the call names it as an argument, and the task's `output` is not
/// read on the way in.
#[test]
fn a_task_writes_back_through_its_output_arguments() {
    let (design, diags) = elaborate("task_loop.sv", Some("task_loop"));
    let module = module(&design, "task_loop");
    assert!(!diags.has_errors(), "{diags:?}");

    let process = module
        .procs
        .iter()
        .find(|p| names_of(module, &p.writes).contains(&"picked"))
        .expect("nothing writes `picked`");
    assert!(
        !names_of(module, &process.reads).contains(&"picked"),
        "an `output` argument should not be read on the way in: {:?}",
        names_of(module, &process.reads)
    );

    // A task has no result, so there is no net named after the task itself.
    let nets: Vec<&str> = module.nets.iter().map(|n| n.name.as_str()).collect();
    assert!(nets.contains(&"pick.0.byte_out"), "{nets:?}");
    assert!(!nets.contains(&"pick.0"), "a task has no return value: {nets:?}");
}

/// A loop bounded by a signal is that many copies of its body, each guarded.
///
/// `for (idx = 0; idx < n; idx++)` with `n` three bits wide is eight copies —
/// exactly a barrel shifter, and exactly what synthesis builds. Reporting it as
/// unsupported was leaving real logic out of the model.
#[test]
fn a_loop_bounded_by_a_signal_becomes_guarded_copies() {
    let (design, diags) = elaborate("task_loop.sv", Some("task_loop"));
    let module = module(&design, "task_loop");
    assert!(!diags.has_errors(), "{diags:?}");
    assert!(
        diags.iter().all(|d| d.code != rtlscope_ir::DiagCode::UnsupportedConstruct),
        "nothing here is outside the subset: {:?}",
        diags.iter().filter(|d| d.code == rtlscope_ir::DiagCode::UnsupportedConstruct).count()
    );

    let process = module
        .procs
        .iter()
        .find(|p| names_of(module, &p.writes).contains(&"rotated"))
        .expect("nothing writes `rotated`");

    // Eight guarded copies, one per value `n` can take.
    let mut guards = 0;
    process.body.for_each_stmt(&mut |stmt| {
        if matches!(&stmt.kind, rtlscope_ir::StmtKind::If { .. }) {
            guards += 1;
        }
    });
    assert_eq!(guards, 8, "one guard per iteration the loop could take");
}

/// `initial` is what the design powers up holding, not something to skip.
#[test]
fn an_initial_block_is_the_power_on_contents() {
    let (design, _) = elaborate("task_loop.sv", Some("task_loop"));
    let module = module(&design, "task_loop");
    let process = module
        .procs
        .iter()
        .find(|p| p.kind == rtlscope_ir::ProcKind::Initial)
        .expect("no initial process");
    assert_eq!(names_of(module, &process.writes), vec!["rom"]);
}

/// An early `return` settles the result and stands the rest of the body down.
#[test]
fn an_early_return_guards_the_statements_after_it() {
    let (design, _) = elaborate("task_loop.sv", Some("task_loop"));
    let module = module(&design, "task_loop");
    let nets: Vec<&str> = module.nets.iter().map(|n| n.name.as_str()).collect();
    assert!(
        nets.contains(&"rate.2.returned$"),
        "an early return needs a flag to guard by: {nets:?}"
    );

    // Only functions that leave early get one; the rest stay plain.
    assert!(
        !nets.iter().any(|n| n.starts_with("ones4") && n.ends_with("returned$")),
        "a function with no early return should have no flag: {nets:?}"
    );
}

/// A connection has to be reachable, as text, from the elaborated design.
///
/// This is the whole point of `Conn.span`. Everything that shows a wire holds a
/// `Design` — the window does, the agent does — and the span that names a
/// connection used to live only on the un-elaborated UIR, so there was no way
/// to get from "this wire in the diagram" to "this line in the file".
#[test]
fn a_written_connection_leads_back_to_the_line_that_wrote_it() {
    let (design, _) = elaborate("hier.sv", Some("hier_top"));
    let top = design.top_module();
    let inst = top.insts.iter().find(|i| i.name == "u_alu").expect("hier_top has u_alu");
    let child = design.module(inst.of);

    let op = inst
        .conns
        .iter()
        .find(|conn| child.ports[conn.port.0 as usize].name == "op")
        .expect("u_alu.op is connected");

    assert!(!op.span.is_unknown(), "`.op (op)` is written, so it has a place");
    let path = design.files.path(op.span.file).expect("a known file");
    let source = std::fs::read_to_string(path).expect("readable");
    let line = source.lines().nth(op.span.line as usize - 1).expect("the line is there");
    assert!(line.contains(".op"), "the span lands on the connection: {line:?}");
}

/// A port bound by `.*` has no place of its own, and says so.
///
/// `hier_ctrl u_ctrl (.*);` binds four ports and writes none of them. Handing
/// back a position for one would point at the `.*`, which is where four
/// connections were made rather than where this one was.
#[test]
fn a_wildcard_connection_has_no_place_of_its_own() {
    let (design, _) = elaborate("hier.sv", Some("hier_top"));
    let top = design.top_module();
    let inst = top.insts.iter().find(|i| i.name == "u_ctrl").expect("hier_top has u_ctrl");

    assert!(!inst.conns.is_empty(), "`.*` did bind its ports");
    for conn in &inst.conns {
        assert!(conn.span.is_unknown(), "nothing was written for this port");
    }
}

/// A port written `.port()` is a decision, not an oversight.
///
/// Nothing is connected either way, so it looks the same in the design — but
/// the author said so out loud, and reporting it back as "not connected" is
/// telling them what they just wrote.
#[test]
fn a_port_written_open_binds_silently_and_is_not_warned_about() {
    let dir = std::env::temp_dir().join("rtlscope-open-port");
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("open_port.sv");
    std::fs::write(
        &path,
        "module child (input logic a, output logic y);\n\
         assign y = a;\n\
         endmodule\n\
         module open_top (input logic a);\n\
         child u_child (.a (a), .y ());\n\
         endmodule\n",
    )
    .expect("writes");

    let (uir, _) = rtlscope_sv::lower_files(
        std::slice::from_ref(&path),
        &rtlscope_sv::ParseOptions::default(),
    );
    let (design, diags) = rtlscope_elab::elaborate(&uir, Some("open_top"));
    let design = design.expect("elaborates");

    let top = design.top_module();
    let inst = &top.insts[0];
    assert_eq!(inst.conns.len(), 1, "only `a` is connected: {:?}", inst.conns);

    let said: Vec<String> = diags.iter().map(|diag| diag.message.clone()).collect();
    assert!(
        !said.iter().any(|message| message.contains("not connected")),
        "`.y ()` is what the author asked for: {said:?}"
    );
    let _ = std::fs::remove_file(&path);
}

/// A positional connection is an expression, and the expression is the text.
#[test]
fn a_positional_connection_points_at_its_expression() {
    let (design, _) = elaborate("hier.sv", Some("hier_top"));
    let top = design.top_module();
    let inst = top.insts.iter().find(|i| i.name == "u_rf").expect("hier_top has u_rf");
    let child = design.module(inst.of);

    let wdata = inst
        .conns
        .iter()
        .find(|conn| child.ports[conn.port.0 as usize].name == "wdata")
        .expect("u_rf's fourth port is connected positionally");

    assert!(!wdata.span.is_unknown(), "the expression is written, so it has a place");
    let path = design.files.path(wdata.span.file).expect("a known file");
    let source = std::fs::read_to_string(path).expect("readable");
    let line = source.lines().nth(wdata.span.line as usize - 1).expect("the line is there");
    assert!(line.contains("alu_y"), "the span lands on the expression: {line:?}");
}

// ------------------------------------------------------------- packages ---

fn net_width(module: &rtlscope_ir::Module, name: &str) -> u32 {
    module
        .nets
        .iter()
        .find(|net| net.name == name)
        .unwrap_or_else(|| {
            let names: Vec<&str> = module.nets.iter().map(|n| n.name.as_str()).collect();
            panic!("no net `{name}` in `{}`; have {names:?}", module.name)
        })
        .width
}

/// Names from a package resolve however they are reached: through `import
/// pkg::*`, through `import pkg::name`, or spelled out as `pkg::name` — in a
/// port's type, a port's range, a parameter default, a case label, a
/// part-select, and a package that imports another.
#[test]
fn package_names_resolve_by_import_and_by_qualification() {
    let (design, diags) = elaborate("packages.sv", None);
    assert!(!diags.has_errors(), "{diags:?}");
    let unresolved: Vec<_> = diags
        .iter()
        .filter(|d| {
            matches!(
                d.code,
                rtlscope_ir::DiagCode::UnknownDataType
                    | rtlscope_ir::DiagCode::ImplicitNet
                    | rtlscope_ir::DiagCode::UnsupportedConstruct
                    | rtlscope_ir::DiagCode::PackageNotFound
            )
        })
        .collect();
    assert!(unresolved.is_empty(), "{unresolved:#?}");

    let engine = module(&design, "engine");
    assert_eq!(net_width(engine, "mode"), 2, "an imported enum type on a port");
    assert_eq!(net_width(engine, "data"), 8, "an imported constant in a port's range");
    assert_eq!(net_width(engine, "beat"), 9, "a packed struct is its members added up");
    let stages = engine.params.iter().find(|p| p.name == "STAGES").expect("STAGES");
    assert_eq!(stages.value, 4, "a parameter default from a package");

    let qualified = module(&design, "qualified");
    assert_eq!(net_width(qualified, "mode"), 2, "`defs::mode_t`");
    assert_eq!(
        net_width(qualified, "wide"),
        16,
        "`more::TWICE`, through a package that imports another"
    );
    assert_eq!(net_width(qualified, "out"), 8, "`defs::WIDTH`");

    let picked = module(&design, "picked");
    assert_eq!(net_width(picked, "narrow"), 8, "`import defs::WIDTH`");
    assert_eq!(net_width(picked, "g.wide"), 16, "an import inside a generate block");
    assert_eq!(net_width(picked, "count"), 5, "`more::LIMIT` is `defs::DEPTH + 1`");
}

/// A package function is copied into its call site like a module's own, and
/// its body sees the package's constants — whether or not the caller imported
/// them. `double` is `logic [WIDTH-1:0]` in a module that never imported
/// `WIDTH`.
#[test]
fn a_package_function_is_inlined_with_its_own_constants_in_reach() {
    let (design, diags) = elaborate("packages.sv", None);
    for name in ["engine", "qualified"] {
        let caller = module(&design, name);
        assert_eq!(net_width(caller, "double.0"), 8, "the result, in `{name}`");
        assert_eq!(net_width(caller, "double.0.x"), 8, "the argument, in `{name}`");
    }
    assert!(!diags.iter().any(|d| d.message.contains("is not a task or function")), "{diags:?}");
}

/// The enum comes along under both spellings, so a state or a value typed
/// with it can be named by whichever the declaration used.
#[test]
fn a_package_enum_names_the_values_of_the_module_that_uses_it() {
    let (design, _) = elaborate("packages.sv", None);
    let engine = module(&design, "engine");
    assert_eq!(engine.enum_name(Some("mode_t"), 2), Some("FAST"), "imported, so bare");
    assert_eq!(engine.enum_name(Some("defs::mode_t"), 0), Some("OFF"), "and qualified");
    let qualified = module(&design, "qualified");
    assert_eq!(qualified.enum_name(Some("defs::mode_t"), 1), Some("SLOW"));
    assert_eq!(qualified.enum_name(Some("mode_t"), 1), None, "not imported there");
}

/// An import of a package that is in none of the files is said once, with the
/// line, rather than surfacing as an unknown type for every name it would have
/// brought.
#[test]
fn an_import_of_a_package_that_was_never_read_is_reported() {
    let dir = std::env::temp_dir().join("rtlscope-missing-package");
    std::fs::create_dir_all(&dir).expect("temp dir");
    let path = dir.join("missing.sv");
    std::fs::write(
        &path,
        "module missing (input logic clk, output logic [3:0] q);
    import nowhere::*;
    always_ff @(posedge clk) q <= q + 1'b1;
endmodule
",
    )
    .expect("write");
    let (_, diags) = elaborate_paths(&[path], None);
    let missing: Vec<_> =
        diags.iter().filter(|d| d.code == rtlscope_ir::DiagCode::PackageNotFound).collect();
    assert_eq!(missing.len(), 1, "{diags:?}");
    assert!(missing[0].message.contains("`nowhere`"), "{}", missing[0].message);
    assert_eq!(missing[0].span.map(|s| s.line), Some(2));
}

// ----------------------------------------------------------- interfaces ---

/// A port of an interface unfolds into one port per signal the modport
/// lists, with the modport's direction; the bundle remembers they were one.
#[test]
fn an_interface_port_unfolds_into_the_modports_signals() {
    let (design, diags) = elaborate("interfaces.sv", None);
    assert!(diags.is_empty(), "{diags:?}");
    let producer = module(&design, "producer");
    let ports: Vec<String> =
        producer.ports.iter().map(|p| format!("{:?} {}", p.dir, p.name)).collect();
    assert_eq!(
        ports,
        [
            "Input m.clk",
            "Output m.data",
            "Output m.valid",
            "Input m.ready",
            "Input m.fire",
            "Input rst_n"
        ]
    );
    assert_eq!(net_width(producer, "m.data"), 8, "sized by the interface's parameter");
    assert_eq!(producer.bundles.len(), 1);
    let bundle = &producer.bundles[0];
    assert_eq!(bundle.name, "m");
    assert_eq!(bundle.interface, "bus_if");
    assert_eq!(bundle.modport.as_deref(), Some("master"));
    assert_eq!(bundle.ports.len(), 5);
    assert!(bundle.ports.iter().all(|p| producer.ports[p.0 as usize].name.starts_with("m.")));

    // The signals are the module's logic: the register is clocked by `m.clk`
    // and reads and writes the bundle's nets.
    let register = producer
        .procs
        .iter()
        .find(|p| names_of(producer, &p.writes).contains(&"m.data"))
        .expect("the register");
    let reads = names_of(producer, &register.reads);
    assert!(reads.contains(&"m.clk"), "{reads:?}");
    assert!(reads.contains(&"m.fire"), "{reads:?}");
}

/// An instance of an interface is its signals as nets, and its own logic as
/// the module's; `.m(bus)` wires a child's bundle to them one signal at a time.
#[test]
fn an_interface_instance_is_nets_and_a_bundle_connection_is_wired_per_signal() {
    let (design, _) = elaborate("interfaces.sv", None);
    let top = module(&design, "interfaces_top");
    for (name, width) in
        [("bus.clk", 1), ("bus.data", 8), ("bus.valid", 1), ("bus.ready", 1), ("bus.fire", 1)]
    {
        assert_eq!(net_width(top, name), width, "{name}");
    }
    assert_eq!(top.iface_insts.len(), 1);
    assert_eq!(top.iface_insts[0].name, "bus");
    assert_eq!(top.iface_insts[0].interface, "bus_if");
    assert_eq!(top.iface_insts[0].nets.len(), 5);

    // `assign fire = valid && ready;` inside the interface became a process of
    // the module that instantiated it, and `.clk(clk)` drives `bus.clk`.
    let writes: Vec<String> = top
        .procs
        .iter()
        .flat_map(|p| names_of(top, &p.writes).into_iter().map(str::to_string))
        .collect();
    assert!(writes.contains(&"bus.fire".to_string()), "{writes:?}");
    assert!(writes.contains(&"bus.clk".to_string()), "{writes:?}");

    let conns = connections(&design, top, "u_producer");
    assert_eq!(
        conns,
        [
            ".m.clk(bus.clk)",
            ".m.data(bus.data)",
            ".m.valid(bus.valid)",
            ".m.ready(bus.ready)",
            ".m.fire(bus.fire)",
            ".rst_n(rst_n)"
        ]
    );
}

/// `interface.slave watched` is whatever was connected: the bundle's
/// interface comes from the instantiation, the modport from the declaration.
#[test]
fn a_generic_interface_port_takes_the_interface_it_is_given() {
    let (design, _) = elaborate("interfaces.sv", None);
    let monitor = module(&design, "monitor");
    let bundle = &monitor.bundles[0];
    assert_eq!(bundle.name, "watched");
    assert_eq!(bundle.interface, "bus_if");
    assert_eq!(bundle.modport.as_deref(), Some("slave"));
    assert_eq!(net_width(monitor, "watched.data"), 8);
    let conns = connections(&design, module(&design, "interfaces_top"), "u_monitor");
    assert!(conns.contains(&".watched.valid(bus.valid)".to_string()), "{conns:?}");
}

/// A name that reaches into an instance is not a wire of this module. It is
/// said so, rather than silently becoming a net called `u_sub.count`.
#[test]
fn a_reference_into_an_instance_is_reported_as_hierarchical() {
    let dir = std::env::temp_dir().join("rtlscope-hierarchical-ref");
    std::fs::create_dir_all(&dir).expect("temp dir");
    let path = dir.join("hier_ref.sv");
    std::fs::write(
        &path,
        "module sub (input logic clk, output logic [3:0] count);
    always_ff @(posedge clk) count <= count + 1'b1;
endmodule
module hier_ref (input logic clk, output logic peek);
    sub u_sub (.clk(clk), .count());
    assign peek = u_sub.count[0];
endmodule
",
    )
    .expect("write");
    let (_, diags) = elaborate_paths(&[path], Some("hier_ref"));
    let reported: Vec<&str> = diags
        .iter()
        .filter(|d| d.message.contains("reaches into an instance"))
        .map(|d| d.message.as_str())
        .collect();
    assert_eq!(reported.len(), 1, "{diags:?}");
    assert!(reported[0].contains("`u_sub.count`"), "{}", reported[0]);
}
