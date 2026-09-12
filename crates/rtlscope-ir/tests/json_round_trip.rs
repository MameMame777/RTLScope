//! Pins the JSON wire format of the IR.
//!
//! `rtlscope dump-ir` emits this and the Phase 6 MCP server will serve it, so the
//! shape is a public contract from the first release. A refactor that changes a
//! field name or an enum tag has to break this test on the way past.

use rtlscope_ir::*;

fn build_design() -> Design {
    let mut files = FileTable::new();
    let file = files.intern("tests/fixtures/counter.sv");
    let sp = |line: u32| Span::new(file, line, 5, 4);

    let mut modules = Arena::new();

    // A child with only a header — the shape an IP stub or a missing file takes.
    let child = modules.alloc(Module {
        enums: Vec::new(),
        bundles: Vec::new(),
        iface_insts: Vec::new(),
        name: "child".into(),
        base_name: "child".into(),
        written: None,
        params: Vec::new(),
        ports: Vec::new(),
        nets: Arena::new(),
        insts: Vec::new(),
        procs: Vec::new(),
        skipped: Vec::new(),
        is_blackbox: true,
        span: sp(1),
    });

    let mut nets = Arena::new();
    let clk = nets.alloc(Net {
        name: "clk".into(),
        written: None,
        width: 1,
        kind: NetKind::Logic,
        type_name: None,
        synthesised: false,
        span: sp(4),
    });
    let rst_n = nets.alloc(Net {
        name: "rst_n".into(),
        written: None,
        width: 1,
        kind: NetKind::Logic,
        type_name: None,
        synthesised: false,
        span: sp(5),
    });
    let en = nets.alloc(Net {
        name: "en".into(),
        written: None,
        width: 1,
        kind: NetKind::Logic,
        type_name: None,
        synthesised: false,
        span: sp(6),
    });
    let count = nets.alloc(Net {
        name: "count".into(),
        written: None,
        width: 8,
        kind: NetKind::Logic,
        type_name: None,
        synthesised: false,
        span: sp(7),
    });
    let mem = nets.alloc(Net {
        name: "mem".into(),
        written: None,
        width: 8,
        kind: NetKind::Memory { depth: 16 },
        type_name: None,
        synthesised: false,
        span: sp(8),
    });

    let full = |net: NetId| NetRef::Full { net };
    let lit = |w: u32, v: u64, line: u32| {
        Expr::new(ExprKind::Lit { value: ConstBits::from_u64(w, v) }, sp(line))
    };
    let read = |net: NetId, line: u32| Expr::new(ExprKind::Ref { net: full(net) }, sp(line));

    // if (!rst_n) count <= 0; else if (en) count <= count + 1;
    let body = Stmt::new(
        StmtKind::If {
            cond: Expr::new(
                ExprKind::Unary { op: UnOp::LogNot, operand: Box::new(read(rst_n, 11)) },
                sp(11),
            ),
            then_branch: Box::new(Stmt::new(
                StmtKind::Assign {
                    lhs: full(count),
                    lhs_index: None,
                    rhs: lit(8, 0, 12),
                    blocking: false,
                },
                sp(12),
            )),
            else_branch: Some(Box::new(Stmt::new(
                StmtKind::If {
                    cond: read(en, 13),
                    then_branch: Box::new(Stmt::new(
                        StmtKind::Assign {
                            lhs: full(count),
                            lhs_index: None,
                            rhs: Expr::new(
                                ExprKind::Binary {
                                    op: BinOp::Add,
                                    lhs: Box::new(read(count, 14)),
                                    rhs: Box::new(lit(1, 1, 14)),
                                },
                                sp(14),
                            ),
                            blocking: false,
                        },
                        sp(14),
                    )),
                    else_branch: None,
                },
                sp(13),
            ))),
        },
        sp(11),
    );

    let top = modules.alloc(Module {
        enums: Vec::new(),
        bundles: Vec::new(),
        iface_insts: Vec::new(),
        name: "counter$W=8".into(),
        base_name: "counter".into(),
        written: None,
        params: vec![Param {
            name: "W".into(),
            written: None,
            value: 8,
            is_local: false,
            width: None,
            span: sp(3),
        }],
        ports: vec![
            Port { name: "clk".into(), written: None, dir: PortDir::Input, net: clk, span: sp(4) },
            Port {
                name: "rst_n".into(),
                written: None,
                dir: PortDir::Input,
                net: rst_n,
                span: sp(5),
            },
            Port { name: "en".into(), written: None, dir: PortDir::Input, net: en, span: sp(6) },
            Port {
                name: "count".into(),
                written: None,
                dir: PortDir::Output,
                net: count,
                span: sp(7),
            },
        ],
        nets,
        insts: vec![Instance {
            name: "u_child".into(),
            written: None,
            of: child,
            conns: vec![
                Conn::new(PortId(0), full(clk), sp(9)),
                Conn::new(PortId(1), NetRef::Slice { net: count, msb: 3, lsb: 0 }, sp(10)),
                // Bound by `.*`, so nothing was written for it and the span
                // says so. The round trip has to keep that.
                Conn::new(
                    PortId(2),
                    NetRef::Const { value: ConstBits::from_u64(2, 3) },
                    Span::UNKNOWN,
                ),
            ],
            span: sp(9),
        }],
        procs: vec![Process {
            kind: ProcKind::Ff {
                clk: full(clk),
                edge: Edge::Pos,
                rst: Some(Reset { net: full(rst_n), kind: ResetKind::Sync, active: Level::Low }),
            },
            reads: vec![full(rst_n), full(en), full(count)],
            writes: vec![full(count)],
            body,
            span: sp(10),
        }],
        skipped: vec![Skipped { construct: "initial".into(), span: sp(20) }],
        is_blackbox: false,
        span: sp(2),
    });

    let _ = mem;

    // Populated, not left default: this test exists so that a new field cannot
    // be added without passing through it, and a field that serialises to
    // nothing would slip by.

    Design { modules, top, files, generated: Vec::new() }
}

