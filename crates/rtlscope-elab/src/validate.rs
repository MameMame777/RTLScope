//! Checks the three invariants an elaborated design is supposed to hold.
//!
//! Everything reported here is a bug in RTLScope, not in the user's RTL — which
//! is exactly why it runs. The invariants are what every consumer downstream is
//! allowed to assume: the graph builder assumes `reads`/`writes` describe the
//! body, "jump to source" assumes every node has a span, and the diagram
//! assumes no width is still an unevaluated expression. A violation that slips
//! through does not crash, it draws the wrong picture.

use rtlscope_ir::{Conn, Design, Diag, DiagCode, Diagnostics, Module, NetRef, Severity};

use crate::proc::derive_reads_writes;

pub fn validate(design: &Design) -> Diagnostics {
    let mut diags = Diagnostics::new();

    if design.modules.get(design.top).is_none() {
        diags.push(Diag::error(DiagCode::InvariantViolation, "top module index is out of range"));
        return diags;
    }

    for (id, module) in design.modules.iter_enumerated() {
        let where_ = format!("module `{}`", module.name);
        spans_are_resolvable(design, module, &where_, &mut diags);
        references_are_in_range(design, module, &where_, &mut diags);
        reads_and_writes_match_the_body(module, &where_, &mut diags);
        widths_are_resolved(module, &where_, &mut diags);
        let _ = id;
    }

    diags
}

/// Invariant 1: every node carries a span that names a real file.
fn spans_are_resolvable(design: &Design, module: &Module, where_: &str, diags: &mut Diagnostics) {
    check_span(design, module.span, where_, "the module itself", diags);
    for net in module.nets.iter() {
        check_span(design, net.span, where_, &format!("net `{}`", net.name), diags);
    }
    for inst in &module.insts {
        check_span(design, inst.span, where_, &format!("instance `{}`", inst.name), diags);
    }
    for (index, process) in module.procs.iter().enumerate() {
        check_span(design, process.span, where_, &format!("process #{index}"), diags);
        process.body.for_each_stmt(&mut |stmt| {
            if stmt.span.is_unknown() {
                diags.push(Diag::error(
                    DiagCode::InvariantViolation,
                    format!("{where_}: a statement in process #{index} has no location"),
                ));
            }
        });
    }
}

fn check_span(
    design: &Design,
    span: rtlscope_ir::Span,
    where_: &str,
    what: &str,
    diags: &mut Diagnostics,
) {
    if span.is_unknown() || design.files.path(span.file).is_none() {
        diags.push(Diag::error(
            DiagCode::InvariantViolation,
            format!("{where_}: {what} has no resolvable source location"),
        ));
    }
}

/// Every index actually points at something.
fn references_are_in_range(
    design: &Design,
    module: &Module,
    where_: &str,
    diags: &mut Diagnostics,
) {
    for port in &module.ports {
        if module.nets.get(port.net).is_none() {
            diags.push(Diag::error(
                DiagCode::InvariantViolation,
                format!("{where_}: port `{}` points at no net", port.name),
            ));
        }
    }

    for inst in &module.insts {
        let Some(child) = design.modules.get(inst.of) else {
            diags.push(Diag::error(
                DiagCode::InvariantViolation,
                format!("{where_}: instance `{}` refers to no module", inst.name),
            ));
            continue;
        };
        for Conn { port, net: net_ref, .. } in &inst.conns {
            if child.ports.get(port.0 as usize).is_none() {
                diags.push(Diag::error(
                    DiagCode::InvariantViolation,
                    format!(
                        "{where_}: instance `{}` binds a port `{}` has not got",
                        inst.name, child.name
                    ),
                ));
            }
            check_net(module, net_ref, where_, &format!("a connection of `{}`", inst.name), diags);
        }
    }

    for (index, process) in module.procs.iter().enumerate() {
        for net_ref in process.reads.iter().chain(&process.writes) {
            check_net(module, net_ref, where_, &format!("process #{index}"), diags);
        }
    }
}

fn check_net(module: &Module, net_ref: &NetRef, where_: &str, what: &str, diags: &mut Diagnostics) {
    if let Some(net) = net_ref.net_id()
        && module.nets.get(net).is_none()
    {
        diags.push(Diag::error(
            DiagCode::InvariantViolation,
            format!("{where_}: {what} refers to a net outside this module"),
        ));
    }
}

/// Invariant 2: the two vectors are what a traversal of the body yields.
fn reads_and_writes_match_the_body(module: &Module, where_: &str, diags: &mut Diagnostics) {
    for (index, process) in module.procs.iter().enumerate() {
        let (reads, writes) = derive_reads_writes(&process.body, &process.kind);

        for (label, stored, derived) in
            [("reads", &process.reads, &reads), ("writes", &process.writes, &writes)]
        {
            let stored_nets = net_set(stored);
            let derived_nets = net_set(derived);
            if stored_nets != derived_nets {
                diags.push(Diag::error(
                    DiagCode::InvariantViolation,
                    format!(
                        "{where_}: process #{index} `{label}` does not match its body \
                         ({} stored, {} derived)",
                        stored_nets.len(),
                        derived_nets.len()
                    ),
                ));
            }
        }
    }
}

/// Invariant 3: nothing is still an unevaluated expression.
fn widths_are_resolved(module: &Module, where_: &str, diags: &mut Diagnostics) {
    for net in module.nets.iter() {
        if net.width == 0 {
            diags.push(Diag::error(
                DiagCode::InvariantViolation,
                format!("{where_}: net `{}` has zero width", net.name),
            ));
        }
        if let rtlscope_ir::NetKind::Memory { depth } = net.kind
            && depth == 0
        {
            diags.push(Diag::error(
                DiagCode::InvariantViolation,
                format!("{where_}: memory `{}` has zero depth", net.name),
            ));
        }
    }
}

fn net_set(refs: &[NetRef]) -> std::collections::BTreeSet<rtlscope_ir::NetId> {
    refs.iter().filter_map(NetRef::net_id).collect()
}

/// Convenience for tests and the CLI: did validation find anything?
pub fn is_clean(diags: &Diagnostics) -> bool {
    diags.count(Severity::Error) == 0
}
