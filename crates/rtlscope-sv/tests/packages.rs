//! Packages: what the front end keeps of them, and how a qualified name is
//! spelled on its way to elaboration.

use rtlscope_ir::{UDesign, UItem};
use rtlscope_sv::ParseOptions;

fn lowered() -> UDesign {
    let path = rtlscope_fixtures::path("packages.sv");
    let (design, diags) = rtlscope_sv::lower_files(&[path], &ParseOptions::default());
    assert!(!diags.has_errors(), "{diags:?}");
    design
}

/// The whole module as text, for asking whether a spelling is in it anywhere:
/// the tree of expressions is deep, and the question is only whether the name
/// arrived whole.
fn text_of(design: &UDesign, module: &str) -> String {
    let module = design.module_by_name(module).unwrap_or_else(|| panic!("module `{module}`"));
    serde_json::to_string(module).expect("serialisable")
}

#[test]
fn a_package_is_kept_with_its_parameters_types_and_functions() {
    let design = lowered();
    let names: Vec<&str> = design.packages.iter().map(|p| p.name.as_str()).collect();
    assert_eq!(names, ["defs", "more", "walk_pkg"], "in the order they were read");

    let defs = &design.packages[0];
    let params: Vec<&str> = defs
        .items
        .iter()
        .filter_map(|item| match item {
            UItem::Param { param } => Some(param.name.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(params, ["WIDTH", "DEPTH"]);
    assert!(
        defs.items.iter().any(|item| matches!(
            item,
            UItem::TypedefEnum { name, members, .. } if name == "mode_t" && members.len() == 3
        )),
        "{:?}",
        defs.items
    );
    assert!(
        defs.items.iter().any(|item| matches!(
            item,
            UItem::TypedefStruct { name, members, .. } if name == "beat_t" && members.len() == 2
        )),
        "a packed struct is kept with its members"
    );
    let func = defs
        .items
        .iter()
        .find_map(|item| match item {
            UItem::Function { func } => Some(func),
            _ => None,
        })
        .expect("the function");
    assert_eq!(func.name, "double");
    assert_eq!(func.package.as_deref(), Some("defs"), "a function remembers its package");

    // A package that imports another.
    let more = &design.packages[1];
    assert_eq!(more.imports.len(), 1);
    assert_eq!(more.imports[0].package, "defs");
    assert_eq!(more.imports[0].item, None, "`*`");
}

#[test]
fn imports_are_found_wherever_they_were_written() {
    let design = lowered();
    let imports = |name: &str| -> Vec<String> {
        design
            .module_by_name(name)
            .unwrap_or_else(|| panic!("module `{name}`"))
            .imports
            .iter()
            .map(|import| format!("{}::{}", import.package, import.item.as_deref().unwrap_or("*")))
            .collect()
    };
    assert_eq!(imports("engine"), ["defs::*"], "in the header, before the parameters");
    assert_eq!(imports("qualified"), Vec::<String>::new(), "none at all");
    assert_eq!(
        imports("picked"),
        ["defs::WIDTH", "more::TWICE"],
        "one name in the body, and one inside a generate block"
    );
    assert_eq!(imports("walker"), ["walk_pkg::*"]);
}

/// `defs::WIDTH` arrives as one name, package and all. `sv-parser` gives a
/// qualified name in several shapes, and reading only the first identifier
/// out of any of them yields the *package* — which is how `defs::double(x)`
/// used to be reported as "`defs` is not a function".
#[test]
fn a_qualified_name_is_carried_whole() {
    let design = lowered();
    let qualified = design.module_by_name("qualified").expect("module");

    // A port's type, and a port's range.
    let mode = qualified.ports.iter().find(|p| p.name == "mode").expect("port");
    assert_eq!(mode.type_name.as_deref(), Some("defs::mode_t"));
    let text = text_of(&design, "qualified");
    assert!(text.contains(r#""name":"more::TWICE""#), "the range of `wide`: {text}");
    // The call, the constant beside it, and the constant inside a select.
    assert!(text.contains(r#""name":"defs::double""#), "the call: {text}");
    assert!(text.contains(r#""name":"defs::SLOW""#), "the enum member: {text}");
    assert!(text.contains(r#""name":"defs::WIDTH""#), "in the part-select: {text}");
    // And never the package on its own.
    assert!(!text.contains(r#""name":"defs""#), "{text}");
    assert!(!text.contains(r#""name":"more""#), "{text}");

    // The bare spelling, where the author imported the package.
    let engine = design.module_by_name("engine").expect("module");
    let mode = engine.ports.iter().find(|p| p.name == "mode").expect("port");
    assert_eq!(mode.type_name.as_deref(), Some("mode_t"));
    let text = text_of(&design, "engine");
    assert!(text.contains(r#""name":"double""#), "{text}");
}

/// A `typedef struct` used to fall through to the net declaration pass, which
/// found the members' names and declared a net for each of them in the module.
#[test]
fn a_struct_typedef_declares_no_nets() {
    let design = lowered();
    let defs = &design.packages[0];
    assert!(!defs.items.iter().any(|item| matches!(item, UItem::Net { .. })), "{:?}", defs.items);
}