#[test]
fn design_survives_a_json_round_trip() {
    let design = build_design();
    let json = serde_json::to_string(&design).expect("serialise");
    let back: Design = serde_json::from_str(&json).expect("deserialise");
    assert_eq!(back, design);
}

#[test]
fn top_level_shape_is_stable() {
    let design = build_design();
    let value: serde_json::Value = serde_json::to_value(&design).unwrap();

    let obj = value.as_object().expect("design is an object");
    let mut keys: Vec<&str> = obj.keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(keys, ["files", "modules", "top"]);

    assert!(value["modules"].is_array(), "arena serialises as a bare array");
    assert!(value["top"].is_number(), "an index serialises as a bare integer");
    assert_eq!(value["files"][0], "tests/fixtures/counter.sv");
}

#[test]
fn tagged_enums_keep_their_discriminators() {
    let design = build_design();
    let value: serde_json::Value = serde_json::to_value(&design).unwrap();
    let top = &value["modules"][1];

    assert_eq!(top["procs"][0]["kind"]["kind"], "Ff");
    assert_eq!(top["procs"][0]["kind"]["rst"]["kind"], "sync");
    assert_eq!(top["procs"][0]["kind"]["rst"]["active"], "low");
    assert_eq!(top["procs"][0]["body"]["stmt"], "If");
    assert_eq!(top["procs"][0]["body"]["cond"]["node"], "Unary");

    // A connection is an object now, not a pair: it carries where it was
    // written as well as what it binds.
    assert_eq!(top["insts"][0]["conns"][0]["net"]["kind"], "Full");
    assert_eq!(top["insts"][0]["conns"][1]["net"]["kind"], "Slice");
    assert_eq!(top["insts"][0]["conns"][2]["net"]["kind"], "Const");
    assert!(top["insts"][0]["conns"][0]["span"]["line"].is_number());

    assert_eq!(top["nets"][4]["kind"]["kind"], "Memory");
    assert_eq!(top["nets"][4]["kind"]["depth"], 16);
    assert_eq!(top["ports"][0]["dir"], "input");
}

#[test]
fn empty_skip_lists_stay_out_of_the_json() {
    let design = build_design();
    let value: serde_json::Value = serde_json::to_value(&design).unwrap();
    assert!(value["modules"][0].get("skipped").is_none(), "a clean module carries no noise");
    assert_eq!(value["modules"][1]["skipped"][0]["construct"], "initial");
}

#[test]
fn every_node_carries_a_resolvable_span() {
    let design = build_design();
    let module = design.top_module();

    for net in module.nets.iter() {
        assert!(!net.span.is_unknown(), "net {} has no span", net.name);
        assert!(design.files.path(net.span.file).is_some());
    }
    for inst in &module.insts {
        assert!(!inst.span.is_unknown(), "instance {} has no span", inst.name);
    }
    for proc in &module.procs {
        assert!(!proc.span.is_unknown());
        proc.body.for_each_stmt(&mut |s| assert!(!s.span.is_unknown()));
    }
}

#[test]
fn reads_and_writes_match_a_traversal_of_the_body() {
    // The real check lives in rtlscope-elab::validate; this pins the shape the
    // checker relies on, so the two cannot drift before that crate exists.
    let design = build_design();
    let proc = &design.top_module().procs[0];

    let mut assigned = Vec::new();
    let mut referenced = Vec::new();
    proc.body.for_each_stmt(&mut |stmt| match &stmt.kind {
        StmtKind::Assign { lhs, lhs_index, rhs, .. } => {
            assigned.extend(lhs.net_id());
            if let Some(index) = lhs_index {
                index.for_each_ref(&mut |r| referenced.extend(r.net_id()));
            }
            rhs.for_each_ref(&mut |r| referenced.extend(r.net_id()));
        }
        StmtKind::If { cond, .. } => cond.for_each_ref(&mut |r| referenced.extend(r.net_id())),
        StmtKind::Case { subject, .. } => {
            subject.for_each_ref(&mut |r| referenced.extend(r.net_id()));
        }
        StmtKind::Block { .. } | StmtKind::Unsupported { .. } => {}
    });

    assigned.sort();
    assigned.dedup();
    referenced.sort();
    referenced.dedup();

    let mut declared_writes: Vec<NetId> = proc.writes.iter().filter_map(NetRef::net_id).collect();
    declared_writes.sort();
    let mut declared_reads: Vec<NetId> = proc.reads.iter().filter_map(NetRef::net_id).collect();
    declared_reads.sort();

    assert_eq!(assigned, declared_writes);
    assert_eq!(referenced, declared_reads);
}
