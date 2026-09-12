//! Interfaces: what the front end keeps of them, and how a port of one and a
//! signal reached through one are spelled on their way to elaboration.

use rtlscope_ir::{PortDir, UDesign, UItem};
use rtlscope_sv::ParseOptions;

fn lowered() -> UDesign {
    let path = rtlscope_fixtures::path("interfaces.sv");
    let (design, diags) = rtlscope_sv::lower_files(&[path], &ParseOptions::default());
    assert!(!diags.has_errors(), "{diags:?}");
    design
}

#[test]
fn an_interface_is_kept_with_its_parameters_ports_signals_and_modports() {
    let design = lowered();
    assert_eq!(design.interfaces.len(), 1, "{:?}", design.interfaces);
    let bus = &design.interfaces[0];
    assert_eq!(bus.name, "bus_if");
    assert_eq!(bus.params.len(), 1);
    assert_eq!(bus.params[0].name, "W");
    let ports: Vec<&str> = bus.ports.iter().map(|p| p.name.as_str()).collect();
    assert_eq!(ports, ["clk"], "the interface's own port");

    let nets: Vec<&str> = bus
        .items
        .iter()
        .filter_map(|item| match item {
            UItem::Net { net } => Some(net.name.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(nets, ["data", "valid", "ready", "fire"]);
    assert!(
        bus.items.iter().any(|item| matches!(item, UItem::Assign { .. })),
        "the interface's own logic is kept: {:?}",
        bus.items
    );

    let modports: Vec<&str> = bus.modports.iter().map(|m| m.name.as_str()).collect();
    assert_eq!(modports, ["master", "slave"]);
    let master = &bus.modports[0];
    let members: Vec<(&str, PortDir)> =
        master.members.iter().map(|m| (m.name.as_str(), m.dir)).collect();
    assert_eq!(
        members,
        [
            ("clk", PortDir::Input),
            ("data", PortDir::Output),
            ("valid", PortDir::Output),
            ("ready", PortDir::Input),
            ("fire", PortDir::Input),
        ]
    );
    assert!(design.modules.iter().all(|m| m.name != "bus_if"), "an interface is not a module");
}

/// `bus_if.master m`, `interface.slave watched`: a port of an interface says
/// what it is a port of, and nothing about a width, which it has none of.
#[test]
fn a_port_of_an_interface_names_the_interface_and_the_modport() {
    let design = lowered();
    let producer = design.module_by_name("producer").expect("producer");
    let m = producer.ports.iter().find(|p| p.name == "m").expect("port m");
    let iface = m.iface.as_ref().expect("an interface port");
    assert_eq!(iface.interface.as_deref(), Some("bus_if"));
    assert_eq!(iface.modport.as_deref(), Some("master"));
    assert_eq!(m.dir, None);
    assert_eq!(m.packed, None);

    let monitor = design.module_by_name("monitor").expect("monitor");
    let watched = monitor.ports.iter().find(|p| p.name == "watched").expect("port watched");
    let iface = watched.iface.as_ref().expect("an interface port");
    assert_eq!(iface.interface, None, "generic: whatever is connected");
    assert_eq!(iface.modport.as_deref(), Some("slave"));

    let rst_n = producer.ports.iter().find(|p| p.name == "rst_n").expect("port rst_n");
    assert_eq!(rst_n.iface, None, "a wire is still a wire");
}

/// `m.data` is one name, dot included — the net a signal of an interface
/// becomes — on both sides of an assignment and in a sensitivity list.
#[test]
fn a_signal_reached_through_an_interface_is_one_dotted_name() {
    let design = lowered();
    let producer = design.module_by_name("producer").expect("producer");
    let text = serde_json::to_string(producer).expect("serialisable");
    for name in ["m.clk", "m.data", "m.valid", "m.fire"] {
        assert!(text.contains(&format!(r#""name":"{name}""#)), "{name} in {text}");
    }
    // The bare `m` appears once — as the port's own name — and never as a
    // signal: that would be a phantom one-bit net.
    assert_eq!(text.matches(r#""name":"m""#).count(), 1, "{text}");

    let consumer = design.module_by_name("consumer").expect("consumer");
    let target = consumer
        .items
        .iter()
        .find_map(|item| match item {
            UItem::Assign { lhs, .. } => Some(lhs),
            _ => None,
        })
        .expect("assign s.ready = rst_n");
    assert_eq!(format!("{target}"), "s.ready");
}
